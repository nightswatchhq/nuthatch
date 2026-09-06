//! A deterministic memo for the analytical surface (#1186).
//!
//! The dashboard views a nest serves are whole-history folds recomputed on every request: after
//! #1183 the Lodestar nest still spent 89 s answering its 47 distinct statements, 42 s of it in
//! four. Those statements repeat, and between two repeats nothing they read has usually changed -
//! a cursor at a five-minute poll interval commits a handful of times an hour.
//!
//! This is not a TTL cache and never serves anything stale. An entry is keyed on **every input the
//! answer depends on**: the nest, the statement and its row cap, the sealed watermark, the hot
//! store's write generation, each maintained entity's watermark, and the content of the authored
//! files (`nuthatch.toml`, `views/*.sql`, `labels/*.json`). Same key, same rows, by construction; a
//! single commit, seal or edited view changes the key and the next request computes. Determinism in
//! the core is what makes this admissible: the answer is a function of its inputs, so remembering
//! it is remembering a fact.
//!
//! Bounded by bytes (`NUTHATCH_SQL_MEMO_BYTES`, default 64 MiB, `0` disables), evicted least
//! recently used, process-local, and cleared by a restart. A degraded answer - a table reduced by an
//! unreadable segment, or a tip that would not scan - is never remembered: it describes a fault, not
//! the data, and the next request should look again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex};

use sha2::{Digest, Sha256};

use crate::analytics::QueryOutput;

/// Default ceiling on remembered rows, in serialized bytes.
pub const DEFAULT_MAX_BYTES: usize = 64 << 20;
/// Environment override for the ceiling; `0` turns the memo off.
pub const ENV_MAX_BYTES: &str = "NUTHATCH_SQL_MEMO_BYTES";
/// One entry may not take more than this share of the ceiling, or a single wide answer would evict
/// everything else for the benefit of one caller.
const MAX_ENTRY_SHARE: usize = 4;

/// The identity of an answer: a hash over every input it depends on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key([u8; 32]);

/// Everything a `/sql` answer is a function of. Building one is cheap - the file stamps are the
/// hashes `analytics::duck_inputs` already computes per request - and none of it needs a permit.
pub struct Inputs<'a> {
    pub dir: &'a Path,
    pub sql: &'a str,
    pub max_rows: usize,
    pub sealed_through: u64,
    pub write_generation: u64,
    pub entity_watermarks: &'a BTreeMap<String, u64>,
    pub files: &'a BTreeMap<PathBuf, String>,
}

impl Inputs<'_> {
    pub fn key(&self) -> Key {
        let mut h = Sha256::new();
        // Length-prefixed so no two field layouts can collide by concatenation.
        let mut field = |bytes: &[u8]| {
            h.update((bytes.len() as u64).to_le_bytes());
            h.update(bytes);
        };
        field(self.dir.to_string_lossy().as_bytes());
        field(self.sql.as_bytes());
        field(&(self.max_rows as u64).to_le_bytes());
        field(&self.sealed_through.to_le_bytes());
        field(&self.write_generation.to_le_bytes());
        for (name, through) in self.entity_watermarks {
            field(name.as_bytes());
            field(&through.to_le_bytes());
        }
        for (path, stamp) in self.files {
            field(path.to_string_lossy().as_bytes());
            field(stamp.as_bytes());
        }
        Key(h.finalize().into())
    }
}

/// A remembered answer: the output and the entity watermarks it was answered from, which ride out
/// in the provenance exactly as they did the first time.
pub struct Entry {
    pub out: QueryOutput,
    pub watermarks: BTreeMap<String, u64>,
    /// The provenance the rows were computed under - `last_block` and the sealed watermark read in
    /// the same blocking task as the query. A hit cites these, never the live store: the store may
    /// have moved between the lookup and the response, and a citation that names a newer state for
    /// older rows is false even when every row in it is right (Jules on #1189).
    pub as_of: Option<u64>,
    pub sealed_through: u64,
    bytes: usize,
}

struct State {
    entries: HashMap<Key, (Arc<Entry>, u64)>,
    bytes: usize,
    tick: u64,
}

/// One memo: the process has one, and a test may build its own so eviction can be exercised against
/// a ceiling of its choosing without touching shared state.
pub struct Memo {
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for Memo {
    fn default() -> Self {
        Self::new()
    }
}

impl Memo {
    pub fn new() -> Self {
        Memo {
            state: Mutex::new(State {
                entries: HashMap::new(),
                bytes: 0,
                tick: 0,
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The remembered answer for `key`, if any. Counted as a hit or a miss either way.
    pub fn get(&self, key: &Key, cap: usize) -> Option<Arc<Entry>> {
        if cap == 0 {
            return None;
        }
        let mut st = self.state();
        st.tick += 1;
        let tick = st.tick;
        match st.entries.get_mut(key) {
            Some((entry, used)) => {
                *used = tick;
                self.hits.fetch_add(1, Relaxed);
                Some(Arc::clone(entry))
            }
            None => {
                self.misses.fetch_add(1, Relaxed);
                None
            }
        }
    }

    /// Remember `out` under `key`. Returns whether it was kept: a degraded or tip-less answer is not,
    /// and neither is one larger than a quarter of the ceiling. Evicts least recently used entries
    /// until the total fits.
    pub fn put(
        &self,
        key: Key,
        out: &QueryOutput,
        watermarks: &BTreeMap<String, u64>,
        provenance: (Option<u64>, u64),
        cap: usize,
    ) -> bool {
        if cap == 0 || out.degraded() || out.tip_unavailable {
            return false;
        }
        let bytes = size_of(out);
        if bytes > cap / MAX_ENTRY_SHARE {
            return false;
        }
        let mut st = self.state();
        st.tick += 1;
        let tick = st.tick;
        if let Some((old, _)) = st.entries.remove(&key) {
            st.bytes -= old.bytes;
        }
        while st.bytes + bytes > cap {
            let Some(victim) = st
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            if let Some((gone, _)) = st.entries.remove(&victim) {
                st.bytes -= gone.bytes;
            }
        }
        st.bytes += bytes;
        st.entries.insert(
            key,
            (
                Arc::new(Entry {
                    out: out.clone(),
                    watermarks: watermarks.clone(),
                    as_of: provenance.0,
                    sealed_through: provenance.1,
                    bytes,
                }),
                tick,
            ),
        );
        true
    }

    pub fn bytes(&self) -> usize {
        self.state().bytes
    }
    pub fn entries(&self) -> usize {
        self.state().entries.len()
    }
    pub fn hits(&self) -> u64 {
        self.hits.load(Relaxed)
    }
    pub fn misses(&self) -> u64 {
        self.misses.load(Relaxed)
    }
}

/// The process's memo, behind the `/sql` handler.
static GLOBAL: LazyLock<Memo> = LazyLock::new(Memo::new);

/// The byte ceiling in force: `NUTHATCH_SQL_MEMO_BYTES` if set and parseable, else the default.
pub fn max_bytes() -> usize {
    std::env::var(ENV_MAX_BYTES)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_BYTES)
}

pub fn get(key: &Key) -> Option<Arc<Entry>> {
    GLOBAL.get(key, max_bytes())
}
pub fn put(
    key: Key,
    out: &QueryOutput,
    watermarks: &BTreeMap<String, u64>,
    provenance: (Option<u64>, u64),
) -> bool {
    GLOBAL.put(key, out, watermarks, provenance, max_bytes())
}
pub fn bytes() -> usize {
    GLOBAL.bytes()
}
pub fn entries() -> usize {
    GLOBAL.entries()
}
pub fn hits() -> u64 {
    GLOBAL.hits()
}
pub fn misses() -> u64 {
    GLOBAL.misses()
}

/// Serialized size of the rows, which is what the response carries and what the ceiling is about.
fn size_of(out: &QueryOutput) -> usize {
    serde_json::to_vec(&out.rows).map(|v| v.len()).unwrap_or(0) + 64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inputs<'a>(
        dir: &'a Path,
        sql: &'a str,
        generation: u64,
        wm: &'a BTreeMap<String, u64>,
        files: &'a BTreeMap<PathBuf, String>,
    ) -> Inputs<'a> {
        Inputs {
            dir,
            sql,
            max_rows: 100,
            sealed_through: 10,
            write_generation: generation,
            entity_watermarks: wm,
            files,
        }
    }

    fn rows(n: usize) -> QueryOutput {
        QueryOutput {
            rows: (0..n)
                .map(|i| json!({"n": i, "pad": "x".repeat(100)}))
                .collect(),
            ..Default::default()
        }
    }

    /// Every input is part of the identity: change any one and the key differs, so no answer can be
    /// served for a state it was not computed from.
    #[test]
    fn every_input_changes_the_key() {
        let d = std::path::PathBuf::from("/n");
        let wm: BTreeMap<String, u64> = [("e".to_string(), 5)].into_iter().collect();
        let files: BTreeMap<PathBuf, String> =
            [(PathBuf::from("/n/views/a.sql"), "h1".to_string())]
                .into_iter()
                .collect();
        let base = inputs(&d, "SELECT 1", 3, &wm, &files).key();
        assert_eq!(
            base,
            inputs(&d, "SELECT 1", 3, &wm, &files).key(),
            "deterministic"
        );
        assert_ne!(
            base,
            inputs(&d, "SELECT 2", 3, &wm, &files).key(),
            "statement"
        );
        assert_ne!(
            base,
            inputs(&d, "SELECT 1", 4, &wm, &files).key(),
            "write generation"
        );
        let mut i = inputs(&d, "SELECT 1", 3, &wm, &files);
        i.sealed_through = 11;
        assert_ne!(base, i.key(), "sealed watermark");
        let mut i = inputs(&d, "SELECT 1", 3, &wm, &files);
        i.max_rows = 101;
        assert_ne!(base, i.key(), "row cap");
        let wm2: BTreeMap<String, u64> = [("e".to_string(), 6)].into_iter().collect();
        assert_ne!(
            base,
            inputs(&d, "SELECT 1", 3, &wm2, &files).key(),
            "entity watermark"
        );
        let files2: BTreeMap<PathBuf, String> =
            [(PathBuf::from("/n/views/a.sql"), "h2".to_string())]
                .into_iter()
                .collect();
        assert_ne!(
            base,
            inputs(&d, "SELECT 1", 3, &wm, &files2).key(),
            "authored file content"
        );
        let d2 = std::path::PathBuf::from("/m");
        assert_ne!(base, inputs(&d2, "SELECT 1", 3, &wm, &files).key(), "nest");
    }

    /// A degraded answer describes a fault, not the data, and is not remembered.
    #[test]
    fn a_degraded_or_tipless_answer_is_not_remembered() {
        let wm = BTreeMap::new();
        let mut out = rows(1);
        out.degraded_tables.insert("t".into());
        assert!(!put(Key([1; 32]), &out, &wm, (None, 0)));
        let mut out = rows(1);
        out.tip_unavailable = true;
        assert!(!put(Key([2; 32]), &out, &wm, (None, 0)));
        assert!(get(&Key([1; 32])).is_none());
        assert!(get(&Key([2; 32])).is_none());
    }

    /// The ceiling holds and the least recently used entry goes first. A memo of this test's own, so
    /// nothing the process-wide one holds can lean on the arithmetic.
    #[test]
    fn the_ceiling_evicts_least_recently_used() {
        let m = Memo::new();
        let wm = BTreeMap::new();
        let k = |n: u8| Key([n; 32]);
        let one = size_of(&rows(10));
        // Room for four entries and a little, never five; each is under a quarter of it.
        let cap = one * 4 + one / 2;
        for n in 1..=4 {
            assert!(m.put(k(n), &rows(10), &wm, (None, 0), cap));
        }
        assert_eq!(m.entries(), 4);
        assert!(
            m.get(&k(1), cap).is_some(),
            "touching 1 makes 2 the least recently used"
        );
        assert!(m.put(k(5), &rows(10), &wm, (None, 0), cap));
        assert!(m.bytes() <= cap, "the ceiling holds");
        assert_eq!(m.entries(), 4, "exactly one entry made room");
        assert!(
            m.get(&k(2), cap).is_none(),
            "the least recently used entry is the one evicted"
        );
        for n in [1, 3, 4, 5] {
            assert!(m.get(&k(n), cap).is_some(), "entry {n} survives");
        }
        assert_eq!((m.hits(), m.misses()), (5, 1));
    }

    /// One answer may not take the whole memo: an entry over a quarter of the ceiling is declined
    /// rather than evicting everything else for one caller. And a ceiling of zero is the off switch.
    #[test]
    fn an_answer_wider_than_a_quarter_of_the_ceiling_is_declined() {
        let m = Memo::new();
        let wm = BTreeMap::new();
        let out = rows(10);
        let cap = size_of(&out) * MAX_ENTRY_SHARE - 1;
        assert!(!m.put(Key([0xb1; 32]), &out, &wm, (None, 0), cap));
        assert!(m.get(&Key([0xb1; 32]), cap).is_none());
        assert!(m.put(Key([0xb2; 32]), &out, &wm, (None, 0), cap + 1));
        assert!(
            !m.put(Key([0xb3; 32]), &out, &wm, (None, 0), 0),
            "zero disables"
        );
        assert!(
            m.get(&Key([0xb2; 32]), 0).is_none(),
            "zero disables reads too"
        );
    }
}
