//! Bounded instrumentation; source timestamps have only one-second precision.
use crate::models::TradeEvent;
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Default, Serialize)]
pub struct Metrics {
    pub execution_mode: String,
    pub unique_signals_delivered: u64,
    pub duplicate_signals: u64,
    pub expired_signals: u64,
    pub polling_errors: u64,
    pub polling_page_limit_hits: u64,
    pub last_poll_duration_ms: Option<u64>,
    pub order_acceptances: u64,
    pub last_fill_to_accept_upper_bound_ms: Option<i64>,
    pub last_receive_to_accept_ms: Option<u64>,
    pub last_accept_source: Option<String>,
    pub last_accept_event_key: Option<String>,
    pub websocket_target_events: u64,
    pub polling_target_events: u64,
    pub order_dispatches: u64,
    pub last_receive_to_dispatch_ms: Option<u64>,
    pub last_fill_to_dispatch_upper_bound_ms: Option<i64>,
    pub last_source: Option<String>,
    pub last_dispatch_event_key: Option<String>,
    pub last_dispatch_at: Option<i64>,
    pub last_websocket_fill_age_upper_bound_ms: Option<i64>,
    pub dispatch_latency_samples: usize,
    pub receive_to_dispatch_p50_ms: Option<u64>,
    pub receive_to_dispatch_p95_ms: Option<u64>,
    pub receive_to_dispatch_p99_ms: Option<u64>,
    pub acceptance_latency_samples: usize,
    pub receive_to_accept_p50_ms: Option<u64>,
    pub receive_to_accept_p95_ms: Option<u64>,
    #[serde(skip)]
    recent_dispatch_ms: VecDeque<(Instant, u64)>,
    #[serde(skip)]
    recent_accept_ms: VecDeque<(Instant, u64)>,
}
const LATENCY_SAMPLE_CAP: usize = 1024;
const LATENCY_SAMPLE_WINDOW: Duration = Duration::from_secs(300);

fn record_sample(samples: &mut VecDeque<(Instant, u64)>, value: u64) {
    while samples
        .front()
        .is_some_and(|(at, _)| at.elapsed() > LATENCY_SAMPLE_WINDOW)
    {
        samples.pop_front();
    }
    if samples.len() == LATENCY_SAMPLE_CAP {
        samples.pop_front();
    }
    samples.push_back((Instant::now(), value));
}

fn percentile(sorted: &[u64], percent: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let index = (sorted.len() * percent).div_ceil(100).saturating_sub(1);
    sorted.get(index).copied()
}
type Observation = (Instant, &'static str);
static METRICS: LazyLock<Mutex<Metrics>> = LazyLock::new(|| Mutex::new(Metrics::default()));
static RECEIVED: LazyLock<Mutex<HashMap<String, Observation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PREPARED: LazyLock<Mutex<HashMap<String, (TradeEvent, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn signal_delivered() {
    METRICS.lock().unwrap().unique_signals_delivered += 1;
}
pub fn signal_duplicate() {
    METRICS.lock().unwrap().duplicate_signals += 1;
}
pub fn signal_expired() {
    METRICS.lock().unwrap().expired_signals += 1;
}
pub fn poll_error() {
    METRICS.lock().unwrap().polling_errors += 1;
}
pub fn poll_page_limit() {
    METRICS.lock().unwrap().polling_page_limit_hits += 1;
}
pub fn poll_complete(ms: u64) {
    METRICS.lock().unwrap().last_poll_duration_ms = Some(ms);
}

pub fn prepare(event: &TradeEvent) {
    let mut pending = PREPARED.lock().unwrap();
    pending.retain(|_, (_, at)| at.elapsed().as_secs() < 60);
    if pending.len() >= 20000 {
        pending.clear();
    }
    pending.insert(event.token_id.clone(), (event.clone(), Instant::now()));
}
pub fn clear_prepared(token: &str) {
    PREPARED.lock().unwrap().remove(token);
}

/// Called after signing and durable intent, immediately before CLOB POST
/// (or before the paper submitter's simulated matching delay).
pub fn dispatch_token(token: &str) {
    let event = PREPARED
        .lock()
        .unwrap()
        .remove(token)
        .map(|(event, _)| event);
    if let Some(event) = event {
        dispatch(&event);
    }
}

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
    if let Some(ms) = metrics.last_receive_to_dispatch_ms {
        record_sample(&mut metrics.recent_dispatch_ms, ms);
    }
    metrics.last_source = observation.map(|(_, source)| source.to_string());
    metrics.last_dispatch_event_key = observation.map(|_| crate::rtds::event_key(event));
    metrics.last_fill_to_dispatch_upper_bound_ms = observation
        .map(|_| chrono::Utc::now().timestamp_millis() - event.timestamp.saturating_mul(1000));
    metrics.last_dispatch_at = Some(chrono::Utc::now().timestamp_millis());
    tracing::info!("Order latency: source={:?} receive_to_dispatch_ms={:?} fill_to_dispatch_upper_bound_ms={:?}", metrics.last_source, metrics.last_receive_to_dispatch_ms, metrics.last_fill_to_dispatch_upper_bound_ms);
}
pub fn snapshot() -> Metrics {
    let mut metrics = METRICS.lock().unwrap().clone();
    metrics.execution_mode = std::env::var("POLYCOPIER_MODE").unwrap_or_else(|_| "unknown".into());
    let mut dispatch = metrics
        .recent_dispatch_ms
        .iter()
        .filter(|(at, _)| at.elapsed() <= LATENCY_SAMPLE_WINDOW)
        .map(|(_, ms)| *ms)
        .collect::<Vec<_>>();
    dispatch.sort_unstable();
    metrics.dispatch_latency_samples = dispatch.len();
    metrics.receive_to_dispatch_p50_ms = percentile(&dispatch, 50);
    metrics.receive_to_dispatch_p95_ms = percentile(&dispatch, 95);
    metrics.receive_to_dispatch_p99_ms = percentile(&dispatch, 99);
    let mut accepted = metrics
        .recent_accept_ms
        .iter()
        .filter(|(at, _)| at.elapsed() <= LATENCY_SAMPLE_WINDOW)
        .map(|(_, ms)| *ms)
        .collect::<Vec<_>>();
    accepted.sort_unstable();
    metrics.acceptance_latency_samples = accepted.len();
    metrics.receive_to_accept_p50_ms = percentile(&accepted, 50);
    metrics.receive_to_accept_p95_ms = percentile(&accepted, 95);
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
    metrics.last_accept_event_key = observation.map(|_| crate::rtds::event_key(event));
    metrics.last_receive_to_accept_ms =
        observation.map(|(when, _)| when.elapsed().as_millis() as u64);
    if let Some(ms) = metrics.last_receive_to_accept_ms {
        record_sample(&mut metrics.recent_accept_ms, ms);
    }
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
    fn bounded_latency_samples_report_nearest_rank_percentiles() {
        let mut samples = VecDeque::new();
        for ms in 1..=LATENCY_SAMPLE_CAP as u64 + 1 {
            record_sample(&mut samples, ms);
        }
        assert_eq!(samples.len(), LATENCY_SAMPLE_CAP);
        assert_eq!(samples.front().map(|(_, ms)| *ms), Some(2));
        let sorted = samples.into_iter().map(|(_, ms)| ms).collect::<Vec<_>>();
        assert_eq!(percentile(&sorted, 50), Some(513));
        assert_eq!(percentile(&sorted, 95), Some(974));
        assert_eq!(percentile(&sorted, 99), Some(1015));
        assert_eq!(percentile(&[], 95), None);
    }
    #[test]
    fn stale_latency_samples_do_not_affect_current_window() {
        let mut samples = VecDeque::from([
            (
                Instant::now() - LATENCY_SAMPLE_WINDOW - Duration::from_secs(1),
                9000,
            ),
            (Instant::now(), 120),
        ]);
        record_sample(&mut samples, 100);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples.front().map(|(_, ms)| *ms), Some(120));
    }
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
        let before = snapshot().order_dispatches;
        prepare(&event);
        assert_eq!(snapshot().order_dispatches, before);
        dispatch_token(&event.token_id);
        assert_eq!(snapshot().order_dispatches, before + 1);
        assert_eq!(snapshot().last_source.as_deref(), Some("websocket"));
        assert!(snapshot().last_fill_to_dispatch_upper_bound_ms.is_some());
        assert!(snapshot().last_receive_to_dispatch_ms.is_some());
        accepted(&event);
        assert_eq!(snapshot().last_accept_source.as_deref(), Some("websocket"));
        assert!(snapshot().last_fill_to_accept_upper_bound_ms.is_some());
        assert_eq!(
            snapshot().last_dispatch_event_key,
            snapshot().last_accept_event_key
        );
        prepare(&event);
        clear_prepared(&event.token_id);
        dispatch_token(&event.token_id);
        assert_eq!(snapshot().order_dispatches, before + 1);
    }
}
