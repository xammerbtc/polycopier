use alloy::primitives::U256;
use polycopier::{
    execution_journal::Record,
    live_fills::{project, Fill},
    models::{OrderRequest, TradeSide},
    redemption_settlement::{self, Asset},
};
use rust_decimal_macros::dec;
use serde_json::{json, Value};
use std::collections::BTreeMap;
const FUNDER: &str = "0x2d2d2b32e5199edf833594a99b9c54e5668ab69d";
fn fixture() -> (Value, BTreeMap<String, Asset>) {
    let r: Value =
        serde_json::from_str(include_str!("fixtures/ctf-public-redemption-receipt.json")).unwrap();
    let data = r["logs"][0]["data"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap();
    let token = U256::from_str_radix(&data[4 * 64..5 * 64], 16)
        .unwrap()
        .to_string();
    (
        r,
        BTreeMap::from([(
            token,
            Asset {
                entry_order_id: "fixture-order".into(),
                shares: dec!(114.980296),
                payout_per_share: dec!(1),
            },
        )]),
    )
}
#[test]
fn public_redemption_proves_exact_delegated_burn_and_pusd_payout() {
    let (r, assets) = fixture();
    let proof =
        redemption_settlement::decode(&r, r["transactionHash"].as_str().unwrap(), FUNDER, &assets)
            .unwrap();
    assert_eq!(
        proof
            .payouts
            .values()
            .copied()
            .sum::<rust_decimal::Decimal>(),
        dec!(114.980296)
    );
    assert!(proof.block_timestamp > 0);
    let mut dup = r.clone();
    dup["logs"]
        .as_array_mut()
        .unwrap()
        .push(r["logs"][0].clone());
    assert_eq!(
        proof,
        redemption_settlement::decode(
            &dup,
            r["transactionHash"].as_str().unwrap(),
            FUNDER,
            &assets
        )
        .unwrap()
    );
}
#[test]
fn mismatched_shares_payout_routes_receipts_and_missing_burn_fail_closed() {
    let (r, assets) = fixture();
    let mut too_small = assets.clone();
    too_small.values_mut().next().unwrap().shares -= dec!(0.000001);
    assert!(redemption_settlement::decode(
        &r,
        r["transactionHash"].as_str().unwrap(),
        FUNDER,
        &too_small
    )
    .is_err());
    let mut loser = assets.clone();
    loser.values_mut().next().unwrap().payout_per_share = dec!(0);
    assert!(redemption_settlement::decode(
        &r,
        r["transactionHash"].as_str().unwrap(),
        FUNDER,
        &loser
    )
    .is_err());
    for which in 0..5 {
        let mut changed = r.clone();
        match which {
            0 => {
                changed["logs"].as_array_mut().unwrap().remove(1);
            }
            1 => changed["logs"][0]["topics"][3] = json!(format!("0x{}", "f".repeat(64))),
            2 => changed["status"] = json!("0x0"),
            3 => changed["logs"][0]["removed"] = json!(true),
            _ => changed["logs"][3]["data"] = json!(format!("0x{:064x}", U256::from(114_980_295))),
        }
        assert!(redemption_settlement::decode(
            &changed,
            r["transactionHash"].as_str().unwrap(),
            FUNDER,
            &assets
        )
        .is_err());
    }
    assert!(
        redemption_settlement::decode(&r, &format!("0x{}", "f".repeat(64)), FUNDER, &assets)
            .is_err()
    );
}
#[test]
fn delegated_multi_hop_burn_is_traced_without_counting_other_wallets() {
    let (mut r, assets) = fixture();
    let adapter = format!(
        "0x{}ada100db00ca00073811820692005400218fce1f",
        "0".repeat(24)
    );
    let auto = r["logs"][0]["topics"][3].clone();
    r["logs"][0]["topics"][3] = json!(adapter.clone());
    let mut forwarded = r["logs"][1].clone();
    forwarded["topics"][2] = json!(adapter.clone());
    forwarded["topics"][3] = auto;
    r["logs"][1]["logIndex"] = json!("0x47d");
    r["logs"][2]["logIndex"] = json!("0x47e");
    r["logs"].as_array_mut().unwrap().push(forwarded.clone());
    assert!(redemption_settlement::decode(
        &r,
        r["transactionHash"].as_str().unwrap(),
        FUNDER,
        &assets
    )
    .is_ok());
    forwarded["topics"][2] = json!(format!("0x{}", "1".repeat(64)));
    forwarded["topics"][3] = json!(adapter);
    *r["logs"].as_array_mut().unwrap().last_mut().unwrap() = forwarded;
    assert!(redemption_settlement::decode(
        &r,
        r["transactionHash"].as_str().unwrap(),
        FUNDER,
        &assets
    )
    .is_err());
}
#[test]
fn losing_position_requires_a_real_burn_but_no_positive_collateral_credit() {
    let (mut r, mut assets) = fixture();
    assets.values_mut().next().unwrap().payout_per_share = dec!(0);
    r["logs"][3]["data"] = json!(format!("0x{}", "0".repeat(64)));
    let proof =
        redemption_settlement::decode(&r, r["transactionHash"].as_str().unwrap(), FUNDER, &assets)
            .unwrap();
    assert_eq!(
        proof
            .payouts
            .values()
            .copied()
            .sum::<rust_decimal::Decimal>(),
        dec!(0)
    );
    r["logs"].as_array_mut().unwrap().remove(1);
    assert!(redemption_settlement::decode(
        &r,
        r["transactionHash"].as_str().unwrap(),
        FUNDER,
        &assets
    )
    .is_err());
}
#[test]
fn verified_payout_closes_fee_inclusive_inventory_and_realizes_once_per_projection() {
    let (r, assets) = fixture();
    let token = assets.keys().next().unwrap().clone();
    let proof =
        redemption_settlement::decode(&r, r["transactionHash"].as_str().unwrap(), FUNDER, &assets)
            .unwrap();
    let fill = Fill {
        transaction_hash: format!("0x{}", "1".repeat(64)),
        block_hash: format!("0x{}", "2".repeat(64)),
        block_number: proof.block_number - 1,
        block_timestamp: proof.block_timestamp - 10,
        transaction_index: 0,
        log_index: 0,
        order_id: "fixture-order".into(),
        token_id: token.clone(),
        side: TradeSide::BUY,
        shares: dec!(114.980296),
        collateral: dec!(50),
        fee: dec!(0.01),
    };
    let mut orders = BTreeMap::from([(
        "fixture".into(),
        Record {
            terminal_unfilled: false,
            source_wallet: Some(FUNDER.into()),
            order: OrderRequest {
                token_id: token.clone(),
                side: TradeSide::BUY,
                price: dec!(0.5),
                size: dec!(114.980296),
            },
            status: "accepted".into(),
            order_id: Some("fixture-order".into()),
            created_at: proof.block_timestamp - 10,
        },
    )]);
    let records = BTreeMap::from([(
        "42".into(),
        polycopier::live_redemption::Record {
            assets: assets.clone(),
            proof: Some(proof.clone()),
            status: "confirmed".into(),
            created_at: proof.block_timestamp,
            transaction_hash: Some(proof.transaction_hash.clone()),
        },
    )]);
    let mut book = project(std::slice::from_ref(&fill), &orders).unwrap();
    redemption_settlement::apply(&mut book, &records, std::slice::from_ref(&fill)).unwrap();
    assert_eq!(book.inventory[&token].shares, dec!(0));
    assert_eq!(book.realized_pnl, dec!(64.970296));
    assert_eq!(book.closed_outcomes.len(), 1);
    assert_eq!(book.closed_outcomes[0].1, dec!(64.970296));
    let mut restored = project(std::slice::from_ref(&fill), &orders).unwrap();
    redemption_settlement::apply(&mut restored, &records, std::slice::from_ref(&fill)).unwrap();
    assert_eq!(restored.closed_outcomes, book.closed_outcomes);
    let mut pending = records.clone();
    pending.get_mut("42").unwrap().status = "mined_pending_finality".into();
    assert!(
        redemption_settlement::apply(&mut restored, &pending, std::slice::from_ref(&fill)).is_err()
    );
    let other_order = |side, id: &str| Record {
        terminal_unfilled: false,
        source_wallet: Some(FUNDER.into()),
        order: OrderRequest {
            token_id: "999".into(),
            side,
            price: dec!(0.5),
            size: dec!(4),
        },
        status: "accepted".into(),
        order_id: Some(id.into()),
        created_at: proof.block_timestamp - 20,
    };
    orders.insert(
        "other-buy".into(),
        other_order(TradeSide::BUY, "other-entry"),
    );
    orders.insert(
        "other-sell".into(),
        other_order(TradeSide::SELL, "other-exit"),
    );
    let other_buy = Fill {
        token_id: "999".into(),
        order_id: "other-entry".into(),
        shares: dec!(4),
        collateral: dec!(2),
        fee: dec!(0),
        block_number: proof.block_number - 2,
        block_timestamp: proof.block_timestamp - 20,
        ..fill.clone()
    };
    let other_sell = Fill {
        side: TradeSide::SELL,
        order_id: "other-exit".into(),
        collateral: dec!(1.5),
        block_number: proof.block_number + 1,
        block_timestamp: proof.block_timestamp + 2,
        ..other_buy.clone()
    };
    let all = vec![fill, other_buy, other_sell];
    let mut timeline = project(&all, &orders).unwrap();
    redemption_settlement::apply(&mut timeline, &records, &all).unwrap();
    assert!(timeline.closed_outcomes[0].0.starts_with("redeem:"));
    assert_eq!(timeline.closed_outcomes[1].1, dec!(-0.5));
    assert_eq!(timeline.realized_pnl, dec!(64.470296));
}
