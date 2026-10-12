pub mod api;
pub mod backoff;
pub mod clients;
pub mod config;
pub mod copied_counter;
pub mod copy_ledger;
pub mod execution_journal;
pub mod fees;
pub mod latency;
pub mod listener;
pub mod live_fills;
pub mod live_redemption;
pub mod log_capture;
pub mod markets;
pub mod models;
pub mod order_watcher;
pub mod position_scanner;
pub mod prices;
pub mod redemption;
pub mod redemption_settlement;
pub mod risk;
pub mod rtds;
pub mod simulation;
pub mod state;
pub mod strategy;
pub mod ui;
pub mod utils;
pub mod wallet_sync;

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing_subscriber::prelude::*;

fn get_latest_modified_time(
    path: &std::path::Path,
) -> Result<std::time::SystemTime, std::io::Error> {
    let meta = std::fs::metadata(path)?;
    if meta.is_file() {
        return meta.modified();
    }
    let mut latest = meta.modified()?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            if let Ok(mod_time) = get_latest_modified_time(&entry.path()) {
                if mod_time > latest {
                    latest = mod_time;
                }
            }
        }
    }
    Ok(latest)
}

fn needs_ui_build() -> bool {
    let dist_path = std::path::Path::new("web/dist/index.html");
    if !dist_path.exists() {
        return true;
    }
    let dist_mod = match std::fs::metadata(dist_path).and_then(|m| m.modified()) {
        Ok(m) => m,
        Err(_) => return true,
    };

    let mut latest_src_mod = std::time::UNIX_EPOCH;
    for path in ["web/src", "web/package.json", "web/vite.config.ts"] {
        let p = std::path::Path::new(path);
        if let Ok(mod_time) = get_latest_modified_time(p) {
            if mod_time > latest_src_mod {
                latest_src_mod = mod_time;
            }
        }
    }

    latest_src_mod > dist_mod
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Run mode ──────────────────────────────────────────────────────────────
    // --daemon     Skip the TUI; log to stdout. Intended for server / PM2.
    // --ui         Skip the TUI; log to stdout; start Web UI; open browser.
    // (default)    Interactive TUI mode for local use.
    let args: Vec<String> = std::env::args().collect();
    let cli = crate::config::parse_cli_args(&args);
    let is_daemon = cli.is_daemon;
    let is_ui = cli.is_ui;
    let skip_open = cli.skip_open;
    let headless = cli.headless;

    // ── Tracing ───────────────────────────────────────────────────────────────
    // File log  : WARN+ always written to ./polycopier.log (full message, no truncation).
    // TUI mode  : WARN+ also captured in-memory and shown in the log panel.
    // Headless  : INFO+ written to stdout (journalctl / Docker friendly).

    // File layer — shared by both modes so errors are always inspectable.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("polycopier.log")
        .expect("Failed to open polycopier.log");
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_target(true)
        .with_level(true)
        .with_ansi(false)
        .with_filter(tracing_subscriber::filter::LevelFilter::WARN);

    let log_buffer = if headless {
        use tracing_subscriber::EnvFilter;
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("polycopier=info,warn"));
        tracing_subscriber::registry()
            .with(file_layer)
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_level(true),
            )
            .init();
        log_capture::new_log_buffer()
    } else {
        let log_buffer = log_capture::new_log_buffer();
        let tui_layer = log_capture::TuiLogLayer::new(log_buffer.clone());
        tracing_subscriber::registry()
            .with(file_layer)
            .with(tui_layer.with_filter(tracing_subscriber::filter::LevelFilter::WARN))
            .init();
        log_buffer
    };

    if is_daemon {
        tracing::info!(
            "polycopier starting in DAEMON mode. Send SIGTERM or SIGINT (Ctrl-C) to stop."
        );
    } else if is_ui {
        // Automatically build the UI if it hasn't been built yet or if src/ changed
        if needs_ui_build() {
            tracing::info!("UI updates detected. Attempting to build automatically...");
            let node_check = std::process::Command::new("node").arg("-v").output();
            if node_check.is_err() {
                tracing::error!("Node.js and npm are required to build the Web UI. Please install them and try again.");
                std::process::exit(1);
            }

            tracing::info!("Installing npm dependencies...");
            let install_status = std::process::Command::new("npm")
                .arg("install")
                .current_dir("web")
                .status()
                .expect("Failed to execute npm install");

            if !install_status.success() {
                tracing::error!("npm install failed.");
                std::process::exit(1);
            }

            tracing::info!("Building Web UI...");
            let build_status = std::process::Command::new("npm")
                .arg("run")
                .arg("build")
                .current_dir("web")
                .status()
                .expect("Failed to execute npm run build");

            if !build_status.success() {
                tracing::error!("npm run build failed.");
                std::process::exit(1);
            }
            tracing::info!("Web UI successfully built.");
        }

        tracing::info!(
            "polycopier starting in Web UI mode. Dashboard available at http://localhost:3000"
        );
        if !skip_open {
            let _ = std::process::Command::new("open")
                .arg("http://localhost:3000")
                .spawn();
        }
    }

    // ── Boot sequence ─────────────────────────────────────────────────────────

    // ── Boot sequence ─────────────────────────────────────────────────────────

    // Load .env early to evaluate whether keys are actually missing or invalid
    let _ = dotenvy::dotenv_override();
    let private_key = std::env::var("PRIVATE_KEY").unwrap_or_default();
    let needs_setup = private_key
        .trim()
        .trim_matches('"')
        .trim_start_matches("0x")
        .len()
        != 64
        || !std::path::Path::new("config.toml").exists();

    // If setup is needed and the user requested the Web UI setup mode,
    // we launch a dedicated lightweight Setup API on port 3000 and suspend the backend.
    // If they run the standard `cargo run`, it ignores this and uses the terminal natively.
    if is_ui && needs_setup && !cli.is_sim {
        let setup_router = api::create_setup_router();
        tokio::spawn(async move {
            if let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:3000").await {
                let _ = axum::serve(listener, setup_router).await;
            }
        });
    }

    let mut config = if cli.is_sim {
        let cfg: config::BotConfig = toml::from_str(&std::fs::read_to_string("config.toml")?)?;
        config::validate_config(&cfg)?;
        config::Config::from_parts(
            String::new(),
            "0x0000000000000000000000000000000000000000".to_string(),
            cfg,
        )
    } else {
        config::Config::load_or_prompt(is_ui).await?
    };
    config.is_sim = cli.is_sim;
    config.sim_balance = cli.sim_balance;
    std::env::set_var(
        "POLYCOPIER_MODE",
        if config.is_sim { "simulation" } else { "live" },
    );
    std::env::set_var("HOT_RELOAD_ENABLED", "true");

    if config.is_sim {
        tracing::warn!("===========================================================");
        tracing::warn!("                 SIMULATION MODE ACTIVE                    ");
        tracing::warn!("   No real orders will be placed. Tracking mock P&L.       ");
        tracing::warn!("===========================================================");
    }

    let state = Arc::new(RwLock::new(state::BotState::new(
        config.is_sim,
        config.sim_balance,
    )));
    if config.is_sim {
        simulation::restore(&mut *state.write().await)?;
        let snapshot_state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let mut guard = snapshot_state.write().await;
                if guard.pending_orders.is_empty() {
                    if let Err(e) = simulation::commit(&mut guard) {
                        guard.started = true;
                        tracing::error!("Simulation snapshot failed: {e}");
                    }
                }
            }
        });
    }
    // Legacy watcher argument retained for API compatibility. Real outcomes are
    // delivered to the strategy's authoritative risk engine through BotState.
    let risk_engine = Arc::new(Mutex::new(risk::RiskEngine::new(config.clone())));

    let (poly_submitter, balance_fetcher, clob) = if config.is_sim {
        let (submitter, fetcher) = clients::build_sim_order_submitter(&config).await?;
        (submitter, fetcher, None)
    } else {
        let (submitter, fetcher, client) = clients::build_order_submitter(&config).await?;
        (submitter, fetcher, Some(client))
    };

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(4096);

    // ── Copy ledger ───────────────────────────────────────────────────────────
    // Load the persisted copy ledger.  This records which positions we entered
    // and from which target wallet, enabling correct SELL classification and the
    // one-position-per-token rule across restarts.
    let ledger_path = if config.is_sim {
        "sim_copy_ledger.json"
    } else {
        "copy_ledger.json"
    };
    let copy_ledger = Arc::new(Mutex::new(copy_ledger::CopyLedger::load_from(ledger_path)));

    // No strategy or signal producer runs before authoritative wallet/order seeds.
    // A failed wallet read is not an empty wallet.
    if !config.is_sim {
        if std::env::var_os("RISK_STATE_PATH").is_none() {
            std::env::set_var("RISK_STATE_PATH", "runtime-risk.json");
        }
        wallet_sync::seed_own_positions(&config.funder_address, state.clone()).await?;
        wallet_sync::seed_pending_orders(clob.as_ref().expect("live client"), state.clone())
            .await?;
        live_fills::reconcile(
            clob.as_ref().expect("live client"),
            &config.funder_address,
            &state,
            &copy_ledger,
        )
        .await?;
    }

    let mut strategy_risk = risk::RiskEngine::new(config.clone());
    strategy_risk.restore_persistent()?;
    if !config.is_sim {
        live_fills::sync_risk(&mut strategy_risk)?;
        live_fills::start(
            clob.as_ref().expect("live client").clone(),
            config.funder_address.clone(),
            state.clone(),
            copy_ledger.clone(),
        );
    }
    strategy::start_strategy_engine(
        event_rx,
        state.clone(),
        strategy_risk,
        poly_submitter,
        config.clone(),
        copy_ledger.clone(),
        if config.is_sim {
            strategy::make_no_op_holds_query()
        } else {
            strategy::make_live_holds_query()
        },
        strategy::make_live_end_date_query(),
    );

    // ── Background tasks ──────────────────────────────────────────────────────
    // Seed OUR positions AND live GTC orders before starting the scanner.
    // Both must complete so the scanner's first run sees accurate SkippedOwned
    // and already-queued state — preventing duplicate orders on restart.

    // Reconcile the copy ledger against our live wallet positions.
    // Any open ledger entry where we no longer hold the token is marked closed.
    // This corrects for positions that were closed while the bot was offline.
    {
        let live_token_ids: HashSet<String> = {
            let guard = state.read().await;
            guard.positions.keys().cloned().collect()
        };
        let mut ledger = copy_ledger.lock().await;
        if config.is_sim {
            ledger.reconcile(&live_token_ids);
        }
        // Prune closed entries older than configured retention period (Gap 11).
        ledger.prune_closed_older_than(config.ledger_retention_days);
    }
    rtds::start(&config, event_tx.clone()).await?;

    // Ongoing wallet sync (positions, prices, balance) — all fire-and-forget loops.
    wallet_sync::start_position_sync(
        config.clone(), // Pass the full config so it can check is_sim
        state.clone(),
        copy_ledger.clone(), // Gap 4: passed so sync can update fill sizes
    );

    if !config.is_sim {
        wallet_sync::start_balance_poll(balance_fetcher, state.clone());
    }

    // Position scanner — safe to start now; seed has completed.
    position_scanner::start_position_scanner(config.clone(), state.clone(), event_tx.clone());
    prices::start(state.clone());

    // Position-close sweep — backstop that emits synthetic SELLs for any
    // position we hold that no target still holds (catches missed WS SELL events).
    // Gap 2 fix: pass copy_ledger so sweep uses the correct source_wallet.
    if !(config.is_sim && config.sizing_mode == config::SizingMode::TargetScalar) {
        wallet_sync::start_position_close_sweep(state.clone(), event_tx, copy_ledger.clone());
    }

    // Copied counter (header "Copied: N" — live API intersection every 30 s)
    copied_counter::start_copied_counter(
        config.funder_address.clone(),
        config.target_wallets.clone(),
        state.clone(),
        30,
    );

    // Order watcher (cancel stale GTC orders every 10 s)
    // Gap C + Gap E: now receives risk_engine Arc to call record_loss() on loss-triggered
    // cancellations, and uses exponential backoff on CLOB errors.
    let api_client = clob.clone();
    if !config.is_sim {
        order_watcher::start_order_watcher(
            config.clone(),
            clob.expect("live client"),
            state.clone(),
            copy_ledger.clone(),
            risk_engine,
        );
    }

    // ── Local Web API Server ──────────────────────────────────────────────────
    let api_router = api::create_router_with_client(state.clone(), copy_ledger.clone(), api_client);
    if is_ui {
        tokio::spawn(async move {
            let bind =
                std::env::var("DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
            match tokio::net::TcpListener::bind(&bind).await {
                Ok(listener) => {
                    if let Err(e) = axum::serve(listener, api_router).await {
                        tracing::error!("Local API Server crashed: {}", e);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "FATAL: Could not bind to Port 3000. Is another instance of Polycopier already running? Error: {}",
                        e
                    );
                    std::process::exit(1);
                }
            }
        });
    }

    // ── Main thread: TUI or headless wait ─────────────────────────────────────
    if headless {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate())?;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT — shutting down."),
                _ = sigterm.recv()          => tracing::info!("Received SIGTERM — shutting down."),
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await?;
            tracing::info!("Received Ctrl-C — shutting down.");
        }
    } else {
        match ui::start_tui(state.clone(), config.clone(), log_buffer).await? {
            ui::TuiExit::Quit => {}
            ui::TuiExit::Settings => {
                // Settings saved inside TUI — replace this process with a fresh instance.
                let exe = std::env::current_exe()?;
                let args: Vec<String> = std::env::args().collect();

                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    let err = std::process::Command::new(&exe).args(&args[1..]).exec();
                    return Err(anyhow::anyhow!("exec failed: {}", err));
                }
                #[cfg(not(unix))]
                {
                    let _ = std::process::Command::new(&exe).args(&args[1..]).spawn();
                    std::process::exit(0);
                }
            }
        }
    }

    state.write().await.started = true;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    while !state.read().await.pending_orders.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if config.is_sim {
        simulation::commit(&mut *state.write().await)?;
    }
    Ok(())
}
