//! ABI resolution: Sourcify first (open, no key), Etherscan v2 as a keyed fallback.
//! Correctness-critical decoding lives elsewhere; this is just acquisition.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// Resolve a contract ABI. Returns the ABI as a JSON array on success.
pub async fn resolve(chain_id: u64, address: &str) -> Result<Value> {
    match sourcify(chain_id, address).await {
        Ok(abi) => {
            tracing::info!("ABI resolved via Sourcify");
            Ok(abi)
        }
        Err(e) => {
            tracing::warn!("Sourcify miss ({e:#}); trying Etherscan");
            etherscan(chain_id, address).await
        }
    }
}

async fn sourcify(chain_id: u64, address: &str) -> Result<Value> {
    // Sourcify server API v2. The legacy /server/files endpoint is retired.
    let url = format!("https://sourcify.dev/server/v2/contract/{chain_id}/{address}?fields=abi");
    let resp = reqwest::get(&url)
        .await
        .context("Sourcify request failed")?;
    if !resp.status().is_success() {
        bail!("Sourcify returned HTTP {}", resp.status());
    }
    let body: Value = resp
        .json()
        .await
        .context("Sourcify response was not JSON")?;
    parse_sourcify(&body)
}

/// The ABI out of a Sourcify v2 response body - split from the request so it is testable against
/// fixtures rather than only against the live service (the network half has no interesting logic; this
/// half decides whether we accept what we were handed).
fn parse_sourcify(body: &Value) -> Result<Value> {
    body.get("abi")
        .filter(|a| a.is_array())
        .cloned()
        .ok_or_else(|| anyhow!("Sourcify had no ABI for this contract"))
}

async fn etherscan(chain_id: u64, address: &str) -> Result<Value> {
    let key = std::env::var("ETHERSCAN_API_KEY").map_err(|_| {
        anyhow!(
            "Sourcify had no verified ABI and ETHERSCAN_API_KEY is not set - \
                 set it, or use a Sourcify-verified contract"
        )
    })?;
    let url = format!(
        "https://api.etherscan.io/v2/api?chainid={chain_id}&module=contract&action=getabi&address={address}&apikey={key}"
    );
    let body: Value = reqwest::get(&url)
        .await
        .context("Etherscan request failed")?
        .json()
        .await
        .context("Etherscan response was not JSON")?;
    let abi = parse_etherscan(&body)?;
    tracing::info!("ABI resolved via Etherscan");
    Ok(abi)
}

/// The ABI out of an Etherscan v2 response body - split from the request so it is testable against
/// fixtures. Etherscan signals failure *in a 200 body* (`status: "0"`, with the reason in `result`),
/// so this check is the only thing standing between a rate-limit notice and a "parsed" ABI - and the
/// ABI itself arrives as a JSON string that must be parsed a second time.
fn parse_etherscan(body: &Value) -> Result<Value> {
    if body.get("status").and_then(Value::as_str) != Some("1") {
        let msg = body
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        bail!("Etherscan could not return an ABI: {msg}");
    }
    let result = body
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Etherscan result missing"))?;
    serde_json::from_str(result).context("Etherscan ABI was not valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ABI: &str = r#"[{"type":"event","name":"Transfer","inputs":[]}]"#;

    #[test]
    fn sourcify_success_error_and_malformed() {
        // Success: the `abi` field is an array and comes back verbatim.
        let body =
            json!({ "abi": [{"type": "event", "name": "Transfer"}], "match": "exact_match" });
        let abi = parse_sourcify(&body).unwrap();
        assert!(abi.is_array());
        assert_eq!(abi[0]["name"], "Transfer");

        // Unverified contract: v2 answers 200 with no `abi` field at all.
        let err = parse_sourcify(&json!({ "match": null })).unwrap_err();
        assert!(err.to_string().contains("no ABI"), "{err}");

        // Malformed: `abi` present but not an array. Accepting this would hand a non-ABI to the
        // decoder, so the `is_array` filter is load-bearing rather than decorative.
        for bad in [
            json!({"abi": "not-an-array"}),
            json!({"abi": {}}),
            json!({"abi": null}),
        ] {
            assert!(
                parse_sourcify(&bad).is_err(),
                "a non-array `abi` must be refused: {bad}"
            );
        }
    }

    #[test]
    fn etherscan_success_error_and_malformed() {
        // Success: `status: "1"`, ABI as a JSON *string* needing a second parse.
        let abi = parse_etherscan(&json!({"status": "1", "result": ABI})).unwrap();
        assert!(abi.is_array());
        assert_eq!(abi[0]["name"], "Transfer");

        // The failure mode that matters: Etherscan reports errors in a 200 body. Without the status
        // check, a rate-limit notice would be parsed as if it were an ABI - and the message must be
        // surfaced, because "rate limited" and "not verified" need different operator responses.
        let err = parse_etherscan(&json!({
            "status": "0",
            "result": "Max rate limit reached"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("Max rate limit reached"), "{err}");

        let err =
            parse_etherscan(&json!({"status": "0", "result": "Contract source code not verified"}))
                .unwrap_err();
        assert!(err.to_string().contains("not verified"), "{err}");

        // A failure body with no usable reason still fails, with a placeholder rather than a panic.
        assert!(parse_etherscan(&json!({"status": "0"})).is_err());

        // Malformed successes: result missing, or not parseable as JSON.
        assert!(parse_etherscan(&json!({"status": "1"})).is_err());
        let err = parse_etherscan(&json!({"status": "1", "result": "{not json"})).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }
}
