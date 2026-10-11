//! Preview settlement uses authoritative winner flags, never an inferred price.
use rust_decimal::Decimal;
use serde_json::Value;

pub fn settle_paper(
    state: &mut crate::state::BotState,
    token: &str,
    payout: Decimal,
) -> Option<(Decimal, Decimal)> {
    if payout != Decimal::ZERO && payout != Decimal::ONE {
        return None;
    }
    let position = state.positions.remove(token)?;
    let proceeds = position.size * payout;
    let pnl = proceeds - position.size * position.average_entry_price;
    state.total_balance += proceeds;
    state.realized_pnl += pnl;
    Some((proceeds, pnl))
}

pub fn payout(market: &Value, token: &str) -> Option<Decimal> {
    if market["closed"].as_bool() != Some(true) {
        return None;
    }
    let tokens = market["tokens"].as_array()?;
    // Ambiguous / void / split resolutions need a payout vector, not winner flags.
    if tokens.len() != 2
        || tokens
            .iter()
            .filter(|t| t["winner"].as_bool() == Some(true))
            .count()
            != 1
    {
        return None;
    }
    let outcome = tokens
        .iter()
        .find(|t| t["token_id"].as_str() == Some(token))?;
    match outcome["winner"].as_bool()? {
        true => Some(Decimal::ONE),
        false => Some(Decimal::ZERO),
    }
}

pub async fn resolved_payout(
    client: &reqwest::Client,
    token: &str,
) -> anyhow::Result<Option<Decimal>> {
    let markets: Value = client
        .get("https://gamma-api.polymarket.com/markets")
        .query(&[("clob_token_ids", token)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(market) = markets.as_array().and_then(|v| v.first()) else {
        return Ok(None);
    };
    let Some(condition) = market["conditionId"].as_str() else {
        return Ok(None);
    };
    anyhow::ensure!(
        condition.starts_with("0x")
            && matches!(condition.len(), 64 | 66)
            && condition[2..].chars().all(|c| c.is_ascii_hexdigit()),
        "Invalid condition ID"
    );
    let settled: Value = client
        .get(format!("https://clob.polymarket.com/markets/{condition}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(payout(&settled, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paper_settlement_credits_once_and_realizes_winner_and_loser() {
        use crate::{models::Position, state::BotState};
        let mut state = BotState::new(true, Some(Decimal::from(190)));
        for token in ["yes", "no"] {
            state.positions.insert(
                token.into(),
                Position {
                    token_id: token.into(),
                    size: Decimal::from(10),
                    average_entry_price: Decimal::new(5, 1),
                },
            );
        }
        assert_eq!(settle_paper(&mut state, "yes", Decimal::new(5, 1)), None);
        assert_eq!(state.positions.len(), 2);
        assert_eq!(
            settle_paper(&mut state, "yes", Decimal::ONE),
            Some((Decimal::from(10), Decimal::from(5)))
        );
        assert_eq!(settle_paper(&mut state, "yes", Decimal::ONE), None);
        assert_eq!(
            settle_paper(&mut state, "no", Decimal::ZERO),
            Some((Decimal::ZERO, Decimal::from(-5)))
        );
        assert_eq!(state.total_balance, Decimal::from(200));
        assert_eq!(state.realized_pnl, Decimal::ZERO);
        assert!(state.positions.is_empty());
    }
    #[test]
    fn requires_closed_unambiguous_resolution_and_exact_token() {
        let mut market = serde_json::json!({"closed":true,"tokens":[{"token_id":"yes","winner":true},{"token_id":"no","winner":false}]});
        assert_eq!(payout(&market, "yes"), Some(Decimal::ONE));
        assert_eq!(payout(&market, "no"), Some(Decimal::ZERO));
        assert_eq!(payout(&market, "other"), None);
        market["closed"] = false.into();
        assert_eq!(payout(&market, "yes"), None);
        market["closed"] = true.into();
        market["tokens"][0]["winner"] = false.into();
        assert_eq!(payout(&market, "no"), None);
        market["tokens"][0]["winner"] = true.into();
        market["tokens"][1]["winner"] = true.into();
        assert_eq!(payout(&market, "yes"), None);
    }
}
