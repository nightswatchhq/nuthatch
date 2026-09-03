//! ABI resolution: Sourcify first, then keyless Blockscout where it is available, then Etherscan v2
//! as a last-resort keyed fallback.
//! Correctness-critical decoding lives elsewhere; this is just acquisition.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// An ABI plus which resolver produced it, so a caller with a presentation layer (`init`/`add`'s
/// pretty printer) can say which one won without a `tracing` line crashing through its output -
/// see #675. `fallback_reason` is set only when Sourcify was tried and missed before Etherscan won.
pub struct Resolved {
    pub abi: Value,
    pub via: &'static str,
    pub fallback_reason: Option<String>,
    /// Sourcify v2 `name` when present. Not a decode input; `init`/`add` use it as a default alias.
    pub contract_name: Option<String>,
}

/// Resolve a contract ABI without making an API token the normal path. Sourcify is the primary
/// source; Blockscout is the second, keyless Etherscan-compatible source on the chains where it
/// operates; Etherscan is retained for chains Blockscout does not cover and as a final fallback.
pub async fn resolve(chain_id: u64, address: &str) -> Result<Resolved> {
    match sourcify(chain_id, address).await {
        Ok((abi, name)) => Ok(Resolved {
            abi,
            via: "Sourcify",
            fallback_reason: None,
            contract_name: name,
        }),
        Err(sourcify_err) => match blockscout(chain_id, address).await {
            Ok(abi) => Ok(Resolved {
                abi,
                via: "Blockscout",
                fallback_reason: Some(format!("Sourcify miss: {sourcify_err:#}")),
                contract_name: None,
            }),
            Err(blockscout_err) => {
                let abi = etherscan(chain_id, address).await?;
                Ok(Resolved {
                    abi,
                    via: "Etherscan",
                    fallback_reason: Some(format!(
                        "Sourcify miss: {sourcify_err:#}; Blockscout miss: {blockscout_err:#}"
                    )),
                    contract_name: None,
                })
            }
        },
    }
}

async fn sourcify(chain_id: u64, address: &str) -> Result<(Value, Option<String>)> {
    // Sourcify server API v2. The legacy /server/files endpoint is retired.
    //
    // `compilation`, not `name` (#1138). The contract's identifier used to be a top-level field and
    // is now `compilation.name`; asking for `name` gets HTTP 400 `Field selector name is not a valid
    // field`, on every chain, which silently demoted this whole path to "Blockscout where wired,
    // else an Etherscan key" for however long it stood.
    let url = format!(
        "https://sourcify.dev/server/v2/contract/{chain_id}/{address}?fields=abi,compilation"
    );
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
    Ok((parse_sourcify(&body)?, sourcify_contract_name(&body)))
}

/// Sourcify v2's contract identifier - the verified contract's name, not an ABI field. Used as the
/// default alias when `init`/`add` are not given `--alias` (#774).
///
/// Read from `compilation.name`, where the v2 schema keeps it today, and from the top-level `name`
/// it used to be at (#1138) - the second because the field has moved once already and the alias is a
/// hint, so the cheap thing is to accept either rather than break on the next move.
fn sourcify_contract_name(body: &Value) -> Option<String> {
    body.pointer("/compilation/name")
        .or_else(|| body.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
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

/// Keyless Blockscout API roots we have independently verified. Do not guess a host for a chain:
/// an invented fallback is merely an outage with a more misleading error message. More instances
/// can be added once an ABI response has been measured against the actual chain.
fn blockscout_api(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("https://eth.blockscout.com/api"),
        8453 => Some("https://base.blockscout.com/api"),
        100 => Some("https://gnosis.blockscout.com/api"),
        _ => None,
    }
}

async fn blockscout(chain_id: u64, address: &str) -> Result<Value> {
    let base = blockscout_api(chain_id).ok_or_else(|| {
        anyhow!("no keyless Blockscout ABI endpoint is configured for chain {chain_id}")
    })?;
    let url = format!("{base}?module=contract&action=getabi&address={address}");
    let body: Value = reqwest::get(&url)
        .await
        .context("Blockscout request failed")?
        .json()
        .await
        .context("Blockscout response was not JSON")?;
    parse_etherscan(&body).context("Blockscout could not return an ABI")
}

/// Why `init` cannot continue without `ETHERSCAN_API_KEY`. The key is the last resort, never the
/// only option: `--abi` is always available, and Blockscout is named only on chains where we have
/// measured an instance. Inventing one for BSC (#762) would be the same class of lie as a $93
/// formula that omits the rest of the bill.
fn missing_etherscan_key(chain_id: u64) -> String {
    match blockscout_api(chain_id) {
        Some(_) => "Sourcify and Blockscout had no verified ABI, and ETHERSCAN_API_KEY is not set. \
                    Set it, pass --abi path/to.json, or use a Sourcify-verified contract."
            .into(),
        None => format!(
            "Sourcify had no verified ABI, chain {chain_id} has no keyless Blockscout ABI endpoint, \
             and ETHERSCAN_API_KEY is not set. Set it, pass --abi path/to.json, or use a \
             Sourcify-verified contract."
        ),
    }
}

async fn etherscan(chain_id: u64, address: &str) -> Result<Value> {
    let key = std::env::var("ETHERSCAN_API_KEY")
        .map_err(|_| anyhow!("{}", missing_etherscan_key(chain_id)))?;
    let url = format!(
        "https://api.etherscan.io/v2/api?chainid={chain_id}&module=contract&action=getabi&address={address}&apikey={key}"
    );
    let body: Value = reqwest::get(&url)
        .await
        .context("Etherscan request failed")?
        .json()
        .await
        .context("Etherscan response was not JSON")?;
    parse_etherscan(&body)
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
    fn sourcify_name_is_the_alias_hint_and_empty_is_absent() {
        // Where v2 keeps it today (#1138): measured 2026-09-03, mainnet USDC answers
        // `{"compilation": {"name": "FiatTokenProxy", "fullyQualifiedName": ...}, "abi": [...]}`.
        assert_eq!(
            sourcify_contract_name(
                &json!({"compilation": {"name": "FiatTokenProxy", "language": "Solidity"}, "abi": []})
            )
            .as_deref(),
            Some("FiatTokenProxy")
        );
        // Where it used to be, still accepted.
        assert_eq!(
            sourcify_contract_name(&json!({"name": "DelegationManager", "abi": []})).as_deref(),
            Some("DelegationManager")
        );
        assert_eq!(
            sourcify_contract_name(&json!({"compilation": {"name": ""}, "abi": []})),
            None
        );
        assert_eq!(
            sourcify_contract_name(&json!({"name": "", "abi": []})),
            None
        );
        assert_eq!(sourcify_contract_name(&json!({"abi": []})), None);
    }

    /// The request must not ask for a selector Sourcify refuses (#1138). Pinned as a string test
    /// because the network half has no other test, and `fields=abi,name` was accepted when #774
    /// wrote it and is HTTP 400 now.
    #[test]
    fn sourcify_request_asks_for_compilation_not_name() {
        // The request line only, not the whole file: this test's own strings would otherwise match.
        let request = include_str!("abi.rs")
            .lines()
            .find(|l| l.contains("sourcify.dev/server/v2/contract/"))
            .expect("the v2 request URL line");
        assert!(
            request.contains("?fields=abi,compilation\""),
            "the Sourcify v2 request must select `compilation`, where the contract name lives: {request}"
        );
        assert!(
            !request.contains(",name"),
            "`name` is not a valid v2 field selector any more and gets HTTP 400: {request}"
        );
    }

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

        // The failure mode that matters: Etherscan-compatible APIs report errors in a 200 body. Without the status
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

    #[test]
    fn keyless_blockscout_is_used_only_where_its_instance_is_verified() {
        assert_eq!(blockscout_api(1), Some("https://eth.blockscout.com/api"));
        assert_eq!(
            blockscout_api(8453),
            Some("https://base.blockscout.com/api")
        );
        assert_eq!(
            blockscout_api(100),
            Some("https://gnosis.blockscout.com/api")
        );
        assert_eq!(blockscout_api(56), None, "do not invent a BSC fallback");
    }

    #[test]
    fn missing_etherscan_key_does_not_claim_a_blockscout_that_does_not_exist() {
        let bsc = missing_etherscan_key(56);
        assert!(
            bsc.contains("chain 56 has no keyless Blockscout"),
            "BSC must not be told to try a host we do not ship: {bsc}"
        );
        assert!(bsc.contains("--abi"), "{bsc}");
        let mainnet = missing_etherscan_key(1);
        assert!(
            mainnet.contains("Sourcify and Blockscout"),
            "mainnet already tried Blockscout: {mainnet}"
        );
        assert!(!mainnet.contains("has no keyless Blockscout"), "{mainnet}");
    }
}
