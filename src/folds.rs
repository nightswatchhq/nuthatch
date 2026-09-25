//! RFC-0059: checkpointed folds. Built only with the `folds` feature (which `graph` enables).
//!
//! A fold is one SELECT in `folds/<name>.sql`, declared in `folds/folds.toml`. Loading binds every
//! fold against the nest's surface without reading a row, and refuses anything a checkpoint could not
//! carry exactly: a volatile call, an input it cannot enumerate, or an output that is not its carry.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::analytics;
use crate::registry::TableSchema;

pub const FOLDS_DIR: &str = "folds";
pub const FOLDS_TOML: &str = "folds.toml";

/// What a checkpoint may hold: types that round-trip exactly through Parquet and sort
/// deterministically. Big integers are carried as VARCHAR and cast in the step.
const CARRY_TYPES: &[&str] = &[
    "VARCHAR", "BOOLEAN", "BIGINT", "UBIGINT", "INTEGER", "UINTEGER", "SMALLINT", "BLOB",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldKey {
    Columns(Vec<String>),
    Singleton,
    Unkeyed,
}

#[derive(Debug, Clone)]
pub struct Fold {
    pub name: String,
    pub file: String,
    pub sql: String,
    pub key: FoldKey,
    /// `(column, DuckDB type)` in output order, types as DuckDB spells them.
    pub carry: Vec<(String, String)>,
    pub max_rows: u64,
    /// Every fact table and view the fold reads, through views. Other folds and carries excluded.
    pub reaches: BTreeSet<String>,
    /// Earlier folds this one reads, at `hi` or through their carry.
    pub deps: BTreeSet<String>,
    /// RFC-0059 §5 `fold_hash`: the content address of everything that decides this fold's output
    /// for a given set of facts. Its checkpoints live under `checkpoints/<hash>/`.
    pub hash: String,
    /// #1504: each view this fold reaches that looks back into history, and how. Inside a fold such a
    /// view answers over the window alone, with nothing else to say so.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FoldSet {
    /// In load order, which is dependency order.
    pub folds: Vec<Fold>,
    pub retention: Retention,
}

/// Which checkpoint files are kept (RFC-0059 §5). Every log entry is kept whatever happens to its
/// file, because the identity chain needs each link; only the Parquet goes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Retention {
    /// Every checkpoint among the latest this many is kept.
    pub recent: usize,
    /// Before those, the earliest checkpoint in each span of this many blocks, so a historical read
    /// steps at most about two spans of facts.
    pub every_blocks: u64,
    /// When set, nothing older than this many blocks behind the latest checkpoint is kept, and a read
    /// into that dropped history is refused by name.
    pub horizon_blocks: Option<u64>,
}

pub const DEFAULT_RETAIN_RECENT: usize = 8;
pub const DEFAULT_RETAIN_EVERY_BLOCKS: u64 = 10_000_000;

impl Default for Retention {
    fn default() -> Self {
        Retention {
            recent: DEFAULT_RETAIN_RECENT,
            every_blocks: DEFAULT_RETAIN_EVERY_BLOCKS,
            horizon_blocks: None,
        }
    }
}

impl Retention {
    /// Of these checkpoint blocks, ascending, the ones whose files are kept.
    fn keep(&self, blocks: &[u64]) -> BTreeSet<u64> {
        let Some(&latest) = blocks.last() else {
            return BTreeSet::new();
        };
        let floor = self.horizon_blocks.map_or(0, |h| latest.saturating_sub(h));
        let mut keep: BTreeSet<u64> = blocks.iter().rev().take(self.recent).copied().collect();
        let mut bucket = None;
        for &b in blocks.iter().filter(|b| **b >= floor) {
            if bucket != Some(b / self.every_blocks) {
                bucket = Some(b / self.every_blocks);
                keep.insert(b);
            }
        }
        keep
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FoldsToml {
    #[serde(default)]
    fold: Vec<FoldDecl>,
    #[serde(default)]
    retention: Retention,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FoldDecl {
    name: String,
    key: KeyDecl,
    carry: Vec<String>,
    max_rows: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum KeyDecl {
    Columns(Vec<String>),
    Word(String),
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `10-deployment_signal.sql` is the fold `deployment_signal`; the prefix only orders files, as in
/// `views/`.
fn fold_name_of(file: &str) -> Option<&str> {
    let stem = file.strip_suffix(".sql")?;
    Some(match stem.split_once('-') {
        Some((prefix, rest))
            if !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit()) =>
        {
            rest
        }
        _ => stem,
    })
}

impl FoldSet {
    /// Load and validate `folds/`. An absent directory is an empty set. Every problem is a refusal.
    pub fn load(dir: &Path, schema: &[TableSchema]) -> Result<FoldSet> {
        let root = dir.join(FOLDS_DIR);
        if !root.exists() {
            return Ok(FoldSet::default());
        }
        let raw = std::fs::read_to_string(root.join(FOLDS_TOML))
            .with_context(|| format!("{FOLDS_DIR}/ needs {FOLDS_DIR}/{FOLDS_TOML}"))?;
        let decls: FoldsToml =
            toml::from_str(&raw).with_context(|| format!("parsing {FOLDS_DIR}/{FOLDS_TOML}"))?;
        let retention = decls.retention.clone();
        // `check --folds` resumes from the checkpoint before the latest, so two must always be kept.
        if retention.recent < 2 || retention.every_blocks == 0 {
            bail!("{FOLDS_DIR}/{FOLDS_TOML}: [retention] needs recent >= 2 and every_blocks >= 1");
        }

        let mut files: Vec<String> = std::fs::read_dir(&root)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|f| f.ends_with(".sql"))
            .collect();
        files.sort();

        let mut by_name: BTreeMap<String, String> = BTreeMap::new();
        for file in &files {
            let name = fold_name_of(file).unwrap_or_default();
            if !is_identifier(name) {
                bail!("{FOLDS_DIR}/{file}: `{name}` is not a fold name (lowercase letters, digits, _)");
            }
            if let Some(other) = by_name.insert(name.to_string(), file.clone()) {
                bail!("{FOLDS_DIR}/{file} and {FOLDS_DIR}/{other} both define the fold `{name}`");
            }
        }
        let mut decl_by_name: BTreeMap<String, FoldDecl> = BTreeMap::new();
        for d in decls.fold {
            if !by_name.contains_key(&d.name) {
                bail!(
                    "{FOLDS_DIR}/{FOLDS_TOML} declares `{}`, which has no .sql file in {FOLDS_DIR}/",
                    d.name
                );
            }
            let name = d.name.clone();
            if decl_by_name.insert(name.clone(), d).is_some() {
                bail!("{FOLDS_DIR}/{FOLDS_TOML} declares `{name}` twice");
            }
        }

        let binder = analytics::FoldBinder::open(dir)?;
        let surface = analytics::nest_relation_names(dir, schema)?;
        let view_bodies = analytics::nest_view_bodies(dir);
        // Bind only what the folds read. What a fold reaches is known from its parse alone; a fold
        // whose reach cannot be worked out is refused by `load_one` below.
        let mut wanted = BTreeSet::new();
        for file in &files {
            let sql = std::fs::read_to_string(root.join(file))?;
            let Some(refs) = binder.base_tables(&sql) else {
                continue;
            };
            let direct: BTreeSet<String> = refs
                .into_iter()
                .filter(|t| !by_name.contains_key(t.strip_suffix("__carry").unwrap_or(t)))
                .collect();
            wanted.extend(binder.reachable(dir, &direct).unwrap_or_default());
        }
        binder.bind(dir, schema, &wanted)?;

        let mut set = FoldSet {
            retention,
            ..FoldSet::default()
        };
        for file in &files {
            let name = fold_name_of(file).unwrap_or_default().to_string();
            let at = format!("{FOLDS_DIR}/{file}");
            let decl = decl_by_name
                .remove(&name)
                .with_context(|| format!("{at} has no [[fold]] in {FOLDS_DIR}/{FOLDS_TOML}"))?;
            let sql = std::fs::read_to_string(root.join(file))?;
            let fold = load_one(
                &binder,
                dir,
                &surface,
                &view_bodies,
                &set,
                &at,
                name,
                decl,
                sql,
            )?;
            // Later folds bind against this one at `hi`, and against its carry.
            binder.execute(&format!(
                "CREATE TABLE \"{0}\" AS {1}; CREATE TABLE \"{0}__carry\" AS {1};",
                fold.name,
                empty_relation(&fold.carry)
            ))?;
            for w in &fold.warnings {
                tracing::warn!("{w}");
            }
            set.folds.push(Fold {
                file: file.clone(),
                ..fold
            });
        }
        Ok(set)
    }
}

/// Walks a fold set forward one window at a time: facts in `(lo, hi]`, each fold's output at `lo` as
/// its carry. The first step starts from genesis with empty carries.
pub struct Stepper<'a> {
    set: &'a FoldSet,
    eval: analytics::FoldEvaluator,
    wanted: BTreeSet<String>,
    /// The fact tables among `wanted`: what a window's inputs are digested over.
    facts: BTreeSet<String>,
    dir: std::path::PathBuf,
    /// Where the last window started, so a checkpoint knows its predecessor and its inputs.
    from: Option<u64>,
    at: Option<u64>,
}

impl FoldSet {
    pub fn stepper<'a>(&'a self, dir: &Path, schema: &[TableSchema]) -> Result<Stepper<'a>> {
        let wanted: BTreeSet<String> = self.folds.iter().flat_map(|f| f.reaches.clone()).collect();
        let views = analytics::nest_view_bodies(dir);
        let facts = wanted
            .iter()
            .filter(|t| !views.contains_key(*t))
            .cloned()
            .collect();
        Ok(Stepper {
            set: self,
            eval: analytics::FoldEvaluator::new(dir, schema)?,
            wanted,
            facts,
            dir: dir.to_path_buf(),
            from: None,
            at: None,
        })
    }
}

impl Stepper<'_> {
    /// The block the folds were last evaluated at, or `None` before the first step.
    pub fn at(&self) -> Option<u64> {
        self.at
    }

    /// Evaluate every fold at `hi` from its state at the previous step. All or nothing: a refused
    /// step leaves every fold as it was at the previous step, so a retry cannot fold a window twice.
    pub fn step_to(
        &mut self,
        hot: &analytics::HotRows,
        sealed_through: u64,
        hi: u64,
    ) -> Result<()> {
        if let Some(lo) = self.at {
            if hi <= lo {
                bail!("a fold steps forward: {hi} is not after {lo}");
            }
        }
        self.eval.begin()?;
        match self.advance(hot, sealed_through, hi) {
            Ok(()) => {
                self.eval.commit()?;
                self.from = self.at;
                self.at = Some(hi);
                Ok(())
            }
            Err(e) => {
                self.eval.rollback()?;
                Err(e)
            }
        }
    }

    /// Evaluate every fold at `hi`, run `read` over the result, then discard it: the stepper stays
    /// where it was. This is a head read (RFC-0059 §4), one window from the last step every time.
    pub fn probe<R>(
        &mut self,
        hot: &analytics::HotRows,
        sealed_through: u64,
        hi: u64,
        read: impl FnOnce(&Self) -> Result<R>,
    ) -> Result<R> {
        if let Some(lo) = self.at {
            if hi <= lo {
                bail!("a probe reads ahead: {hi} is not after {lo}");
            }
        }
        self.eval.begin()?;
        let out = self
            .advance(hot, sealed_through, hi)
            .and_then(|()| read(self));
        self.eval.rollback()?;
        out
    }

    fn advance(&mut self, hot: &analytics::HotRows, sealed_through: u64, hi: u64) -> Result<()> {
        for f in &self.set.folds {
            let carry = match self.at {
                Some(_) => format!("SELECT * FROM \"{}\"", f.name),
                None => empty_relation(&f.carry),
            };
            self.eval.execute(&format!(
                "CREATE OR REPLACE TABLE \"{}__carry\" AS {carry}",
                f.name
            ))?;
        }
        self.eval
            .bind_window(hot, sealed_through, self.at, hi, &self.wanted)?;
        for f in &self.set.folds {
            let step = format!("__step_{}", f.name);
            self.eval
                .execute(&format!("CREATE OR REPLACE TABLE \"{step}\" AS {}", f.sql))
                .with_context(|| format!("evaluating fold `{}` at {hi}", f.name))?;
            // A keyed fold emits the keys its window touched; every other key passes through as it was.
            let output = match &f.key {
                FoldKey::Columns(cols) => {
                    let same: Vec<String> = cols
                        .iter()
                        .map(|c| format!("s.\"{c}\" IS NOT DISTINCT FROM c.\"{c}\""))
                        .collect();
                    format!(
                        "SELECT * FROM \"{step}\" UNION ALL SELECT * FROM \"{0}__carry\" c \
                         WHERE NOT EXISTS (SELECT 1 FROM \"{step}\" s WHERE {1})",
                        f.name,
                        same.join(" AND ")
                    )
                }
                FoldKey::Singleton | FoldKey::Unkeyed => format!("SELECT * FROM \"{step}\""),
            };
            self.eval.execute(&format!(
                "CREATE OR REPLACE TABLE \"{}\" AS {output}",
                f.name
            ))?;
            let rows = self.eval.count(&f.name)?;
            if rows > f.max_rows {
                bail!(
                    "fold `{}` holds {rows} rows at {hi}, over its declared max_rows {}",
                    f.name,
                    f.max_rows
                );
            }
        }
        Ok(())
    }

    /// A fold's state at the last step, ordered by every column so it compares deterministically.
    pub fn rows(&self, fold: &str) -> Result<Vec<serde_json::Value>> {
        let f = self
            .set
            .folds
            .iter()
            .find(|f| f.name == fold)
            .with_context(|| format!("no fold `{fold}`"))?;
        let order: Vec<String> = (1..=f.carry.len()).map(|i| i.to_string()).collect();
        self.eval.rows(&format!(
            "SELECT * FROM \"{fold}\" ORDER BY {}",
            order.join(", ")
        ))
    }
}

pub const CHECKPOINTS_DIR: &str = "checkpoints";
const CHECKPOINT_LOG: &str = "manifest.json";

/// One fold's state at `block`, as a content address of its definition and every sealed fact up to
/// `block` (RFC-0059 §5). Identity rests on inputs and logical rows, never on Parquet bytes: a
/// provisional segment folded into a wider one (#1150) changes the catalogue and not one row a
/// window read, so the inputs are the rows themselves, digested per table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub block: u64,
    /// `H(fold_hash, id(prev), the rows of each fact table in the window (prev, block])`.
    pub id: String,
    pub prev: Option<u64>,
    pub rows: u64,
    /// Order-independent digest of the rows: the sum, mod 2^256, of each row's sha256.
    pub row_digest: String,
    /// Every fact table the fold reaches that had a row in `(prev, block]`, in name order.
    pub inputs: Vec<WindowInput>,
    /// Retention removed this checkpoint's file. The entry stays: later ids chain through it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pruned: bool,
}

/// The rows one fact table contributed to a window, digested as a checkpoint's own rows are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInput {
    pub table: String,
    pub rows: u64,
    pub digest: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CheckpointLog {
    fold: String,
    fold_hash: String,
    checkpoints: Vec<Checkpoint>,
}

fn checkpoint_dir(dir: &Path, fold: &Fold) -> std::path::PathBuf {
    dir.join(CHECKPOINTS_DIR).join(&fold.hash)
}

fn load_log(dir: &Path, fold: &Fold) -> Result<CheckpointLog> {
    let path = checkpoint_dir(dir, fold).join(CHECKPOINT_LOG);
    if !path.exists() {
        return Ok(CheckpointLog {
            fold: fold.name.clone(),
            fold_hash: fold.hash.clone(),
            checkpoints: Vec::new(),
        });
    }
    let log: CheckpointLog = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("reading {}", path.display()))?;
    if log.fold_hash != fold.hash {
        bail!("{} belongs to a different fold definition", path.display());
    }
    Ok(log)
}

/// Write `bytes` to `path` so a crash leaves either the old file or the new one, never half of one.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// The carry columns, in carry order: what a digest covers. Load already requires the output to be
/// exactly the carry, but the digest should not depend on that holding.
fn carry_select(fold: &Fold) -> String {
    let cols: Vec<String> = fold.carry.iter().map(|(c, _)| format!("\"{c}\"")).collect();
    format!("SELECT {} FROM \"{}\"", cols.join(", "), fold.name)
}

/// The digest by materialising every row: the reference the streamed one must match exactly.
#[cfg(test)]
fn row_digest(fold: &Fold, rows: &[serde_json::Value]) -> String {
    let mut sum = [0u8; 32];
    for row in rows {
        let cells: Vec<&serde_json::Value> =
            fold.carry.iter().map(|(c, _)| &row[c.as_str()]).collect();
        let h: [u8; 32] = Sha256::digest(serde_json::to_vec(&cells).expect("json")).into();
        let mut carry = 0u16;
        for i in (0..32).rev() {
            let s = sum[i] as u16 + h[i] as u16 + carry;
            sum[i] = s as u8;
            carry = s >> 8;
        }
    }
    hex::encode(sum)
}

fn checkpoint_id(fold: &Fold, prev: Option<&Checkpoint>, inputs: &[WindowInput]) -> String {
    let mut h = Sha256::new();
    h.update(fold.hash.as_bytes());
    match prev {
        Some(p) => h.update(p.id.as_bytes()),
        None => h.update(b"genesis"),
    }
    for i in inputs {
        h.update(i.table.as_bytes());
        h.update([0]);
        h.update(i.rows.to_le_bytes());
        h.update(i.digest.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

/// How many leading checkpoints of a log form a chain: each names the one before it and recomputes
/// to its recorded id. Whatever follows the first broken link is not history anyone can resume from,
/// however well its rows match their digest.
fn verified_prefix(fold: &Fold, log: &CheckpointLog) -> usize {
    let mut prev: Option<&Checkpoint> = None;
    for (i, c) in log.checkpoints.iter().enumerate() {
        if c.prev != prev.map(|p| p.block) || c.id != checkpoint_id(fold, prev, &c.inputs) {
            return i;
        }
        prev = Some(c);
    }
    log.checkpoints.len()
}

/// The first checkpoint in a log that does not chain, and why.
fn chain_break(fold: &Fold, log: &CheckpointLog) -> Option<(u64, &'static str)> {
    let ok = verified_prefix(fold, log);
    let c = log.checkpoints.get(ok)?;
    let why = if c.prev != log.checkpoints[..ok].last().map(|p| p.block) {
        "the checkpoint does not follow the one before it"
    } else {
        "the checkpoint does not recompute to its recorded id"
    };
    Some((c.block, why))
}

fn verify_chain(fold: &Fold, log: &CheckpointLog, block: u64) -> Result<()> {
    if let Some((b, why)) = chain_break(fold, log).filter(|(b, _)| *b <= block) {
        bail!("fold `{}` at {b}: {why}", fold.name);
    }
    let ok = verified_prefix(fold, log);
    if !log.checkpoints[..ok].iter().any(|c| c.block == block) {
        bail!("fold `{}` has no checkpoint at {block}", fold.name);
    }
    Ok(())
}

impl Stepper<'_> {
    /// What the last window fed `fold`: each reached fact table's rows in it, digested. Tables with no
    /// row in the window are left out, so the answer does not depend on which tables the catalogue
    /// happens to list yet.
    fn window_inputs(&self, fold: &Fold) -> Result<Vec<WindowInput>> {
        let mut out = Vec::new();
        for t in &fold.reaches {
            if !self.facts.contains(t) {
                continue;
            }
            let (rows, digest) = self.eval.digest(&format!("SELECT * FROM \"{t}\""))?;
            if rows > 0 {
                out.push(WindowInput {
                    table: t.clone(),
                    rows,
                    digest,
                });
            }
        }
        Ok(out)
    }

    /// Write every fold's current state as a checkpoint at the block it was last evaluated at.
    pub fn checkpoint(&self) -> Result<()> {
        let at = self
            .at
            .context("nothing to checkpoint before the first step")?;
        for f in &self.set.folds {
            let mut log = load_log(&self.dir, f)?;
            if log.checkpoints.iter().any(|c| c.block == at) {
                continue;
            }
            let prev = log.checkpoints.last();
            if prev.map(|p| p.block) != self.from {
                bail!(
                    "fold `{}`: the last checkpoint is at {:?}, but this step started from {:?}",
                    f.name,
                    prev.map(|p| p.block),
                    self.from
                );
            }
            let inputs = self.window_inputs(f)?;
            let (rows, row_digest) = self.eval.digest(&carry_select(f))?;
            let entry = Checkpoint {
                block: at,
                id: checkpoint_id(f, prev, &inputs),
                prev: self.from,
                rows,
                row_digest,
                inputs,
                pruned: false,
            };
            let cdir = checkpoint_dir(&self.dir, f);
            std::fs::create_dir_all(&cdir)?;
            let path = cdir.join(format!("{at}.parquet"));
            let tmp = cdir.join(format!("{at}.parquet.tmp"));
            self.eval.execute(&format!(
                "COPY (SELECT * FROM \"{}\" ORDER BY ALL) TO '{}' (FORMAT parquet)",
                f.name,
                tmp.display().to_string().replace('\'', "''")
            ))?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            // The Parquet file is in place before the log names it; an unlisted file is never read.
            log.checkpoints.push(entry);
            write_atomically(
                &cdir.join(CHECKPOINT_LOG),
                &serde_json::to_vec_pretty(&log)?,
            )?;
        }
        Ok(())
    }

    /// Load every fold's checkpoint at `block` and continue from there. A checkpoint whose rows no
    /// longer match its digest is refused.
    pub fn resume(&mut self, block: u64) -> Result<()> {
        if self.at.is_some() {
            bail!("resume a fresh stepper only");
        }
        for f in &self.set.folds {
            let log = load_log(&self.dir, f)?;
            let entry = log
                .checkpoints
                .iter()
                .find(|c| c.block == block)
                .with_context(|| format!("fold `{}` has no checkpoint at {block}", f.name))?;
            if entry.pruned {
                bail!(
                    "fold `{}`: the checkpoint at {block} was removed by retention",
                    f.name
                );
            }
            verify_chain(f, &log, block)?;
            let path = checkpoint_dir(&self.dir, f).join(format!("{block}.parquet"));
            let cols: Vec<String> = f
                .carry
                .iter()
                .map(|(c, t)| format!("CAST(\"{c}\" AS {t}) AS \"{c}\""))
                .collect();
            self.eval.execute(&format!(
                "CREATE OR REPLACE TABLE \"{}\" AS SELECT {} FROM read_parquet('{}')",
                f.name,
                cols.join(", "),
                path.display().to_string().replace('\'', "''")
            ))?;
            let (rows, digest) = self.eval.digest(&carry_select(f))?;
            if rows != entry.rows || digest != entry.row_digest {
                bail!(
                    "fold `{}`: the checkpoint at {block} does not match its recorded digest",
                    f.name
                );
            }
        }
        self.at = Some(block);
        Ok(())
    }
}

impl FoldSet {
    /// The latest block every fold has a checkpoint at, at or before `n`.
    pub fn latest_checkpoint(&self, dir: &Path, n: u64) -> Result<Option<u64>> {
        let mut common: Option<BTreeSet<u64>> = None;
        for f in &self.folds {
            let blocks: BTreeSet<u64> = load_log(dir, f)?
                .checkpoints
                .iter()
                .filter(|c| !c.pruned)
                .map(|c| c.block)
                .filter(|b| *b <= n)
                .collect();
            common = Some(match common {
                None => blocks,
                Some(c) => c.intersection(&blocks).copied().collect(),
            });
        }
        Ok(common.and_then(|c| c.last().copied()))
    }

    /// Bring every fold's log back to history a writer can continue from, and say where that is.
    ///
    /// A writer killed between one fold's log and the next leaves the folds disagreeing about the
    /// latest checkpoint; a crash inside a log write leaves a link that does not chain. Each log is
    /// cut back to its verified prefix, then every log to the latest block all of them still share,
    /// and the Parquet files nothing names any more go. What is left is exactly what a fresh walk
    /// would have written, so the next step continues from it.
    pub fn reconcile(&self, dir: &Path) -> Result<Option<u64>> {
        let mut logs: Vec<(usize, CheckpointLog)> = Vec::new();
        for (i, f) in self.folds.iter().enumerate() {
            let mut log = load_log(dir, f)?;
            log.checkpoints.truncate(verified_prefix(f, &log));
            logs.push((i, log));
        }
        let mut common: Option<BTreeSet<u64>> = None;
        for (_, log) in &logs {
            let blocks: BTreeSet<u64> = log.checkpoints.iter().map(|c| c.block).collect();
            common = Some(match common {
                None => blocks,
                Some(c) => c.intersection(&blocks).copied().collect(),
            });
        }
        let latest = common.and_then(|c| c.last().copied());
        for (i, mut log) in logs {
            let f = &self.folds[i];
            log.checkpoints
                .retain(|c| latest.is_some_and(|l| c.block <= l));
            let cdir = checkpoint_dir(dir, f);
            if !cdir.exists() {
                continue;
            }
            let recorded = load_log(dir, f)?;
            if recorded.checkpoints != log.checkpoints {
                tracing::warn!(
                    "fold `{}`: {} checkpoint(s) after {:?} did not survive a crash; rebuilding them",
                    f.name,
                    recorded.checkpoints.len() - log.checkpoints.len(),
                    latest
                );
                write_atomically(
                    &cdir.join(CHECKPOINT_LOG),
                    &serde_json::to_vec_pretty(&log)?,
                )?;
            }
            let named: BTreeSet<String> = log
                .checkpoints
                .iter()
                .filter(|c| !c.pruned)
                .map(|c| format!("{}.parquet", c.block))
                .collect();
            for entry in std::fs::read_dir(&cdir)?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if (name.ends_with(".parquet") && !named.contains(&name)) || name.ends_with(".tmp")
                {
                    std::fs::remove_file(entry.path())?;
                }
            }
        }
        Ok(latest)
    }

    /// Remove the checkpoint files retention no longer keeps. The log is written first, so a crash
    /// between the two leaves an unnamed file for `reconcile`, never a named one that is gone.
    pub fn prune(&self, dir: &Path) -> Result<usize> {
        let mut removed = 0;
        for f in &self.folds {
            let mut log = load_log(dir, f)?;
            let blocks: Vec<u64> = log.checkpoints.iter().map(|c| c.block).collect();
            let keep = self.retention.keep(&blocks);
            let mut gone = Vec::new();
            for c in log.checkpoints.iter_mut().filter(|c| !c.pruned) {
                if !keep.contains(&c.block) {
                    c.pruned = true;
                    gone.push(c.block);
                }
            }
            if gone.is_empty() {
                continue;
            }
            let cdir = checkpoint_dir(dir, f);
            write_atomically(
                &cdir.join(CHECKPOINT_LOG),
                &serde_json::to_vec_pretty(&log)?,
            )?;
            for b in gone {
                match std::fs::remove_file(cdir.join(format!("{b}.parquet"))) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(removed)
    }

    /// Where a walk from `start` to `sealed_through` checkpoints: at the end of about every
    /// `window_rows` sealed rows the folds read, and at the last sealed block they read.
    pub fn cuts(
        &self,
        dir: &Path,
        start: Option<u64>,
        sealed_through: u64,
        window_rows: u64,
    ) -> Result<Vec<u64>> {
        let reached: BTreeSet<String> = self.folds.iter().flat_map(|f| f.reaches.clone()).collect();
        let manifest = crate::seal::load_manifest_with_hash(dir)?.0;
        let mut ends: Vec<(u64, usize)> = manifest
            .tables
            .iter()
            .filter(|(t, _)| reached.contains(&t.to_ascii_lowercase()))
            .flat_map(|(_, segs)| segs.iter().map(|s| (s.to_block, s.rows)))
            .filter(|(to, _)| *to <= sealed_through && start.is_none_or(|s| *to > s))
            .collect();
        ends.sort();
        let mut cuts = Vec::new();
        let mut acc = 0u64;
        for (i, (to, rows)) in ends.iter().enumerate() {
            acc += *rows as u64;
            let last_at_block = ends.get(i + 1).is_none_or(|(next, _)| next != to);
            if acc >= window_rows && last_at_block {
                cuts.push(*to);
                acc = 0;
            }
        }
        if let Some((to, _)) = ends.last() {
            if cuts.last() != Some(to) {
                cuts.push(*to);
            }
        }
        Ok(cuts)
    }

    /// Walk sealed history from the latest checkpoint to `sealed_through`, checkpointing every fold
    /// each time about `window_rows` sealed rows have been read. One window in memory at a time, and
    /// no RPC. Returns the blocks checkpointed.
    pub fn build(
        &self,
        dir: &Path,
        schema: &[TableSchema],
        sealed_through: u64,
        window_rows: u64,
    ) -> Result<Vec<u64>> {
        let start = self.reconcile(dir)?.filter(|b| *b <= sealed_through);
        let cuts = self.cuts(dir, start, sealed_through, window_rows)?;
        let mut s = self.stepper(dir, schema)?;
        if let Some(b) = start {
            s.resume(b)?;
        }
        let empty = analytics::HotRows::new();
        for &cut in &cuts {
            s.step_to(&empty, sealed_through, cut)?;
            s.checkpoint()?;
            self.prune(dir)?;
        }
        Ok(cuts)
    }

    /// `nuthatch check --folds`: recompute checkpoints from sealed facts and compare them with what
    /// the logs record, window inputs and rows alike. The last window from its predecessor, or every
    /// window from genesis. Reads only; a writer holding the nest is not disturbed. Returns one line
    /// per checkpoint that does not recompute, empty when every one does.
    pub fn verify(
        &self,
        dir: &Path,
        schema: &[TableSchema],
        from_genesis: bool,
    ) -> Result<Vec<String>> {
        let mut logs = Vec::new();
        let mut common: Option<BTreeSet<u64>> = None;
        for f in &self.folds {
            let log = load_log(dir, f)?;
            let blocks: BTreeSet<u64> = log.checkpoints.iter().map(|c| c.block).collect();
            common = Some(match common {
                None => blocks,
                Some(c) => c.intersection(&blocks).copied().collect(),
            });
            logs.push(log);
        }
        let blocks: Vec<u64> = common.unwrap_or_default().into_iter().collect();
        if blocks.is_empty() {
            bail!("no checkpoint every fold shares; nothing to verify");
        }
        // Rows that recompute say nothing about the links between checkpoints, and a broken link is
        // named here rather than tripped over by the resume below.
        let broken: Vec<String> = self
            .folds
            .iter()
            .zip(&logs)
            .filter_map(|(f, log)| {
                chain_break(f, log).map(|(b, why)| format!("fold `{}` at {b}: {why}", f.name))
            })
            .collect();
        if !broken.is_empty() {
            return Ok(broken);
        }
        // Every checkpointed block is sealed, so the sealed ceiling the walk needs is the catalogue's.
        let manifest = crate::seal::load_manifest_with_hash(dir)?.0;
        let sealed_through = manifest
            .tables
            .values()
            .flatten()
            .map(|s| s.to_block)
            .max()
            .unwrap_or(0);
        let mut s = self.stepper(dir, schema)?;
        let walk: &[u64] = if from_genesis {
            &blocks
        } else {
            let n = blocks.len();
            if n > 1 {
                s.resume(blocks[n - 2])?;
            }
            &blocks[n - 1..]
        };
        let empty = analytics::HotRows::new();
        let mut out = Vec::new();
        for &b in walk {
            s.step_to(&empty, sealed_through, b)?;
            for (f, log) in self.folds.iter().zip(&logs) {
                let recorded = log
                    .checkpoints
                    .iter()
                    .find(|c| c.block == b)
                    .expect("a shared block is in every log");
                let inputs = s.window_inputs(f)?;
                if inputs != recorded.inputs {
                    out.push(format!(
                        "fold `{}` at {b}: the window read other rows than the checkpoint recorded",
                        f.name
                    ));
                }
                let (rows, digest) = s.eval.digest(&carry_select(f))?;
                if rows != recorded.rows || digest != recorded.row_digest {
                    out.push(format!(
                        "fold `{}` at {b}: recomputed to {rows} row(s), digest {digest}; the \
                         checkpoint records {} row(s), digest {}",
                        f.name, recorded.rows, recorded.row_digest
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Every fold's state at `n`: the latest checkpoint at or before it, stepped forward over the
    /// facts in between. Without one, from genesis.
    pub fn read_at<'a>(
        &'a self,
        dir: &Path,
        schema: &[TableSchema],
        hot: &analytics::HotRows,
        sealed_through: u64,
        n: u64,
    ) -> Result<Stepper<'a>> {
        let c = self.latest_checkpoint(dir, n)?;
        if c.is_none() {
            // Stepping from genesis is exact, but past a removed checkpoint it is exactly the
            // history the horizon was set to stop paying for.
            for f in &self.folds {
                let log = load_log(dir, f)?;
                if log.checkpoints.iter().any(|c| c.pruned && c.block <= n) {
                    let oldest = log.checkpoints.iter().find(|c| !c.pruned).map(|c| c.block);
                    bail!(
                        "block {n} predates retained history for fold `{}` (oldest checkpoint: {})",
                        f.name,
                        oldest.map_or("none".to_string(), |b| b.to_string())
                    );
                }
            }
        }
        let mut s = self.stepper(dir, schema)?;
        if let Some(b) = c {
            s.resume(b)?;
        }
        if c != Some(n) {
            s.step_to(hot, sealed_through, n)?;
        }
        Ok(s)
    }
}

/// Sealed rows a walk reads before it checkpoints, when nothing narrower is asked for.
pub const DEFAULT_WINDOW_ROWS: u64 = 2_000_000;

/// What the seal loop's writer is doing (RFC-0059 §5), for `/ready` and `/metrics`.
#[derive(Debug, Default)]
pub struct WriterStatus {
    /// Latest block every fold is checkpointed at; 0 before the first.
    checkpointed_through: std::sync::atomic::AtomicU64,
    /// The last sealed block the folds have rows through: what the writer is working towards.
    target: std::sync::atomic::AtomicU64,
    /// Unix seconds a checkpoint was last written; 0 until one has been.
    last_progress: std::sync::atomic::AtomicU64,
    fault: std::sync::Mutex<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterSnapshot {
    pub checkpointed_through: u64,
    pub target: u64,
    /// Sealed blocks whose rows no fold is checkpointed through yet.
    pub lag_blocks: u64,
    pub last_progress: u64,
    pub fault: Option<String>,
}

impl WriterStatus {
    pub fn snapshot(&self) -> WriterSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        let checkpointed_through = self.checkpointed_through.load(Relaxed);
        let target = self.target.load(Relaxed);
        WriterSnapshot {
            checkpointed_through,
            target,
            lag_blocks: target.saturating_sub(checkpointed_through),
            last_progress: self.last_progress.load(Relaxed),
            fault: self.fault.lock().unwrap().clone(),
        }
    }
}

/// The seal loop's checkpoint writer: one thread per nest, told each time `sealed_through` advances,
/// stepping every fold from its last checkpoint to the new watermark and writing the result. It
/// runs beside the seal, never inside it: a slow or failed checkpoint delays no seal, and a read
/// meanwhile is exact over a longer window. A writer that fails stays failed, visibly, until the
/// nest restarts; the next start reconciles the logs and continues.
pub struct Writer {
    tx: Option<std::sync::mpsc::Sender<u64>>,
    thread: Option<std::thread::JoinHandle<()>>,
    status: std::sync::Arc<WriterStatus>,
    /// The highest watermark handed to the thread, so an unchanged one costs nothing per poll.
    told: std::sync::atomic::AtomicU64,
}

impl Writer {
    /// Start the writer for `dir`, catching up to `sealed_through` first. `None` when the nest has no
    /// `folds/`. `metrics` mirrors the lag for Prometheus.
    pub fn start(
        dir: std::path::PathBuf,
        schema: Vec<TableSchema>,
        sealed_through: u64,
        metrics: std::sync::Arc<crate::metrics::NestMetrics>,
    ) -> Result<Option<Writer>> {
        if !dir.join(FOLDS_DIR).exists() {
            return Ok(None);
        }
        let status = std::sync::Arc::new(WriterStatus::default());
        let (tx, rx) = std::sync::mpsc::channel::<u64>();
        let worker = status.clone();
        let thread = std::thread::Builder::new()
            .name("fold-writer".into())
            .spawn(move || {
                if let Err(e) = write_loop(&dir, &schema, rx, &worker, &metrics) {
                    tracing::error!("fold writer stopped: {e:#}");
                    *worker.fault.lock().unwrap() = Some(format!("{e:#}"));
                    metrics.set_fold_writer_faulted(true);
                }
            })?;
        let w = Writer {
            tx: Some(tx),
            thread: Some(thread),
            status,
            told: std::sync::atomic::AtomicU64::new(0),
        };
        w.sealed_through_advanced(sealed_through);
        Ok(Some(w))
    }

    /// The tip loop calls this after every seal pass with the current watermark. Only a higher
    /// value than last time reaches the thread.
    pub fn sealed_through_advanced(&self, sealed_through: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        if sealed_through == 0 || self.told.fetch_max(sealed_through, Relaxed) >= sealed_through {
            return;
        }
        if let Some(tx) = &self.tx {
            // A closed channel is a writer that has already failed and said so.
            let _ = tx.send(sealed_through);
        }
    }

    pub fn status(&self) -> WriterSnapshot {
        self.status.snapshot()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn write_loop(
    dir: &Path,
    schema: &[TableSchema],
    rx: std::sync::mpsc::Receiver<u64>,
    status: &WriterStatus,
    metrics: &crate::metrics::NestMetrics,
) -> Result<()> {
    use std::sync::atomic::Ordering::Relaxed;
    let set = FoldSet::load(dir, schema)?;
    let start = set.reconcile(dir)?;
    let mut s = set.stepper(dir, schema)?;
    if let Some(b) = start {
        s.resume(b)?;
        status.checkpointed_through.store(b, Relaxed);
    }
    let empty = analytics::HotRows::new();
    while let Ok(mut sealed_through) = rx.recv() {
        // Several advances while a window was being folded collapse into the latest.
        while let Ok(later) = rx.try_recv() {
            sealed_through = sealed_through.max(later);
        }
        let cuts = set.cuts(dir, s.at(), sealed_through, DEFAULT_WINDOW_ROWS)?;
        if let Some(&target) = cuts.last() {
            status.target.store(target, Relaxed);
            metrics.set_fold_checkpoint_lag_blocks(
                target.saturating_sub(status.checkpointed_through.load(Relaxed)),
            );
        }
        for cut in cuts {
            s.step_to(&empty, sealed_through, cut)?;
            s.checkpoint()?;
            set.prune(dir)?;
            status.checkpointed_through.store(cut, Relaxed);
            status
                .last_progress
                .store(crate::metrics::now_unix(), Relaxed);
            metrics.set_fold_checkpoint_lag_blocks(status.target.load(Relaxed).saturating_sub(cut));
            tracing::debug!("folds checkpointed at {cut}");
        }
    }
    Ok(())
}

/// `nuthatch fold ...`. Opens the store itself, so it refuses while `dev` holds the nest, whose own
/// writer (S2) would otherwise be a second one.
pub fn run(cmd: crate::cli::FoldCommand) -> Result<()> {
    use crate::cli::FoldCommand;
    let mut phases = Phases::default();
    phases.mark("start");
    let dir = match &cmd {
        FoldCommand::Build { dir, .. }
        | FoldCommand::Read { dir, .. }
        | FoldCommand::Bench { dir, .. } => std::path::PathBuf::from(dir),
    };
    let dir = dir.as_path();
    let store = crate::store::Store::open_existing(&dir.join(crate::config::DB_FILE))
        .context("opening the nest's store (is `dev` running on it?)")?;
    phases.mark("store opened");
    let sealed_through = store.sealed_through();
    let set = FoldSet::load(dir, &[])?;
    phases.mark("folds loaded");
    if set.folds.is_empty() {
        bail!("{} has no folds/", dir.display());
    }
    match cmd {
        FoldCommand::Build { window_rows, .. } => {
            let cuts = set.build(dir, &[], sealed_through, window_rows)?;
            match (cuts.first(), cuts.last()) {
                (Some(a), Some(b)) => {
                    println!(
                        "checkpointed {} fold(s) at {} block(s), {a}..={b}",
                        set.folds.len(),
                        cuts.len()
                    )
                }
                _ => println!("up to date through {sealed_through}"),
            }
        }
        FoldCommand::Read { fold, at, .. } => {
            let hot = store.hot_rows_by_table()?;
            let s = set.read_at(dir, &[], &hot, sealed_through, at)?;
            for row in s.rows(&fold)? {
                println!("{row}");
            }
        }
        FoldCommand::Bench { iters, .. } => {
            let hot = store.hot_rows_by_table()?;
            phases.mark("hot rows read");
            println!(
                "{}",
                bench(dir, &set, &hot, sealed_through, iters, &mut phases)?
            );
        }
    }
    Ok(())
}

/// Peak resident memory since the last reset, and current resident memory, in KiB. Linux only: the
/// S1 gate is measured on the ThinkPad, and a 120 ms RSS poll can miss a 150 ms peak.
fn memory_kib() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
    };
    Some((field("VmHWM:")?, field("VmRSS:")?))
}

/// Resident and peak memory at named points, so a fixed cost can be told apart from evaluation.
#[derive(Default)]
struct Phases(Vec<serde_json::Value>);

impl Phases {
    fn mark(&mut self, at: &str) {
        if let Some((hwm, rss)) = memory_kib() {
            self.0.push(serde_json::json!({
                "at": at,
                "rss_mib": rss as f64 / 1024.0,
                "peak_mib": hwm as f64 / 1024.0,
            }));
        }
    }
}

fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// RFC-0059 S1's gate: head evaluation in one warm process, measured per the 2026-09-24 ruling. The
/// latest checkpoint is resumed once; each hot block is then evaluated from it and discarded.
fn bench(
    dir: &Path,
    set: &FoldSet,
    hot: &analytics::HotRows,
    sealed_through: u64,
    iters: usize,
    phases: &mut Phases,
) -> Result<serde_json::Value> {
    if iters == 0 {
        bail!("--iters must be at least 1");
    }
    let from = set
        .latest_checkpoint(dir, sealed_through)?
        .context("no checkpoint to resume; run `nuthatch fold build` first")?;
    let reached: BTreeSet<String> = set.folds.iter().flat_map(|f| f.reaches.clone()).collect();
    let mut heads: Vec<u64> = hot
        .iter()
        .filter(|(t, _)| reached.contains(&t.to_ascii_lowercase()))
        .flat_map(|(_, rows)| rows.iter().filter_map(|r| r.get("block_number")?.as_u64()))
        .filter(|b| *b > from)
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();
    if heads.is_empty() {
        heads.push(sealed_through.max(from + 1));
    }
    let mut s = set.stepper(dir, &[])?;
    s.resume(from)?;
    phases.mark("checkpoint resumed");
    let counts = |s: &Stepper| -> Result<u64> {
        set.folds
            .iter()
            .map(|f| s.eval.count(&f.name))
            .sum::<Result<u64>>()
    };
    // Warm-up: the first evaluation pays for loading extensions and binding views, once per process.
    s.probe(hot, sealed_through, heads[0], counts)?;
    phases.mark("warmed up");

    let mut wall = Vec::with_capacity(iters);
    let (mut peak_kib, mut delta_kib) = (0u64, 0u64);
    for i in 0..iters {
        let head = heads[i % heads.len()];
        reset_peak();
        let base = memory_kib().map(|(_, rss)| rss);
        let started = std::time::Instant::now();
        s.probe(hot, sealed_through, head, counts)?;
        wall.push(started.elapsed().as_secs_f64() * 1000.0);
        if let (Some((hwm, _)), Some(base)) = (memory_kib(), base) {
            peak_kib = peak_kib.max(hwm);
            delta_kib = delta_kib.max(hwm.saturating_sub(base));
        }
    }
    wall.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| wall[((wall.len() as f64 * p).ceil() as usize).clamp(1, wall.len()) - 1];
    Ok(serde_json::json!({
        "folds": set.folds.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
        "checkpoint": from,
        "heads": { "count": heads.len(), "first": heads.first(), "last": heads.last() },
        "evaluations": wall.len(),
        "wall_ms": { "p50": pct(0.50), "p99": pct(0.99), "max": wall.last() },
        "peak_rss_mib": (peak_kib > 0).then(|| peak_kib as f64 / 1024.0),
        "peak_increase_mib": (peak_kib > 0).then(|| delta_kib as f64 / 1024.0),
        "targets": { "p99_ms": 500, "peak_rss_mib": 300 },
        "phases": phases.0,
    }))
}

fn empty_relation(cols: &[(String, String)]) -> String {
    let select: Vec<String> = cols
        .iter()
        .map(|(c, t)| format!("CAST(NULL AS {t}) AS \"{c}\""))
        .collect();
    format!("SELECT {} WHERE false", select.join(", "))
}

#[allow(clippy::too_many_arguments)]
fn load_one(
    binder: &analytics::FoldBinder,
    dir: &Path,
    surface: &BTreeSet<String>,
    view_bodies: &BTreeMap<String, String>,
    earlier: &FoldSet,
    at: &str,
    name: String,
    decl: FoldDecl,
    sql: String,
) -> Result<Fold> {
    let carry_name = format!("{name}__carry");
    for taken in [&name, &carry_name] {
        if surface.contains(taken) {
            bail!("{at}: `{taken}` is already a table or view in this nest");
        }
    }

    // Exactly one SELECT, parsed. SQL that will not parse cannot be checked for volatility, so it is
    // refused rather than waved through.
    let kinds = binder
        .statement_kinds(&sql)
        .with_context(|| at.to_string())?;
    if !matches!(
        kinds.as_slice(),
        [k] if k == "SELECT_NODE" || k == "SET_OPERATION_NODE"
    ) {
        bail!("{at}: a fold is exactly one SELECT");
    }
    analytics::reject_file_access(&sql).with_context(|| at.to_string())?;
    analytics::reject_replacement_scan(&sql).with_context(|| at.to_string())?;

    // What it reads. A fold whose inputs cannot be enumerated cannot be content-addressed.
    let referenced = binder
        .base_tables(&sql)
        .with_context(|| format!("{at}: cannot enumerate the tables it reads"))?;
    let mut deps = BTreeSet::new();
    let mut direct = BTreeSet::new();
    for t in &referenced {
        let base = t.strip_suffix("__carry").unwrap_or(t);
        if base == name {
            if t == &name {
                bail!("{at}: reads itself; its previous state is `{carry_name}`");
            }
            continue;
        }
        if earlier.folds.iter().any(|f| f.name == base) {
            deps.insert(base.to_string());
            continue;
        }
        direct.insert(t.clone());
    }
    let reaches = binder.reachable(dir, &direct).with_context(|| {
        format!(
            "{at}: cannot enumerate what it reads through views \
             (factory `__children` views are not bound inside a fold)"
        )
    })?;
    for t in &reaches {
        if t.starts_with("offchain__") || t == "labels" {
            bail!("{at}: reads `{t}`, which is not bound inside a fold");
        }
        if !surface.contains(t) {
            bail!("{at}: reads `{t}`, which is neither a table, a view nor an earlier fold");
        }
    }

    // Volatility, in the fold and in every view it reaches: a fold over a volatile view is volatile.
    let mut sources = vec![(String::from("it"), sql.clone())];
    for t in &reaches {
        if let Some(body) = view_bodies.get(t) {
            sources.push((format!("view `{t}`"), body.clone()));
        }
    }
    for (what, text) in &sources {
        if let Some(r) = binder.refusals(text).first() {
            bail!("{at}: {what} {r}; a checkpoint must be reproducible");
        }
    }

    // A reached view that ranks, recurses or looks up earlier rows was written over the whole
    // history and gives a window-only answer inside a fold, with nothing else to say so (#1504). The
    // fold's own body is exempt: `QUALIFY row_number() OVER` over carry ∪ window is the idiom.
    let mut warnings = Vec::new();
    for t in &reaches {
        if let Some(body) = view_bodies.get(t) {
            for how in binder.lookbacks(body, &reaches) {
                warnings.push(format!(
                    "{at}: view `{t}` uses {how}, which inside a fold sees only the window \
                     (RFC-0059 §3: history is not in scope inside a fold)"
                ));
            }
        }
    }

    let carry = declared_carry(binder, at, &decl)?;
    let key = match decl.key {
        KeyDecl::Word(w) if w == "singleton" => FoldKey::Singleton,
        KeyDecl::Word(w) if w == "unkeyed" => FoldKey::Unkeyed,
        KeyDecl::Word(w) => {
            bail!("{at}: key is a list of columns, \"singleton\" or \"unkeyed\", not \"{w}\"")
        }
        KeyDecl::Columns(cols) => {
            if cols.is_empty() {
                bail!("{at}: an empty key; say \"singleton\" or \"unkeyed\"");
            }
            for c in &cols {
                if !carry.iter().any(|(n, _)| n == c) {
                    bail!("{at}: key column `{c}` is not in the carry");
                }
            }
            FoldKey::Columns(cols)
        }
    };
    if decl.max_rows == 0 {
        bail!("{at}: max_rows must be at least 1");
    }

    // The output is the carry: same names, same types, same order. Bound with its own carry in place.
    binder.execute(&format!(
        "CREATE OR REPLACE TEMP VIEW \"{carry_name}\" AS {}",
        empty_relation(&carry)
    ))?;
    let output = binder
        .describe(&sql)
        .with_context(|| format!("{at}: does not bind"))?;
    binder.execute(&format!("DROP VIEW \"{carry_name}\""))?;
    if output != carry {
        bail!("{at}: {}", schema_mismatch(&carry, &output));
    }

    // Identity: the plan, the declaration, every view and fact schema it reads, the folds it reads,
    // the engine, and the scalar set a graph build registers. `max_rows` changes no output.
    // A fact table counts by its declared columns, never by what the binder sees: a table bound
    // before its first seal describes differently from one bound over Parquet, and a fold's identity
    // must not depend on whether anything had sealed when the process started.
    let declared: BTreeMap<String, Vec<(String, String)>> = analytics::declared_columns(dir)
        .into_iter()
        .map(|(t, mut cols)| {
            cols.sort();
            (t.to_ascii_lowercase(), cols)
        })
        .collect();
    let mut h = Sha256::new();
    let mut part = |label: &str, text: &str| {
        h.update(label.as_bytes());
        h.update((text.len() as u64).to_le_bytes());
        h.update(text.as_bytes());
    };
    part("v", "nuthatch-fold-v2");
    part("plan", &binder.plan_text(&sql));
    part("key", &format!("{key:?}"));
    part("carry", &format!("{carry:?}"));
    for t in &reaches {
        match view_bodies.get(t) {
            Some(body) => part(&format!("view:{t}"), &binder.plan_text(body)),
            None => {
                let cols = declared.get(t).with_context(|| {
                    format!(
                        "{at}: reads `{t}`, which schema.json does not declare, so its checkpoints \
                         could not name the schema they were built from; run `nuthatch schema`"
                    )
                })?;
                part(&format!("table:{t}"), &format!("{cols:?}"))
            }
        }
    }
    for d in &deps {
        let dep = earlier
            .folds
            .iter()
            .find(|f| &f.name == d)
            .expect("a dep is an earlier fold");
        part(&format!("fold:{d}"), &dep.hash);
    }
    part("engine", &binder.engine_version());
    part(
        "scalars",
        if cfg!(feature = "graph") {
            "graph"
        } else {
            "core"
        },
    );
    let hash = hex::encode(h.finalize());

    Ok(Fold {
        name,
        file: String::new(),
        sql,
        key,
        carry,
        max_rows: decl.max_rows,
        reaches,
        deps,
        hash,
        warnings,
    })
}

fn declared_carry(
    binder: &analytics::FoldBinder,
    at: &str,
    decl: &FoldDecl,
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for spec in &decl.carry {
        let (col, ty) = spec
            .trim()
            .split_once(char::is_whitespace)
            .with_context(|| format!("{at}: carry entry `{spec}` is not `<column> <TYPE>`"))?;
        if !is_identifier(col) {
            bail!("{at}: carry column `{col}` is not a plain lowercase name");
        }
        if out.iter().any(|(c, _)| c == col) {
            bail!("{at}: carry column `{col}` is declared twice");
        }
        // DuckDB's own spelling, so `INT` and `INTEGER` compare equal.
        let canonical = binder
            .describe(&format!("SELECT CAST(NULL AS {}) AS c", ty.trim()))
            .with_context(|| format!("{at}: carry column `{col}` has an unknown type `{ty}`"))?
            .remove(0)
            .1;
        let allowed = CARRY_TYPES.contains(&canonical.as_str())
            || canonical
                .strip_prefix("DECIMAL(")
                .and_then(|r| r.split(',').next())
                .and_then(|p| p.parse::<u32>().ok())
                .is_some_and(|p| p <= 38);
        if !allowed {
            bail!(
                "{at}: carry column `{col}` is {canonical}, which a checkpoint cannot hold exactly; \
                 carry big integers as VARCHAR and cast them in the step"
            );
        }
        out.push((col.to_string(), canonical));
    }
    if out.is_empty() {
        bail!("{at}: an empty carry");
    }
    Ok(out)
}

fn schema_mismatch(carry: &[(String, String)], output: &[(String, String)]) -> String {
    let mut parts = Vec::new();
    for (c, t) in carry {
        match output.iter().find(|(o, _)| o == c) {
            None => parts.push(format!("missing `{c}`")),
            Some((_, ot)) if ot != t => parts.push(format!("`{c}` is {ot}, the carry says {t}")),
            _ => {}
        }
    }
    for (o, _) in output {
        if !carry.iter().any(|(c, _)| c == o) {
            parts.push(format!("extra `{o}`"));
        }
    }
    if parts.is_empty() {
        let order: Vec<&str> = output.iter().map(|(o, _)| o.as_str()).collect();
        parts.push(format!("columns are in the order {}", order.join(", ")));
    }
    format!("output is not its carry: {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const SCHEMA: &str = r#"{"tables":[{"table":"t","columns":[{"name":"block_number","storage":"u64"},{"name":"k","storage":"varchar"},{"name":"v","storage":"varchar"}]}]}"#;

    fn nest(folds: &[(&str, &str)], decls: &str, views: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), SCHEMA).unwrap();
        std::fs::create_dir_all(dir.path().join("folds")).unwrap();
        for (file, sql) in folds {
            std::fs::write(dir.path().join("folds").join(file), sql).unwrap();
        }
        std::fs::write(dir.path().join("folds/folds.toml"), decls).unwrap();
        if !views.is_empty() {
            std::fs::create_dir_all(dir.path().join("views")).unwrap();
            for (file, sql) in views {
                std::fs::write(dir.path().join("views").join(file), sql).unwrap();
            }
        }
        dir
    }

    fn refusal(folds: &[(&str, &str)], decls: &str, views: &[(&str, &str)]) -> String {
        let dir = nest(folds, decls, views);
        format!(
            "{:#}",
            FoldSet::load(dir.path(), &[]).expect_err("must be refused")
        )
    }

    const COUNT: &str = "SELECT CAST(count(*) AS UBIGINT) AS n FROM t";
    const COUNT_DECL: &str =
        "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n";

    #[test]
    fn a_valid_set_loads_in_name_order_with_its_dependencies() {
        let dir = nest(
            &[
                ("20-latest.sql", "SELECT k, v FROM t UNION ALL SELECT k, v FROM latest__carry"),
                (
                    "10-running.sql",
                    "SELECT CAST(count(*) + coalesce((SELECT max(n) FROM running__carry), 0) AS UBIGINT) AS n FROM t",
                ),
                ("30-copy.sql", "SELECT n FROM running"),
            ],
            "[[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 10\n\
             [[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
             [[fold]]\nname = \"copy\"\nkey = \"unkeyed\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
            &[],
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let names: Vec<&str> = set.folds.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["running", "latest", "copy"]);
        assert_eq!(set.folds[0].file, "10-running.sql");
        assert_eq!(set.folds[1].key, FoldKey::Columns(vec!["k".into()]));
        assert_eq!(set.folds[2].deps, BTreeSet::from(["running".to_string()]));
        assert_eq!(set.folds[0].reaches, BTreeSet::from(["t".to_string()]));
    }

    #[test]
    fn a_volatile_fold_is_refused_and_so_is_one_over_a_volatile_view() {
        for (sql, function) in [
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n FROM t WHERE now() IS NOT NULL",
                "now",
            ),
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n FROM t WHERE random() < 2",
                "random",
            ),
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n FROM t WHERE current_date IS NOT NULL",
                "current_date",
            ),
        ] {
            let msg = refusal(&[("c.sql", sql)], COUNT_DECL, &[]);
            assert!(
                msg.contains(function) && msg.contains("reproducible"),
                "{msg}"
            );
        }
        let msg = refusal(
            &[("c.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM noisy")],
            COUNT_DECL,
            &[(
                "10-noisy.sql",
                "CREATE VIEW noisy AS SELECT * FROM t WHERE random() < 2;",
            )],
        );
        assert!(
            msg.contains("view `noisy`") && msg.contains("random"),
            "{msg}"
        );
    }

    #[test]
    fn a_view_that_looks_back_into_history_is_warned_by_name_and_construct() {
        const FOLD: &str = "SELECT CAST(count(*) AS UBIGINT) AS n FROM h";
        for (view, construct) in [
            (
                "CREATE VIEW h AS SELECT k, v, row_number() OVER (PARTITION BY k ORDER BY block_number) AS rn FROM t;",
                "a window function (`row_number() OVER`)",
            ),
            (
                "CREATE VIEW h AS WITH RECURSIVE chain AS (SELECT k, v, block_number FROM t UNION ALL SELECT k, v, block_number + 1 FROM chain WHERE block_number < 0) SELECT k, v FROM chain;",
                "a recursive CTE (`chain`)",
            ),
            (
                "CREATE VIEW h AS SELECT a.k, a.v FROM t a WHERE EXISTS (SELECT 1 FROM t b WHERE b.k = a.k AND b.block_number < a.block_number);",
                "an EXISTS subquery over `t`",
            ),
            (
                "CREATE VIEW h AS SELECT a.k, (SELECT min(b.block_number) FROM t b WHERE b.k = a.k) AS first_seen FROM t a;",
                "a scalar subquery over `t`",
            ),
        ] {
            let dir = nest(&[("c.sql", FOLD)], COUNT_DECL, &[("10-h.sql", view)]);
            let set = FoldSet::load(dir.path(), &[]).unwrap();
            let w = set.folds[0].warnings.join("\n");
            assert!(
                w.contains("folds/c.sql: view `h` uses ")
                    && w.contains(construct)
                    && w.contains("RFC-0059 §3"),
                "{view}\n{w}"
            );
            assert_eq!(set.folds[0].warnings.len(), 1, "{w}");
        }

        // Reached through another view, the warning still names the view that looks back.
        let dir = nest(
            &[("c.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM o")],
            COUNT_DECL,
            &[
                (
                    "10-h.sql",
                    "CREATE VIEW h AS SELECT k, row_number() OVER (ORDER BY block_number) AS rn FROM t;",
                ),
                ("20-o.sql", "CREATE VIEW o AS SELECT k FROM h;"),
            ],
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let w = set.folds[0].warnings.join("\n");
        assert!(w.contains("view `h` uses a window function"), "{w}");
        assert!(!w.contains("view `o`"), "{w}");

        // A view that only projects each event is silent, and so is the fold's own ranking over
        // carry ∪ window, which is the idiom rather than a lookback.
        let dir = nest(
            &[(
                "c.sql",
                "SELECT k, v FROM (SELECT k, v FROM p UNION ALL SELECT k, v FROM c__carry) \
                 QUALIFY row_number() OVER (PARTITION BY k ORDER BY v DESC) = 1",
            )],
            "[[fold]]\nname = \"c\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 10\n",
            &[("10-p.sql", "CREATE VIEW p AS SELECT k, upper(v) AS v FROM t;")],
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        assert!(
            set.folds[0].warnings.is_empty(),
            "{:?}",
            set.folds[0].warnings
        );
    }

    #[test]
    fn an_output_that_is_not_its_carry_is_refused_by_what_differs() {
        let decl = "[[fold]]\nname = \"c\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"n UBIGINT\"]\nmax_rows = 9\n";
        for (sql, expect) in [
            (
                "SELECT k, CAST(count(*) AS BIGINT) AS n FROM t GROUP BY k",
                "`n` is BIGINT, the carry says UBIGINT",
            ),
            ("SELECT k FROM t", "missing `n`"),
            (
                "SELECT k, CAST(count(*) AS UBIGINT) AS n, 1 AS x FROM t GROUP BY k",
                "extra `x`",
            ),
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n, k FROM t GROUP BY k",
                "in the order n, k",
            ),
        ] {
            let msg = refusal(&[("c.sql", sql)], decl, &[]);
            assert!(msg.contains(expect), "{sql}: {msg}");
        }
    }

    #[test]
    fn a_carry_type_a_checkpoint_cannot_hold_exactly_is_refused() {
        for ty in ["HUGEINT", "DOUBLE", "VARCHAR[]"] {
            let decl = format!(
                "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n {ty}\"]\nmax_rows = 1\n"
            );
            let msg = refusal(&[("c.sql", COUNT)], &decl, &[]);
            assert!(msg.contains("cannot hold exactly"), "{ty}: {msg}");
        }
    }

    #[test]
    fn names_order_and_declarations_are_checked() {
        let fwd = "[[fold]]\nname = \"a\"\nkey = \"unkeyed\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
                   [[fold]]\nname = \"b\"\nkey = \"unkeyed\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n";
        let msg = refusal(
            &[("10-a.sql", "SELECT n FROM b"), ("20-b.sql", COUNT)],
            fwd,
            &[],
        );
        assert!(msg.contains("`b`") && msg.contains("earlier fold"), "{msg}");

        let clash = COUNT_DECL.replace("\"c\"", "\"t\"");
        let msg = refusal(&[("t.sql", COUNT)], &clash, &[]);
        assert!(msg.contains("already a table or view"), "{msg}");

        let msg = refusal(&[("c.sql", COUNT), ("d.sql", COUNT)], COUNT_DECL, &[]);
        assert!(msg.contains("d.sql has no [[fold]]"), "{msg}");

        let msg = refusal(
            &[("c.sql", COUNT)],
            &format!("{COUNT_DECL}{}", COUNT_DECL.replace("\"c\"", "\"e\"")),
            &[],
        );
        assert!(msg.contains("`e`, which has no .sql file"), "{msg}");

        let msg = refusal(
            &[("c.sql", "SELECT 1 AS n; SELECT 2 AS n")],
            COUNT_DECL,
            &[],
        );
        assert!(
            msg.contains("exactly one SELECT") || msg.contains("parse"),
            "{msg}"
        );
    }
}

#[cfg(test)]
mod stepping_support {
    use super::*;
    use serde_json::json;

    /// Blocks 1..=30 sealed as three segments, 31..=35 hot. Key `k` cycles a, b, c; `v` is the block.
    pub(super) fn corpus() -> (tempfile::TempDir, analytics::HotRows) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), super::tests::SCHEMA).unwrap();
        crate::seal::test_set_table_floor(dir.path(), 0);
        let row = |b: u64| {
            let k = ["a", "b", "c"][(b % 3) as usize];
            json!({"table": "t", "block_number": b, "k": k, "v": b.to_string()})
        };
        for (from, to) in [(1, 10), (11, 20), (21, 30)] {
            let rows: Vec<String> = (from..=to).map(|b| row(b).to_string()).collect();
            crate::seal::seal_range(dir.path(), &rows, from, to).unwrap();
        }
        let mut hot = analytics::HotRows::new();
        hot.insert("t".into(), (31..=35).map(row).collect());
        (dir, hot)
    }

    pub(super) fn fold_files(dir: &Path, folds: &[(&str, &str)], decls: &str) {
        std::fs::create_dir_all(dir.join("folds")).unwrap();
        for (file, sql) in folds {
            std::fs::write(dir.join("folds").join(file), sql).unwrap();
        }
        std::fs::write(dir.join("folds/folds.toml"), decls).unwrap();
    }
}

#[cfg(test)]
mod stepping {
    use super::stepping_support::*;
    use super::*;

    fn n(rows: &[serde_json::Value]) -> u64 {
        rows[0]["n"].as_u64().unwrap()
    }

    /// RFC-0059 S1's first criterion: `count(*)` over a fact table inside a fold counts the window,
    /// not history, while a fold that adds its carry reaches the history total.
    #[test]
    fn a_fold_sees_its_window_and_its_carry_holds_the_rest() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[
                ("10-window.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM t"),
                (
                    "20-running.sql",
                    "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
                ),
            ],
            "[[fold]]\nname = \"window\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
             [[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        // Cuts inside a segment (15, 25) and across the sealed/hot boundary (33).
        for (hi, window, running) in [(15, 15, 15), (25, 10, 25), (33, 8, 33), (35, 2, 35)] {
            s.step_to(&hot, 30, hi).unwrap();
            assert_eq!(n(&s.rows("window").unwrap()), window, "window at {hi}");
            assert_eq!(n(&s.rows("running").unwrap()), running, "running at {hi}");
        }
        assert!(
            s.step_to(&hot, 30, 35).is_err(),
            "a fold never steps backwards or in place"
        );
    }

    /// A keyed fold emits only the keys its window touched; untouched keys keep their carried rows.
    #[test]
    fn a_keyed_fold_passes_untouched_keys_through() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[(
                "latest.sql",
                "SELECT k, v FROM t QUALIFY row_number() OVER (PARTITION BY k ORDER BY block_number DESC) = 1",
            )],
            "[[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 3\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        s.step_to(&hot, 30, 30).unwrap();
        // (30, 31] touches only block 31, key b.
        s.step_to(&hot, 30, 31).unwrap();
        let got: Vec<(String, String)> = s
            .rows("latest")
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["k"].as_str().unwrap().into(),
                    r["v"].as_str().unwrap().into(),
                )
            })
            .collect();
        let want =
            [("a", "30"), ("b", "31"), ("c", "29")].map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(got, want);
    }

    /// A step refused part-way leaves every fold at the previous step. Otherwise an earlier fold has
    /// already advanced, and a retry folds the same window into it twice.
    #[test]
    fn a_refused_step_changes_nothing_and_a_retry_does_not_double_count() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[
                (
                    "10-running.sql",
                    "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
                ),
                // Blocks 1, 2, 3 bring keys b, c, a: the third step breaks max_rows.
                ("20-keys.sql", "SELECT DISTINCT k FROM t"),
            ],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
             [[fold]]\nname = \"keys\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\"]\nmax_rows = 2\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        s.step_to(&hot, 30, 1).unwrap();
        s.step_to(&hot, 30, 2).unwrap();
        for _ in 0..2 {
            assert!(s.step_to(&hot, 30, 3).is_err());
            assert_eq!(s.at(), Some(2));
            assert_eq!(
                n(&s.rows("running").unwrap()),
                2,
                "running advanced on a refused step"
            );
        }
    }

    /// The views a first step defines roll back with it, so a retry has to define them again.
    #[test]
    fn a_refused_first_step_leaves_the_views_a_retry_needs() {
        let (dir, hot) = corpus();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-recent.sql"),
            "CREATE VIEW recent AS SELECT * FROM t;",
        )
        .unwrap();
        fold_files(
            dir.path(),
            &[
                ("10-running.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM recent"),
                ("20-keys.sql", "SELECT DISTINCT k FROM t"),
            ],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
             [[fold]]\nname = \"keys\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        for attempt in 0..2 {
            let err = format!("{:#}", s.step_to(&hot, 30, 30).unwrap_err());
            assert!(err.contains("max_rows"), "attempt {attempt}: {err}");
        }
    }

    #[test]
    fn a_fold_over_its_max_rows_is_refused() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[("keys.sql", "SELECT DISTINCT k FROM t")],
            "[[fold]]\nname = \"keys\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\"]\nmax_rows = 2\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        let err = s.step_to(&hot, 30, 30).unwrap_err();
        assert!(
            format!("{err:#}").contains("over its declared max_rows 2"),
            "{err:#}"
        );
    }
}

#[cfg(test)]
mod short_windows {
    use super::stepping_support::*;
    use super::*;

    /// `/sql` answers short and flags it; a fold step would checkpoint the short answer as the truth.
    #[test]
    fn a_window_missing_sealed_data_is_refused() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[("c.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM t")],
            "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let manifest = crate::seal::load_manifest_with_hash(dir.path()).unwrap().0;
        let seg = manifest.tables["t"]
            .iter()
            .find(|s| s.from_block == 11)
            .unwrap();
        std::fs::remove_file(crate::seal::segment_path(dir.path(), &seg.file, &seg.hash)).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        let err = s.step_to(&hot, 30, 25).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing sealed data for t"),
            "{err:#}"
        );
    }
}

#[cfg(test)]
mod checkpoints {
    use super::stepping_support::*;
    use super::*;

    const FOLDS: &[(&str, &str)] = &[
        (
            "10-running.sql",
            "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
        ),
        (
            "20-latest.sql",
            "SELECT k, v FROM t QUALIFY row_number() OVER (PARTITION BY k ORDER BY block_number DESC) = 1",
        ),
    ];
    const DECLS: &str = "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
                         [[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 3\n";

    fn nest() -> (tempfile::TempDir, analytics::HotRows, FoldSet) {
        let (dir, hot) = corpus();
        fold_files(dir.path(), FOLDS, DECLS);
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        (dir, hot, set)
    }

    fn state(s: &Stepper) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        (s.rows("running").unwrap(), s.rows("latest").unwrap())
    }

    /// The same facts in one window from genesis: what every other path must agree with.
    fn one_window(
        dir: &Path,
        set: &FoldSet,
        hot: &analytics::HotRows,
        n: u64,
    ) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let mut s = set.stepper(dir, &[]).unwrap();
        s.step_to(hot, 30, n).unwrap();
        state(&s)
    }

    #[test]
    fn a_read_from_checkpoints_equals_one_window_from_genesis() {
        let (dir, hot, set) = nest();
        assert_eq!(set.build(dir.path(), &[], 30, 10).unwrap(), [10, 20, 30]);
        for n in [5, 10, 15, 25, 30, 31, 33, 35] {
            let s = set.read_at(dir.path(), &[], &hot, 30, n).unwrap();
            assert_eq!(state(&s), one_window(dir.path(), &set, &hot, n), "at {n}");
        }
        // A second build finds nothing new to do.
        assert!(set.build(dir.path(), &[], 30, 10).unwrap().is_empty());
    }

    /// RFC-0059 §8: compared at the first event after every cut, not only at the end, because a fold
    /// heals itself and an end-state check misses a carry that was wrong in between.
    #[test]
    fn partitions_agree_at_the_first_event_after_every_cut() {
        let mut ends = Vec::new();
        for cuts in [[7, 19, 26, 30], [3, 15, 29, 30], [12, 22, 28, 30]] {
            let (dir, hot, set) = nest();
            let mut s = set.stepper(dir.path(), &[]).unwrap();
            for c in cuts {
                s.step_to(&hot, 30, c).unwrap();
                s.checkpoint().unwrap();
            }
            for c in cuts {
                let probe = c + 1;
                let read = set.read_at(dir.path(), &[], &hot, 30, probe).unwrap();
                assert_eq!(
                    state(&read),
                    one_window(dir.path(), &set, &hot, probe),
                    "partition {cuts:?}, first event after {c}"
                );
            }
            let log = load_log(dir.path(), &set.folds[1]).unwrap();
            ends.push(log.checkpoints.last().unwrap().row_digest.clone());
        }
        assert!(ends.windows(2).all(|w| w[0] == w[1]), "{ends:?}");
    }

    #[test]
    fn a_checkpoint_that_no_longer_matches_its_digest_is_refused() {
        let (dir, hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let cdir = checkpoint_dir(dir.path(), &set.folds[1]);
        std::fs::copy(cdir.join("10.parquet"), cdir.join("20.parquet")).unwrap();
        let err = set
            .read_at(dir.path(), &[], &hot, 30, 25)
            .err()
            .expect("refused");
        assert!(
            format!("{err:#}").contains("does not match its recorded digest"),
            "{err:#}"
        );
    }

    #[test]
    fn a_checkpoint_file_the_log_does_not_name_is_never_read() {
        let (dir, hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        for f in &set.folds {
            let cdir = checkpoint_dir(dir.path(), f);
            // A stray file at 25, holding the state at 10, and a half-written one.
            std::fs::copy(cdir.join("10.parquet"), cdir.join("25.parquet")).unwrap();
            std::fs::write(cdir.join("27.parquet.tmp"), b"half").unwrap();
        }
        assert_eq!(set.latest_checkpoint(dir.path(), 27).unwrap(), Some(20));
        let s = set.read_at(dir.path(), &[], &hot, 30, 27).unwrap();
        assert_eq!(state(&s), one_window(dir.path(), &set, &hot, 27));
    }

    #[test]
    fn checkpoints_chain_and_record_what_each_window_read() {
        let (dir, _hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let log = load_log(dir.path(), &set.folds[0]).unwrap();
        let blocks: Vec<(Option<u64>, u64)> =
            log.checkpoints.iter().map(|c| (c.prev, c.block)).collect();
        assert_eq!(blocks, [(None, 10), (Some(10), 20), (Some(20), 30)]);
        // Each window read ten rows of `t` and nothing else, and the ids chain.
        assert!(log
            .checkpoints
            .iter()
            .all(|c| c.inputs.len() == 1 && c.inputs[0].table == "t" && c.inputs[0].rows == 10));
        assert_ne!(
            log.checkpoints[0].inputs[0].digest,
            log.checkpoints[1].inputs[0].digest
        );
        let mut prev = None;
        for c in &log.checkpoints {
            assert_eq!(c.id, checkpoint_id(&set.folds[0], prev, &c.inputs));
            prev = Some(c);
        }
    }

    /// A writer dies between one fold's log and the next, or inside a log write. The next build
    /// cuts every log back to what all folds share, drops the files nothing names, and rewrites the
    /// missing checkpoints exactly as they were.
    #[test]
    fn a_writer_killed_mid_checkpoint_is_repaired_by_the_next_build() {
        let (dir, _hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let intact: Vec<CheckpointLog> = set
            .folds
            .iter()
            .map(|f| load_log(dir.path(), f).unwrap())
            .collect();
        // `running` logged 30; `latest` wrote its file and died before its log named it.
        let cdir = checkpoint_dir(dir.path(), &set.folds[1]);
        let mut log = load_log(dir.path(), &set.folds[1]).unwrap();
        log.checkpoints.pop();
        std::fs::write(cdir.join(CHECKPOINT_LOG), serde_json::to_vec(&log).unwrap()).unwrap();
        std::fs::write(cdir.join("30.parquet.tmp"), b"half").unwrap();

        assert_eq!(set.reconcile(dir.path()).unwrap(), Some(20));
        for f in &set.folds {
            let log = load_log(dir.path(), f).unwrap();
            assert_eq!(log.checkpoints.len(), 2, "{}", f.name);
            let cdir = checkpoint_dir(dir.path(), f);
            assert!(!cdir.join("30.parquet").exists(), "{}", f.name);
            assert!(!cdir.join("30.parquet.tmp").exists());
            assert!(cdir.join("20.parquet").exists());
        }
        assert_eq!(set.build(dir.path(), &[], 30, 10).unwrap(), [30]);
        for (f, before) in set.folds.iter().zip(&intact) {
            let after = load_log(dir.path(), f).unwrap();
            assert_eq!(after.checkpoints, before.checkpoints, "{}", f.name);
        }
    }

    #[test]
    fn a_link_that_does_not_chain_is_cut_away_with_everything_after_it() {
        let (dir, _hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let mut log = load_log(dir.path(), &set.folds[0]).unwrap();
        log.checkpoints[1].id = "0".repeat(64);
        let path = checkpoint_dir(dir.path(), &set.folds[0]).join(CHECKPOINT_LOG);
        std::fs::write(path, serde_json::to_vec(&log).unwrap()).unwrap();
        assert_eq!(set.reconcile(dir.path()).unwrap(), Some(10));
        for f in &set.folds {
            assert_eq!(load_log(dir.path(), f).unwrap().checkpoints.len(), 1);
        }
        assert_eq!(set.build(dir.path(), &[], 30, 10).unwrap(), [20, 30]);
    }

    #[test]
    fn a_fold_hash_ignores_formatting_and_follows_meaning() {
        let hash_of = |folds: &[(&str, &str)]| {
            let (dir, _) = corpus();
            fold_files(dir.path(), folds, DECLS);
            let set = FoldSet::load(dir.path(), &[]).unwrap();
            (set.folds[0].hash.clone(), set.folds[1].hash.clone())
        };
        let base = hash_of(FOLDS);
        let spaced = FOLDS[0]
            .1
            .replace(" + ", "\n  +  ")
            .replace("FROM t", "FROM\n t");
        assert_eq!(hash_of(&[(FOLDS[0].0, spaced.as_str()), FOLDS[1]]), base);
        let changed = FOLDS[1].1.replace("DESC", "ASC");
        let other = hash_of(&[FOLDS[0], (FOLDS[1].0, changed.as_str())]);
        assert_eq!(other.0, base.0, "an unrelated fold keeps its identity");
        assert_ne!(other.1, base.1);
    }
}

#[cfg(test)]
mod checkpoints_are_used {
    use super::stepping_support::*;
    use super::*;

    fn nest() -> (tempfile::TempDir, analytics::HotRows, FoldSet) {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[(
                "running.sql",
                "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
            )],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        (dir, hot, set)
    }

    /// The point of a checkpoint: a read past it never touches the history before it. With the
    /// first segment gone, only a read that really starts from the checkpoint at 20 can answer.
    #[test]
    fn a_read_past_a_checkpoint_does_not_need_the_history_before_it() {
        let (dir, hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let manifest = crate::seal::load_manifest_with_hash(dir.path()).unwrap().0;
        let first = manifest.tables["t"]
            .iter()
            .find(|s| s.from_block == 1)
            .unwrap();
        std::fs::remove_file(crate::seal::segment_path(
            dir.path(),
            &first.file,
            &first.hash,
        ))
        .unwrap();
        let s = set.read_at(dir.path(), &[], &hot, 30, 25).unwrap();
        assert_eq!(s.rows("running").unwrap()[0]["n"], 25);
    }

    /// An id addresses every sealed fact up to its block, not only its last window: two histories
    /// whose last windows read the same segment still get different ids.
    #[test]
    fn a_checkpoint_id_covers_the_whole_history_not_the_last_window() {
        let mut last = Vec::new();
        for cuts in [&[10, 20, 30][..], &[20, 30][..]] {
            let (dir, hot, set) = nest();
            let mut s = set.stepper(dir.path(), &[]).unwrap();
            for &c in cuts {
                s.step_to(&hot, 30, c).unwrap();
                s.checkpoint().unwrap();
            }
            let log = load_log(dir.path(), &set.folds[0]).unwrap();
            let end = log.checkpoints.last().unwrap().clone();
            assert_eq!(
                end.inputs.len(),
                1,
                "both last windows read only the third segment"
            );
            last.push(end);
        }
        assert_eq!(last[0].row_digest, last[1].row_digest, "same state");
        assert_eq!(last[0].inputs, last[1].inputs, "same last window");
        assert_ne!(
            last[0].id, last[1].id,
            "different histories, so different ids"
        );
    }
}

#[cfg(test)]
mod cli {
    use super::stepping_support::*;
    use super::*;
    use crate::cli::FoldCommand;

    /// The command path end to end over a real store: it reads the watermark from redb, builds, then
    /// reads a block past the last checkpoint. It also holds redb's lock, like any other writer.
    #[test]
    fn fold_build_then_read_over_a_real_store() {
        let (dir, _hot) = corpus();
        fold_files(
            dir.path(),
            &[("c.sql", "SELECT CAST(coalesce((SELECT max(n) FROM c__carry), 0) + count(*) AS UBIGINT) AS n FROM t")],
            "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let db = dir.path().join(crate::config::DB_FILE);
        crate::store::Store::open(&db)
            .unwrap()
            .set_meta("sealed_through", "30")
            .unwrap();
        let d = dir.path().display().to_string();
        run(FoldCommand::Build {
            dir: d.clone(),
            window_rows: 10,
        })
        .unwrap();
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        assert_eq!(
            set.latest_checkpoint(dir.path(), u64::MAX).unwrap(),
            Some(30)
        );
        run(FoldCommand::Read {
            dir: d.clone(),
            fold: "c".into(),
            at: 25,
        })
        .unwrap();

        let _held = crate::store::Store::open(&db).unwrap();
        let err = run(FoldCommand::Build {
            dir: d,
            window_rows: 10,
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("is `dev` running"), "{err:#}");
    }
}

#[cfg(test)]
mod bench_and_probe {
    use super::stepping_support::*;
    use super::*;

    fn nest() -> (tempfile::TempDir, analytics::HotRows, FoldSet) {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[(
                "running.sql",
                "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
            )],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        (dir, hot, set)
    }

    /// A head read answers at the head and leaves the stepper where it was, so the next head is again
    /// one window from the checkpoint rather than an accumulating walk.
    #[test]
    fn a_probe_answers_at_the_head_and_changes_nothing() {
        let (dir, hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        s.resume(30).unwrap();
        for head in [33, 35, 31] {
            let n = s
                .probe(&hot, 30, head, |s| {
                    Ok(s.rows("running")?[0]["n"].as_u64().unwrap())
                })
                .unwrap();
            assert_eq!(n, head, "at {head}");
            assert_eq!(s.at(), Some(30));
            assert_eq!(s.rows("running").unwrap()[0]["n"], 30);
        }
    }

    #[test]
    fn the_bench_times_every_hot_head_from_the_checkpoint() {
        let (dir, hot, set) = nest();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let report = bench(dir.path(), &set, &hot, 30, 12, &mut Phases::default()).unwrap();
        assert_eq!(report["checkpoint"], 30);
        assert_eq!(report["heads"]["count"], 5);
        assert_eq!(report["evaluations"], 12);
        assert!(
            report["wall_ms"]["p99"].as_f64().unwrap()
                >= report["wall_ms"]["p50"].as_f64().unwrap()
        );
        assert!(bench(dir.path(), &set, &hot, 30, 0, &mut Phases::default()).is_err());
    }
}

#[cfg(test)]
mod provenance {
    use super::stepping_support::*;
    use super::*;

    fn built() -> (tempfile::TempDir, analytics::HotRows, FoldSet) {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[(
                "running.sql",
                "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
            )],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        set.build(dir.path(), &[], 30, 10).unwrap();
        (dir, hot, set)
    }

    fn edit_log(dir: &Path, set: &FoldSet, f: impl FnOnce(&mut CheckpointLog)) {
        let mut log = load_log(dir, &set.folds[0]).unwrap();
        f(&mut log);
        let path = checkpoint_dir(dir, &set.folds[0]).join(CHECKPOINT_LOG);
        std::fs::write(path, serde_json::to_vec(&log).unwrap()).unwrap();
    }

    fn refusal(dir: &Path, set: &FoldSet, hot: &analytics::HotRows) -> String {
        format!(
            "{:#}",
            set.read_at(dir, &[], hot, 30, 25).err().expect("refused")
        )
    }

    #[test]
    fn a_checkpoint_that_claims_another_predecessor_is_refused() {
        let (dir, hot, set) = built();
        edit_log(dir.path(), &set, |log| log.checkpoints[1].prev = None);
        assert!(
            refusal(dir.path(), &set, &hot).contains("does not follow"),
            "predecessor"
        );
    }

    #[test]
    fn a_checkpoint_whose_id_does_not_recompute_is_refused() {
        let (dir, hot, set) = built();
        edit_log(dir.path(), &set, |log| {
            log.checkpoints[0].id = "0".repeat(64)
        });
        assert!(refusal(dir.path(), &set, &hot).contains("recorded id"));
    }

    /// #1150 folds a table's provisional segment into its next one, so the catalogue names a
    /// different file for a window whose rows have not changed. Identity rests on the rows, so the
    /// checkpoints written before the fold still resume, and the walk continues from them.
    #[test]
    fn a_provisional_segment_folded_into_a_wider_one_leaves_the_checkpoints_valid() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), super::tests::SCHEMA).unwrap();
        crate::seal::test_set_table_floor(dir.path(), 1_000);
        let seal = |from: u64, to: u64| {
            let rows: Vec<String> = (from..=to)
                .map(|b| {
                    json!({"table": "t", "block_number": b, "k": (["a", "b", "c"][(b % 3) as usize]), "v": b.to_string()})
                        .to_string()
                })
                .collect();
            crate::seal::seal_range(dir.path(), &rows, from, to).unwrap();
        };
        seal(1, 10);
        fold_files(
            dir.path(),
            &[(
                "running.sql",
                "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
            )],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        assert_eq!(set.build(dir.path(), &[], 10, 5).unwrap(), [10]);
        let before = load_log(dir.path(), &set.folds[0]).unwrap();

        seal(11, 20);
        let segments = &crate::seal::load_manifest_with_hash(dir.path())
            .unwrap()
            .0
            .tables["t"];
        assert_eq!(
            segments
                .iter()
                .map(|s| (s.from_block, s.to_block))
                .collect::<Vec<_>>(),
            [(1, 20)],
            "the first segment was folded into the second"
        );

        let hot = analytics::HotRows::new();
        let s = set.read_at(dir.path(), &[], &hot, 20, 15).unwrap();
        assert_eq!(s.rows("running").unwrap()[0]["n"], 15);
        assert_eq!(set.build(dir.path(), &[], 20, 5).unwrap(), [20]);
        let after = load_log(dir.path(), &set.folds[0]).unwrap();
        assert_eq!(after.checkpoints[0], before.checkpoints[0]);
        assert!(set.verify(dir.path(), &[], true).unwrap().is_empty());
    }
}

#[cfg(test)]
mod verification {
    use super::stepping_support::*;
    use super::*;

    const FOLDS: &[(&str, &str)] = &[
        (
            "10-running.sql",
            "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
        ),
        (
            "20-latest.sql",
            "SELECT k, v FROM t QUALIFY row_number() OVER (PARTITION BY k ORDER BY block_number DESC) = 1",
        ),
    ];
    const DECLS: &str = "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
                         [[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 3\n";

    fn built() -> (tempfile::TempDir, FoldSet) {
        let (dir, _hot) = corpus();
        fold_files(dir.path(), FOLDS, DECLS);
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        assert_eq!(set.build(dir.path(), &[], 30, 10).unwrap(), [10, 20, 30]);
        (dir, set)
    }

    fn edit_log(dir: &Path, fold: &Fold, f: impl FnOnce(&mut CheckpointLog)) {
        let mut log = load_log(dir, fold).unwrap();
        f(&mut log);
        let path = checkpoint_dir(dir, fold).join(CHECKPOINT_LOG);
        std::fs::write(path, serde_json::to_vec(&log).unwrap()).unwrap();
    }

    #[test]
    fn check_folds_recomputes_the_last_window_and_names_what_differs() {
        let (dir, set) = built();
        assert!(set.verify(dir.path(), &[], false).unwrap().is_empty());
        // The chain still verifies with the last entry's rows misrecorded: that is what recomputing
        // the window is for.
        edit_log(dir.path(), &set.folds[1], |log| {
            log.checkpoints[2].row_digest = "0".repeat(64)
        });
        // Inputs feed the id, so a forger who changes them re-signs the link; only recomputing the
        // window can then tell.
        let fold = set.folds[0].clone();
        edit_log(dir.path(), &fold, |log| {
            log.checkpoints[2].inputs[0].rows += 1;
            let id = checkpoint_id(&fold, Some(&log.checkpoints[1]), &log.checkpoints[2].inputs);
            log.checkpoints[2].id = id;
        });
        let out = set.verify(dir.path(), &[], false).unwrap();
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(
            out[0].starts_with("fold `running` at 30: the window read other rows"),
            "{out:?}"
        );
        assert!(
            out[1].starts_with("fold `latest` at 30: recomputed to 3 row(s)"),
            "{out:?}"
        );
    }

    #[test]
    fn check_folds_from_genesis_walks_every_window() {
        let (dir, set) = built();
        assert!(set.verify(dir.path(), &[], true).unwrap().is_empty());
        edit_log(dir.path(), &set.folds[0], |log| {
            log.checkpoints[1].row_digest = "0".repeat(64)
        });
        // The last window resumes from that checkpoint, and resuming checks its rows first.
        let err = set.verify(dir.path(), &[], false).unwrap_err();
        assert!(
            format!("{err:#}").contains("does not match its recorded digest"),
            "{err:#}"
        );
        let out = set.verify(dir.path(), &[], true).unwrap();
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].starts_with("fold `running` at 20: recomputed to 1 row(s)"),
            "{out:?}"
        );
    }

    /// Every row recomputes and the chain is still forged: both modes name the link.
    #[test]
    fn check_folds_names_a_link_that_does_not_chain() {
        for from_genesis in [false, true] {
            let (dir, set) = built();
            edit_log(dir.path(), &set.folds[0], |log| {
                log.checkpoints[1].id = "0".repeat(64)
            });
            edit_log(dir.path(), &set.folds[1], |log| {
                log.checkpoints[2].prev = None
            });
            assert_eq!(
                set.verify(dir.path(), &[], from_genesis).unwrap(),
                [
                    "fold `running` at 20: the checkpoint does not recompute to its recorded id",
                    "fold `latest` at 30: the checkpoint does not follow the one before it",
                ],
                "from_genesis = {from_genesis}"
            );
        }
    }
}

#[cfg(test)]
mod streamed_digest {
    use super::stepping_support::*;
    use super::*;

    /// Checkpoints already on disk carry the materialised digest; the streamed one must equal it.
    #[test]
    fn the_streamed_digest_equals_the_materialised_one() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[(
                "latest.sql",
                "SELECT k, v FROM t QUALIFY row_number() OVER (PARTITION BY k ORDER BY block_number DESC) = 1",
            )],
            "[[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 3\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        s.step_to(&hot, 30, 33).unwrap();
        let rows = s.rows("latest").unwrap();
        assert_eq!(rows.len(), 3);
        let (n, streamed) = s.eval.digest(&carry_select(&set.folds[0])).unwrap();
        assert_eq!(n, 3);
        assert_eq!(streamed, row_digest(&set.folds[0], &rows));
    }
}

#[cfg(test)]
mod name_case {
    use super::*;

    /// Relation names compare lowercased, and a fold name can only be lowercase, so a case variant
    /// can neither be declared nor slip past a clash.
    #[test]
    fn a_fold_name_is_lowercase_and_clashes_ignore_case() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"Users","columns":[{"name":"block_number","storage":"u64"}]}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("folds")).unwrap();
        let decl = |n: &str| {
            format!("[[fold]]\nname = \"{n}\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n")
        };
        let sql = "SELECT CAST(count(*) AS UBIGINT) AS n FROM \"Users\"";
        std::fs::write(dir.path().join("folds/Users.sql"), sql).unwrap();
        std::fs::write(dir.path().join("folds/folds.toml"), decl("Users")).unwrap();
        let err = format!("{:#}", FoldSet::load(dir.path(), &[]).unwrap_err());
        assert!(err.contains("is not a fold name"), "{err}");

        std::fs::remove_file(dir.path().join("folds/Users.sql")).unwrap();
        std::fs::write(dir.path().join("folds/users.sql"), sql).unwrap();
        std::fs::write(dir.path().join("folds/folds.toml"), decl("users")).unwrap();
        let err = format!("{:#}", FoldSet::load(dir.path(), &[]).unwrap_err());
        assert!(err.contains("already a table or view"), "{err}");
    }
}

#[cfg(test)]
mod writer {
    use super::stepping_support::*;
    use super::*;

    const FOLDS: &[(&str, &str)] = &[
        (
            "10-running.sql",
            "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
        ),
        (
            "20-latest.sql",
            "SELECT k, v FROM t QUALIFY row_number() OVER (PARTITION BY k ORDER BY block_number DESC) = 1",
        ),
    ];
    const DECLS: &str = "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
                         [[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 3\n";

    fn metrics() -> std::sync::Arc<crate::metrics::NestMetrics> {
        std::sync::Arc::new(crate::metrics::NestMetrics::default())
    }

    fn settle(w: &Writer, what: impl Fn(&WriterSnapshot) -> bool) -> WriterSnapshot {
        let start = std::time::Instant::now();
        loop {
            let s = w.status();
            if what(&s) || start.elapsed() > std::time::Duration::from_secs(30) {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn logs(dir: &Path, set: &FoldSet) -> Vec<Vec<Checkpoint>> {
        set.folds
            .iter()
            .map(|f| load_log(dir, f).unwrap().checkpoints)
            .collect()
    }

    #[test]
    fn a_nest_without_folds_has_no_writer() {
        let (dir, _hot) = corpus();
        assert!(Writer::start(dir.path().into(), vec![], 30, metrics())
            .unwrap()
            .is_none());
    }

    /// The writer's checkpoints are the ones `fold build` writes over the same history, and each
    /// later watermark continues the chain rather than starting another.
    #[test]
    fn the_writer_checkpoints_each_advance_and_matches_a_build() {
        let (dir, _hot) = corpus();
        fold_files(dir.path(), FOLDS, DECLS);
        let m = metrics();
        let w = Writer::start(dir.path().into(), vec![], 20, m.clone())
            .unwrap()
            .expect("a writer");
        let s = settle(&w, |s| s.checkpointed_through == 20);
        assert_eq!(
            (s.checkpointed_through, s.lag_blocks, s.fault),
            (20, 0, None)
        );
        w.sealed_through_advanced(30);
        let s = settle(&w, |s| s.checkpointed_through == 30);
        assert_eq!(s.checkpointed_through, 30);
        assert_eq!(m.fold_checkpoint_lag_blocks(), 0);
        drop(w);

        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let written = logs(dir.path(), &set);
        assert_eq!(
            written[0]
                .iter()
                .map(|c| (c.prev, c.block))
                .collect::<Vec<_>>(),
            [(None, 20), (Some(20), 30)]
        );
        let (other, _) = corpus();
        fold_files(other.path(), FOLDS, DECLS);
        set.build(other.path(), &[], 20, DEFAULT_WINDOW_ROWS)
            .unwrap();
        set.build(other.path(), &[], 30, DEFAULT_WINDOW_ROWS)
            .unwrap();
        assert_eq!(logs(other.path(), &set), written);
        assert!(set.verify(dir.path(), &[], true).unwrap().is_empty());
    }

    /// A restarted writer resumes from the logs and continues; one killed between two folds' logs
    /// finishes the checkpoint it was writing.
    #[test]
    fn a_restarted_writer_continues_from_its_checkpoints() {
        let (dir, _hot) = corpus();
        fold_files(dir.path(), FOLDS, DECLS);
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        set.build(dir.path(), &[], 30, 10).unwrap();
        let intact = logs(dir.path(), &set);
        let cdir = checkpoint_dir(dir.path(), &set.folds[1]);
        let mut log = load_log(dir.path(), &set.folds[1]).unwrap();
        log.checkpoints.pop();
        std::fs::write(cdir.join(CHECKPOINT_LOG), serde_json::to_vec(&log).unwrap()).unwrap();

        let w = Writer::start(dir.path().into(), vec![], 30, metrics())
            .unwrap()
            .unwrap();
        let s = settle(&w, |s| s.checkpointed_through == 30);
        assert_eq!((s.checkpointed_through, s.fault), (30, None));
        drop(w);
        assert_eq!(logs(dir.path(), &set), intact);
    }

    /// A fold that fails leaves the writer failed and saying so, and the seal side is untouched:
    /// telling a failed writer about a new watermark neither blocks nor panics.
    #[test]
    fn a_writer_that_fails_says_so_and_stops() {
        let (dir, _hot) = corpus();
        fold_files(
            dir.path(),
            &[("latest.sql", FOLDS[1].1)],
            "[[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 2\n",
        );
        let m = metrics();
        let w = Writer::start(dir.path().into(), vec![], 30, m.clone())
            .unwrap()
            .unwrap();
        let s = settle(&w, |s| s.fault.is_some());
        assert!(
            s.fault
                .as_deref()
                .unwrap_or("")
                .contains("over its declared max_rows 2"),
            "{s:?}"
        );
        assert!(m.fold_writer_faulted());
        assert_eq!(s.checkpointed_through, 0);
        w.sealed_through_advanced(40);
        drop(w);
    }
}

#[cfg(test)]
mod identity {
    use super::stepping_support::*;
    use super::*;
    use serde_json::json;

    /// A fold loaded before a nest's first seal and one loaded after it are the same fold. The
    /// binder describes a table differently once Parquet backs it, so a hash taken from the binder
    /// sent a restarted writer to a second checkpoint directory.
    #[test]
    fn a_fold_hash_does_not_depend_on_whether_anything_has_sealed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), super::tests::SCHEMA).unwrap();
        crate::seal::test_set_table_floor(dir.path(), 0);
        fold_files(
            dir.path(),
            &[(
                "running.sql",
                "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
            )],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let before = FoldSet::load(dir.path(), &[]).unwrap().folds[0]
            .hash
            .clone();
        let rows: Vec<String> = (1..=10u64)
            .map(|b| {
                json!({"table": "t", "block_number": b, "k": "a", "v": b.to_string()}).to_string()
            })
            .collect();
        crate::seal::seal_range(dir.path(), &rows, 1, 10).unwrap();
        let after = FoldSet::load(dir.path(), &[]).unwrap().folds[0]
            .hash
            .clone();
        assert_eq!(before, after);
    }
}

#[cfg(test)]
mod retention {
    use super::stepping_support::*;
    use super::*;
    use serde_json::json;

    const RUNNING: &str =
        "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t";

    /// One sealed segment per block, 1..=30, so a walk checkpoints at every block.
    fn nest(retention: &str) -> (tempfile::TempDir, FoldSet) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), super::tests::SCHEMA).unwrap();
        crate::seal::test_set_table_floor(dir.path(), 0);
        for b in 1..=30u64 {
            let row = json!({"table": "t", "block_number": b, "k": "a", "v": b.to_string()});
            crate::seal::seal_range(dir.path(), &[row.to_string()], b, b).unwrap();
        }
        fold_files(
            dir.path(),
            &[("running.sql", RUNNING)],
            &format!(
                "{retention}\n[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n"
            ),
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        assert_eq!(set.build(dir.path(), &[], 30, 1).unwrap().len(), 30);
        (dir, set)
    }

    fn files(dir: &Path, set: &FoldSet) -> Vec<u64> {
        let mut out: Vec<u64> = std::fs::read_dir(checkpoint_dir(dir, &set.folds[0]))
            .unwrap()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .strip_suffix(".parquet")
                    .and_then(|b| b.parse().ok())
            })
            .collect();
        out.sort();
        out
    }

    fn n_at(dir: &Path, set: &FoldSet, n: u64) -> u64 {
        let s = set
            .read_at(dir, &[], &analytics::HotRows::new(), 30, n)
            .unwrap();
        s.rows("running").unwrap()[0]["n"].as_u64().unwrap()
    }

    #[test]
    fn the_keep_rule_takes_the_recent_and_the_first_of_each_span() {
        let r = Retention {
            recent: 2,
            every_blocks: 10,
            horizon_blocks: None,
        };
        let blocks: Vec<u64> = (1..=30).collect();
        assert_eq!(
            r.keep(&blocks).into_iter().collect::<Vec<_>>(),
            [1, 10, 20, 29, 30]
        );
        let h = Retention {
            horizon_blocks: Some(10),
            ..r
        };
        assert_eq!(
            h.keep(&blocks).into_iter().collect::<Vec<_>>(),
            [20, 29, 30]
        );
        assert!(r.keep(&[]).is_empty());
    }

    /// Thirty checkpoints, five files: storage follows the settings, every read is still exact, and
    /// the chain still verifies through the entries whose files are gone.
    #[test]
    fn retention_bounds_the_files_and_every_read_stays_exact() {
        let (dir, set) = nest("[retention]\nrecent = 2\nevery_blocks = 10\n");
        assert_eq!(files(dir.path(), &set), [1, 10, 20, 29, 30]);
        let log = load_log(dir.path(), &set.folds[0]).unwrap();
        assert_eq!(log.checkpoints.len(), 30, "every entry is kept");
        for n in 1..=30 {
            assert_eq!(n_at(dir.path(), &set, n), n, "at {n}");
        }
        assert!(set.verify(dir.path(), &[], true).unwrap().is_empty());
        assert!(set.verify(dir.path(), &[], false).unwrap().is_empty());
    }

    #[test]
    fn a_read_into_history_past_the_horizon_is_refused_by_name() {
        let (dir, set) = nest("[retention]\nrecent = 2\nevery_blocks = 10\nhorizon_blocks = 10\n");
        assert_eq!(files(dir.path(), &set), [20, 29, 30]);
        assert_eq!(n_at(dir.path(), &set, 25), 25);
        let err = set
            .read_at(dir.path(), &[], &analytics::HotRows::new(), 30, 15)
            .err()
            .expect("refused");
        assert_eq!(
            format!("{err:#}"),
            "block 15 predates retained history for fold `running` (oldest checkpoint: 20)"
        );
    }

    #[test]
    fn resuming_at_a_removed_checkpoint_says_so() {
        let (dir, set) = nest("[retention]\nrecent = 2\nevery_blocks = 10\n");
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        let err = s.resume(15).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "fold `running`: the checkpoint at 15 was removed by retention"
        );
    }

    #[test]
    fn a_retention_that_would_break_check_folds_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), super::tests::SCHEMA).unwrap();
        fold_files(
            dir.path(),
            &[("running.sql", RUNNING)],
            "[retention]\nrecent = 1\n[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let err = FoldSet::load(dir.path(), &[]).unwrap_err();
        assert!(format!("{err:#}").contains("recent >= 2"), "{err:#}");
    }
}
