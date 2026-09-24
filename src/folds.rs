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
}

#[derive(Debug, Clone, Default)]
pub struct FoldSet {
    /// In load order, which is dependency order.
    pub folds: Vec<Fold>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FoldsToml {
    #[serde(default)]
    fold: Vec<FoldDecl>,
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

        let binder = analytics::FoldBinder::new(dir, schema)?;
        let surface = binder.relations()?;
        let view_bodies = analytics::nest_view_bodies(dir);

        let mut set = FoldSet::default();
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
    dir: std::path::PathBuf,
    /// Where the last window started, so a checkpoint knows its predecessor and its inputs.
    from: Option<u64>,
    at: Option<u64>,
}

impl FoldSet {
    pub fn stepper<'a>(&'a self, dir: &Path, schema: &[TableSchema]) -> Result<Stepper<'a>> {
        Ok(Stepper {
            set: self,
            eval: analytics::FoldEvaluator::new(dir, schema)?,
            wanted: self.folds.iter().flat_map(|f| f.reaches.clone()).collect(),
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
/// `block` (RFC-0059 §5). Identity rests on inputs and logical rows, never on Parquet bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub block: u64,
    /// `H(fold_hash, id(prev), the sealed segments the window (prev, block] read)`.
    pub id: String,
    pub prev: Option<u64>,
    pub rows: u64,
    /// Order-independent digest of the rows: the sum, mod 2^256, of each row's sha256.
    pub row_digest: String,
    /// `(table, segment hash)` for every sealed segment overlapping `(prev, block]`.
    pub inputs: Vec<(String, String)>,
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

fn checkpoint_id(fold: &Fold, prev: Option<&Checkpoint>, inputs: &[(String, String)]) -> String {
    let mut h = Sha256::new();
    h.update(fold.hash.as_bytes());
    match prev {
        Some(p) => h.update(p.id.as_bytes()),
        None => h.update(b"genesis"),
    }
    for (table, seg) in inputs {
        h.update(table.as_bytes());
        h.update([0]);
        h.update(seg.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

/// Sealed segments of `tables` overlapping `(after, through]`, sorted.
fn window_inputs(
    dir: &Path,
    tables: &BTreeSet<String>,
    after: Option<u64>,
    through: u64,
) -> Result<Vec<(String, String)>> {
    let manifest = crate::seal::load_manifest_with_hash(dir)?.0;
    Ok(inputs_in(&manifest, tables, after, through))
}

fn inputs_in(
    manifest: &crate::seal::Manifest,
    tables: &BTreeSet<String>,
    after: Option<u64>,
    through: u64,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = manifest
        .tables
        .iter()
        .filter(|(t, _)| tables.contains(&t.to_ascii_lowercase()))
        .flat_map(|(t, segs)| {
            segs.iter()
                .filter(|s| after.is_none_or(|lo| s.to_block > lo) && s.from_block <= through)
                .map(|s| (t.clone(), s.hash.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort();
    out
}

/// Walk a fold's log from genesis to `block`: every link must name its predecessor, recompute to its
/// id, and have read exactly the sealed segments the catalogue now holds for its window. A checkpoint
/// whose history cannot be shown is not resumed from, however well its rows match their digest.
fn verify_chain(dir: &Path, fold: &Fold, log: &CheckpointLog, block: u64) -> Result<()> {
    let manifest = crate::seal::load_manifest_with_hash(dir)?.0;
    let mut prev: Option<&Checkpoint> = None;
    for c in log.checkpoints.iter().take_while(|c| c.block <= block) {
        let why = if c.prev != prev.map(|p| p.block) {
            Some("does not follow the checkpoint before it")
        } else if c.inputs != inputs_in(&manifest, &fold.reaches, c.prev, c.block) {
            Some("read segments the catalogue no longer holds for its window")
        } else if c.id != checkpoint_id(fold, prev, &c.inputs) {
            Some("does not recompute to its recorded id")
        } else {
            None
        };
        if let Some(why) = why {
            bail!("fold `{}`: the checkpoint at {} {why}", fold.name, c.block);
        }
        prev = Some(c);
    }
    if prev.map(|p| p.block) != Some(block) {
        bail!("fold `{}` has no checkpoint at {block}", fold.name);
    }
    Ok(())
}

impl Stepper<'_> {
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
            let inputs = window_inputs(&self.dir, &f.reaches, self.from, at)?;
            let rows = self.rows(&f.name)?;
            let entry = Checkpoint {
                block: at,
                id: checkpoint_id(f, prev, &inputs),
                prev: self.from,
                rows: rows.len() as u64,
                row_digest: row_digest(f, &rows),
                inputs,
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
            verify_chain(&self.dir, f, &log, block)?;
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
            let rows = self.rows(&f.name)?;
            if rows.len() as u64 != entry.rows || row_digest(f, &rows) != entry.row_digest {
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
        let start = self.latest_checkpoint(dir, sealed_through)?;
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
        let mut s = self.stepper(dir, schema)?;
        if let Some(b) = start {
            s.resume(b)?;
        }
        let empty = analytics::HotRows::new();
        for &cut in &cuts {
            s.step_to(&empty, sealed_through, cut)?;
            s.checkpoint()?;
        }
        Ok(cuts)
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
        let mut s = self.stepper(dir, schema)?;
        let c = self.latest_checkpoint(dir, n)?;
        if let Some(b) = c {
            s.resume(b)?;
        }
        if c != Some(n) {
            s.step_to(hot, sealed_through, n)?;
        }
        Ok(s)
    }
}

/// `nuthatch fold ...`. Opens the store itself, so it refuses while `dev` holds the nest: in S1 this
/// is the only writer of checkpoints.
pub fn run(cmd: crate::cli::FoldCommand) -> Result<()> {
    use crate::cli::FoldCommand;
    let dir = match &cmd {
        FoldCommand::Build { dir, .. }
        | FoldCommand::Read { dir, .. }
        | FoldCommand::Bench { dir, .. } => std::path::PathBuf::from(dir),
    };
    let dir = dir.as_path();
    let store = crate::store::Store::open_existing(&dir.join(crate::config::DB_FILE))
        .context("opening the nest's store (is `dev` running on it?)")?;
    let sealed_through = store.sealed_through();
    let set = FoldSet::load(dir, &[])?;
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
            println!("{}", bench(dir, &set, &hot, sealed_through, iters)?);
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
    let counts = |s: &Stepper| -> Result<u64> {
        set.folds
            .iter()
            .map(|f| s.eval.count(&f.name))
            .sum::<Result<u64>>()
    };
    // Warm-up: the first evaluation pays for loading extensions and binding views, once per process.
    s.probe(hot, sealed_through, heads[0], counts)?;

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
        "targets": { "p99_ms": 500, "peak_rss_mib": 256 },
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
    let mut h = Sha256::new();
    let mut part = |label: &str, text: &str| {
        h.update(label.as_bytes());
        h.update((text.len() as u64).to_le_bytes());
        h.update(text.as_bytes());
    };
    part("v", "nuthatch-fold-v1");
    part("plan", &binder.plan_text(&sql));
    part("key", &format!("{key:?}"));
    part("carry", &format!("{carry:?}"));
    for t in &reaches {
        match view_bodies.get(t) {
            Some(body) => part(&format!("view:{t}"), &binder.plan_text(body)),
            None => part(
                &format!("table:{t}"),
                &format!("{:?}", binder.describe(&format!("SELECT * FROM \"{t}\""))?),
            ),
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
        // Each window read exactly one sealed segment of `t`, and the ids chain.
        assert!(log
            .checkpoints
            .iter()
            .all(|c| c.inputs.len() == 1 && c.inputs[0].0 == "t"));
        let mut prev = None;
        for c in &log.checkpoints {
            assert_eq!(c.id, checkpoint_id(&set.folds[0], prev, &c.inputs));
            prev = Some(c);
        }
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
        let report = bench(dir.path(), &set, &hot, 30, 12).unwrap();
        assert_eq!(report["checkpoint"], 30);
        assert_eq!(report["heads"]["count"], 5);
        assert_eq!(report["evaluations"], 12);
        assert!(
            report["wall_ms"]["p99"].as_f64().unwrap()
                >= report["wall_ms"]["p50"].as_f64().unwrap()
        );
        assert!(bench(dir.path(), &set, &hot, 30, 0).is_err());
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

    /// Rows and digest can be perfect and the history still wrong: a segment in its window has since
    /// been replaced, so the checkpoint no longer describes these facts.
    #[test]
    fn a_checkpoint_over_segments_the_catalogue_no_longer_holds_is_refused() {
        let (dir, hot, set) = built();
        let path = dir
            .path()
            .join(crate::seal::SEGMENTS_DIR)
            .join(crate::seal::MANIFEST_FILE);
        let raw = std::fs::read_to_string(&path).unwrap();
        let manifest = crate::seal::load_manifest_with_hash(dir.path()).unwrap().0;
        let old = &manifest.tables["t"]
            .iter()
            .find(|s| s.from_block == 11)
            .unwrap()
            .hash;
        std::fs::write(&path, raw.replace(old.as_str(), &"f".repeat(64))).unwrap();
        assert!(refusal(dir.path(), &set, &hot).contains("no longer holds"));
    }
}
