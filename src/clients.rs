use crate::config::Config;
use crate::models::{OrderRequest, TradeSide};
use alloy::primitives::Address;
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use anyhow::{bail, Result};
use core::pin::Pin;
use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk::clob::types::Side as SdkSide;
use polymarket_client_sdk::clob::types::SignatureType;
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::types::U256;
use rust_decimal::Decimal;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

pub type OrderSubmitter = Arc<
    dyn Fn(OrderRequest) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>
        + Send
        + Sync,
>;
pub type BalanceFetcher =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Decimal>> + Send + 'static>> + Send + Sync>;

use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;

pub type AuthedClobClient = ClobClient<Authenticated<Normal>>;

pub fn signature_type(value: &str) -> Result<SignatureType> {
    match value.to_lowercase().as_str() {
        "0" | "eoa" => Ok(SignatureType::Eoa),
        "1" | "proxy" => Ok(SignatureType::Proxy),
        "2" | "safe" | "gnosis_safe" => Ok(SignatureType::GnosisSafe),
        _ => bail!("CLOB_SIGNATURE_TYPE must be eoa/0, proxy/1 or safe/2"),
    }
}

pub async fn build_order_submitter(
    config: &Config,
) -> Result<(OrderSubmitter, BalanceFetcher, AuthedClobClient)> {
    crate::execution_journal::initialize()?;
    let signer = LocalSigner::from_str(&config.private_key)?.with_chain_id(Some(config.chain_id));
    let funder = Address::from_str(&config.funder_address)?;

    // Connect directly to CLOB and authenticate automatically
    let clob = ClobClient::new("https://clob.polymarket.com", ClobConfig::default())?
        .authentication_builder(&signer)
        .funder(funder)
        .signature_type(signature_type(
            &std::env::var("CLOB_SIGNATURE_TYPE").unwrap_or_else(|_| "proxy".into()),
        )?)
        .authenticate()
        .await?;

    let ok = clob.ok().await?;
    tracing::info!("CLOB Authenticated successfully. Ping: {}", ok);

    // Clone the client for the balance fetcher before it moves into the order closure
    let clob_for_balance = clob.clone();

    let balance_fetcher: BalanceFetcher = Arc::new(move || {
        let clob = clob_for_balance.clone();
        Box::pin(async move {
            let resp = clob
                .balance_allowance(BalanceAllowanceRequest::default())
                .await?;
            // balance is returned in micro-USDC (6 decimal places) - convert to USDC
            Ok(resp.balance / Decimal::from(1_000_000))
        })
    });

    let clob_for_submitter = clob.clone();
    let order_submitter: OrderSubmitter = Arc::new(move |order: OrderRequest| {
        let clob = clob_for_submitter.clone();
        let signer = signer.clone();

        Box::pin(async move {
            tracing::info!(
                "Executing Limit Order: Side={:?}, Price={}, Size={}, Token={}",
                order.side,
                order.price,
                order.size,
                order.token_id
            );

            let side = match order.side {
                TradeSide::BUY => SdkSide::Buy,
                TradeSide::SELL => SdkSide::Sell,
            };

            let order_req = clob
                .limit_order()
                // token_id from Data API is a decimal U256 string
                .token_id(U256::from_str(&order.token_id)?)
                .size(order.size)
                .price(order.price)
                .side(side)
                .build()
                .await?;

            let signed_order = clob.sign(&signer, order_req).await?;
            let intent = crate::execution_journal::begin_contextual(&order)?;

            match clob.post_order(signed_order).await {
                Ok(res) => {
                    // The CLOB returns HTTP 200 even for rejected orders.
                    // We must check res.success and res.error_msg explicitly.
                    if !res.success {
                        if crate::execution_journal::finish(&intent, "rejected", None).is_err() {
                            crate::execution_journal::uncertain(&intent);
                            return Err(crate::execution_journal::UncertainExecution.into());
                        }
                        let msg = res.error_msg.as_deref().unwrap_or("(no message)");
                        tracing::warn!(
                            "Order rejected by CLOB: error_msg={:?}, status={:?}, \
                             making_amount={}, taking_amount={}",
                            msg,
                            res.status,
                            res.making_amount,
                            res.taking_amount
                        );
                        bail!(
                            "CLOB rejected order: {}",
                            if msg.is_empty() {
                                "unknown error (empty error_msg)"
                            } else {
                                msg
                            }
                        );
                    }
                    if let Some(msg) = &res.error_msg {
                        if !msg.is_empty() {
                            tracing::warn!("Order success=true but error_msg non-empty: {:?}", msg);
                        }
                    }
                    tracing::info!(
                        "Order accepted: id={}, status={:?}, making={}, taking={}",
                        res.order_id,
                        res.status,
                        res.making_amount,
                        res.taking_amount
                    );
                    if res.order_id.is_empty()
                        || crate::execution_journal::finish(&intent, "accepted", Some(res.order_id))
                            .is_err()
                    {
                        crate::execution_journal::uncertain(&intent);
                        return Err(crate::execution_journal::UncertainExecution.into());
                    }
                    Ok(())
                }
                Err(e) => {
                    tracing::error!("API Execution Failed: {}", e);
                    crate::execution_journal::uncertain(&intent);
                    Err(crate::execution_journal::UncertainExecution.into())
                }
            }
        })
    });

    Ok((order_submitter, balance_fetcher, clob))
}

pub async fn build_sim_order_submitter(
    _config: &Config,
) -> Result<(OrderSubmitter, BalanceFetcher)> {
    let balance_fetcher: BalanceFetcher = Arc::new(|| {
        Box::pin(async move {
            // Balance is managed internally by the strategy logic in sim mode.
            // This is just a dummy fetcher.
            Ok(Decimal::from(10000))
        })
    });

    let order_submitter: OrderSubmitter = Arc::new(|order: OrderRequest| {
        Box::pin(async move {
            tracing::warn!(
                "[SIMULATION] Paper-filling Limit Order: Side={:?}, Price={}, Size={}, Token={}",
                order.side,
                order.price,
                order.size,
                order.token_id
            );
            // Simulate normal API delay
            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
            Ok(())
        })
    });

    Ok((order_submitter, balance_fetcher))
}
