use crate::{
    config::Config,
    models::{TradeEvent, TradeSide},
};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use std::{
    collections::{HashSet, VecDeque},
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::{
    sync::mpsc,
    time::{timeout, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub static CONNECTED: AtomicBool = AtomicBool::new(false);
pub static LAST_MESSAGE: AtomicU64 = AtomicU64::new(0);

pub fn event_key(e: &TradeEvent) -> String {
    format!(
        "{}:{}:{}:{:?}:{}:{}",
        e.transaction_hash.to_lowercase(),
        e.taker_address.to_lowercase(),
        e.token_id,
        e.side,
        e.price.normalize(),
        e.size.normalize()
    )
}

pub fn parse_activity(v: &serde_json::Value, targets: &[String]) -> Option<TradeEvent> {
    if v.get("topic")?.as_str()? != "activity" || v.get("type")?.as_str()? != "trades" {
        return None;
    }
    let p = v.get("payload")?;
    let wallet = p.get("proxyWallet")?.as_str()?.to_lowercase();
    if !targets.contains(&wallet) {
        return None;
    }
    let decimal = |key: &str| {
        Decimal::from_str(
            &p.get(key)?
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| p[key].to_string()),
        )
        .ok()
    };
    let side = match p.get("side")?.as_str()? {
        "BUY" => TradeSide::BUY,
        "SELL" => TradeSide::SELL,
        _ => return None,
    };
    Some(TradeEvent {
        transaction_hash: p.get("transactionHash")?.as_str()?.to_string(),
        maker_address: wallet.clone(),
        taker_address: wallet,
        token_id: p.get("asset")?.as_str()?.to_string(),
        price: decimal("price")?,
        size: decimal("size")?,
        side,
        timestamp: p.get("timestamp")?.as_i64()?,
    })
}

pub async fn start(config: &Config, tx: mpsc::Sender<TradeEvent>) -> anyhow::Result<()> {
    let (raw_tx, mut raw_rx) = mpsc::channel::<TradeEvent>(1000);
    crate::listener::start_ws_listener(config, raw_tx.clone()).await?;
    tokio::spawn(async move {
        let mut seen = HashSet::new();
        let mut order = VecDeque::new();
        while let Some(event) = raw_rx.recv().await {
            let age = chrono::Utc::now().timestamp() - event.timestamp;
            if !(0..=300).contains(&age) {
                continue;
            }
            let key = event_key(&event);
            if seen.contains(&key) {
                continue;
            }
            if tx.send(event).await.is_err() {
                return;
            }
            if order.len() >= 20000 {
                if let Some(old) = order.pop_front() {
                    seen.remove(&old);
                }
            }
            seen.insert(key.clone());
            order.push_back(key);
        }
    });
    let mut cfg = config.clone();
    tokio::spawn(async move {
        let mut errors = 0;
        loop {
            CONNECTED.store(false, Ordering::Relaxed);
            let result = async {
                let (mut socket, _) = timeout(Duration::from_secs(15), connect_async("wss://ws-live-data.polymarket.com")).await??;
                socket.send(Message::Text(serde_json::json!({"action":"subscribe","subscriptions":[{"topic":"activity","type":"trades"}]}).to_string().into())).await?;
                CONNECTED.store(true, Ordering::Relaxed);
                tracing::info!("RTDS WebSocket connected; polling fallback active");
                let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
                let mut last = tokio::time::Instant::now();
                loop {
                    tokio::select! {
                        _ = heartbeat.tick() => {
                            cfg.hot_reload();
                            anyhow::ensure!(last.elapsed() < Duration::from_secs(45), "RTDS heartbeat timeout");
                            socket.send(Message::Text("PING".into())).await?;
                        }
                        msg = socket.next() => {
                            let msg = msg.ok_or_else(|| anyhow::anyhow!("RTDS closed"))??;
                            last = tokio::time::Instant::now();
                            LAST_MESSAGE.store(chrono::Utc::now().timestamp() as u64, Ordering::Relaxed);
                            errors = 0;
                            match msg {
                                Message::Text(body) => if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                                    if let Some(e) = parse_activity(&v, &cfg.target_wallets) { crate::latency::receive(&e, "websocket"); raw_tx.send(e).await.map_err(|_| anyhow::anyhow!("listener closed"))?; }
                                },
                                Message::Ping(p) => socket.send(Message::Pong(p)).await?,
                                Message::Close(_) => anyhow::bail!("RTDS closed"),
                                _ => {}
                            }
                        }
                    }
                }
                #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
            }.await;
            CONNECTED.store(false, Ordering::Relaxed);
            errors += 1;
            tracing::warn!("RTDS disconnected: {:?}; polling continues", result.err());
            tokio::time::sleep(Duration::from_secs(crate::backoff::next_backoff(
                errors, 2, 60,
            )))
            .await;
        }
    });
    Ok(())
}
