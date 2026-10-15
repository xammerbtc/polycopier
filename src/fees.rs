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

/// CLOB V2 publishes the executable market's fee details by condition ID.
/// Verify that the requested token is one of that market's outcomes before
/// using its rate for paper accounting.
pub fn clob_rate(market: &Value, token: &str, condition: &str) -> anyhow::Result<Decimal> {
    anyhow::ensure!(
        market["c"].as_str() == Some(condition),
        "CLOB condition mismatch"
    );
    anyhow::ensure!(
        market["t"].as_array().is_some_and(|tokens| tokens
            .iter()
            .any(|entry| entry["t"].as_str() == Some(token))),
        "CLOB fee market token mismatch"
    );
    let fee = &market["fd"];
    if fee.is_null() {
        return Ok(Decimal::ZERO);
    }
    anyhow::ensure!(
        fee["e"].as_u64() == Some(1) && fee["to"].as_bool() == Some(true),
        "Unsupported CLOB fee schedule"
    );
    let rate: Decimal = match &fee["r"] {
        Value::String(value) => value.parse()?,
        Value::Number(value) => value.to_string().parse()?,
        _ => anyhow::bail!("Missing CLOB fee rate"),
    };
    anyhow::ensure!(
        (Decimal::ZERO..=Decimal::ONE).contains(&rate),
        "Invalid CLOB fee rate"
    );
    Ok(rate)
}

async fn fetch_clob_rate(client: &reqwest::Client, token: &str) -> anyhow::Result<Decimal> {
    anyhow::ensure!(
        !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit()),
        "Invalid CLOB token ID"
    );
    let lookup: Value = client
        .get(format!(
            "https://clob.polymarket.com/markets-by-token/{token}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let condition = lookup["condition_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing CLOB condition ID"))?;
    anyhow::ensure!(
        condition.len() == 66
            && condition.starts_with("0x")
            && condition[2..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid CLOB condition ID"
    );
    let market: Value = client
        .get(format!(
            "https://clob.polymarket.com/clob-markets/{condition}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    clob_rate(&market, token, condition)
}
pub fn public_query() -> FeeQuery {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .tcp_nodelay(true)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("polycopier/0.1")
        .build()
        .expect("fee HTTP client");
    let requests = Arc::new(tokio::sync::Semaphore::new(2));
    Arc::new(move |token| {
        let client = client.clone();
        let requests = requests.clone();
        Box::pin(async move {
            match fetch_clob_rate(&client, &token).await {
                Ok(rate) => return Ok(rate),
                Err(error) => {
                    tracing::warn!("CLOB fee lookup failed for {token}; trying Gamma: {error}")
                }
            }
            for attempt in 0..3 {
                let permit = requests
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| anyhow::anyhow!("fee query limiter closed"))?;
                let result = crate::markets::fetch(&client, &token).await;
                drop(permit);
                match result {
                    Ok(market) => return rate(&market, &token),
                    Err(error) if attempt < 2 && is_rate_limited(&error) => {
                        let delay_ms = 250_u64 << attempt;
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            anyhow::bail!("Gamma fee lookup exhausted retries")
        })
    })
}

fn is_rate_limited(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .is_some_and(|status| status == reqwest::StatusCode::TOO_MANY_REQUESTS)
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

    #[test]
    fn exact_clob_market_fee_schedule() {
        let market = serde_json::json!({"c":"0xabc", "t":[{"t":"123"},{"t":"456"}],
            "fd":{"r":0.05,"e":1,"to":true}});
        assert_eq!(
            clob_rate(&market, "123", "0xabc").unwrap(),
            Decimal::new(5, 2)
        );
        assert!(clob_rate(&market, "999", "0xabc").is_err());
        assert!(clob_rate(&market, "123", "0xdef").is_err());
    }
}
