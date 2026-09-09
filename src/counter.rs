//! The optional x402 counter (RFC-0046 S2).
//!
//! It verifies and records locally. Settlement is deliberately absent: that belongs to the separate
//! S3 back-office process, so neither an RPC nor a facilitator sits between a question and its answer.

use alloy_primitives::{Address, U256};
use anyhow::{bail, Context, Result};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::{LazyLock, Mutex};

/// The verifier stays beneath the same feature gate as the counter. Keeping the declaration here
/// means the default crate contains neither payment implementation nor a reachable payment module.
#[path = "x402.rs"]
#[cfg(feature = "counter")]
pub mod x402;

static AUTHORISATION_LOG: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
const LOG: &str = "authorisations.jsonl";

/// Operator-owned mount configuration. The price is USDC base units, as a string so TOML cannot
/// round a 256-bit amount before the verifier sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub price: String,
    pub recipient: String,
    pub network: Network,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Config {
    pub fn seller(&self) -> Result<x402::SellerConfig> {
        let pay_to = Address::from_str(&self.recipient)
            .with_context(|| format!("counter recipient '{}' is not an address", self.recipient))?;
        let price_base_units = U256::from_str(&self.price).with_context(|| {
            format!("counter price '{}' is not an unsigned integer", self.price)
        })?;
        if price_base_units.is_zero() {
            bail!("counter price must be greater than zero");
        }
        Ok(x402::SellerConfig {
            network: match self.network {
                Network::Mainnet => x402::SellerNetwork::Mainnet,
                Network::Testnet => x402::SellerNetwork::Testnet,
            },
            pay_to,
            price_base_units,
        })
    }
}

/// Verify a presented authorisation and durably record it before the query is served. Reusing a
/// nonce is refused locally: without that, one signed promise could purchase an unlimited number of
/// queries before the S3 settler ever saw it.
pub fn verify_and_record(
    dir: &std::path::Path,
    cfg: &Config,
    header: &str,
    now: u64,
) -> Result<(), x402::Refusal> {
    let seller = cfg.seller().map_err(|_| x402::Refusal::MissingFields)?;
    let accepted = x402::verify_payment(&seller, header, now)?;
    let _guard = AUTHORISATION_LOG
        .lock()
        .expect("authorisation log lock poisoned");
    let path = dir.join(LOG);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let nonce = accepted.nonce.to_string();
        if existing.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("nonce").and_then(|n| n.as_str()).map(str::to_owned))
                .is_some_and(|seen| seen == nonce)
        }) {
            return Err(x402::Refusal::AlreadyUsed);
        }
    }
    let row = serde_json::json!({
        "received_at": now,
        "payer": accepted.from.to_string(),
        "nonce": accepted.nonce.to_string(),
        "value": accepted.value.to_string(),
        "authorisation": header,
    });
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|_| x402::Refusal::RecordFailed)?;
    writeln!(file, "{row}").map_err(|_| x402::Refusal::RecordFailed)?;
    file.sync_data().map_err(|_| x402::Refusal::RecordFailed)?;
    Ok(())
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Admit one named-query request. The only failures disclosed here are the same challenge: a
/// malformed signature must not become an oracle for an attacker, and the buyer already has the
/// precise terms it needs in the challenge header.
pub fn admit(
    dir: &std::path::Path,
    cfg: &Config,
    headers: &HeaderMap,
    resource: &str,
    description: &str,
) -> Result<(), axum::response::Response> {
    let seller = cfg
        .seller()
        .map_err(|_| challenge(cfg, resource, description))?;
    let Some(header) = headers
        .get("Payment-Signature")
        .and_then(|v| v.to_str().ok())
    else {
        return Err(challenge(cfg, resource, description));
    };
    verify_and_record(dir, cfg, header, now())
        .map_err(|_| challenge(cfg, resource, description))?;
    // `seller` is constructed before receipt verification, so an invalid operator config cannot
    // issue a challenge that promises a payment the verifier will not accept.
    let _ = seller;
    Ok(())
}

fn challenge(cfg: &Config, resource: &str, description: &str) -> axum::response::Response {
    let header = cfg
        .seller()
        .map(|seller| x402::challenge_header(&seller, resource, description))
        .unwrap_or_default();
    (
        StatusCode::PAYMENT_REQUIRED,
        [("payment-required", header)],
        Json(serde_json::json!({
            "error": "Payment required",
            "resource": resource,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_price_is_refused() {
        let err = Config {
            price: "0".into(),
            recipient: "0x1111111111111111111111111111111111111111".into(),
            network: Network::Testnet,
        }
        .seller()
        .unwrap_err()
        .to_string();
        assert!(err.contains("greater than zero"), "{err}");
    }
}
