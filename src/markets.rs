//! Exact token lookup includes closed markets, which Gamma hides by default.
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn tokens(market: &Value) -> anyhow::Result<Vec<String>> {
    match &market["clobTokenIds"] {
        Value::String(raw) => Ok(serde_json::from_str(raw)?),
        Value::Array(_) => Ok(serde_json::from_value(market["clobTokenIds"].clone())?),
        _ => anyhow::bail!("Missing CTF token IDs"),
    }
}

pub fn select(markets: &Value, token: &str) -> anyhow::Result<Option<Value>> {
    let mut matches = markets
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid market response"))?
        .iter()
        .filter(|m| tokens(m).is_ok_and(|ids| ids.iter().any(|id| id == token)));
    let result = matches.next().cloned();
    anyhow::ensure!(matches.next().is_none(), "Multiple exact token markets");
    Ok(result)
}

pub async fn fetch(client: &reqwest::Client, token: &str) -> anyhow::Result<Value> {
    for closed in ["false", "true"] {
        let response: Value = client
            .get("https://gamma-api.polymarket.com/markets")
            .query(&[("clob_token_ids", token), ("closed", closed)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(market) = select(&response, token)? {
            return Ok(market);
        }
    }
    anyhow::bail!("No exact token market, including closed markets")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Details {
    pub title: String,
    pub outcome: String,
    pub end_date: Option<String>,
    pub closed: bool,
    pub reference_price: Option<Decimal>,
    pub price_source: String,
}

pub fn details(market: &Value, token: &str) -> anyhow::Result<Details> {
    let index = tokens(market)?
        .iter()
        .position(|id| id == token)
        .ok_or_else(|| anyhow::anyhow!("Market token mismatch"))?;
    let parse_list = |key: &str| -> anyhow::Result<Vec<String>> {
        match &market[key] {
            Value::String(raw) => Ok(serde_json::from_str(raw)?),
            Value::Array(_) => Ok(serde_json::from_value(market[key].clone())?),
            _ => anyhow::bail!("Missing {key}"),
        }
    };
    let reference_price = parse_list("outcomePrices")
        .ok()
        .and_then(|prices| prices.get(index)?.parse::<Decimal>().ok())
        .filter(|price| *price >= Decimal::ZERO && *price <= Decimal::ONE);
    Ok(Details {
        title: market["question"].as_str().unwrap_or(token).to_string(),
        outcome: parse_list("outcomes")?
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Missing outcome"))?,
        end_date: market["endDate"].as_str().map(str::to_string),
        closed: market["closed"].as_bool().unwrap_or(false),
        reference_price,
        price_source: "Gamma reference".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_lookup_and_closed_outcome_price() {
        let market = serde_json::json!({"question":"Resolved market", "clobTokenIds":"[\"1\",\"2\"]", "outcomes":"[\"Up\",\"Down\"]", "outcomePrices":"[\"1\",\"0\"]", "closed":true});
        assert!(select(&serde_json::json!([]), "1").unwrap().is_none());
        let list = serde_json::json!([{"clobTokenIds":"[\"3\",\"4\"]"}, market.clone()]);
        assert_eq!(select(&list, "1").unwrap().unwrap(), market);
        assert!(select(&serde_json::json!([market.clone(), market.clone()]), "1").is_err());
        let winner = details(&market, "1").unwrap();
        assert_eq!(winner.reference_price, Some(Decimal::ONE));
        assert_eq!(winner.outcome, "Up");
        assert!(winner.closed);
        assert_eq!(
            details(&market, "2").unwrap().reference_price,
            Some(Decimal::ZERO)
        );
        assert!(details(&market, "3").is_err());
    }
}
