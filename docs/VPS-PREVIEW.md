# Isolated VPS deployment

This deployment runs **DRY-RUN / SIMULATION**. The $200 balance and all
positions/PnL displayed by this deployment are paper values. No CLOB
authentication, signed orders, cancellations, or on-chain transactions occur.

## Installed layout

- Source/build/UI/state: `/opt/polycopier`
- Service account: `polycopier` (no login shell)
- Service: `polycopier-preview.service`
- Dashboard: `127.0.0.1:3000`, configurable through `DASHBOARD_BIND`
- Token: `/opt/polycopier/.env`, mode 0600
- Donor wallet credentials: `/opt/polycopier/.env.credentials`, root-only 0600;
  this file is **not loaded** by the preview service
- Public tunables: `config.toml`
- Paper state: `simulation_state.json` and `sim_copy_ledger.json`
- Daily volume / circuit-breaker state: `runtime-risk.json`
- Logs: journald and `polycopier.log`

The existing arbigab deployment has separate services, files and ports. Its
service lifecycle and risk settings are not changed by this installer.

## Build and install on Ubuntu

Prerequisites: Docker, Python 3, Node/npm, systemd, OpenSSL development/runtime
libraries appropriate to the Ubuntu host. Build in the Bookworm Rust image;
record its immutable digest and retain `Cargo.lock` for repeatable builds.

```bash
cd /opt/polycopier
bash deploy/build-vps.sh
# Only after tests, clippy, release build and frontend build succeed:
flock -n /opt/arbigab/operator_control/locks/production.lock bash deploy/install-preview.sh
sudo install -m 644 deploy/logrotate-preview /etc/logrotate.d/polycopier-preview
python3 deploy/verify-preview.py
```

Use the VPS's existing SSH connection to access the dashboard:

```bash
ssh -N -L 13000:127.0.0.1:3000 arbigab-vps
```

Open `http://127.0.0.1:13000`. Username: `polycopier`. Password is the
`DASHBOARD_TOKEN` stored on the VPS. Retrieve it through your own secure SSH
session; do not paste it into issues, commits, reports, or chat logs. Token
values are never returned by the dashboard API.

For a reverse proxy, keep the backend bound to localhost, use HTTPS, preserve
the Host header, and keep the API's Basic authentication enabled. Example:

```nginx
location / {
    proxy_pass http://127.0.0.1:3000;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

## Live settings

Wallets can include an optional scalar: `0xADDRESS:0.01`. A scalar of `0`
temporarily disables that wallet while keeping it in the configuration.
Set `scanner.max_entries_per_cycle=0` to disable catch-up entries while
continuing to display target portfolios. Target changes are consumed by polling
every cycle and RTDS heartbeat every five seconds. Risk/sizing/execution settings
are consumed before evaluating the next event. Config writes are atomic and
invalid settings are rejected with HTTP 400.

RTDS subscribes to the public `activity/trades` feed and filters target wallet
addresses locally. Polling remains active to cover outages. The combined stream
deduplicates by transaction, wallet, token, side, price and size, retaining
different token fills from one transaction. Heartbeats run every five seconds;
45 seconds without inbound messages triggers reconnect with bounded backoff.

The <1.5-second latency goal requires measured target fills. A connected socket
alone does not prove that latency, and target event timestamps have only
second-level precision. Observe actual events before making any latency claim.

## API endpoints

All paths except `/health` require Basic authentication.

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Process status, execution mode, websocket connection, poll timestamp, active listener targets |
| GET | `/api/state` | Balance, paper/live mode, PnL, volume, positions, signal feed and source mapping |
| GET | `/api/config` | Public tunables and configured targets; JSON export |
| POST | `/api/config` | Validate and atomically save tunables, consumed without restarting |
| GET | `/api/env` | Public funder address and blank private-key field for compatibility |
| POST | `/api/control` | `{"action":"pause"}`, `stop`, `start`, or `resume` |
| GET | `/api/ledger` | Copy history and ledger export |
| GET | `/api/logs` | Last 200 file-log lines |
| POST | `/api/action/restart` | Replace only the polycopier process, preserving CLI simulation flags |

The secret-writing `/api/env` POST is disabled. Credential changes belong in a
secure server-side workflow. Force Redeem is visibly blocked in dry-run because
it sends an on-chain transaction; it is not a claim of completed redemption.

## Operations and rollback

```bash
systemctl status polycopier-preview
journalctl -u polycopier-preview --since '30 minutes ago'
curl -fsS http://127.0.0.1:3000/health
systemctl stop polycopier-preview
```

Graceful shutdown pauses new execution, allows up to 25 seconds for in-flight
simulation orders, then persists the paper state. Invalid or unreadable persisted
state fails startup instead of silently resetting balances. Configuration reload
preserves the execution mode and credentials.

Before an update, preserve the existing binary and `web/dist`, together with
the config and runtime state, in a private timestamped directory. Build and test
the update before restarting this service. Rollback restores that binary/UI and
restarts `polycopier-preview`; retain current ledgers and snapshots. Stop only
the new service if verification fails. Do not restart the existing trading bot.

Live trading has not been enabled or certified by this preview deployment.
