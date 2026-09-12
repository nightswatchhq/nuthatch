//! The optional x402 counter (RFC-0046 S2).
//!
//! It verifies and records locally. Settlement is deliberately outside the nest, in the `settle`
//! back-office command, so neither an RPC nor a facilitator sits between a question and its answer.

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
pub(crate) const LOG: &str = "authorisations.jsonl";
/// Compact spent keys the back office leaves after draining the pending log (RFC-0046 S3).
pub(crate) const SPENT: &str = "spent.jsonl";
/// Per-payer settled/failed record. A failed row is why the nest stops serving that payer.
pub(crate) const PAYERS: &str = "payers.jsonl";

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

/// Whether this `(network, payer, nonce)` tuple already appears in the durable log.
///
/// Split out of [`verify_and_record`] so the three properties that matter can be tested without
/// forging a signature: the key is the pair and not the nonce alone, an absent log is empty rather
/// than an error, and an unreadable one refuses rather than admits.
pub(crate) fn nonce_is_spent(
    dir: &std::path::Path,
    network: &str,
    payer: &str,
    nonce: &str,
) -> Result<bool, x402::Refusal> {
    if is_spent(&dir.join(LOG), network, payer, nonce)? {
        return Ok(true);
    }
    is_spent(&dir.join(SPENT), network, payer, nonce)
}

/// A payer whose promise did not clear is not sold to again (RFC-0046 §5.3).
pub(crate) fn payer_has_failed(
    dir: &std::path::Path,
    network: &str,
    payer: &str,
) -> Result<bool, x402::Refusal> {
    let file = match std::fs::File::open(dir.join(PAYERS)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
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
        if v.get("network").and_then(|n| n.as_str()) == Some(network)
            && v.get("payer").and_then(|n| n.as_str()) == Some(payer)
            && v.get("outcome").and_then(|n| n.as_str()) == Some("failed")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_spent(
    path: &std::path::Path,
    network: &str,
    payer: &str,
    nonce: &str,
) -> Result<bool, x402::Refusal> {
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
        // Logs written before payment-domain scoping have no `network`. Treat those conservatively
        // as spent in every domain: an upgrade must not reopen an already-recorded promise.
        if v.get("network")
            .and_then(|n| n.as_str())
            .is_none_or(|recorded| recorded == network)
            && v.get("payer").and_then(|n| n.as_str()) == Some(payer)
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
    // **Keyed by (payment domain, payer, nonce), and streamed rather than slurped.**
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
    let network = match cfg.network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    };
    if payer_has_failed(dir, network, &accepted.from.to_string())? {
        return Err(x402::Refusal::UnreliablePayer);
    }
    if nonce_is_spent(
        dir,
        network,
        &accepted.from.to_string(),
        &accepted.nonce.to_string(),
    )? {
        return Err(x402::Refusal::AlreadyUsed);
    }
    let row = serde_json::json!({
        "received_at": now,
        "network": network,
        "payer": accepted.from.to_string(),
        "nonce": accepted.nonce.to_string(),
        "value": accepted.value.to_string(),
        "authorisation": header,
    });
    use std::io::Write;
    let existed = path.try_exists().map_err(|_| x402::Refusal::RecordFailed)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|_| x402::Refusal::RecordFailed)?;
    writeln!(file, "{row}").map_err(|_| x402::Refusal::RecordFailed)?;
    file.sync_data().map_err(|_| x402::Refusal::RecordFailed)?;
    // `sync_data` makes the row durable, but a first write has also created a directory entry.
    // Without syncing that parent directory, a crash can lose the name after the query has been
    // served, turning the same signed promise into an apparently unspent nonce after restart.
    if !existed {
        std::fs::File::open(dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| x402::Refusal::RecordFailed)?;
    }
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
/// The error is boxed because an `axum` `Response` is 128 bytes and clippy's `result_large_err`
/// is right that a `Result` this wide should not be moved through the happy path of a request
/// handler. The caller unboxes it once, on the refusal branch only.
pub fn admit(
    dir: &std::path::Path,
    cfg: &Config,
    headers: &HeaderMap,
    resource: &str,
    description: &str,
) -> Result<(), Box<axum::response::Response>> {
    preflight(cfg, headers, resource, description)?;
    let header = headers
        .get("Payment-Signature")
        .and_then(|v| v.to_str().ok())
        .expect("preflight checked the payment header");
    verify_and_record(dir, cfg, header, now())
        .map_err(|_| Box::new(challenge(cfg, resource, description)))?;
    Ok(())
}

/// Reject missing or invalid signatures before spending query resources. This does not consume
/// the nonce: `admit` rechecks and durably records it only once there is an answer to return.
pub fn preflight(
    cfg: &Config,
    headers: &HeaderMap,
    resource: &str,
    description: &str,
) -> Result<(), Box<axum::response::Response>> {
    let seller = cfg
        .seller()
        .map_err(|_| Box::new(challenge(cfg, resource, description)))?;
    let Some(header) = headers
        .get("Payment-Signature")
        .and_then(|v| v.to_str().ok())
    else {
        return Err(Box::new(challenge(cfg, resource, description)));
    };
    x402::verify_payment(&seller, header, now())
        .map_err(|_| Box::new(challenge(cfg, resource, description)))?;
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
    const MAINNET: &str = "mainnet";
    const TESTNET: &str = "testnet";

    fn log_with(dir: &std::path::Path, rows: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let path = dir.join(LOG);
        let body: String = rows
            .iter()
            .map(|(network, payer, nonce)| {
                format!(
                    "{}\n",
                    serde_json::json!({"network": network, "payer": payer, "nonce": nonce})
                )
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
        let path = log_with(dir.path(), &[(TESTNET, A, N)]);
        assert!(
            is_spent(&path, TESTNET, A, N).unwrap(),
            "payer A signed this nonce, so A may not reuse it"
        );
        assert!(
            !is_spent(&path, TESTNET, B, N).unwrap(),
            "payer B's authorisation is unspent; the same nonce from a different payer is a \
             different authorisation and must not be refused"
        );
    }

    #[test]
    fn a_payer_may_not_reuse_one_of_its_own_nonces() {
        let dir = tempfile::tempdir().unwrap();
        let other = "0x0000000000000000000000000000000000000000000000000000000000000001";
        let path = log_with(
            dir.path(),
            &[(TESTNET, A, other), (TESTNET, B, N), (TESTNET, A, N)],
        );
        assert!(
            is_spent(&path, TESTNET, A, N).unwrap(),
            "recorded later in the log"
        );
        assert!(
            is_spent(&path, TESTNET, A, other).unwrap(),
            "recorded first"
        );
        assert!(!is_spent(&path, TESTNET, B, other).unwrap());
    }

    #[test]
    fn a_nonce_is_only_spent_in_its_payment_domain() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_with(dir.path(), &[(TESTNET, A, N)]);
        assert!(is_spent(&path, TESTNET, A, N).unwrap());
        assert!(
            !is_spent(&path, MAINNET, A, N).unwrap(),
            "a mainnet authorisation is independent of the same testnet nonce"
        );
    }

    #[test]
    fn a_drained_nonce_stays_spent_from_the_spent_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SPENT),
            format!(
                "{}\n",
                serde_json::json!({"network": TESTNET, "payer": A, "nonce": N})
            ),
        )
        .unwrap();
        assert!(
            nonce_is_spent(dir.path(), TESTNET, A, N).unwrap(),
            "the back office drained the pending log; the nest must still refuse the nonce"
        );
        assert!(!nonce_is_spent(dir.path(), TESTNET, B, N).unwrap());
    }

    #[test]
    fn a_failed_settlement_refuses_that_payer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PAYERS),
            format!(
                "{}\n",
                serde_json::json!({"network": TESTNET, "payer": A, "nonce": N, "outcome": "failed"})
            ),
        )
        .unwrap();
        assert!(payer_has_failed(dir.path(), TESTNET, A).unwrap());
        assert!(!payer_has_failed(dir.path(), TESTNET, B).unwrap());
        assert!(!payer_has_failed(dir.path(), MAINNET, A).unwrap());
    }

    #[test]
    fn an_absent_log_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !is_spent(&dir.path().join(LOG), TESTNET, A, N).unwrap(),
            "the first paid request of a mount's life has no log to read"
        );
    }

    /// A log that cannot be read is not evidence that a nonce is unspent. Admitting on that basis is
    /// how one signed promise buys an unlimited number of queries.
    ///
    /// **There are two failure arms and they need a test each.** Opening can fail, or opening can
    /// succeed and reading fail. The first version of this test used a directory in the log's place
    /// and covered only the second: on macOS `File::open` succeeds on a directory and the error
    /// arrives from `lines()`. Reverting the open arm to fail open left that test green, which is
    /// how the gap was found - the mutation survived, so the test was not guarding what it claimed.
    #[test]
    fn a_log_that_cannot_be_opened_refuses_rather_than_admitting() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file standing where the log's *parent directory* should be. `File::open` then
        // fails with `NotADirectory` rather than `NotFound`, which is the arm under test.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join(LOG);
        let err = std::fs::File::open(&path).unwrap_err();
        assert_ne!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "this fixture must not produce NotFound, or it tests the empty-log arm instead"
        );
        assert!(
            matches!(
                is_spent(&path, TESTNET, A, N),
                Err(x402::Refusal::RecordFailed)
            ),
            "a log that cannot be opened must refuse, not admit"
        );
    }

    #[test]
    fn a_log_that_cannot_be_read_refuses_rather_than_admitting() {
        let dir = tempfile::tempdir().unwrap();
        // A directory in the log's place: opening may succeed, and the read then fails.
        let path = dir.path().join(LOG);
        std::fs::create_dir(&path).unwrap();
        assert!(
            matches!(
                is_spent(&path, TESTNET, A, N),
                Err(x402::Refusal::RecordFailed)
            ),
            "a log that cannot be read must refuse, not admit"
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
                serde_json::json!({"network": TESTNET, "payer": A, "nonce": N})
            ),
        )
        .unwrap();
        assert!(
            is_spent(&path, TESTNET, A, N).unwrap(),
            "a line the scan cannot parse must not curtail the scan"
        );
    }
}
