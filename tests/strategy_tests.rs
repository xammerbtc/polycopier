//! Tests for strategy engine helpers: limit price calculation, entry sizing,
//! and the compute_order_usd sizing function.

use polycopier::models::TradeSide;
use polycopier::strategy::{calculate_entry_size, calculate_limit_price};
use rust_decimal_macros::dec;

#[test]
fn buy_limit_price_adds_slippage() {
    let price = dec!(0.50);
    let slippage = dec!(0.02);
    let result = calculate_limit_price(price, TradeSide::BUY, slippage);
    assert_eq!(result, dec!(0.51));
}

#[test]
fn sell_limit_price_subtracts_slippage() {
    let price = dec!(0.50);
    let slippage = dec!(0.02);
    let result = calculate_limit_price(price, TradeSide::SELL, slippage);
    assert_eq!(result, dec!(0.49));
}

#[test]
fn entry_size_within_budget_unchanged() {
    // 10 shares at $0.40 = $4 -- under $10 max
    let result = calculate_entry_size(dec!(10), dec!(0.40), dec!(10));
    assert_eq!(result, dec!(10));
}

#[test]
fn entry_size_capped_to_max_usd() {
    // 100 shares at $0.40 = $40 -- over $10 max => 10/0.40 = 25 shares
    let result = calculate_entry_size(dec!(100), dec!(0.40), dec!(10));
    assert_eq!(result, dec!(25));
}

#[test]
fn zero_slippage_keeps_price_unchanged() {
    let price = dec!(0.77);
    let result = calculate_limit_price(price, TradeSide::BUY, dec!(0));
    assert_eq!(result, price);
}

#[test]
fn safe_profile_slippage_is_a_hard_bound_after_tick_rounding() {
    for mills in 30..=970 {
        let source = rust_decimal::Decimal::new(mills, 3);
        for slippage in [dec!(0), dec!(0.015), dec!(0.02)] {
            let buy = calculate_limit_price(source, TradeSide::BUY, slippage);
            let sell = calculate_limit_price(source, TradeSide::SELL, slippage);
            assert!(
                buy <= source * (dec!(1) + slippage),
                "BUY price exceeded configured slippage for {source}"
            );
            assert!(
                sell >= source * (dec!(1) - slippage),
                "SELL price exceeded configured slippage for {source}"
            );
            assert_eq!(buy, buy.round_dp(2));
            assert_eq!(sell, sell.round_dp(2));
        }
    }
    assert_eq!(
        calculate_limit_price(dec!(0.65), TradeSide::BUY, dec!(0.015)),
        dec!(0.65)
    );
    assert_eq!(
        calculate_limit_price(dec!(0.80), TradeSide::SELL, dec!(0.02)),
        dec!(0.79)
    );
}
