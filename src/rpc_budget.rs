//! Optional operator-owned request budget. Reserve durably before sending, including retries.
//! Units are operator-defined; this is not a provider billing API or a dollar guarantee.
use anyhow::{bail, Context, Result};
use redb::{Database, Durability, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

const STATE: TableDefinition<&str, u64> = TableDefinition::new("rpc_budget");
const POLICY: TableDefinition<&str, &str> = TableDefinition::new("rpc_budget_policy");
type OpenBudgets = BTreeMap<PathBuf, Weak<Budget>>;
static OPEN: OnceLock<Mutex<OpenBudgets>> = OnceLock::new();

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    limit_units: u64,
    methods: BTreeMap<String, u64>,
}

pub(crate) struct Budget {
    db: Database,
    policy: Policy,
}

impl Budget {
    pub(crate) fn from_env() -> Result<Option<Arc<Self>>> {
        match std::env::var_os("NUTHATCH_RPC_BUDGET") {
            None => Ok(None),
            Some(path) => Self::open(Path::new(&path)).map(Some),
        }
    }

    pub(crate) fn open(path: &Path) -> Result<Arc<Self>> {
        let path = path
            .canonicalize()
            .context("RPC budget policy is unavailable")?;
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            bail!("RPC budget policy must use a .toml filename distinct from its ledger");
        }
        let text = std::fs::read(&path).context("read RPC budget policy")?;
        if text.len() > 65536 {
            bail!("RPC budget policy exceeds 64 KiB");
        }
        let policy: Policy = toml::from_str(std::str::from_utf8(&text)?)?;
        if policy.version != 1 || policy.limit_units == 0 || policy.methods.is_empty() {
            bail!("RPC budget needs version 1, a positive limit, and explicit method costs");
        }
        let mut open = OPEN
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("RPC budget registry lock poisoned"))?;
        if let Some(budget) = open.get(&path).and_then(Weak::upgrade) {
            if budget.policy != policy {
                bail!("RPC budget policy changed while in use");
            }
            return Ok(budget);
        }
        // A separate process cannot open the same ledger concurrently. Log and archive clients
        // in this process share the Arc above instead of receiving independent allowances.
        let ledger = path.with_extension("redb");
        let marker = path.with_extension("budget-initialized");
        let existing_ledger = ledger.try_exists()?;
        if marker.try_exists()? {
            if !existing_ledger {
                bail!("RPC budget ledger is missing after initialization; refusing to recreate its allowance");
            }
        } else {
            let marker_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)?;
            marker_file.sync_all()?;
            // If initialization is interrupted, refuse to guess whether any allowance was used.
            #[cfg(unix)]
            std::fs::File::open(path.parent().context("RPC budget policy has no parent")?)?
                .sync_all()?;
        }
        let mut builder = Database::builder();
        builder.set_cache_size(1024 * 1024);
        let db = if existing_ledger {
            builder
                .open(&ledger)
                .context("open existing RPC budget ledger")?
        } else {
            builder.create(&ledger)?
        };
        let mut tx = db.begin_write()?;
        tx.set_durability(Durability::Immediate);
        {
            let mut stored = tx.open_table(POLICY)?;
            let encoded = serde_json::to_string(&policy)?;
            let old = stored.get("policy")?.map(|v| v.value().to_owned());
            if let Some(old) = old {
                if old != encoded {
                    let previous: Policy = serde_json::from_str(&old)
                        .context("RPC budget ledger contains an invalid policy")?;
                    let state = tx.open_table(STATE)?;
                    let used = state
                        .get("used")?
                        .context("RPC budget ledger has no usage")?
                        .value();
                    if previous.version != policy.version
                        || previous.methods != policy.methods
                        || policy.limit_units < previous.limit_units
                        || used > policy.limit_units
                    {
                        bail!("RPC budget policy differs from its durable ledger; only a larger allowance with identical pricing may be migrated");
                    }
                    // An operator-approved allowance increase preserves every unit already
                    // reserved. Method prices and the version remain immutable.
                    stored.insert("policy", encoded.as_str())?;
                }
            } else {
                if existing_ledger {
                    bail!("existing RPC budget ledger has no policy; refusing to reset allowance");
                }
                stored.insert("policy", encoded.as_str())?;
            }
            let mut state = tx.open_table(STATE)?;
            if state.get("used")?.is_none() {
                if existing_ledger {
                    bail!("existing RPC budget ledger has no usage; refusing to reset allowance");
                }
                state.insert("used", 0)?;
            }
        }
        tx.commit()?;
        #[cfg(unix)]
        std::fs::File::open(path.parent().context("RPC budget policy has no parent")?)?
            .sync_all()?;
        let budget = Arc::new(Self { db, policy });
        open.insert(path, Arc::downgrade(&budget));
        Ok(budget)
    }

    pub(crate) fn reserved_units(&self) -> Result<u64> {
        let tx = self.db.begin_read()?;
        let state = tx.open_table(STATE)?;
        let used = state
            .get("used")?
            .context("RPC budget ledger has no usage")?
            .value();
        Ok(used)
    }

    fn cost(&self, body: &Value) -> Result<u64> {
        let one = |request: &Value| -> Result<u64> {
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .context("RPC budget refuses a request without a method")?;
            self.policy
                .methods
                .get(method)
                .copied()
                .with_context(|| format!("RPC budget has no price for method {method}"))
        };
        match body {
            Value::Array(requests) if !requests.is_empty() => {
                requests.iter().try_fold(0u64, |sum, request| {
                    sum.checked_add(one(request)?)
                        .context("RPC budget batch cost overflow")
                })
            }
            Value::Object(_) => one(body),
            _ => bail!("RPC budget refuses an invalid request envelope"),
        }
    }

    pub(crate) fn reserve(&self, body: &Value) -> Result<()> {
        let cost = self.cost(body)?;
        let mut tx = self.db.begin_write()?;
        tx.set_durability(Durability::Immediate);
        {
            let mut state = tx.open_table(STATE)?;
            let used = state
                .get("used")?
                .context("RPC budget ledger has no usage")?
                .value();
            let next = used
                .checked_add(cost)
                .context("RPC budget usage overflow")?;
            if next > self.policy.limit_units {
                bail!(
                    "RPC budget exhausted: {used} units reserved, request needs {cost}, limit {}",
                    self.policy.limit_units
                );
            }
            state.insert("used", next)?;
        }
        // A crash after this commit may over-count one unsent request, never under-count a sent one.
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_budget_is_shared_durable_and_refuses_unknown_or_oversized_batches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        std::fs::write(
            &path,
            "version=1\nlimit_units=60\n[methods]\neth_call=26\neth_chainId=0\n",
        )
        .unwrap();
        let a = Budget::open(&path).unwrap();
        assert_eq!(a.reserved_units().unwrap(), 0);
        let b = Budget::open(&path).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(a
            .reserve(&json!([{"method":"eth_call"},{"method":"unknown"}]))
            .is_err());
        assert!(a
            .reserve(&json!([{"method":"eth_call"},{"method":"eth_call"},{"method":"eth_call"}]))
            .is_err());
        a.reserve(&json!({"method":"eth_call"})).unwrap();
        b.reserve(&json!({"method":"eth_call"})).unwrap();
        assert_eq!(a.reserved_units().unwrap(), 52);
        drop(a);
        drop(b);
        let resumed = Budget::open(&path).unwrap();
        assert_eq!(resumed.reserved_units().unwrap(), 52);
        assert!(resumed
            .reserve(&json!({"method":"eth_call"}))
            .unwrap_err()
            .to_string()
            .contains("52 units reserved"));
        resumed.reserve(&json!({"method":"eth_chainId"})).unwrap();
        drop(resumed);
        std::fs::write(
            &path,
            "version=1\nlimit_units=600\n[methods]\neth_call=26\neth_chainId=0\n",
        )
        .unwrap();
        let migrated = Budget::open(&path).unwrap();
        assert_eq!(migrated.reserved_units().unwrap(), 52);
        migrated.reserve(&json!({"method":"eth_call"})).unwrap();
        assert_eq!(migrated.reserved_units().unwrap(), 78);
        drop(migrated);
        std::fs::write(
            &path,
            "version=1\nlimit_units=601\n[methods]\neth_call=27\neth_chainId=0\n",
        )
        .unwrap();
        assert!(
            Budget::open(&path).is_err(),
            "changing method prices must remain refused"
        );
    }

    #[test]
    fn concurrent_reservations_cannot_overspend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        std::fs::write(&path, "version=1\nlimit_units=20\n[methods]\neth_call=1\n").unwrap();
        let budget = Budget::open(&path).unwrap();
        let threads: Vec<_> = (0..40)
            .map(|_| {
                let budget = budget.clone();
                std::thread::spawn(move || budget.reserve(&json!({"method":"eth_call"})).is_ok())
            })
            .collect();
        let accepted = threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>();
        assert_eq!(accepted, 20);
        assert!(budget.reserve(&json!({"method":"eth_call"})).is_err());
    }

    #[test]
    fn an_existing_ledger_with_missing_accounting_is_not_reinitialized() {
        for missing_policy in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("budget.toml");
            std::fs::write(&path, "version=1\nlimit_units=20\n[methods]\neth_call=1\n").unwrap();
            let budget = Budget::open(&path).unwrap();
            budget.reserve(&json!({"method":"eth_call"})).unwrap();
            drop(budget);
            let db = Database::open(path.with_extension("redb")).unwrap();
            let tx = db.begin_write().unwrap();
            if missing_policy {
                tx.open_table(POLICY).unwrap().remove("policy").unwrap();
            } else {
                tx.open_table(STATE).unwrap().remove("used").unwrap();
            }
            tx.commit().unwrap();
            drop(db);
            assert!(
                Budget::open(&path).is_err(),
                "missing accounting must not restore allowance"
            );
        }
    }

    #[test]
    fn a_truncated_ledger_is_not_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        std::fs::write(&path, "version=1\nlimit_units=20\n[methods]\neth_call=1\n").unwrap();
        let budget = Budget::open(&path).unwrap();
        budget.reserve(&json!({"method":"eth_call"})).unwrap();
        drop(budget);
        std::fs::write(path.with_extension("redb"), []).unwrap();
        assert!(
            Budget::open(&path).is_err(),
            "an empty existing ledger is not a new allowance"
        );
    }

    #[test]
    fn a_missing_ledger_does_not_silently_reset_the_allowance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.toml");
        std::fs::write(&path, "version=1\nlimit_units=20\n[methods]\neth_call=1\n").unwrap();
        let budget = Budget::open(&path).unwrap();
        budget.reserve(&json!({"method":"eth_call"})).unwrap();
        drop(budget);
        std::fs::remove_file(path.with_extension("redb")).unwrap();
        assert!(Budget::open(&path).is_err());
    }
}
