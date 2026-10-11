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
    pub execution_mode: String,
    pub order_acceptances: u64,
    pub last_fill_to_accept_upper_bound_ms: Option<i64>,
    pub last_receive_to_accept_ms: Option<u64>,
    pub last_accept_source: Option<String>,
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
        .get(&crate::rtds::event_key(event))
        .copied();
    let mut metrics = METRICS.lock().unwrap();
    metrics.order_dispatches += 1;
    metrics.last_receive_to_dispatch_ms =
        observation.map(|(when, _)| when.elapsed().as_millis() as u64);
    metrics.last_source = observation.map(|(_, source)| source.to_string());
    metrics.last_fill_to_dispatch_upper_bound_ms = observation
        .map(|_| chrono::Utc::now().timestamp_millis() - event.timestamp.saturating_mul(1000));
    metrics.last_dispatch_at = Some(chrono::Utc::now().timestamp_millis());
    tracing::info!("Order latency: source={:?} receive_to_dispatch_ms={:?} fill_to_dispatch_upper_bound_ms={:?}", metrics.last_source, metrics.last_receive_to_dispatch_ms, metrics.last_fill_to_dispatch_upper_bound_ms);
}
pub fn snapshot() -> Metrics {
    let mut metrics = METRICS.lock().unwrap().clone();
    metrics.execution_mode = std::env::var("POLYCOPIER_MODE").unwrap_or_else(|_| "unknown".into());
    metrics
}
pub fn accepted(event: &TradeEvent) {
    let observation = RECEIVED
        .lock()
        .unwrap()
        .remove(&crate::rtds::event_key(event));
    let mut metrics = METRICS.lock().unwrap();
    metrics.order_acceptances += 1;
    metrics.last_accept_source = observation.map(|(_, source)| source.into());
    metrics.last_receive_to_accept_ms =
        observation.map(|(when, _)| when.elapsed().as_millis() as u64);
    metrics.last_fill_to_accept_upper_bound_ms = observation
        .map(|_| chrono::Utc::now().timestamp_millis() - event.timestamp.saturating_mul(1000));
    tracing::info!(
        "Order acceptance latency: source={:?}, fill_to_accept_upper_bound_ms={:?}",
        metrics.last_accept_source,
        metrics.last_fill_to_accept_upper_bound_ms
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synthetic_events_cannot_be_reported_as_measured_fill_latency() {
        let event = TradeEvent {
            transaction_hash: "latency-test".into(),
            maker_address: "wallet".into(),
            taker_address: "wallet".into(),
            token_id: "token".into(),
            price: rust_decimal::Decimal::ONE,
            size: rust_decimal::Decimal::ONE,
            side: crate::models::TradeSide::BUY,
            timestamp: chrono::Utc::now().timestamp(),
        };
        dispatch(&event);
        assert_eq!(snapshot().last_fill_to_dispatch_upper_bound_ms, None);
        accepted(&event);
        assert_eq!(snapshot().last_fill_to_accept_upper_bound_ms, None);
        receive(&event, "websocket");
        dispatch(&event);
        assert_eq!(snapshot().last_source.as_deref(), Some("websocket"));
        assert!(snapshot().last_fill_to_dispatch_upper_bound_ms.is_some());
        assert!(snapshot().last_receive_to_dispatch_ms.is_some());
        accepted(&event);
        assert_eq!(snapshot().last_accept_source.as_deref(), Some("websocket"));
        assert!(snapshot().last_fill_to_accept_upper_bound_ms.is_some());
    }
}
