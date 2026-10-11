//! Bounded instrumentation; source timestamps have only one-second precision.
use crate::models::TradeEvent;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::Instant,
};

#[derive(Clone, Default, Serialize)]
pub struct Metrics {
    pub websocket_target_events: u64,
    pub polling_target_events: u64,
    pub order_dispatches: u64,
    pub last_receive_to_dispatch_ms: Option<u64>,
    pub last_fill_to_dispatch_upper_bound_ms: Option<i64>,
    pub last_source: Option<String>,
    pub last_dispatch_at: Option<i64>,
    pub last_websocket_fill_age_upper_bound_ms: Option<i64>,
}
type Observation = (Instant, &'static str);
static METRICS: LazyLock<Mutex<Metrics>> = LazyLock::new(|| Mutex::new(Metrics::default()));
static RECEIVED: LazyLock<Mutex<HashMap<String, Observation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn receive(event: &TradeEvent, source: &'static str) {
    let mut metrics = METRICS.lock().unwrap();
    if source == "websocket" {
        metrics.websocket_target_events += 1;
        metrics.last_websocket_fill_age_upper_bound_ms =
            Some(chrono::Utc::now().timestamp_millis() - event.timestamp.saturating_mul(1000));
    } else {
        metrics.polling_target_events += 1;
    }
    drop(metrics);
    let mut received = RECEIVED.lock().unwrap();
    received.retain(|_, (when, _)| when.elapsed().as_secs() < 60);
    if received.len() >= 20000 {
        received.clear();
    }
    received
        .entry(crate::rtds::event_key(event))
        .or_insert((Instant::now(), source));
}
pub fn dispatch(event: &TradeEvent) {
    let observation = RECEIVED
        .lock()
        .unwrap()
        .remove(&crate::rtds::event_key(event));
    let mut metrics = METRICS.lock().unwrap();
    metrics.order_dispatches += 1;
    metrics.last_receive_to_dispatch_ms =
        observation.map(|(when, _)| when.elapsed().as_millis() as u64);
    metrics.last_source = observation.map(|(_, source)| source.to_string());
    metrics.last_fill_to_dispatch_upper_bound_ms =
        Some(chrono::Utc::now().timestamp_millis() - event.timestamp.saturating_mul(1000));
    metrics.last_dispatch_at = Some(chrono::Utc::now().timestamp_millis());
    tracing::info!("Order latency: source={:?} receive_to_dispatch_ms={:?} fill_to_dispatch_upper_bound_ms={:?}", metrics.last_source, metrics.last_receive_to_dispatch_ms, metrics.last_fill_to_dispatch_upper_bound_ms);
}
pub fn snapshot() -> Metrics {
    METRICS.lock().unwrap().clone()
}
