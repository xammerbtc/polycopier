use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use base64::Engine;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tower::ServiceExt;

#[tokio::test]
async fn dashboard_requires_authentication_and_never_returns_key() {
    let token = "a-test-only-token-with-at-least-24-characters";
    std::env::set_var("DASHBOARD_TOKEN", token);
    std::env::set_var("PRIVATE_KEY", "test-secret-must-never-appear");
    let bot_state = Arc::new(RwLock::new(polycopier::state::BotState::default()));
    let router = polycopier::api::create_router(
        bot_state.clone(),
        Arc::new(Mutex::new(polycopier::copy_ledger::CopyLedger::default())),
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let header = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("polycopier:{token}"))
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/env")
                .header("authorization", &header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["private_key"], "");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/env")
                .header("authorization", &header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    bot_state.write().await.paper_integrity_fault = true;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/control")
                .header("authorization", &header)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"resume"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    std::env::set_var("POLYCOPIER_MODE", "live");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/redeem")
                .header("authorization", &header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
