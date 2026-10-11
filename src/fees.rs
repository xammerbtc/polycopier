//! Paper taker fees use the market's current Gamma schedule, not category guesses.
use rust_decimal::Decimal;
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};
pub type FeeQuery = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<Decimal>> + Send>> + Send + Sync,
>;

pub fn rate(market: &Value, token: &str) -> anyhow::Result<Decimal> {
    let tokens: Vec<String> = serde_json::from_str(
        market["clobTokenIds"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing CTF token IDs"))?,
    )?;
    anyhow::ensure!(
        tokens.iter().any(|id| id == token),
        "Fee market token mismatch"
    );
    match market["feesEnabled"].as_bool() {
        Some(false) => Ok(Decimal::ZERO),
        Some(true) => {
            let schedule = &market["feeSchedule"];
            anyhow::ensure!(
                schedule["exponent"].as_f64() == Some(1.0)
                    && schedule["takerOnly"].as_bool() == Some(true),
                "Unsupported fee schedule"
            );
            let raw = match &schedule["rate"] {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => anyhow::bail!("Missing fee rate"),
            };
            let rate: Decimal = raw.parse()?;
            anyhow::ensure!(
                rate >= Decimal::ZERO && rate <= Decimal::ONE,
                "Invalid fee rate"
            );
            Ok(rate)
        }
        None => anyhow::bail!("Missing feesEnabled flag"),
    }
}
pub fn taker_fee(shares: Decimal, price: Decimal, rate: Decimal) -> Decimal {
    (shares * rate * price * (Decimal::ONE - price)).round_dp(5)
}
pub fn public_query() -> FeeQuery {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("fee HTTP client");
    Arc::new(move |token| {
        let client = client.clone();
        Box::pin(async move {
            let markets: Value = client
                .get("https://gamma-api.polymarket.com/markets")
                .query(&[("clob_token_ids", &token)])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let markets = markets
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Invalid fee market response"))?;
            anyhow::ensure!(markets.len() == 1, "Ambiguous fee market response");
            rate(&markets[0], &token)
        })
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_market_schedule_and_precision() {
        let mut market = serde_json::json!({"clobTokenIds":"[\"1\",\"2\"]", "feesEnabled":true, "feeSchedule":{"rate":0.07,"exponent":1,"takerOnly":true}});
        let r = rate(&market, "1").unwrap();
        assert_eq!(
            taker_fee(Decimal::from(100), Decimal::new(5, 1), r),
            Decimal::new(175, 2)
        );
        assert!(rate(&market, "3").is_err());
        market["feeSchedule"]["exponent"] = 2.into();
        assert!(rate(&market, "1").is_err());
        market["feesEnabled"] = false.into();
        assert_eq!(rate(&market, "1").unwrap(), Decimal::ZERO);
        market["feesEnabled"] = Value::Null;
        assert!(rate(&market, "1").is_err());
    }
}
