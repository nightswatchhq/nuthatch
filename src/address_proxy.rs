//! The Etherscan `proxy` and `logs` modules on a rotki-mode nest (RFC-0063 §5): the JSON-RPC
//! calls rotki makes of an indexer when its own nodes fail, answered from the nest's RPC in
//! Etherscan's shapes, so a nest can stand in for Etherscan entirely.
//!
//! Nothing here is stored. The nest's own rows are Etherscan account records, from which an RPC
//! transaction or receipt object cannot be rebuilt whole, so every call is forwarded.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// One JSON-RPC call to the nest's RPC: method, params, result.
pub type Forward =
    Arc<dyn Fn(String, Value) -> futures::future::BoxFuture<'static, Result<Value>> + Send + Sync>;

/// Etherscan's `getLogs` answers at most this many logs; a caller pages by block past them.
const MAX_LOGS: usize = 1000;

/// The answer to a `proxy` or `logs` request, or `None` for any other module.
pub async fn answer(
    q: &HashMap<String, String>,
    chain_id: u64,
    forward: &Forward,
) -> Option<Value> {
    let module = q.get("module").map(String::as_str)?;
    if module != "proxy" && module != "logs" {
        return None;
    }
    if let Some(c) = q.get("chainid") {
        if c.parse::<u64>().ok() != Some(chain_id) {
            return Some(crate::address_history::unsupported(&format!(
                "chainid {c} is not this nest's chain {chain_id}"
            )));
        }
    }
    let action = q.get("action").map(String::as_str).unwrap_or("");
    let result = if module == "proxy" {
        proxy(q, action, forward).await
    } else if action == "getLogs" {
        logs(q, forward).await
    } else {
        return Some(crate::address_history::unsupported(&format!(
            "logs/{action} is not served by this nest"
        )));
    };
    Some(match result {
        Ok(Answer::Rpc(result)) => json!({ "jsonrpc": "2.0", "id": 1, "result": result }),
        Ok(Answer::Logs(rows)) => json!({ "status": "1", "message": "OK", "result": rows }),
        Ok(Answer::Unsupported(why)) => crate::address_history::unsupported(&why),
        Err(e) => json!({ "status": "0", "message": "NOTOK", "result": format!("Error! {e:#}") }),
    })
}

enum Answer {
    Rpc(Value),
    Logs(Vec<Value>),
    Unsupported(String),
}

async fn proxy(q: &HashMap<String, String>, action: &str, forward: &Forward) -> Result<Answer> {
    let get = |k: &str| {
        q.get(k)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("Missing {k}"))
    };
    let tag = || q.get("tag").map_or("latest", String::as_str);
    let params = match action {
        "eth_blockNumber" => json!([]),
        "eth_getBlockByNumber" => json!([get("tag")?, get("boolean")? == "true"]),
        "eth_getTransactionByHash" | "eth_getTransactionReceipt" => json!([get("txhash")?]),
        "eth_getCode" => json!([get("address")?, tag()]),
        "eth_call" => json!([{ "to": get("to")?, "data": get("data")? }, tag()]),
        other => {
            return Ok(Answer::Unsupported(format!(
                "proxy/{other} is not served by this nest"
            )))
        }
    };
    Ok(Answer::Rpc(forward(action.to_string(), params).await?))
}

/// `logs/getLogs` in Etherscan's shape: every quantity hex, with the block's timestamp and the
/// transaction's gas price and gas used, which `eth_getLogs` does not carry.
async fn logs(q: &HashMap<String, String>, forward: &Forward) -> Result<Answer> {
    let block = |k: &str| -> Result<Value> {
        let v = q.get(k).map_or("latest", String::as_str);
        Ok(match v {
            "latest" | "earliest" | "pending" | "finalized" | "safe" => json!(v),
            n => json!(format!(
                "0x{:x}",
                n.parse::<u64>().map_err(|_| anyhow!("invalid {k} `{n}`"))?
            )),
        })
    };
    // Etherscan combines topics with and/or operators; eth_getLogs can only AND them.
    if q.iter()
        .any(|(k, v)| k.starts_with("topic") && k.ends_with("_opr") && v != "and")
    {
        return Ok(Answer::Unsupported(
            "getLogs with an `or` topic operator is not served by this nest".into(),
        ));
    }
    let topics: Vec<Value> = (0..4)
        .map(|i| {
            q.get(&format!("topic{i}"))
                .map_or(Value::Null, |t| json!(t))
        })
        .collect();
    let last = topics
        .iter()
        .rposition(|t| !t.is_null())
        .map_or(0, |i| i + 1);
    let mut filter = Map::new();
    filter.insert("fromBlock".into(), block("fromBlock")?);
    filter.insert("toBlock".into(), block("toBlock")?);
    if let Some(a) = q.get("address") {
        filter.insert("address".into(), json!(a));
    }
    filter.insert("topics".into(), Value::Array(topics[..last].to_vec()));
    let found = forward("eth_getLogs".into(), json!([filter])).await?;
    let mut logs = found
        .as_array()
        .ok_or_else(|| anyhow!("eth_getLogs answered a non-list"))?
        .clone();
    logs.truncate(MAX_LOGS);

    let mut paid: BTreeMap<String, (Value, Value)> = BTreeMap::new();
    let mut stamped: BTreeMap<String, Value> = BTreeMap::new();
    let mut rows = Vec::with_capacity(logs.len());
    for log in logs {
        let field = |k: &str| -> Result<Value> {
            log.get(k)
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or_else(|| anyhow!("a log without {k}"))
        };
        let tx = field("transactionHash")?
            .as_str()
            .ok_or_else(|| anyhow!("a log's transactionHash is not a string"))?
            .to_string();
        if !paid.contains_key(&tx) {
            let receipt = forward("eth_getTransactionReceipt".into(), json!([tx])).await?;
            let get = |k: &str| {
                receipt
                    .get(k)
                    .filter(|v| !v.is_null())
                    .cloned()
                    .ok_or_else(|| anyhow!("the receipt of {tx} has no {k}"))
            };
            paid.insert(tx.clone(), (get("effectiveGasPrice")?, get("gasUsed")?));
        }
        let number = field("blockNumber")?;
        let key = number.as_str().unwrap_or_default().to_string();
        let ts = match log.get("blockTimestamp").filter(|v| !v.is_null()) {
            Some(ts) => ts.clone(),
            None => match stamped.get(&key) {
                Some(ts) => ts.clone(),
                None => {
                    let header =
                        forward("eth_getBlockByNumber".into(), json!([number, false])).await?;
                    let ts = header
                        .get("timestamp")
                        .cloned()
                        .ok_or_else(|| anyhow!("block {key} has no timestamp"))?;
                    stamped.insert(key, ts.clone());
                    ts
                }
            },
        };
        if log.get("removed").and_then(Value::as_bool) == Some(true) {
            bail!("the RPC answered a removed log in {tx}");
        }
        let (gas_price, gas_used) = paid[&tx].clone();
        rows.push(json!({
            "address": field("address")?,
            "topics": field("topics")?,
            "data": field("data")?,
            "blockNumber": number,
            "blockHash": field("blockHash")?,
            "timeStamp": ts,
            "gasPrice": gas_price,
            "gasUsed": gas_used,
            "logIndex": field("logIndex")?,
            "transactionHash": tx,
            "transactionIndex": field("transactionIndex")?,
        }));
    }
    Ok(Answer::Logs(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// A forwarder answering from `f`, recording every call.
    fn fake(
        f: impl Fn(&str, &Value) -> Value + Send + Sync + 'static,
    ) -> (Forward, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let f = Arc::new(f);
        let forward: Forward = Arc::new(move |m: String, p: Value| {
            seen.lock().unwrap().push((m.clone(), p.clone()));
            let out = f(&m, &p);
            Box::pin(async move { Ok(out) })
        });
        (forward, calls)
    }

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn proxy_calls_reach_the_rpc_as_json_rpc_and_come_back_in_etherscans_shape() {
        let (forward, calls) = fake(|m, _| json!(format!("answer to {m}")));
        for (pairs, method, params) in [
            (
                vec![("action", "eth_blockNumber")],
                "eth_blockNumber",
                json!([]),
            ),
            (
                vec![
                    ("action", "eth_getBlockByNumber"),
                    ("tag", "0x10"),
                    ("boolean", "false"),
                ],
                "eth_getBlockByNumber",
                json!(["0x10", false]),
            ),
            (
                vec![("action", "eth_getTransactionReceipt"), ("txhash", "0xaa")],
                "eth_getTransactionReceipt",
                json!(["0xaa"]),
            ),
            (
                vec![("action", "eth_getCode"), ("address", "0xbb")],
                "eth_getCode",
                json!(["0xbb", "latest"]),
            ),
            (
                vec![
                    ("action", "eth_call"),
                    ("to", "0xcc"),
                    ("data", "0x01"),
                    ("tag", "0x5"),
                ],
                "eth_call",
                json!([{"to": "0xcc", "data": "0x01"}, "0x5"]),
            ),
        ] {
            let mut p = q(&pairs);
            p.insert("module".into(), "proxy".into());
            let got = answer(&p, 1, &forward).await.unwrap();
            assert_eq!(got["result"], format!("answer to {method}"), "{method}");
            assert_eq!(got["jsonrpc"], "2.0");
            assert_eq!(
                calls.lock().unwrap().last().unwrap(),
                &(method.to_string(), params)
            );
        }
        let refused = answer(
            &q(&[("module", "proxy"), ("action", "eth_sendRawTransaction")]),
            1,
            &forward,
        )
        .await
        .unwrap();
        assert!(refused["result"]
            .as_str()
            .unwrap()
            .starts_with("NUTHATCH_UNSUPPORTED:"));
        let other_chain = answer(
            &q(&[
                ("module", "proxy"),
                ("action", "eth_blockNumber"),
                ("chainid", "10"),
            ]),
            1,
            &forward,
        )
        .await
        .unwrap();
        assert!(other_chain["result"]
            .as_str()
            .unwrap()
            .contains("chainid 10"));
        assert!(answer(
            &q(&[("module", "account"), ("action", "txlist")]),
            1,
            &forward
        )
        .await
        .is_none());
    }

    /// Etherscan's log carries the block's timestamp and its transaction's gas price and gas used;
    /// a receipt is read once per transaction, and a header only when the log lacks the timestamp.
    #[tokio::test]
    async fn get_logs_adds_what_etherscan_adds() {
        let log = |tx: &str, index: &str, ts: Option<&str>| {
            let mut l = json!({"address": "0xc0", "topics": ["0xt0", "0xt1"], "data": "0x",
                "blockNumber": "0x64", "blockHash": "0xbh", "logIndex": index,
                "transactionHash": tx, "transactionIndex": "0x2", "removed": false});
            if let Some(ts) = ts {
                l["blockTimestamp"] = json!(ts);
            }
            l
        };
        let logs = json!([
            log("0xa", "0x0", Some("0x5f")),
            log("0xa", "0x1", Some("0x5f")),
            log("0xb", "0x2", None)
        ]);
        let (forward, calls) = fake(move |m, p| match m {
            "eth_getLogs" => logs.clone(),
            "eth_getTransactionReceipt" => {
                json!({"effectiveGasPrice": format!("{}9", p[0].as_str().unwrap()), "gasUsed": "0x5208"})
            }
            "eth_getBlockByNumber" => json!({"timestamp": "0x60"}),
            other => panic!("unexpected {other}"),
        });
        let got = answer(
            &q(&[
                ("module", "logs"),
                ("action", "getLogs"),
                ("fromBlock", "100"),
                ("toBlock", "100"),
                ("address", "0xc0"),
                ("topic0", "0xt0"),
                ("topic2", "0xt2"),
                ("topic0_2_opr", "and"),
            ]),
            1,
            &forward,
        )
        .await
        .unwrap();
        assert_eq!(got["status"], "1");
        let rows = got["result"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            json!({"address": "0xc0", "topics": ["0xt0", "0xt1"], "data": "0x", "blockNumber": "0x64",
                "blockHash": "0xbh", "timeStamp": "0x5f", "gasPrice": "0xa9", "gasUsed": "0x5208",
                "logIndex": "0x0", "transactionHash": "0xa", "transactionIndex": "0x2"})
        );
        assert_eq!(rows[2]["timeStamp"], "0x60");
        assert_eq!(rows[2]["gasPrice"], "0xb9");
        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls[0],
            (
                "eth_getLogs".into(),
                json!([{"fromBlock": "0x64", "toBlock": "0x64", "address": "0xc0",
                "topics": ["0xt0", null, "0xt2"]}])
            )
        );
        let receipts = calls
            .iter()
            .filter(|(m, _)| m == "eth_getTransactionReceipt")
            .count();
        assert_eq!(receipts, 2, "one receipt per transaction");

        let or = answer(
            &q(&[
                ("module", "logs"),
                ("action", "getLogs"),
                ("topic0", "0xt0"),
                ("topic1", "0xt1"),
                ("topic0_1_opr", "or"),
            ]),
            1,
            &forward,
        )
        .await
        .unwrap();
        assert!(or["result"]
            .as_str()
            .unwrap()
            .starts_with("NUTHATCH_UNSUPPORTED:"));
    }
}
