use alloy_primitives::B256;
use polyfill_rs::errors::OrderErrorKind;
use polyfill_rs::types::{ApiCredentials, OrderOptions, PostOrder};
use polyfill_rs::{ClobClient, OrderArgs, OrderType, PolyfillError, Result, Side};
use serde_json::Value;
use std::env;
use std::str::FromStr;

const CHAIN_ID: u64 = 137;

struct RequiredEnv {
    host: String,
    private_key: String,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    condition_id: String,
    token_id: String,
}

impl RequiredEnv {
    fn load() -> Result<Self> {
        let required_names = [
            "POLYMARKET_CLOB_HOST",
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_API_KEY",
            "POLYMARKET_API_SECRET",
            "POLYMARKET_API_PASSPHRASE",
            "POLYMARKET_CONDITION_ID",
            "POLYMARKET_TOKEN_ID",
        ];
        let missing = required_names
            .iter()
            .filter(|name| matches!(env::var(name), Err(env::VarError::NotPresent)))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(PolyfillError::config(format!(
                "Blocked: missing required env vars {}",
                missing.join(", ")
            )));
        }

        Ok(Self {
            host: required_env("POLYMARKET_CLOB_HOST")?,
            private_key: required_env("POLYMARKET_PRIVATE_KEY")?,
            api_key: required_env("POLYMARKET_API_KEY")?,
            api_secret: required_env("POLYMARKET_API_SECRET")?,
            api_passphrase: required_env("POLYMARKET_API_PASSPHRASE")?,
            condition_id: required_env("POLYMARKET_CONDITION_ID")?,
            token_id: required_env("POLYMARKET_TOKEN_ID")?,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let required_env = RequiredEnv::load()?;
    let api_creds = ApiCredentials {
        api_key: required_env.api_key,
        secret: required_env.api_secret,
        passphrase: required_env.api_passphrase,
    };
    let builder = match env::var("POLYMARKET_BUILDER") {
        Ok(value) => B256::from_str(&value)
            .map_err(|e| PolyfillError::config(format!("Invalid POLYMARKET_BUILDER: {}", e)))?,
        Err(env::VarError::NotPresent) => B256::ZERO,
        Err(e) => {
            return Err(PolyfillError::config(format!(
                "Invalid POLYMARKET_BUILDER: {}",
                e
            )))
        },
    };

    let client = ClobClient::with_l2_headers(
        &required_env.host,
        &required_env.private_key,
        CHAIN_ID,
        api_creds.clone(),
    );

    let market = client
        .get_clob_market_info(&required_env.condition_id)
        .await?;
    if !market
        .tokens
        .iter()
        .any(|token| token.token_id == required_env.token_id)
    {
        return Err(PolyfillError::validation(format!(
            "POLYMARKET_TOKEN_ID {} is not in market {}",
            required_env.token_id, required_env.condition_id
        )));
    }
    println!(
        "market-info fetch: condition_id={} tokens={} minimum_tick_size={} minimum_order_size={} neg_risk={}",
        market.condition_id,
        market.tokens.len(),
        market.minimum_tick_size,
        market.minimum_order_size,
        market.neg_risk
    );

    let order_args = OrderArgs::new(
        &required_env.token_id,
        market.minimum_tick_size,
        market.minimum_order_size,
        Side::BUY,
    );
    let options = OrderOptions {
        tick_size: Some(market.minimum_tick_size),
        neg_risk: Some(market.neg_risk),
        builder,
    };
    let signed_order = client.create_order(&order_args, 0, Some(&options)).await?;
    let body = PostOrder::with_post_only(
        signed_order.clone(),
        api_creds.api_key.clone(),
        OrderType::GTC,
        true,
    );
    verify_v2_body(&body)?;
    println!("V1-field absence check: taker=false nonce=false feeRateBps=false");

    let post_response = client
        .post_order_with_options(signed_order, OrderType::GTC, true)
        .await?;
    println!("post response: {}", post_response);

    let order_id = post_response
        .get("orderID")
        .and_then(Value::as_str)
        .ok_or_else(|| PolyfillError::parse("Post response missing orderID", None))?;

    match client.get_order(order_id).await {
        Ok(order) => println!("query response: {:?}", order),
        Err(error) => {
            if let Err(cancel_error) = client.cancel(order_id).await {
                return Err(PolyfillError::order(
                    format!(
                        "query failed after post: {}; cleanup cancel failed: {}",
                        error, cancel_error
                    ),
                    OrderErrorKind::CancellationFailed,
                ));
            }
            return Err(error);
        },
    }

    let cancel_response = client.cancel(order_id).await?;
    println!("cancel response: {}", cancel_response);

    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Err(PolyfillError::config(format!(
            "Blocked: missing required env var {}",
            name
        ))),
        Err(e) => Err(PolyfillError::config(format!("Invalid {}: {}", name, e))),
    }
}

fn verify_v2_body(body: &PostOrder) -> Result<()> {
    let value = serde_json::to_value(body)?;
    let order = value
        .get("order")
        .ok_or_else(|| PolyfillError::parse("Serialized PostOrder missing order", None))?;

    for field in ["taker", "nonce", "feeRateBps"] {
        if order.get(field).is_some() {
            return Err(PolyfillError::validation(format!(
                "Serialized V2 order contains removed V1 field {}",
                field
            )));
        }
    }

    Ok(())
}
