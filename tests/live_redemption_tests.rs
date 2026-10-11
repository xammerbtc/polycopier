#[cfg(unix)]
#[tokio::test]
async fn confirmed_and_uncertain_intents_are_durable_and_never_automatically_retried() {
    use std::os::unix::fs::PermissionsExt;
    let original = std::env::current_dir().unwrap();
    let directory =
        std::env::temp_dir().join(format!("polycopier-redemption-test-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    std::env::set_current_dir(&directory).unwrap();
    let node = directory.join("fake-node");
    let confirmed = format!("#!/bin/sh\ncat >/dev/null\nprintf x >> helper-runs\nprintf '%s\\n' '{{\"status\":\"confirmed\",\"market_id\":\"42\",\"transaction_hash\":\"0x{}\"}}'\n", "4".repeat(64));
    std::fs::write(&node, confirmed).unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_var("REDEEM_NODE", &node);
    std::env::set_var("POLYCOPIER_MODE", "simulation");
    std::env::set_var("LIVE_REDEEM_ENABLED", "true");
    for name in [
        "PRIVATE_KEY",
        "FUNDER_ADDRESS",
        "POLYMARKET_RELAYER_API_KEY",
        "POLYMARKET_RELAYER_API_KEY_ADDRESS",
    ] {
        std::env::set_var(name, "test-only-value");
    }
    assert!(polycopier::live_redemption::execute("42").await.is_err());
    assert!(!directory.join("helper-runs").exists());
    std::env::set_var("POLYCOPIER_MODE", "live");
    let receipt = polycopier::live_redemption::execute("42").await.unwrap();
    assert_eq!(receipt["status"], "confirmed");
    assert_eq!(
        polycopier::live_redemption::records().unwrap()["42"].status,
        "confirmed"
    );
    assert!(polycopier::live_redemption::execute("42").await.is_err());
    assert_eq!(std::fs::read_to_string("helper-runs").unwrap(), "x");
    std::fs::write(
        &node,
        "#!/bin/sh\ncat >/dev/null\nprintf x >> helper-runs\nexit 1\n",
    )
    .unwrap();
    assert!(polycopier::live_redemption::execute("43").await.is_err());
    assert_eq!(
        polycopier::live_redemption::records().unwrap()["43"].status,
        "pending_reconciliation"
    );
    assert!(polycopier::live_redemption::execute("43").await.is_err());
    assert_eq!(std::fs::read_to_string("helper-runs").unwrap(), "xx");
    std::env::set_current_dir(original).unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
