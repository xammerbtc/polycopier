use alloy::primitives::{keccak256, U256};
use polycopier::{
    execution_journal::Record,
    live_fills::{decode_receipt, project},
    models::{OrderRequest, TradeSide},
};
use rust_decimal_macros::dec;
use serde_json::{json, Value};
use std::collections::BTreeMap;
const FUNDER: &str = "0x2222222222222222222222222222222222222222";

#[test]
fn real_public_v2_event_preserves_exact_settlement_fee() {
    // A public target-wallet receipt, not an order placed by this copier.
    let r: Value =
        serde_json::from_str(include_str!("fixtures/ctf-v2-public-receipt.json")).unwrap();
    let id = r["logs"][0]["topics"][1].as_str().unwrap();
    let token = "39149205690560386934532903283562315630593314765734603696007696865904985113548";
    let j = BTreeMap::from([(
        "fixture".into(),
        Record {
            terminal_unfilled: false,
            source_wallet: Some("0x2d2d2b32e5199edf833594a99b9c54e5668ab69d".into()),
            order: OrderRequest {
                token_id: token.into(),
                price: dec!(0.65),
                size: dec!(24),
                side: TradeSide::BUY,
                budget_usd: None,
            },
            status: "accepted".into(),
            order_id: Some(id.into()),
            created_at: 0,
        },
    )]);
    let fills = decode_receipt(
        &r,
        "0x099aa76ffbab88788d4f07c6a370579c95e7c7c2fabb24e062981c4599b6092c",
        u64::MAX,
        "0x2d2d2b32e5199edf833594a99b9c54e5668ab69d",
        &j,
    )
    .unwrap();
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].shares, dec!(23.076922));
    assert_eq!(fills[0].collateral, dec!(14.999999));
    assert_eq!(fills[0].fee, dec!(0.36749));
    let p = project(&fills, &j).unwrap();
    assert_eq!(p.inventory[token].cost_basis, dec!(15.367489));
    assert_eq!(p.realized_pnl, dec!(0));
}
fn order(side: TradeSide, id: &str, size: rust_decimal::Decimal) -> Record {
    Record {
        terminal_unfilled: false,
        source_wallet: Some(FUNDER.into()),
        order: OrderRequest {
            token_id: "123".into(),
            price: dec!(0.5),
            size,
            side,
            budget_usd: None,
        },
        status: "accepted".into(),
        order_id: Some(id.into()),
        created_at: 0,
    }
}

#[test]
fn budget_order_accepts_more_shares_at_better_prices_but_never_overspends() {
    let mut record = order(TradeSide::BUY, "budget-order", dec!(5.10));
    record.order.price = dec!(0.98);
    record.order.budget_usd = Some(dec!(5));
    let journal = BTreeMap::from([("budget".into(), record)]);
    let fill = polycopier::live_fills::Fill {
        transaction_hash: "0x1".into(),
        block_hash: "0x2".into(),
        block_number: 1,
        block_timestamp: 1,
        transaction_index: 0,
        log_index: 0,
        order_id: "budget-order".into(),
        token_id: "123".into(),
        side: TradeSide::BUY,
        shares: dec!(10),
        collateral: dec!(4.5),
        fee: dec!(0.1),
    };
    let projection = project(std::slice::from_ref(&fill), &journal).unwrap();
    assert_eq!(projection.inventory["123"].shares, dec!(10));
    let mut over_budget = fill.clone();
    over_budget.collateral = dec!(4.95);
    assert!(project(&[over_budget], &journal).is_err());
    let mut above_cap = fill;
    above_cap.collateral = dec!(9.9);
    assert!(project(&[above_cap], &journal)
        .unwrap_err()
        .to_string()
        .contains("signed price cap"));
}

#[test]
fn repeated_live_buys_keep_source_lots_separate_on_partial_sale() {
    let mut first = order(TradeSide::BUY, "buy-a", dec!(10));
    first.source_wallet = Some("source-a".into());
    let mut second = order(TradeSide::BUY, "buy-b", dec!(10));
    second.source_wallet = Some("source-b".into());
    let mut sale = order(TradeSide::SELL, "sell-a", dec!(5));
    sale.source_wallet = Some("source-a".into());
    let journal = BTreeMap::from([
        ("a".into(), first),
        ("b".into(), second),
        ("c".into(), sale),
    ]);
    let make_fill =
        |id: &str, side: TradeSide, shares, collateral, log_index| polycopier::live_fills::Fill {
            transaction_hash: format!("tx-{log_index}"),
            block_hash: "block".into(),
            block_number: 1,
            block_timestamp: 1,
            transaction_index: 0,
            log_index,
            order_id: id.into(),
            token_id: "123".into(),
            side,
            shares,
            collateral,
            fee: dec!(0),
        };
    let fills = [
        make_fill("buy-a", TradeSide::BUY, dec!(10), dec!(4), 0),
        make_fill("buy-b", TradeSide::BUY, dec!(10), dec!(7), 1),
        make_fill("sell-a", TradeSide::SELL, dec!(5), dec!(3), 2),
    ];
    let projection = project(&fills, &journal).unwrap();
    assert_eq!(projection.inventory["123"].shares, dec!(15));
    assert_eq!(projection.inventory["123"].cost_basis, dec!(9));
    assert_eq!(projection.realized_pnl, dec!(1));
    let a = projection
        .source_entries
        .iter()
        .find(|entry| entry.source_wallet == "source-a")
        .unwrap();
    let b = projection
        .source_entries
        .iter()
        .find(|entry| entry.source_wallet == "source-b")
        .unwrap();
    assert_eq!(a.filled_size, Some(dec!(5)));
    assert_eq!(a.realized_pnl, dec!(1));
    assert_eq!(b.filled_size, Some(dec!(10)));
    assert_eq!(b.realized_pnl, dec!(0));
}
fn receipt(
    side: u64,
    order_id: &str,
    making: u64,
    taking: u64,
    fee: u64,
    block: u64,
    index: u64,
) -> Value {
    let data = [side, 123, making, taking, fee, 0, 0]
        .map(|v| format!("{:064x}", U256::from(v)))
        .join("");
    json!({"status":"0x1","transactionHash":format!("0x{block:064x}"),"blockHash":format!("0x{:064x}",block+1),"blockNumber":format!("0x{block:x}"),"transactionIndex":"0x0","logs":[{"address":"0xE111180000d2663C0091e4f400237545B87B996B","topics":[format!("{:#x}",keccak256("OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)")),order_id,format!("0x{}{}","0".repeat(24),&FUNDER[2..]),format!("0x{}","0".repeat(64))],"data":format!("0x{data}"),"logIndex":format!("0x{index:x}")}]})
}
fn decode(
    r: &Value,
    j: &BTreeMap<String, Record>,
) -> anyhow::Result<Vec<polycopier::live_fills::Fill>> {
    decode_receipt(r, r["transactionHash"].as_str().unwrap(), 999, FUNDER, j)
}
#[test]
fn partial_fills_exact_fees_cost_basis_and_closed_trade_outcome() {
    let buy = format!("0x{}", "a".repeat(64));
    let sell = format!("0x{}", "b".repeat(64));
    let j = BTreeMap::from([
        ("buy".into(), order(TradeSide::BUY, &buy, dec!(10))),
        ("sell".into(), order(TradeSide::SELL, &sell, dec!(10))),
    ]);
    let b = decode(
        &receipt(0, &buy, 5_000_000, 10_000_000, 100_000, 100, 0),
        &j,
    )
    .unwrap();
    let s1 = decode(&receipt(1, &sell, 4_000_000, 3_000_000, 20_000, 101, 0), &j).unwrap();
    let s2 = decode(&receipt(1, &sell, 6_000_000, 4_500_000, 30_000, 102, 0), &j).unwrap();
    let partial = project(&[b[0].clone(), s1[0].clone()], &j).unwrap();
    assert_eq!(partial.inventory["123"].shares, dec!(6));
    assert_eq!(partial.inventory["123"].cost_basis, dec!(3.06));
    assert_eq!(partial.realized_pnl, dec!(0.94));
    assert!(partial.closed_outcomes.is_empty());
    let completed = project(
        &[s2[0].clone(), b[0].clone(), s1[0].clone(), b[0].clone()],
        &j,
    )
    .unwrap();
    assert_eq!(completed.inventory["123"].shares, dec!(0));
    assert_eq!(completed.realized_pnl, dec!(2.35));
    assert_eq!(completed.fees, dec!(0.15));
    assert_eq!(completed.closed_outcomes.len(), 1);
    assert_eq!(completed.closed_outcomes[0].1, dec!(2.35));
}
#[test]
fn failed_unfinalized_mismatched_and_removed_receipts_cannot_credit_inventory() {
    let id = format!("0x{}", "a".repeat(64));
    let j = BTreeMap::from([("buy".into(), order(TradeSide::BUY, &id, dec!(10)))]);
    let base = receipt(0, &id, 5_000_000, 10_000_000, 0, 100, 0);
    assert!(decode_receipt(
        &base,
        base["transactionHash"].as_str().unwrap(),
        99,
        FUNDER,
        &j
    )
    .is_err());
    assert!(decode_receipt(&base, &format!("0x{}", "c".repeat(64)), 999, FUNDER, &j).is_err());
    for (field, value) in [("status", json!("0x0")), ("blockHash", json!("bad"))] {
        let mut r = base.clone();
        r[field] = value;
        assert!(decode(&r, &j).is_err());
    }
    let mut r = base.clone();
    r["logs"][0]["removed"] = json!(true);
    assert!(decode(&r, &j).is_err());
    let mut r = base.clone();
    r["logs"][0]["topics"][2] = json!(format!("0x{}", "1".repeat(64)));
    assert!(decode(&r, &j).is_err());
}
#[test]
fn foreign_orders_and_unrecognized_contracts_are_excluded() {
    let id = format!("0x{}", "a".repeat(64));
    let other = format!("0x{}", "b".repeat(64));
    let j = BTreeMap::from([("buy".into(), order(TradeSide::BUY, &id, dec!(10)))]);
    assert!(
        decode(&receipt(0, &other, 5_000_000, 10_000_000, 0, 100, 0), &j)
            .unwrap()
            .is_empty()
    );
    let mut r = receipt(0, &id, 5_000_000, 10_000_000, 0, 100, 0);
    r["logs"][0]["address"] = json!(FUNDER);
    assert!(decode(&r, &j).unwrap().is_empty());
}
#[test]
fn conflicting_duplicate_and_excess_settlement_are_rejected() {
    let id = format!("0x{}", "a".repeat(64));
    let j = BTreeMap::from([("buy".into(), order(TradeSide::BUY, &id, dec!(10)))]);
    let mut r = receipt(0, &id, 5_000_000, 10_000_000, 0, 100, 0);
    let log = r["logs"][0].clone();
    r["logs"].as_array_mut().unwrap().push(log);
    assert_eq!(decode(&r, &j).unwrap().len(), 1);
    r["logs"][1]["data"] = json!("0x00");
    assert!(decode(&r, &j).is_err());
    let a = decode(&receipt(0, &id, 5_000_000, 10_000_000, 0, 100, 0), &j).unwrap();
    let b = decode(&receipt(0, &id, 500_000, 1_000_000, 0, 101, 0), &j).unwrap();
    assert!(project(&[a[0].clone(), b[0].clone()], &j).is_err());
}
#[test]
fn a_sale_cannot_consume_another_bots_inventory() {
    let id = format!("0x{}", "b".repeat(64));
    let j = BTreeMap::from([("sell".into(), order(TradeSide::SELL, &id, dec!(10)))]);
    let fills = decode(&receipt(1, &id, 10_000_000, 5_000_000, 0, 100, 0), &j).unwrap();
    assert!(project(&fills, &j).is_err());
}

#[tokio::test]
async fn rpc_requires_polygon_finality_and_matching_canonical_header() {
    use axum::{routing::post, Json, Router};
    let id = format!("0x{}", "a".repeat(64));
    let base = receipt(0, &id, 5_000_000, 10_000_000, 0, 100, 0);
    for (chain, height, canonical, expected) in [
        (137, 200, true, 1),
        (1, 200, true, -1),
        (137, 99, true, 0),
        (137, 200, false, -1),
    ] {
        let base = base.clone();
        let tx = base["transactionHash"].as_str().unwrap().to_string();
        let app=Router::new().route("/",post(move|Json(request):Json<Value>|{let base=base.clone();async move {
            let result=match request["method"].as_str().unwrap() {
                "eth_chainId"=>json!(format!("0x{chain:x}")),
                "eth_getTransactionReceipt"=>base.clone(),
                "eth_getBlockByNumber"=>if request["params"][0]=="finalized" {json!({"number":format!("0x{height:x}")})} else {json!({"timestamp":"0x64","number":base["blockNumber"],"hash":if canonical {base["blockHash"].clone()} else {json!(format!("0x{}","f".repeat(64)))}})},
                _=>panic!("unexpected RPC method"),
            };
            Json(json!({"jsonrpc":"2.0","id":1,"result":result}))
        }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let result = polycopier::live_fills::finalized_receipt(&client, &url, &tx).await;
        task.abort();
        match expected {
            1 => assert!(result.unwrap().is_some()),
            0 => assert!(result.unwrap().is_none()),
            _ => assert!(result.is_err()),
        }
    }
}
