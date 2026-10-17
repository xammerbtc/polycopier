#[test]
fn donor_safe_and_explicit_eoa_proxy_types_are_supported_without_guessing() {
    use polycopier::clients::signature_type;
    use polymarket_client_sdk::clob::types::SignatureType;
    assert!(matches!(
        signature_type("2").unwrap(),
        SignatureType::GnosisSafe
    ));
    assert!(matches!(
        signature_type("safe").unwrap(),
        SignatureType::GnosisSafe
    ));
    assert!(matches!(
        signature_type("proxy").unwrap(),
        SignatureType::Proxy
    ));
    assert!(matches!(signature_type("0").unwrap(), SignatureType::Eoa));
    assert!(signature_type("guess").is_err());
}
