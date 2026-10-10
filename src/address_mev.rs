//! MEV-Boost payments to a watched address (RFC-0063 §8): the blocks it produced, locally or through
//! a relay, with what the relays say each delivered and the on-chain payment that delivered it.
//!
//! A relay's record is a claim, so it is set beside the payment transaction rather than trusted
//! alone. A block no relay knows of has no MEV figure, never a zero one; two relays that disagree
//! about a block are a conflict, served as such.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::address_discovery::{Counted, Discoverer, Rpc};
use crate::address_history::{Action, AddressHistory, Row};

/// The MEV-Boost relays whose public data API answered on 2026-10-10. bloXroute's max-profit relay,
/// Eden, Manifold and others that no longer answer are left out.
pub const RELAYS: [(&str, &str); 6] = [
    ("flashbots", "https://boost-relay.flashbots.net"),
    ("ultrasound", "https://relay.ultrasound.money"),
    (
        "bloxroute-regulated",
        "https://bloxroute.regulated.blxrbdn.com",
    ),
    ("agnostic", "https://agnostic-relay.net"),
    ("aestus", "https://mainnet.aestus.live"),
    ("titan", "https://titanrelay.xyz"),
];

/// Blocks a pass covers at once for one address.
const MEV_WINDOW: u64 = 50_000;

/// One relay's deliveries for one block: relay name and block in, the bid traces out.
pub type Relays = Arc<
    dyn Fn(&'static str, u64) -> futures::future::BoxFuture<'static, Result<Vec<Value>>>
        + Send
        + Sync,
>;

/// The public data API of [`RELAYS`], over HTTP.
pub fn relays_over_http() -> Result<Relays> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("nuthatch/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    Ok(Arc::new(move |name: &'static str, block: u64| {
        let client = client.clone();
        Box::pin(async move {
            let base = RELAYS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, url)| *url)
                .ok_or_else(|| anyhow!("no relay {name}"))?;
            let resp = client
                .get(format!(
                    "{base}/relay/v1/data/bidtraces/proposer_payload_delivered?block_number={block}"
                ))
                .send()
                .await
                .map_err(|e| e.without_url())
                .with_context(|| format!("relay {name}, block {block}"))?;
            if !resp.status().is_success() {
                anyhow::bail!("relay {name} answered {} for block {block}", resp.status());
            }
            let body: Value = resp
                .json()
                .await
                .map_err(|e| e.without_url())
                .with_context(|| format!("relay {name}, block {block}"))?;
            body.as_array()
                .cloned()
                .ok_or_else(|| anyhow!("relay {name} answered a non-list for block {block}"))
        })
    }))
}

/// Every relay's deliveries for `block`, from the cache or asked now. A relay that cannot be asked
/// fails the lookup: a missing answer is not an absent delivery.
async fn relay_record(
    history: &AddressHistory,
    relays: &Relays,
    block: u64,
) -> Result<Map<String, Value>> {
    if let Some(r) = history.relay_record(block)? {
        return Ok(r);
    }
    let mut delivered = Vec::new();
    for (name, _) in RELAYS {
        for trace in relays(name, block).await? {
            if trace.get("block_number").and_then(Value::as_str) != Some(block.to_string().as_str())
            {
                continue;
            }
            let field = |k: &str| {
                trace
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            delivered.push(json!({
                "relay": name,
                "recipient": field("proposer_fee_recipient").to_ascii_lowercase(),
                "value": field("value"),
                "builder": field("builder_pubkey"),
                "proposer": field("proposer_pubkey"),
            }));
        }
    }
    let mut record = Map::new();
    record.insert("delivered".into(), Value::Array(delivered));
    history.cache_relay_record(block, &record)?;
    Ok(record)
}

/// The relays' agreed delivery for a block, if any, and whether they disagree.
struct Delivery {
    relays: Vec<String>,
    recipient: String,
    value: String,
    builder: String,
    proposer: String,
    conflict: bool,
}

fn delivery(record: &Map<String, Value>) -> Option<Delivery> {
    let delivered = record.get("delivered")?.as_array()?;
    let first = delivered.first()?;
    let get = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let claims: BTreeSet<(String, String)> = delivered
        .iter()
        .map(|d| (get(d, "recipient"), get(d, "value")))
        .collect();
    Some(Delivery {
        relays: delivered.iter().map(|d| get(d, "relay")).collect(),
        recipient: get(first, "recipient"),
        value: get(first, "value"),
        builder: get(first, "builder"),
        proposer: get(first, "proposer"),
        conflict: claims.len() > 1,
    })
}

/// The produced-block rows of `address` over `[from, to]`, from its stored produced blocks and
/// normal transactions, both of which must be covered there.
pub async fn produced_blocks<M: Rpc, T: Rpc>(
    d: &Discoverer<Counted<M>, Counted<T>>,
    relays: &Relays,
    history: &AddressHistory,
    address: &str,
    (from, to): (u64, u64),
) -> Result<Vec<Row>> {
    let a = address.to_ascii_lowercase();
    let mined = history.rows_in(Action::MinedBlocks, &a, (from, to))?;
    let txs = history.rows_in(Action::TxList, &a, (from, to))?;
    let text = |r: &Map<String, Value>, k: &str| {
        r.get(k).and_then(Value::as_str).unwrap_or("").to_string()
    };
    let number = |r: &Map<String, Value>, k: &str| -> Result<u64> {
        text(r, k)
            .parse()
            .with_context(|| format!("a stored row's {k}"))
    };

    let mut rows: BTreeMap<u64, Map<String, Value>> = BTreeMap::new();
    for m in &mined {
        let b = number(m, "blockNumber")?;
        let mut r = Map::new();
        r.insert("blockNumber".into(), json!(b.to_string()));
        r.insert("timeStamp".into(), json!(text(m, "timeStamp")));
        r.insert("feeRecipient".into(), json!(a));
        r.insert("blockReward".into(), json!(text(m, "blockReward")));
        rows.insert(b, r);
    }

    // A relay-built block pays the proposer in a transaction of its own, from the builder's fee
    // recipient or, for builders that pay from another account, as the block's last
    // transaction: those are the candidates a relay is asked about.
    let mut incoming: BTreeMap<u64, Vec<&Map<String, Value>>> = BTreeMap::new();
    for t in &txs {
        if text(t, "to") == a && text(t, "value") != "0" && text(t, "isError") != "1" {
            incoming
                .entry(number(t, "blockNumber")?)
                .or_default()
                .push(t);
        }
    }
    for (b, paid) in incoming {
        if rows.contains_key(&b) {
            continue;
        }
        let header = d
            .main
            .call("eth_getBlockByNumber", json!([format!("0x{b:x}"), false]))
            .await
            .with_context(|| format!("block {b}"))?;
        let miner = header
            .get("miner")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("block {b} has no miner"))?
            .to_ascii_lowercase();
        let last = header
            .get("transactions")
            .and_then(Value::as_array)
            .map(|t| t.len().saturating_sub(1).to_string());
        let Some(payment) = paid
            .iter()
            .rev()
            .find(|t| text(t, "from") == miner || Some(text(t, "transactionIndex")) == last)
        else {
            continue;
        };
        let record = relay_record(history, relays, b).await?;
        // Only a relay's word makes a payment a block this address produced.
        if delivery(&record).is_none_or(|dl| dl.recipient != a) {
            continue;
        }
        let mut r = Map::new();
        r.insert("blockNumber".into(), json!(b.to_string()));
        r.insert("timeStamp".into(), json!(text(payment, "timeStamp")));
        r.insert("feeRecipient".into(), json!(miner));
        r.insert(
            "blockReward".into(),
            json!(d.block_reward(b, &header).await?),
        );
        r.insert("paymentTx".into(), json!(text(payment, "hash")));
        r.insert("paymentValue".into(), json!(text(payment, "value")));
        rows.insert(b, r);
    }

    let mut out = Vec::with_capacity(rows.len());
    for (b, mut r) in rows {
        let record = relay_record(history, relays, b).await?;
        let unpaid = !r.contains_key("paymentTx");
        let mut put = |k: &str, v: String| {
            r.insert(k.to_string(), Value::String(v));
        };
        match delivery(&record) {
            Some(dl) => {
                put("mevRecipient", dl.recipient);
                put(
                    "mevReward",
                    if dl.conflict { String::new() } else { dl.value },
                );
                put("relays", dl.relays.join(","));
                put("builderPubkey", dl.builder);
                put("proposerPubkey", dl.proposer);
                put("mev", if dl.conflict { "conflict" } else { "relay" }.into());
            }
            None => {
                put("mevRecipient", String::new());
                put("mevReward", String::new());
                put("relays", String::new());
                put("builderPubkey", String::new());
                put("proposerPubkey", String::new());
                put("mev", "none".into());
            }
        }
        if unpaid {
            put("paymentTx", String::new());
            put("paymentValue", String::new());
        }
        out.push(Row {
            block: b,
            tx_index: 0,
            position: 0,
            record: r,
        });
    }
    Ok(out)
}

/// The cursor's MEV pass: each watched address is covered as far as both its produced blocks and
/// its normal transactions are, a window at a time.
pub async fn mev_pass<M: Rpc, T: Rpc>(
    d: &Discoverer<Counted<M>, Counted<T>>,
    relays: &Relays,
    history: &AddressHistory,
    watched: &[String],
    start: u64,
) -> Result<()> {
    use crate::address_discovery::next_uncovered_for;
    for a in watched {
        loop {
            let from = next_uncovered_for(history, a, start, &[Action::ProducedBlocks])?;
            let through = |action: Action| -> Result<Option<u64>> {
                Ok(history
                    .coverage(action, a)?
                    .into_iter()
                    .find(|(f, t)| *f <= from && from <= *t)
                    .map(|(_, t)| t))
            };
            let (Some(mined), Some(txs)) =
                (through(Action::MinedBlocks)?, through(Action::TxList)?)
            else {
                break;
            };
            let to = mined.min(txs).min(from.saturating_add(MEV_WINDOW - 1));
            let generation = history.generation()?;
            let rows = produced_blocks(d, relays, history, a, (from, to)).await?;
            history.record(Action::ProducedBlocks, a, &rows, (from, to), generation)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address_partitions::tests::Fake;
    use std::sync::Mutex;

    const A: &str = "0x00000000000000000000000000000000000000aa";
    const BUILDER: &str = "0x00000000000000000000000000000000000000b1";

    fn row(block: u64, fields: Value) -> Row {
        let mut record = fields.as_object().unwrap().clone();
        record.insert("blockNumber".into(), json!(block.to_string()));
        Row {
            block,
            tx_index: 0,
            position: 0,
            record,
        }
    }

    /// Relays answering from a table of (relay, block) -> traces, counting what they are asked.
    fn relays(table: Vec<(&'static str, u64, Value)>) -> (Relays, Arc<Mutex<u64>>) {
        let asked = Arc::new(Mutex::new(0));
        let count = asked.clone();
        let relays: Relays = Arc::new(move |name, block| {
            *count.lock().unwrap() += 1;
            let found: Vec<Value> = table
                .iter()
                .filter(|(r, b, _)| *r == name && *b == block)
                .map(|(_, _, t)| t.clone())
                .collect();
            Box::pin(async move { Ok(found) })
        });
        (relays, asked)
    }

    fn trace(block: u64, recipient: &str, value: &str) -> Value {
        json!({"block_number": block.to_string(), "proposer_fee_recipient": recipient,
               "value": value, "builder_pubkey": "0xbb", "proposer_pubkey": "0xpp"})
    }

    /// A self-built block, a relay-built one paid by its builder, a payment from a block's miner
    /// that no relay knows of, a transfer from someone else, and two relays that disagree.
    #[tokio::test]
    async fn produced_blocks_join_mined_blocks_payments_and_relays() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("t.redb")).unwrap();
        let h = AddressHistory::open(store, 1, &[A.into()]).unwrap();
        h.set_head(1_000).unwrap();
        let g = h.generation().unwrap();
        h.record(
            Action::MinedBlocks,
            A,
            &[
                row(100, json!({"timeStamp": "1000", "blockReward": "5"})),
                row(104, json!({"timeStamp": "1048", "blockReward": "6"})),
            ],
            (100, 199),
            g,
        )
        .unwrap();
        let tx = |block: u64, from: &str, value: &str, hash: &str| {
            row(
                block,
                json!({"timeStamp": format!("{}", 1000 + 12 * (block - 100)), "hash": hash,
                              "from": from, "to": A, "value": value, "isError": "0"}),
            )
        };
        h.record(
            Action::TxList,
            A,
            &[
                tx(101, BUILDER, "9000", "0xp1"),
                tx(
                    102,
                    "0x00000000000000000000000000000000000000c3",
                    "1",
                    "0xp2",
                ),
                tx(
                    103,
                    "0x00000000000000000000000000000000000000e4",
                    "7",
                    "0xp3",
                ),
                {
                    // A builder that pays from another account, in the block's last transaction.
                    let mut last = tx(
                        105,
                        "0x00000000000000000000000000000000000000f5",
                        "11",
                        "0xp5",
                    );
                    last.record.insert("transactionIndex".into(), json!("1"));
                    last
                },
            ],
            (100, 199),
            g,
        )
        .unwrap();
        let main = Fake::new(|m, p| {
            let b = crate::address_discovery::hex_u64(&p[0]).unwrap_or(0);
            Ok(match m {
                "eth_getBlockByNumber" => json!({"miner": match b {
                    101 => BUILDER,
                    103 => "0x00000000000000000000000000000000000000e4",
                    _ => "0x00000000000000000000000000000000000000d9",
                }, "baseFeePerGas": "0x1",
                   "transactions": if b == 105 { json!(["0x01", "0x02"]) } else { json!([]) },
                   "uncles": []}),
                "eth_getBlockReceipts" if p[0] == "0x69" => json!([
                    {"transactionHash": "0x01", "gasUsed": "0x1", "effectiveGasPrice": "0x1"},
                    {"transactionHash": "0x02", "gasUsed": "0x1", "effectiveGasPrice": "0x1"},
                ]),
                "eth_getBlockReceipts" => json!([]),
                other => anyhow::bail!("unexpected {other}"),
            })
        });
        let d = Discoverer::new(
            Counted::new(main),
            Counted::new(Fake::new(|_, _| Ok(json!([])))),
        );
        let (rel, asked) = relays(vec![
            ("ultrasound", 101, trace(101, A, "9000")),
            ("titan", 101, trace(101, A, "9000")),
            ("flashbots", 104, trace(104, A, "6")),
            ("agnostic", 104, trace(104, A, "60")),
            ("aestus", 105, trace(105, A, "11")),
        ]);
        let rows = produced_blocks(&d, &rel, &h, A, (100, 199)).await.unwrap();
        let view: Vec<(u64, String, String, String, String)> = rows
            .iter()
            .map(|r| {
                let t = |k: &str| r.record[k].as_str().unwrap().to_string();
                (
                    r.block,
                    t("feeRecipient"),
                    t("mev"),
                    t("mevReward"),
                    t("paymentTx"),
                )
            })
            .collect();
        assert_eq!(
            view,
            [
                (100, A.into(), "none".into(), "".into(), "".into()),
                (
                    101,
                    BUILDER.into(),
                    "relay".into(),
                    "9000".into(),
                    "0xp1".into()
                ),
                (104, A.into(), "conflict".into(), "".into(), "".into()),
                (
                    105,
                    "0x00000000000000000000000000000000000000d9".into(),
                    "relay".into(),
                    "11".into(),
                    "0xp5".into()
                ),
            ]
        );
        assert_eq!(rows[1].record["relays"], "ultrasound,titan");
        assert_eq!(
            rows[1].record["blockReward"], "5000000000000000000",
            "the builder's reward for block 101, a frontier block in this made-up chain"
        );
        let first = *asked.lock().unwrap();
        assert_eq!(
            first,
            6 * 5,
            "blocks 100, 101, 103, 104 and 105, every relay once"
        );

        // Asked again, the relays' answers come from the store.
        produced_blocks(&d, &rel, &h, A, (100, 199)).await.unwrap();
        assert_eq!(*asked.lock().unwrap(), first);
    }

    /// A relay that cannot be asked fails the lookup; nothing is cached as absent.
    #[tokio::test]
    async fn an_unreachable_relay_is_not_an_absent_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("t.redb")).unwrap();
        let h = AddressHistory::open(store, 1, &[A.into()]).unwrap();
        h.set_head(1_000).unwrap();
        let down: Relays = Arc::new(|name, _| {
            Box::pin(async move {
                if name == "titan" {
                    anyhow::bail!("titan is down")
                }
                Ok(vec![])
            })
        });
        assert!(relay_record(&h, &down, 100).await.is_err());
        assert!(h.relay_record(100).unwrap().is_none());
    }
}
