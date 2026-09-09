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
use std::io::BufRead;
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

/// Whether this `(payer, nonce)` pair already appears in the durable log.
///
/// Split out of [`verify_and_record`] so the three properties that matter can be tested without
/// forging a signature: the key is the pair and not the nonce alone, an absent log is empty rather
/// than an error, and an unreadable one refuses rather than admits.
fn is_spent(path: &std::path::Path, payer: &str, nonce: &str) -> Result<bool, x402::Refusal> {
    let file = match std::fs::File::open(path) {
        // The ordinary first-request case. Nothing has been sold yet, so nothing is spent.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        // **Fail closed.** Any other IO error means we cannot show the nonce is unspent, and
        // admitting on that basis is how one signed promise buys an unlimited number of queries.
        Err(_) => return Err(x402::Refusal::RecordFailed),
        Ok(file) => file,
    };
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            return Err(x402::Refusal::RecordFailed);
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("payer").and_then(|n| n.as_str()) == Some(payer)
            && v.get("nonce").and_then(|n| n.as_str()) == Some(nonce)
        {
            return Ok(true);
        }
    }
    Ok(false)
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
    // **Keyed by (payer, nonce), and streamed rather than slurped.**
    //
    // EIP-3009 authorisation state is keyed by authoriser *and* nonce, so a nonce is only ever spent
    // for the payer who signed it. Matching on the nonce alone refuses payer B's perfectly good
    // authorisation because payer A happened to choose the same one - a false denial of a payment
    // somebody actually made, and the kind of failure a counter must never invent.
    //
    // Reading line by line rather than `read_to_string` keeps the memory flat in the size of the
    // log. This is still a linear *scan* per request, which is latency that grows; it is no longer a
    // linear *allocation* per request, which is what would put the 2 GiB cursor budget at risk as a
    // priced mount ages. Compaction is RFC-0046 S3's job: the back office drains this log to settle
    // it, and until S3 exists an operator's log is bounded by how long they have been selling.
    //
    // **The scan fails closed.** An absent log is the ordinary first-request case and reads as
    // empty. Any other IO error means we cannot show the nonce is unspent, and admitting on that
    // basis is how one signed promise buys unlimited queries.
    if is_spent(
        &path,
        &accepted.from.to_string(),
        &accepted.nonce.to_string(),
    )? {
        return Err(x402::Refusal::AlreadyUsed);
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

    const A: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const N: &str = "0x00000000000000000000000000000000000000000000000000000000000000ff";

    fn log_with(dir: &std::path::Path, rows: &[(&str, &str)]) -> std::path::PathBuf {
        let path = dir.join(LOG);
        let body: String = rows
            .iter()
            .map(|(payer, nonce)| {
                format!("{}\n", serde_json::json!({"payer": payer, "nonce": nonce}))
            })
            .collect();
        std::fs::write(&path, body).unwrap();
        path
    }

    /// EIP-3009 keys authorisation state by authoriser *and* nonce. Matching the nonce alone
    /// refuses payer B for a nonce payer A happened to pick first, which denies a payment somebody
    /// actually made.
    #[test]
    fn a_nonce_is_only_spent_for_the_payer_who_signed_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_with(dir.path(), &[(A, N)]);
        assert!(
            is_spent(&path, A, N).unwrap(),
            "payer A signed this nonce, so A may not reuse it"
        );
        assert!(
            !is_spent(&path, B, N).unwrap(),
            "payer B's authorisation is unspent; the same nonce from a different payer is a \
             different authorisation and must not be refused"
        );
    }

    #[test]
    fn a_payer_may_not_reuse_one_of_its_own_nonces() {
        let dir = tempfile::tempdir().unwrap();
        let other = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let path = log_with(dir.path(), &[(A, other), (B, N), (A, N)]);
        assert!(is_spent(&path, A, N).unwrap(), "recorded later in the log");
        assert!(is_spent(&path, A, other).unwrap(), "recorded first");
        assert!(!is_spent(&path, B, other).unwrap());
    }

    #[test]
    fn an_absent_log_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !is_spent(&dir.path().join(LOG), A, N).unwrap(),
            "the first paid request of a mount's life has no log to read"
        );
    }

    /// A log that cannot be read is not evidence that a nonce is unspent. Admitting on that basis is
    /// how one signed promise buys an unlimited number of queries.
    #[test]
    fn an_unreadable_log_refuses_rather_than_admitting() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the log should be: `File::open` succeeds on some platforms and the
        // read then fails, and fails outright on others. Either way it is not `NotFound`, and both
        // paths must refuse.
        let path = dir.path().join(LOG);
        std::fs::create_dir(&path).unwrap();
        assert!(
            matches!(is_spent(&path, A, N), Err(x402::Refusal::RecordFailed)),
            "an unreadable log must refuse, not admit"
        );
    }

    /// A corrupt line is skipped rather than ending the scan, so a spent nonce recorded *after* one
    /// cannot be replayed by appending garbage.
    #[test]
    fn a_corrupt_line_does_not_hide_a_later_spend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG);
        std::fs::write(
            &path,
            format!(
                "not json at all\n{}\n",
                serde_json::json!({"payer": A, "nonce": N})
            ),
        )
        .unwrap();
        assert!(
            is_spent(&path, A, N).unwrap(),
            "a line the scan cannot parse must not curtail the scan"
        );
    }
}
