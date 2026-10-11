#[tokio::test]
async fn invalid_wallet_seed_is_an_error_not_an_empty_authoritative_wallet() {
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    let mut initial = polycopier::state::BotState::new(false, None);
    initial.positions.insert(
        "123".into(),
        polycopier::models::Position {
            token_id: "123".into(),
            size: dec!(4),
            average_entry_price: dec!(0.5),
        },
    );
    let state = Arc::new(RwLock::new(initial));
    assert!(
        polycopier::wallet_sync::seed_own_positions("invalid", state.clone())
            .await
            .is_err()
    );
    assert_eq!(state.read().await.positions["123"].size, dec!(4));
}
