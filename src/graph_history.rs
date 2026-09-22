//! Opt-in historical Graph reads over block-stamped facts. This is a read policy, not an indexer:
//! the nest author must derive entities from complete event/call history in authored SQL views.

use crate::{graph_query::Value, store::HotStore};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    /// Oldest block for which the nest's inputs provide complete state.
    pub first_block: u64,
    /// Latest-state requests fail once the indexed head is older than this.
    pub max_head_age_seconds: u64,
    /// Optional operational limit against this cursor's recently observed chain head.
    #[serde(default)]
    pub max_block_distance: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub number: u64,
    pub hash: String,
    pub timestamp: u64,
}

impl Policy {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let policy: Self = toml::from_str(&text).context("invalid Graph history policy")?;
        if policy.version != 1 || policy.max_head_age_seconds == 0 {
            bail!("Graph history policy requires version = 1 and a positive freshness limit");
        }
        Ok(Some(policy))
    }

    /// Read the retained head without declaring it fresh. Status reporting must expose a stale
    /// checkpoint as stale, rather than hide the last useful observation or advance it to tip.
    pub fn indexed_head(&self, store: &dyn HotStore) -> Result<Snapshot> {
        let number = store
            .get_meta("last_block")?
            .context("Graph history has no indexed head")?
            .parse::<u64>()
            .context("invalid indexed head")?;
        self.at(store, number, number)
    }

    pub fn head(&self, store: &dyn HotStore, now: u64) -> Result<Snapshot> {
        let snapshot = self.indexed_head(store)?;
        let number = snapshot.number;
        if snapshot.timestamp > now.saturating_add(30) {
            bail!("indexed head timestamp is in the future; check the clock and source");
        }
        if now.saturating_sub(snapshot.timestamp) > self.max_head_age_seconds {
            bail!(
                "network data is stale: indexed block {number} is {} seconds old (limit {})",
                now.saturating_sub(snapshot.timestamp),
                self.max_head_age_seconds
            );
        }
        Ok(snapshot)
    }

    pub fn check_observed_head(
        &self,
        indexed: u64,
        tip: u64,
        last_poll: u64,
        now: u64,
    ) -> Result<()> {
        let Some(limit) = self.max_block_distance else {
            return Ok(());
        };
        if tip == 0
            || last_poll == 0
            || last_poll > now.saturating_add(30)
            || now.saturating_sub(last_poll) > self.max_head_age_seconds
        {
            bail!("no recent chain-head observation for the Graph freshness gate");
        }
        if tip < indexed {
            bail!("observed chain head {tip} is behind indexed block {indexed}; refusing an inconsistent freshness observation");
        }
        if tip - indexed > limit {
            bail!("network data is stale: indexed block {indexed} is {} blocks behind observed head {tip} (limit {limit})", tip - indexed);
        }
        Ok(())
    }

    pub fn select(
        &self,
        store: &dyn HotStore,
        head: &Snapshot,
        value: Option<&Value>,
    ) -> Result<Snapshot> {
        let number = self.select_data_block(store, head, value)?;
        self.at(store, head.number, number)
    }

    /// Entity folds need an upper block bound, not a header for every empty block. Metadata still
    /// uses `select`, which requires the real retained header. Hash selectors require a retained
    /// canonical checkpoint on both paths; never infer a hash from a neighbouring block.
    pub fn select_data_block(
        &self,
        store: &dyn HotStore,
        head: &Snapshot,
        value: Option<&Value>,
    ) -> Result<u64> {
        let fields = match value {
            None | Some(Value::Null) => return Ok(head.number),
            Some(Value::Object(fields)) if fields.len() == 1 => fields,
            _ => bail!("block must specify exactly one of number, hash or number_gte"),
        };
        let (kind, value) = fields.iter().next().expect("one selector");
        let number = match (kind.as_str(), value) {
            ("number" | "number_gte", Value::Int(n)) if *n >= 0 => *n as u64,
            ("hash", Value::Str(hash)) => {
                if hash.len() != 66
                    || !hash.starts_with("0x")
                    || !hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    bail!("block.hash must be a 32-byte 0x-prefixed hash");
                }
                if hash.eq_ignore_ascii_case(&head.hash) {
                    return Ok(head.number);
                }
                store
                    .checkpoint_number(hash)?
                    .context("requested block hash is not a retained canonical checkpoint")?
            }
            _ => bail!("invalid block selector"),
        };
        if number > head.number {
            bail!("subgraph has only indexed up to block number {} and data for block number {number} is therefore not yet available", head.number);
        }
        if kind == "number_gte" {
            return Ok(head.number);
        }
        if number < self.first_block {
            bail!(
                "block {number} is outside the available history {}..={}",
                self.first_block,
                head.number
            );
        }
        if let ("hash", Value::Str(hash)) = (kind.as_str(), value) {
            let snapshot = self.at(store, head.number, number)?;
            if !snapshot.hash.eq_ignore_ascii_case(hash) {
                bail!("requested hash is no longer a canonical checkpoint; retry");
            }
        }
        Ok(number)
    }

    fn at(&self, store: &dyn HotStore, head: u64, number: u64) -> Result<Snapshot> {
        if number < self.first_block || number > head {
            bail!(
                "block {number} is outside the available history {}..={head}",
                self.first_block
            );
        }
        let hash = store
            .get_block_hash(number)?
            .context("requested block has no canonical checkpoint hash")?;
        if hash.len() != 66
            || !hash.starts_with("0x")
            || !hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            bail!("canonical checkpoint has a malformed block hash");
        }
        let timestamp = store
            .get_block_timestamp(number)?
            .context("requested block has no checkpoint timestamp")?;
        Ok(Snapshot {
            number,
            hash: hash.to_ascii_lowercase(),
            timestamp,
        })
    }
}

/// Check the wire contract before operational clients see a successful response. A view returning
/// NULL for a required balance, or a JSON number for BigInt, must fail rather than look authoritative.
pub fn validate_result(
    schema: &crate::graph_schema::Schema,
    entity: &str,
    selections: &[crate::graph_query::Selection],
    value: &serde_json::Value,
) -> Result<()> {
    use crate::graph_schema::FieldType;
    let object = value
        .as_object()
        .context("Graph entity did not resolve to an object")?;
    let definition = schema
        .entities
        .iter()
        .find(|e| e.name == entity)
        .context("unknown Graph entity")?;
    for selection in selections {
        let field = definition
            .fields
            .iter()
            .find(|f| f.name == selection.name)
            .context("unknown Graph field")?;
        let path = format!("{entity}.{}", field.name);
        let value = object
            .get(&selection.key)
            .with_context(|| format!("missing selected field {path}"))?;
        if value.is_null() {
            if field.non_null {
                bail!("null value resolved for non-null field {path}");
            }
            continue;
        }
        fn check(
            schema: &crate::graph_schema::Schema,
            ty: &FieldType,
            selections: &[crate::graph_query::Selection],
            value: &serde_json::Value,
            path: &str,
            inner_non_null: bool,
        ) -> Result<()> {
            match ty {
                FieldType::Entity(name) => validate_result(schema, name, selections, value)?,
                FieldType::List(inner) => {
                    let values = value
                        .as_array()
                        .with_context(|| format!("{path} must resolve to a list"))?;
                    for value in values {
                        if value.is_null() {
                            if inner_non_null {
                                bail!("null member in non-null list {path}");
                            }
                        } else {
                            check(schema, inner, selections, value, path, false)?;
                        }
                    }
                }
                FieldType::Enum(name) => {
                    if !value.as_str().is_some_and(|v| {
                        schema
                            .enums
                            .get(name)
                            .is_some_and(|items| items.iter().any(|s| s == v))
                    }) {
                        bail!("invalid enum value at {path}");
                    }
                }
                FieldType::Scalar(name) => {
                    let valid = match name.as_str() {
                        "Int" => value.as_i64().is_some_and(|n| i32::try_from(n).is_ok()),
                        "Boolean" => value.is_boolean(),
                        "Float" => value.is_number(),
                        "BigInt" => value.as_str().is_some_and(|s| {
                            s.parse::<num_bigint::BigInt>()
                                .is_ok_and(|n| n.to_string() == s)
                        }),
                        "Int8" => value
                            .as_str()
                            .is_some_and(|s| s.parse::<i64>().is_ok_and(|n| n.to_string() == s)),
                        "BigDecimal" => value.as_str().is_some_and(decimal_wire),
                        "Bytes" => value.as_str().is_some_and(|s| {
                            s.starts_with("0x")
                                && s.len().is_multiple_of(2)
                                && s[2..]
                                    .bytes()
                                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                        }),
                        "String" | "ID" | "Timestamp" => value.is_string(),
                        _ => false,
                    };
                    if !valid {
                        bail!("invalid {name} wire value at {path}");
                    }
                }
            }
            Ok(())
        }
        check(
            schema,
            &field.ty,
            &selection.sub,
            value,
            &path,
            field.inner_non_null,
        )?;
    }
    Ok(())
}

fn decimal_wire(value: &str) -> bool {
    let value = value.strip_prefix('-').unwrap_or(value);
    let (mantissa, exponent) = value
        .split_once(['e', 'E'])
        .map_or((value, None), |(m, e)| (m, Some(e)));
    if exponent.is_some_and(|e| e.parse::<i32>().is_err()) {
        return false;
    }
    let (integer, fraction) = mantissa
        .split_once('.')
        .map_or((mantissa, None), |(i, f)| (i, Some(f)));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(integer) && fraction.is_none_or(digits)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "graph")]
    use crate::store::Store;

    #[test]
    fn operational_results_refuse_missing_required_fields_and_wrong_scalar_types() {
        let schema = crate::graph_schema::parse(
            "type Account @entity { id: Bytes! balance: BigInt! epoch: Int! }",
        )
        .unwrap();
        let query = crate::graph_query::parse("{ accounts { id balance epoch } }").unwrap();
        let valid = serde_json::json!({"id":"0x1234","balance":"1000000000000000000","epoch":12});
        validate_result(&schema, "Account", &query[0].sel, &valid).unwrap();
        for (field, value) in [
            ("id", serde_json::json!("1234")),
            ("balance", serde_json::json!(123)),
            ("balance", serde_json::Value::Null),
            ("epoch", serde_json::json!("12")),
            ("epoch", serde_json::json!(2147483648u64)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(validate_result(&schema, "Account", &query[0].sel, &invalid).is_err());
        }
    }

    #[test]
    fn decimal_wire_requires_finite_decimal_syntax() {
        for value in ["0", "-1.25", "123.000", "1e-18", "1E+30"] {
            assert!(decimal_wire(value), "{value}");
        }
        for value in [
            "", "NaN", "Infinity", "1.2.3", "1e", "1e2e3", ".", " 1", "1 ",
        ] {
            assert!(!decimal_wire(value), "{value}");
        }
    }

    #[cfg(feature = "graph")]
    #[test]
    fn selectors_resolve_canonical_checkpoints_and_refuse_stale_or_unavailable_data() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.redb")).unwrap();
        let policy = Policy {
            version: 1,
            first_block: 10,
            max_head_age_seconds: 15,
            max_block_distance: None,
        };
        assert!(policy.head(&store, 100).is_err());
        let old_hash = format!("0x{}", "11".repeat(32));
        let hash = format!("0x{}", "22".repeat(32));
        store.set_block_hash(10, &old_hash).unwrap();
        store.set_block_timestamp(10, 90).unwrap();
        store.set_block_hash(20, &hash).unwrap();
        store.set_block_timestamp(20, 100).unwrap();
        store.set_meta("last_block", "20").unwrap();
        let head = policy.head(&store, 110).unwrap();
        let selector = |v| Value::from_json(&v).unwrap();
        let old = selector(serde_json::json!({"hash": old_hash}));
        assert_eq!(policy.select(&store, &head, Some(&old)).unwrap().number, 10);
        assert_eq!(
            policy.select_data_block(&store, &head, Some(&old)).unwrap(),
            10
        );
        assert_eq!(
            policy
                .select_data_block(
                    &store,
                    &head,
                    Some(&selector(serde_json::json!({"number": 15})))
                )
                .unwrap(),
            15
        );
        for value in [
            serde_json::json!({"number": 21}),
            serde_json::json!({"number": 9}),
            serde_json::json!({"number": -1}),
            serde_json::json!({"number": "15"}),
            serde_json::json!({"hash": "0xabc"}),
            serde_json::json!({"hash": format!("0x{}", "33".repeat(32))}),
            serde_json::json!({"number": 10, "number_gte": 10}),
        ] {
            assert!(policy
                .select_data_block(&store, &head, Some(&selector(value)))
                .is_err());
        }
        for value in [
            serde_json::json!({"number": 21}),
            serde_json::json!({"number": 9}),
            serde_json::json!({"number": 15}),
            serde_json::json!({"number": -1}),
            serde_json::json!({"hash": "0xabc"}),
            serde_json::json!({"number": 10, "number_gte": 10}),
        ] {
            assert!(policy
                .select(&store, &head, Some(&selector(value)))
                .is_err());
        }
        assert_eq!(
            policy.select(&store, &head, Some(&Value::Null)).unwrap(),
            head
        );
        assert_eq!(
            policy
                .select(
                    &store,
                    &head,
                    Some(&selector(serde_json::json!({"number_gte": 10})))
                )
                .unwrap(),
            head
        );
        assert!(policy
            .head(&store, 116)
            .unwrap_err()
            .to_string()
            .contains("stale"));
        assert!(policy
            .head(&store, 60)
            .unwrap_err()
            .to_string()
            .contains("future"));
        store.rollback_to(10).unwrap();
        assert!(store.checkpoint_number(&hash).unwrap().is_none());
        assert_eq!(store.checkpoint_number(&old_hash).unwrap(), Some(10));
    }

    #[test]
    fn block_distance_needs_a_recent_consistent_observation() {
        let policy = Policy {
            version: 1,
            first_block: 0,
            max_head_age_seconds: 15,
            max_block_distance: Some(60),
        };
        assert!(policy.check_observed_head(100, 160, 1000, 1010).is_ok());
        for (head, poll, now) in [
            (161, 1000, 1010),
            (99, 1000, 1010),
            (0, 1000, 1010),
            (160, 0, 1010),
            (160, 900, 1010),
            (160, 1100, 1010),
        ] {
            assert!(policy.check_observed_head(100, head, poll, now).is_err());
        }
    }
}
