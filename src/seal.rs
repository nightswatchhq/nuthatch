//! Per-table sealing (RFC-0001 step 4): once a block range is final, each table's rows in that
//! range are written to their own content-addressed Parquet segment, catalogued per table in the
//! manifest. The columnar cold layer is append-only - it never sees a reorg, because reorgs only
//! ever touch the mutable hot store (see store::rollback_to).
//!
//! All tables in a nest ingest from the same block stream and seal together per finalized range, so
//! `sealed_through` stays a single global watermark and the whole range is pruned from hot once every
//! table's segment is durable (the indexer does the prune).
//!
//! ## Scope of the content hash (audit F-D3)
//!
//! A segment's hash is taken over the **Parquet file bytes**, and those bytes include the `created_by`
//! metadata string that `arrow-rs`/`parquet` stamps with its own version. So the guarantee is:
//!
//! - **Same binary → identical bytes → identical hash.** This is the property everything relies on:
//!   re-running a backfill, or two operators running the same release, produce byte-identical segments
//!   that dedupe against each other. Determinism holds.
//! - **Across nuthatch versions built on different arrow-rs releases, segment identity may differ**
//!   even when every decoded row is identical, because `created_by` changed underneath us.
//!
//! That is a limit on *segment identity*, not on correctness: the rows are the same, the queries return
//! the same answers, and re-execution still verifies. What it means practically is that a segment hash
//! is a strong identity within a release and a weak one across releases - so do not use one as a
//! cross-version equality proof (compare decoded rows for that), and expect a version bump that moves
//! arrow-rs to re-seal rather than dedupe. Pinning `created_by` would buy cross-version identity if that
//! ever becomes worth the coupling to a parquet-rs internal; it is deliberately not pinned today.

use anyhow::{Context, Result};
use arrow::array::{Array, ArrayRef, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

pub const SEGMENTS_DIR: &str = "segments";

/// The **shared segment store** for the runtime `dir` belongs to, if it has one (RFC-0033 §11a).
///
/// Derived by convention rather than plumbed: a mounted dataset lives at `<root>/data/<nid>`, so its
/// runtime root is two levels up and the store is `<root>/segments`. A solo `nuthatch dev` nest is
/// not under a `data/` parent, gets `None`, and keeps its segments where they have always been -
/// sharing is a property of a runtime hosting several nests, not of a nest.
///
/// The alternative was threading a root through every seal and query call site, which would have made
/// the layout a parameter of functions that have no business knowing about it.
pub fn shared_store(dir: &Path) -> Option<PathBuf> {
    let parent = dir.parent()?;
    if parent.file_name()? != crate::runtime::DATA_DIR {
        return None;
    }
    Some(parent.parent()?.join(SEGMENTS_DIR))
}

/// Where a segment's bytes actually live: the shared store when there is one, else beside the nest.
///
/// **Falls back to the per-dataset path when the shared copy is absent**, so a dataset migrated
/// before slice C still reads. A missing shared segment is not evidence of corruption; it is evidence
/// of a layout that has not been relocated yet.
pub fn segment_path(dir: &Path, file: &str, hash: &str) -> PathBuf {
    if let Some(store) = shared_store(dir) {
        let shared = store.join(format!("{hash}.parquet"));
        if shared.exists() {
            return shared;
        }
    }
    dir.join(SEGMENTS_DIR).join(file)
}
pub const MANIFEST_FILE: &str = "manifest.json";

/// One sealed Parquet file. `hash` is the content address (sha256 of the file bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub hash: String,
    pub from_block: u64,
    pub to_block: u64,
    pub rows: usize,
    pub file: String,
    /// The discovered-child registry's content hash at seal time (RFC-0009): records exactly which
    /// factory-discovered set produced this segment, so its child rows are reproducible. `None` for a
    /// static (non-factory) nest. Absent in pre-RFC-0009 manifests (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_snapshot: Option<String>,
    /// This table's rows since its last full segment, held under [`SEAL_TABLE_FLOOR`] (#1150). A
    /// provisional segment is a real, content-addressed, queryable Parquet file like any other; the
    /// one thing that distinguishes it is that the table's **next** seal folds it in rather than
    /// sitting beside it, and the file it replaces is removed. At most one per table. Absent in
    /// manifests written before this existed, which reads as `false` - every segment sealed then
    /// was final.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub provisional: bool,
}

/// Rows a table needs before its segment at a cut is final rather than provisional (#1150).
///
/// The cut is global and data-determined (`indexer::take_sealable`): every `SEAL_DIRECT_BATCH` rows
/// across all tables, at a block boundary the chain chose. That keeps segment identity independent
/// of the operator's window, and it is kept. What it also did, on a nest with many tables, was write
/// one file **per table per cut**, so a table seeing three events in twenty thousand got a three-row
/// Parquet file every cut - measured on the Perpl backfill at 9.3%: 15,977 files, 4,702 under 8 KB,
/// and a query pays about 0.15 ms per file it opens (docs/bench/segment-layout.md). Sealed segments
/// are never compacted, so that only grew.
///
/// So a table whose pending rows at a cut are under this floor is sealed **provisionally** and folded
/// into its own next cut, until it has this many. The rule is stated in the same terms as the cut -
/// rows, counted from the data - so two operators still produce identical files; and it changes
/// identity only for tables under the floor, since a table that clears it at every cut is sealed
/// exactly as before. At 20,000 rows a cut that is every table with a share of 5% or more.
///
/// One thousand is a segment whose footer is no longer most of it (about 90 bytes a row on the
/// nests measured, so ~90 KB), and it turns a 0.015% table's file count from one per cut into one
/// per 333 cuts. A row rather than byte floor, because bytes depend on the writer version and rows
/// do not.
pub const SEAL_TABLE_FLOOR: usize = 1_000;

/// The floor `seal_range` applies for `dir`: the constant, unless a test has set otherwise.
fn table_floor(dir: &Path) -> usize {
    #[cfg(test)]
    if let Some(&n) = test_table_floors().lock().unwrap().get(dir) {
        return n;
    }
    let _ = dir;
    SEAL_TABLE_FLOOR
}

/// Test-only knob (#1150), keyed by `dir` like the sweep knobs below so no test's setting leaks
/// into another's tempdir. A floor of `0` makes every seal final, which is the behaviour before the
/// floor existed - the corruption and quarantine tests seal one row per call and damage one
/// segment of several, and they are about what a bad file does to a table, not about how many
/// files a table gets. They say so where they set it. Tests about the floor use the real one.
#[cfg(test)]
fn test_table_floors() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static FLOORS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    FLOORS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn test_set_table_floor(dir: &Path, rows: usize) {
    test_table_floors()
        .lock()
        .unwrap()
        .insert(dir.to_path_buf(), rows);
}

/// The segment catalogue: per-table lists of sealed segments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub tables: BTreeMap<String, Vec<Segment>>,
}

/// What a `seal_range` call sealed.
#[derive(Debug, Default)]
pub struct SealSummary {
    pub tables: usize,
    pub rows: usize,
}

/// Seal every table's rows in a finalized `[from, to]` range. Rows are grouped by their `table`
/// field; each group becomes one content-addressed Parquet segment catalogued under its table.
/// Returns None if the range held no rows.
pub fn seal_range(
    dir: &Path,
    entity_json: &[String],
    from: u64,
    to: u64,
) -> Result<Option<SealSummary>> {
    seal_range_with_snapshot(dir, entity_json, from, to, None)
}

/// Like [`seal_range`], but records `registry_snapshot` (the discovered-child registry's content hash)
/// on each segment's manifest entry - the factory paths (RFC-0009) pass it so a segment records which
/// discovered set produced it. A static nest passes `None` (via `seal_range`).
pub fn seal_range_with_snapshot(
    dir: &Path,
    entity_json: &[String],
    from: u64,
    to: u64,
    registry_snapshot: Option<&str>,
) -> Result<Option<SealSummary>> {
    let mut by_table: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<Value>(j) else {
            continue;
        };
        let table = v
            .get("table")
            .and_then(Value::as_str)
            .unwrap_or("rows")
            .to_string();
        by_table.entry(table).or_default().push(v);
    }
    if by_table.is_empty() {
        return Ok(None);
    }

    let seg_dir = dir.join(SEGMENTS_DIR);
    std::fs::create_dir_all(&seg_dir)
        .with_context(|| format!("cannot create {}", seg_dir.display()))?;
    let mut manifest = load_manifest(dir)?;
    let mut summary = SealSummary::default();
    // Per-nest provisional files a fold has replaced. Removed only once the manifest that no longer
    // names them is durably installed (below), never before: a crash between the two would leave
    // a manifest pointing at a file that is gone, and the folded rows would read as missing.
    let mut folded_away: Vec<PathBuf> = Vec::new();

    for (table, rows) in by_table {
        let batch = rows_to_batch(&rows)?;
        let bytes = write_parquet(&batch)?;
        let hash = hex::encode(Sha256::digest(&bytes));
        let segments = manifest.tables.entry(table.clone()).or_default();
        // Content-addressed idempotency: an identical segment (same table + hash) is already
        // catalogued, so re-sealing the same rows - e.g. re-running `nuthatch screen` over a range to
        // re-audit - is a no-op rather than a double-listed (double-counted) segment. Checked on the
        // incoming rows alone, before any fold, so the rule is the same one it always was.
        if segments.iter().any(|s| s.hash == hash) {
            continue;
        }
        let new_rows = rows.len();

        // Fold this table's provisional segment in, if it has one (#1150): its rows come first,
        // being from earlier cuts, and its `from_block` is the segment's. Read back from the
        // Parquet it was written to rather than kept anywhere else, so there is one copy of the
        // truth and the folded file is byte-identical to sealing all of those rows at once
        // (`a_folded_segment_is_byte_identical_to_sealing_all_its_rows_at_once`).
        let pending = segments.iter().position(|s| s.provisional);
        let (rows, from, bytes, hash, replaced) = match pending {
            None => (rows, from, bytes, hash, None),
            Some(i) => {
                let prev = segments.remove(i);
                let mut all = read_segment_rows(&segment_path(dir, &prev.file, &prev.hash))
                    .with_context(|| {
                        format!("reading provisional segment {} to fold it", prev.file)
                    })?;
                all.extend(rows);
                let bytes = write_parquet(&rows_to_batch(&all)?)?;
                let hash = hex::encode(Sha256::digest(&bytes));
                (all, prev.from_block, bytes, hash, Some(prev))
            }
        };
        let provisional = rows.len() < table_floor(dir);
        let file = format!("{table}-{hash}.parquet");

        // Write once, into the shared store when this dataset belongs to a runtime (RFC-0033 §11a).
        // Content-addressed, so a second nest sealing byte-identical rows finds it already there and
        // the write is a no-op rather than a duplicate.
        match shared_store(dir) {
            Some(store) => {
                std::fs::create_dir_all(&store).context("creating the shared segment store")?;
                let shared = store.join(format!("{hash}.parquet"));
                if !shared.exists() {
                    std::fs::write(&shared, &bytes).context("failed to write shared segment")?;
                }
                // The folded file is left for `nuthatch prune`, which reclaims what no manifest
                // references: another dataset in the store may hold the same bytes under the same
                // hash, and this nest cannot know.
            }
            None => {
                std::fs::write(seg_dir.join(&file), &bytes).context("failed to write segment")?;
                if let Some(prev) = &replaced {
                    // This nest's own copy, and not yet: see `folded_away`.
                    folded_away.push(seg_dir.join(&prev.file));
                }
            }
        }
        summary.tables += 1;
        // New rows only: the folded ones were counted when they were first sealed, and
        // `nuthatch_rows_sealed_total` is a count of rows, not of writes.
        summary.rows += new_rows;
        segments.push(Segment {
            hash,
            from_block: from,
            to_block: to,
            rows: rows.len(),
            file,
            // A folded segment records the snapshot at its last write. Discovery is append-only,
            // so the latest snapshot covers every row in the file; the earlier ones covered less.
            registry_snapshot: registry_snapshot.map(str::to_string),
            provisional,
        });
    }

    save_manifest(dir, &manifest)?;
    // The manifest is installed and fsynced; nothing references these any more. A failure here is
    // a stray file, which is disk and not data, so it is logged rather than returned: the seal
    // itself has already happened.
    for old in folded_away {
        if let Err(e) = std::fs::remove_file(&old) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "folded provisional segment {} not removed: {e}",
                    old.display()
                );
            }
        }
    }
    Ok(Some(summary))
}

/// A segment's rows back as the JSON objects `rows_to_batch` was given, so a provisional segment
/// can be folded by re-sealing (#1150). The inverse of `rows_to_batch` exactly: a `UInt64` column
/// becomes a number, a `Utf8` value a string, and a null is an absent key - which is what
/// `rows_to_batch` maps an absent key *to*, so a fold round-trips to the bytes a single seal of the
/// same rows produces. Nothing else needs this; `read_table_rows` is the typed reader.
fn read_segment_rows(path: &Path) -> Result<Vec<Value>> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file =
        std::fs::File::open(path).with_context(|| format!("opening segment {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .context("reading segment metadata")?
        .build()
        .context("building segment reader")?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.context("reading segment batch")?;
        let schema = batch.schema();
        for i in 0..batch.num_rows() {
            let mut obj = serde_json::Map::new();
            for (c, field) in schema.fields().iter().enumerate() {
                let col = batch.column(c);
                if col.is_null(i) {
                    continue;
                }
                let v = match field.data_type() {
                    DataType::UInt64 => {
                        let a = col
                            .as_any()
                            .downcast_ref::<UInt64Array>()
                            .context("UInt64 column of the wrong array type")?;
                        Value::from(a.value(i))
                    }
                    DataType::Utf8 => {
                        let a = col
                            .as_any()
                            .downcast_ref::<StringArray>()
                            .context("Utf8 column of the wrong array type")?;
                        Value::from(a.value(i))
                    }
                    other => anyhow::bail!(
                        "segment {} has a {other} column, which this project never writes",
                        path.display()
                    ),
                };
                obj.insert(field.name().clone(), v);
            }
            out.push(Value::Object(obj));
        }
    }
    Ok(out)
}

/// Build an Arrow batch from a table's JSON rows. `block_number`/`log_index` are UInt64; every other
/// column is Utf8 (values already carry their canonical text form - hex, decimal, or string).
fn rows_to_batch(rows: &[Value]) -> Result<RecordBatch> {
    let mut columns: BTreeSet<String> = BTreeSet::new();
    for r in rows {
        if let Some(obj) = r.as_object() {
            columns.extend(obj.keys().cloned());
        }
    }
    let columns: Vec<String> = columns.into_iter().collect();

    let mut fields = Vec::with_capacity(columns.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for col in &columns {
        if col == "block_number" || col == "log_index" || col == "_seq" || col == "block_timestamp"
        {
            let vals: Vec<u64> = rows
                .iter()
                .map(|r| r.get(col).and_then(Value::as_u64).unwrap_or(0))
                .collect();
            fields.push(Field::new(col, DataType::UInt64, false));
            arrays.push(Arc::new(UInt64Array::from(vals)));
        } else {
            let vals: Vec<Option<String>> = rows
                .iter()
                .map(|r| match r.get(col) {
                    Some(Value::String(s)) => Some(s.clone()),
                    None | Some(Value::Null) => None,
                    Some(other) => Some(other.to_string()),
                })
                .collect();
            fields.push(Field::new(col, DataType::Utf8, true));
            arrays.push(Arc::new(StringArray::from(vals)));
        }
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .context("failed to build record batch")
}

fn write_parquet(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))
        .context("failed to create parquet writer")?;
    writer.write(batch).context("failed to write batch")?;
    writer.close().context("failed to finalise parquet")?;
    Ok(buf)
}

/// Read a table's sealed rows back as [`DecodedRow`]s, in block order (RFC-0041 §5.3, nuthatch#865).
///
/// **The one conversion, not a second one.** Every cell goes through
/// [`DecodedRow::from_stored`], which is what the reorg path uses on the hot store's JSON. A reader
/// that parsed Parquet into typed values on its own would be a second opinion about what a stored
/// row means, and the two would agree until the day they did not - at which point a retraction stops
/// cancelling its insertion and the entity keeps a fact forever. See nuthatch#864.
///
/// `rows_to_batch` writes `block_number`, `log_index`, `_seq` and `block_timestamp` as `UInt64` and
/// everything else as `Utf8`, so those are the only two column types this has to understand. A
/// segment carrying anything else was not written by this project.
///
/// **This loads the table into memory.** That is what a warm-restart seed is - §5.3 pays the
/// historical fold once per restart rather than once per request - but it is not a streaming reader
/// and should not be mistaken for one.
pub fn read_table_rows(
    dir: &Path,
    schema: &crate::registry::TableSchema,
) -> Result<Vec<crate::registry::DecodedRow>> {
    let mut out = Vec::new();
    read_table_rows_by_segment(dir, schema, &mut |mut rows| {
        out.append(&mut rows);
        Ok(())
    })?;
    Ok(out)
}

/// The same read, handed to `sink` **one sealed segment at a time** instead of accumulated.
///
/// The whole-history `Vec` is a large transient of a restart seed, and it is one nothing else
/// bounds: mount admission prices an entity's *maintained* relation, not the historical facts it
/// folds to build one. Measured against a real Horizon nest (2026-08-26, `tests/seed_scale.rs`,
/// 346,288 sealed rows across 2,985 segments):
///
/// | seed             | peak RSS | wall |
/// |------------------|----------|------|
/// | one window       | 993 MB   | 1.2s |
/// | per segment      | 694 MB   | 2.4s |
/// | one window, join | 790 MB   | 1.9s |
/// | per segment, join| 368 MB   | 2.8s |
///
/// Half the per-cursor budget for one entity over one table, against a third of it, at twice the
/// wall clock on a path that runs once per restart. Worth it.
///
/// Chunking is sound because a seed is `+1`-only: no retraction is fed, so the maintained relation
/// grows monotonically and no chunk can trip `max_rows` earlier than the whole-history window
/// would have. A seed that fits still fits.
pub fn read_table_rows_by_segment(
    dir: &Path,
    schema: &crate::registry::TableSchema,
    sink: &mut dyn FnMut(Vec<crate::registry::DecodedRow>) -> Result<()>,
) -> Result<()> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let manifest = load_manifest(dir)?;
    let Some(segments) = manifest.tables.get(&schema.table) else {
        return Ok(());
    };
    // Block order, and by content address within a block range so a re-seal cannot reorder rows.
    let mut ordered: Vec<&Segment> = segments.iter().collect();
    ordered.sort_by(|a, b| {
        (a.from_block, a.to_block, &a.hash).cmp(&(b.from_block, b.to_block, &b.hash))
    });

    for segment in ordered {
        let mut out = Vec::new();
        let path = segment_path(dir, &segment.file, &segment.hash);
        let file = std::fs::File::open(&path)
            .with_context(|| format!("opening sealed segment {}", path.display()))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .with_context(|| format!("reading sealed segment {}", path.display()))?
            .build()
            .with_context(|| format!("reading sealed segment {}", path.display()))?;
        for batch in reader {
            let batch =
                batch.with_context(|| format!("decoding sealed segment {}", path.display()))?;
            for row in 0..batch.num_rows() {
                let mut stored = serde_json::Map::new();
                for (i, field) in batch.schema().fields().iter().enumerate() {
                    let column = batch.column(i);
                    let value = match field.data_type() {
                        DataType::UInt64 => {
                            let a = column
                                .as_any()
                                .downcast_ref::<UInt64Array>()
                                .context("a UInt64 column that is not one")?;
                            Value::from(a.value(row))
                        }
                        DataType::Utf8 => {
                            let a = column
                                .as_any()
                                .downcast_ref::<StringArray>()
                                .context("a Utf8 column that is not one")?;
                            if a.is_null(row) {
                                Value::Null
                            } else {
                                Value::from(a.value(row))
                            }
                        }
                        other => anyhow::bail!(
                            "sealed segment {} column {} has type {other}, which this project does \
                             not write",
                            path.display(),
                            field.name()
                        ),
                    };
                    stored.insert(field.name().clone(), value);
                }
                out.push(
                    crate::registry::DecodedRow::from_stored(&Value::Object(stored), schema)
                        .with_context(|| {
                            format!("row {row} of sealed segment {}", path.display())
                        })?,
                );
            }
        }
        sink(out)?;
    }
    Ok(())
}

/// Load the segment catalogue (empty if none yet).
pub fn load_manifest(dir: &Path) -> Result<Manifest> {
    let path = manifest_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).context("corrupt segments manifest"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(e).context("failed to read manifest"),
    }
}

/// The **serving-path** counterpart to [`verify_and_quarantine`]: which of `dir`'s manifest segments
/// no longer hash to their content address. Returns their hashes, for `analytics::define_views` to
/// drop from a rebuilt view (issue #433).
///
/// ## Why the hash and not a DuckDB probe
///
/// #430 drops a segment that will not **bind**, probing with `conn.prepare` over `read_parquet`. That
/// catches footer corruption and nothing else. Measured in the DuckDB CLI (1.5.3) against a segment
/// whose data region is overwritten but whose footer is intact:
///
/// - `SELECT 1 FROM read_parquet([f]) LIMIT 0` - the #430 probe - **succeeds**;
/// - `count(*)` and `max(col)` **succeed**, answered from Parquet metadata without reading a page;
/// - `SELECT * … LIMIT 1` fails on that file, and **succeeds** on one where only the late row groups
///   are corrupt, because it never reads them.
///
/// So the only sound DuckDB-side discriminator is a full scan of every column of every segment, and
/// it would be pinned to whatever the query planner prunes this release. The content address is not:
/// sealed segments are immutable, so any changed byte is unambiguous corruption, and this is already
/// the check `verify_and_quarantine` makes at startup. Using the same one here is what stops the
/// serving path and the startup path disagreeing about the same file, which was half of #419.
///
/// **Reduce here, never quarantine.** Quarantine moves bytes, and the shared store's bytes belong to
/// every dataset referencing them (RFC-0033 §11a). Dropping a segment from one query's view changes
/// nothing on disk and so is safe for a shared segment, which is why this returns hashes rather than
/// calling the startup pass.
///
/// ## Cost, and the cache that is deliberately not here
///
/// This reads and hashes segments, every time it is called, with no cache. Two things bound what it
/// costs, and the first one alone was **not enough**:
///
/// - The *caller*: `run` only asks after a query has bound and then died reading rows. That rules
///   out the commonest error on this surface (a typo, a missing column), and nothing more. It was
///   claimed here that "the cheap way to provoke a sweep does not exist" - it did, and it was
///   cheaper than the one this file named. `SELECT CAST('x' AS INTEGER)` is 27 bytes, references no
///   table, sails past every gate on the way in, binds, and dies executing; measured, it hashed all
///   3 segments of a healthy nest, on an unauthenticated surface whose concurrency permits are 2.
/// - So, second and load-bearing: `tables`. Only the segments backing the tables the failed query
///   actually **named** are read. A segment the query never touched cannot be what killed it, so
///   sweeping it was never justified on correctness either. The table-free class above now hashes
///   nothing at all, and `SELECT CAST(c AS INT) FROM t` pays for `t` and not for the other forty
///   tables in the nest.
///
/// The bound that remains: a caller who names the nest's largest table can still make one request
/// hash that table's segments. That is inherent to verifying-on-failure at all - the levers are
/// coalescing concurrent sweeps (below), a deadline shared with the query's own watchdog (`deadline`,
/// #476), and the gateway's rate limiting (#365). Not a cache keyed on anything that can go stale.
///
/// **There was a memo here keyed on the file's `(mtime, len)`, and it was wrong.** The idea was that a
/// segment already verified in this process is a `stat` rather than a read. Measured on this box
/// (btrfs, Linux 6.12): rewriting a file in place with the same length inside one timer tick leaves
/// `st_mtime_ns` **byte-identical**, because the kernel stamps writes from a per-tick cached clock. So
/// the key cannot see an in-place same-length overwrite - which is precisely the corruption this
/// function exists to catch. A cache that reports `intact` about a corrupt segment, inside the fix for
/// reporting healthy about corrupt data, is the bug wearing the fix's clothes. It was removed rather
/// than tuned, and `seal::tests::a_corrupt_segment_is_caught_even_when_its_mtime_is_unchanged` is
/// what stops it coming back. That test exists because my first claim here - that the page-corruption
/// test already covered it - was **false**: mutating the memo back in left the whole suite green.
///
/// ## Coalescing (#476)
///
/// Concurrent callers naming the *same* tables (the shape of the amplifier: the same expensive query,
/// repeated while the two `SQL_MAX_CONCURRENCY` permits are held) share one sweep rather than each
/// paying for their own. This is not a cache: the in-flight entry lives only for the duration of the
/// sweep it names and is removed the moment that sweep finishes, so there is nothing here that can go
/// stale the way the `(mtime, len)` memo did - the next call, even for the identical key, always reads
/// bytes fresh.
///
/// `tables` holds the table names the failed query referenced, lowercased (DuckDB identifiers are
/// case-insensitive). An empty set hashes nothing. `deadline`, when set, bounds the sweep by the same
/// wall-clock budget as the query that triggered it (`analytics::run`'s `QueryGuard`), rather than
/// running unbounded between the query's two attempts - a caller that has already spent its budget
/// gets nothing further hashed on its behalf.
pub fn segments_failing_verification(
    dir: &Path,
    tables: &BTreeSet<String>,
    deadline: Option<Instant>,
) -> BTreeSet<String> {
    if tables.is_empty() {
        return BTreeSet::new();
    }
    let key: SweepKey = (dir.to_path_buf(), tables.clone());
    let (slot, is_leader) = {
        let mut inflight = sweeps_in_flight().lock().unwrap();
        match inflight.get(&key) {
            Some(existing) => (Arc::clone(existing), false),
            None => {
                let slot = Arc::new(SweepSlot::default());
                inflight.insert(key.clone(), Arc::clone(&slot));
                (slot, true)
            }
        }
    };

    // Test-only rendezvous (#529): when a test has armed a registration barrier for this `dir`, every
    // caller - leader and followers alike - blocks here until all of them have made their
    // leader-or-follower decision above. See `test_set_sweep_registration_barrier` for why.
    #[cfg(test)]
    {
        let barrier = test_sweep_registration_barriers()
            .lock()
            .unwrap()
            .get(dir)
            .cloned();
        if let Some(b) = barrier {
            b.wait();
        }
    }

    if is_leader {
        // Removes this sweep's map entry and wakes any follower on every exit path, including a panic
        // unwind out of `sweep_segments` - `fs::read`/`Sha256::digest` don't panic on the errors this
        // code already handles, but leaving a slot that nothing will ever complete stuck in the map
        // forever would hang a *trusted, unguarded* follower (query()/query_cold(), which have no
        // deadline and must run to completion by design) indefinitely rather than just failing loudly.
        struct Cleanup<'a> {
            key: &'a SweepKey,
            slot: &'a SweepSlot,
        }
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                sweeps_in_flight().lock().unwrap().remove(self.key);
                self.slot.done.notify_all();
            }
        }
        let _cleanup = Cleanup {
            key: &key,
            slot: &slot,
        };
        let result = sweep_segments(dir, tables, deadline);
        *slot.result.lock().unwrap() = Some(result.clone());
        return result;
        // `_cleanup` drops here - map entry removed and followers notified only now that the result is
        // published, so a follower checking the map first still finds the slot with a real answer.
    }

    // Follower: wait for the leader, but never past this call's own deadline - the query's time budget
    // is a promise, and that must hold even when it is someone else's sweep in progress, not its own.
    let mut guard = slot.result.lock().unwrap();
    loop {
        if let Some(result) = guard.as_ref() {
            return result.clone();
        }
        // The leader is gone (removed itself, on success or on panic) without ever publishing - stop
        // following a slot nobody will ever complete and retry from scratch, becoming the leader (or
        // following a new one) rather than waiting out a deadline, or forever, for nothing.
        if !sweeps_in_flight().lock().unwrap().contains_key(&key) {
            drop(guard);
            return segments_failing_verification(dir, tables, deadline);
        }
        guard = match deadline {
            None => slot.done.wait(guard).unwrap(),
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return BTreeSet::new();
                }
                slot.done.wait_timeout(guard, remaining).unwrap().0
            }
        };
    }
}

/// A sweep in progress, keyed by exactly what it was asked to verify - see the "Coalescing" section
/// above.
type SweepKey = (PathBuf, BTreeSet<String>);

#[derive(Default)]
struct SweepSlot {
    result: Mutex<Option<BTreeSet<String>>>,
    done: Condvar,
}

fn sweeps_in_flight() -> &'static Mutex<HashMap<SweepKey, Arc<SweepSlot>>> {
    static INFLIGHT: OnceLock<Mutex<HashMap<SweepKey, Arc<SweepSlot>>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test-only knob (#529): how many segment-budget checks the sweep loop gets to make before
/// `sweep_out_of_budget` reports the deadline spent, **regardless of the wall clock** - absent (the
/// default) leaves the real `Instant`-based check in `sweep_out_of_budget` untouched. Replaces a
/// per-segment `thread::sleep` raced against a real deadline: that raced two real clocks (how long
/// the injected delay actually took to wake up vs. how long the deadline had left), and at load
/// average 34 a descheduled thread can wake up arbitrarily later than its requested sleep, closing a
/// margin that was only ever tens of milliseconds. Counting checks instead of milliseconds makes the
/// early exit exact and load-independent: the loop stops after exactly the Nth check, on every run.
///
/// Keyed by `dir`, not a bare global - a first cut at this used a single process-global `AtomicI64`
/// and read it unconditionally in `sweep_out_of_budget`, so for the whole window one test held it
/// armed, *every* sweep in the test binary was truncated at the same check count, including sweeps
/// belonging to tests that never touched the knob and held no lock over it. That is the exact class
/// of bug this issue exists to remove, just relocated from a real clock to a shared atomic - keying
/// by `dir` (same pattern as `test_sweep_starts` and the registration barrier above) removes it
/// structurally: a sweep only ever consults the override for its own tempdir.
#[cfg(test)]
fn test_sweep_expiry_after_checks() -> &'static Mutex<HashMap<PathBuf, i64>> {
    static EXPIRY: OnceLock<Mutex<HashMap<PathBuf, i64>>> = OnceLock::new();
    EXPIRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn test_set_sweep_expire_after_checks(dir: &Path, n: i64) {
    if n < 0 {
        test_sweep_expiry_after_checks().lock().unwrap().remove(dir);
    } else {
        test_sweep_expiry_after_checks()
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), n);
    }
}

/// Test-only knob (#529): a rendezvous point inside `segments_failing_verification`, keyed by `dir`
/// so an unrelated test's concurrent sweep never joins it by accident (same reasoning as
/// `test_sweep_starts` below). `concurrent_identical_sweeps_coalesce_into_one` used to keep the
/// leader busy with a fixed 150ms per-segment delay, long enough - it hoped - for the three
/// followers to be scheduled and find the leader's map entry before it published and removed
/// itself. At load average 34 a follower can sit unscheduled for well over 150ms, arrive to find the
/// entry already gone, and start its own uncoalesced sweep. A barrier removes the race instead of
/// widening the margin: every caller (leader and followers) blocks here until all of them have made
/// their leader-or-follower decision, so the leader cannot possibly finish and clean up before the
/// last follower has registered, no matter how the four threads are scheduled.
#[cfg(test)]
fn test_sweep_registration_barriers() -> &'static Mutex<HashMap<PathBuf, Arc<std::sync::Barrier>>> {
    static BARRIERS: OnceLock<Mutex<HashMap<PathBuf, Arc<std::sync::Barrier>>>> = OnceLock::new();
    BARRIERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn test_set_sweep_registration_barrier(dir: &Path, n: usize) {
    test_sweep_registration_barriers()
        .lock()
        .unwrap()
        .insert(dir.to_path_buf(), Arc::new(std::sync::Barrier::new(n)));
}

#[cfg(test)]
pub(crate) fn test_clear_sweep_registration_barrier(dir: &Path) {
    test_sweep_registration_barriers()
        .lock()
        .unwrap()
        .remove(dir);
}

/// How many times a real (leader) sweep has run, keyed by `dir` - a coalescing test's only window
/// into whether N concurrent identical callers paid for N sweeps or one. Keyed rather than a bare
/// counter because `cargo test` runs the whole suite concurrently: an unrelated test's own (fast,
/// undelayed) sweep against its own tempdir would otherwise land inside this test's delay window and
/// inflate the count for a dir it never touched. Each test's tempdir is unique, so counting per-dir
/// isolates it from that contamination without requiring any process-wide serialisation.
#[cfg(test)]
fn test_sweep_starts() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static STARTS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    STARTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn test_sweep_start_count(dir: &Path) -> usize {
    test_sweep_starts()
        .lock()
        .unwrap()
        .get(dir)
        .copied()
        .unwrap_or(0)
}

/// How many segments a sweep actually read and hashed, keyed by `dir` for the same reason as
/// `test_sweep_starts`. What `sweep_stops_at_its_deadline_instead_of_running_to_completion` pins the
/// early exit on: proof the loop stopped *before* the corrupt segment rather than merely that the
/// corrupt segment happened not to be flagged.
#[cfg(test)]
fn test_sweep_segments_processed() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static PROCESSED: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    PROCESSED.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn test_sweep_segments_processed_count(dir: &Path) -> usize {
    test_sweep_segments_processed()
        .lock()
        .unwrap()
        .get(dir)
        .copied()
        .unwrap_or(0)
}

/// `deadline`'s check, factored out so a test can override it with an exact, `dir`-keyed check-count
/// cutoff (`test_set_sweep_expire_after_checks`) instead of racing a real `Instant` against a real
/// `thread::sleep`. Production behaviour (the path reached whenever no test has armed an override for
/// this `dir`) is unchanged: `Instant::now() >= d`.
fn sweep_out_of_budget(
    #[allow(unused_variables)] idx: usize,
    deadline: Option<Instant>,
    #[allow(unused_variables)] dir: &Path,
) -> bool {
    #[cfg(test)]
    {
        if let Some(&n) = test_sweep_expiry_after_checks().lock().unwrap().get(dir) {
            return idx as i64 >= n;
        }
    }
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// The actual read-and-hash pass, run by whichever caller is the sweep's leader. See
/// [`segments_failing_verification`] for the coalescing and deadline behaviour around this.
fn sweep_segments(
    dir: &Path,
    tables: &BTreeSet<String>,
    deadline: Option<Instant>,
) -> BTreeSet<String> {
    #[cfg(test)]
    {
        *test_sweep_starts()
            .lock()
            .unwrap()
            .entry(dir.to_path_buf())
            .or_insert(0) += 1;
    }

    let mut bad = BTreeSet::new();
    // Enumerate first, hash second: the hashing loop can only ever touch what `segments_to_verify`
    // handed it, so the reachability bound holds by construction rather than by a filter someone
    // could later move below the `fs::read`. `seal::tests::the_sweep_enumerates_only_the_tables_the
    // _query_named` is what pins it.
    for (idx, (table, seg)) in segments_to_verify(dir, tables).into_iter().enumerate() {
        if sweep_out_of_budget(idx, deadline, dir) {
            tracing::warn!(
                "segment integrity sweep ran out of its query's time budget before checking every \
                 segment named by {tables:?} - what it found so far still applies, the rest is \
                 unverified for this request"
            );
            break;
        }
        #[cfg(test)]
        {
            *test_sweep_segments_processed()
                .lock()
                .unwrap()
                .entry(dir.to_path_buf())
                .or_insert(0) += 1;
        }
        let path = segment_path(dir, &seg.file, &seg.hash);
        // An absent file is not corruption, and `define_views` already skips it by existence.
        if !path.exists() {
            continue;
        }
        // An unreadable segment is not intact: it cannot serve rows either way, and saying
        // "fine" about a file we could not read is the failure this whole issue is about.
        let intact =
            std::fs::read(&path).is_ok_and(|bytes| hex::encode(Sha256::digest(&bytes)) == seg.hash);
        if !intact {
            tracing::error!(
                "segment {} for table {table} does not match its content address - dropping it \
                 from this query (cold data reduced). Restart to quarantine it, or re-seal the \
                 range to restore it.",
                seg.file
            );
            bad.insert(seg.hash.clone());
        }
    }
    bad
}

/// The segments [`segments_failing_verification`] is allowed to read: those belonging to a table in
/// `tables` (matched case-insensitively, as DuckDB matches identifiers). Manifest-only, no file IO -
/// which is what makes the sweep's cost bound testable without timing anything.
fn segments_to_verify(dir: &Path, tables: &BTreeSet<String>) -> Vec<(String, Segment)> {
    if tables.is_empty() {
        return Vec::new();
    }
    let Ok(manifest) = load_manifest(dir) else {
        return Vec::new();
    };
    // Both sides are lowercased here rather than trusting the caller to have done it: a table name
    // that arrives shouted would otherwise match nothing, and the failure mode of that is silent -
    // the query loses its reduction and nobody sees a difference except in the answer.
    let wanted: BTreeSet<String> = tables.iter().map(|t| t.to_ascii_lowercase()).collect();
    manifest
        .tables
        .iter()
        .filter(|(table, _)| wanted.contains(&table.to_ascii_lowercase()))
        .flat_map(|(table, segs)| segs.iter().map(move |s| (table.clone(), s.clone())))
        .collect()
}

/// Startup integrity pass: verify every manifest segment's file exists and its bytes hash to the
/// recorded content address. A file that's missing, unreadable, or hash-mismatched is corrupt or
/// tampered with - quarantine it (move to a sibling `quarantine/` dir so `define_views` skips it) and
/// log loudly, then continue. A corrupt segment must *reduce* a table's cold data, never crash-loop the
/// node. Sealed data is immutable and content-addressed, so a hash mismatch is unambiguous corruption.
/// Returns the number of segments quarantined. Best-effort - never fatal (an IO error just logs).
pub fn verify_and_quarantine(dir: &Path) -> Result<usize> {
    let manifest = load_manifest(dir)?;
    // Sibling of `segments/`, deliberately *outside* it - nothing globs or SQL-reads the quarantine.
    let quarantine = dir.join("quarantine");
    let mut quarantined = 0usize;

    for (table, segs) in &manifest.tables {
        for s in segs {
            let path = segment_path(dir, &s.file, &s.hash);
            // **Never quarantine a shared segment** (RFC-0033 §11a). Quarantine *moves* the file, and
            // in the shared store those bytes belong to every dataset referencing them - one nest's
            // integrity pass must not yank data out from under its neighbours. A corrupt shared
            // segment is reported loudly and left in place; the operator decides, with the knowledge
            // that more than one dataset is affected.
            let is_shared = shared_store(dir).is_some_and(|st| path.starts_with(&st));
            let reason = match std::fs::read(&path) {
                Ok(bytes) if hex::encode(Sha256::digest(&bytes)) == s.hash => continue, // intact
                Ok(_) => "hash mismatch (corrupt or tampered)",
                // Already gone from disk - nothing to move; `define_views` skips it. Not counted.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => "unreadable",
            };
            if is_shared {
                tracing::error!(
                    "shared segment {} for table {table} is {reason} - left in place because other \
                     datasets reference it. Remove it deliberately once you know what else breaks.",
                    s.hash
                );
                continue;
            }
            std::fs::create_dir_all(&quarantine).ok();
            let dest = quarantine.join(&s.file);
            match std::fs::rename(&path, &dest) {
                Ok(()) => {
                    tracing::error!(
                        "quarantined segment {} for table {table} ({reason}) → {} - cold data for this \
                         table is reduced; re-seal the range to restore it",
                        s.file,
                        dest.display()
                    );
                    quarantined += 1;
                }
                Err(e) => tracing::error!(
                    "segment {} for {table} is {reason} but could not be quarantined: {e}",
                    s.file
                ),
            }
        }
    }
    if quarantined > 0 {
        tracing::error!(
            "startup integrity: quarantined {quarantined} corrupt segment(s) - data is reduced, node \
             continues; investigate disk health and re-seal the affected ranges"
        );
    }
    Ok(quarantined)
}

fn save_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let raw = serde_json::to_string_pretty(manifest)?;
    // The manifest is the segment catalogue - the crown jewels of a `kill -9`-survivable single binary
    // (a half-written `manifest.json` orphans every otherwise-fine `.parquet` and fails all cold reads,
    // deadlock-review finding M8). Write a sibling temp file then rename it over the target: `rename`
    // is atomic on the same filesystem, so a reader/crash sees either the old manifest or the new one,
    // never a torn one.
    let path = manifest_path(dir);
    let tmp = path.with_extension("json.tmp");
    // COR-9: fsync the temp file's *bytes* before the rename, and the directory *entry* after - so the
    // atomic rename survives power loss, not just process death. Without the fsyncs a `rename` can be
    // reordered before the data hits disk, exposing a torn/empty manifest that orphans the segments.
    {
        let f = std::fs::File::create(&tmp).context("failed to create manifest temp")?;
        use std::io::Write;
        (&f).write_all(raw.as_bytes())
            .context("failed to write manifest temp")?;
        f.sync_all().context("failed to fsync manifest temp")?;
    }
    std::fs::rename(&tmp, &path).context("failed to install manifest")?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all(); // best-effort dir fsync (unsupported on some platforms)
    }
    Ok(())
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(SEGMENTS_DIR).join(MANIFEST_FILE)
}

#[cfg(test)]
mod sealed_rows {
    use super::*;
    use crate::registry::{
        ColumnSchema, DecodedRow, TableKind, TableSchema, Value as DecodedValue,
    };

    fn schema() -> TableSchema {
        let mut columns = crate::registry::implicit_columns(true);
        columns.extend(
            [
                ("from", "address", "address"),
                ("to", "address", "address"),
                ("value", "uint256", "word32"),
                ("ok", "bool", "bool"),
                ("memo", "string", "string"),
            ]
            .iter()
            .map(|(name, sol, storage)| ColumnSchema {
                name: (*name).to_string(),
                sol_type: (*sol).to_string(),
                storage: (*storage).to_string(),
                indexed: false,
            }),
        );
        TableSchema {
            table: "usdc__transfer".into(),
            alias: "usdc".into(),
            kind: TableKind::Event,
            event: String::new(),
            topic0: String::new(),
            function: String::new(),
            selector: String::new(),
            columns,
        }
    }

    fn row(block: u64, log_index: u64, value: alloy_primitives::U256) -> DecodedRow {
        DecodedRow {
            table: "usdc__transfer".into(),
            params: vec![
                ("from".into(), DecodedValue::Address([0x11; 20])),
                ("to".into(), DecodedValue::Address([0x22; 20])),
                (
                    "value".into(),
                    DecodedValue::Word32(value.to_be_bytes::<32>()),
                ),
                ("ok".into(), DecodedValue::Bool(true)),
                // A string that looks exactly like the uint256 beside it. Only the schema separates
                // them, and a reader that lost the distinction would still round-trip through JSON.
                ("memo".into(), DecodedValue::Str("7".into())),
            ],
            block_number: block,
            block_hash: "0xbh".into(),
            block_timestamp: 1_700_000_000 + block,
            timestamps: true,
            log_index,
            tx_hash: "0xtx".into(),
            address: "0xaa".into(),
        }
    }

    /// **The property the warm-restart seed rests on** (RFC-0041 §5.3, nuthatch#864): a row that has
    /// been sealed to Parquet and read back is the *same row* the ingest path produced.
    ///
    /// It matters because the two representations genuinely differ - `rows_to_batch` writes a
    /// `uint256` as a decimal string and a `bool` as the text `"true"` - so equality here is a claim
    /// about the conversion, not a tautology. If it did not hold, a seeded entity and a live one
    /// would key on different rows, and a retraction would stop cancelling its insertion.
    #[test]
    fn a_sealed_row_reads_back_as_the_row_that_was_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let schema = schema();
        let original: Vec<DecodedRow> = vec![
            row(10, 0, alloy_primitives::U256::from(7u64)),
            row(10, 1, alloy_primitives::U256::MAX),
            row(11, 0, alloy_primitives::U256::ZERO),
        ];

        let json: Vec<String> = original.iter().map(|r| r.to_json().to_string()).collect();
        seal_range(dir.path(), &json, 10, 11)
            .unwrap()
            .expect("something sealed");

        let back = read_table_rows(dir.path(), &schema).unwrap();
        assert_eq!(back, original, "sealed and live must be the same rows");
    }

    /// Segments are read in block order however the manifest happens to list them. An entity folds a
    /// commutative relation, so order does not change its answer - but a seed that also replays a
    /// hot tail has to know where the sealed part ended, and "the last row read" is only that if the
    /// rows came back in order.
    #[test]
    fn segments_come_back_in_block_order_not_manifest_order() {
        let dir = tempfile::tempdir().unwrap();
        // One segment per seal, or there is only one segment and no order to get wrong (#1150).
        test_set_table_floor(dir.path(), 0);
        let schema = schema();
        // Three, not two: with two, reversing the manifest happens to produce the sorted order, so
        // a reader that reversed instead of sorting would pass. Sealed 20, then 10, then 30.
        for (from, to, block) in [(20u64, 20u64, 20u64), (10, 10, 10), (30, 30, 30)] {
            let json = vec![row(block, 0, alloy_primitives::U256::from(block))
                .to_json()
                .to_string()];
            seal_range(dir.path(), &json, from, to).unwrap();
        }
        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(
            manifest.tables["usdc__transfer"]
                .iter()
                .map(|s| s.from_block)
                .collect::<Vec<_>>(),
            vec![20, 10, 30],
            "the premise: the manifest lists them out of order, and not merely reversed"
        );

        let back = read_table_rows(dir.path(), &schema).unwrap();
        assert_eq!(
            back.iter().map(|r| r.block_number).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    /// A sealed row missing a column the schema declares must be refused, not read as an empty
    /// string. `rows_to_batch` writes a missing key as a Parquet null, and an empty string would go
    /// on to decode as a zero-length address - a silently wrong row rather than a loud one.
    #[test]
    fn a_null_column_in_a_sealed_row_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let complete = row(10, 0, alloy_primitives::U256::from(7u64)).to_json();
        let mut partial = complete.as_object().unwrap().clone();
        partial.remove("memo");
        // Two rows so the column exists in the batch and is null for one of them.
        let json = vec![complete.to_string(), Value::Object(partial).to_string()];
        seal_range(dir.path(), &json, 10, 10).unwrap().unwrap();

        let err = format!(
            "{:#}",
            read_table_rows(dir.path(), &schema()).expect_err("a null column is not a value")
        );
        assert!(err.contains("null"), "{err}");
    }

    /// A table with no sealed segments is an empty read, not an error. A nest that has sealed nothing
    /// yet is the ordinary case on a young nest, and treating it as a failure would make the seed
    /// refuse exactly the nests it costs least to build.
    #[test]
    fn a_table_with_nothing_sealed_reads_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        assert!(read_table_rows(dir.path(), &schema()).unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use std::fs::File;

    fn transfer(block: u64, li: u64, value: &str) -> String {
        format!(
            r#"{{"table":"usdc__transfer","from":"0xaaaa","to":"0xbbbb","value":"{value}","block_number":{block},"tx_hash":"0xcc","log_index":{li}}}"#
        )
    }
    fn approval(block: u64, li: u64) -> String {
        format!(
            r#"{{"table":"usdc__approval","owner":"0xaaaa","spender":"0xdddd","value":"1","block_number":{block},"tx_hash":"0xcc","log_index":{li}}}"#
        )
    }

    /// The table these fixtures seal into, as the sweep's reachability bound would name it: what a
    /// query over `usdc__transfer` is allowed to make the sweep read.
    fn usdc() -> BTreeSet<String> {
        ["usdc__transfer".to_string()].into_iter().collect()
    }

    /// **Issue #433, the cost bound the review sent back.** The sweep may read only the segments of
    /// the tables the failed query named - so the enumeration is a separate, IO-free step and this
    /// asserts it directly. The hashing loop can only touch what this hands it, which is why the
    /// bound is structural rather than a filter that could later drift below the `fs::read`.
    ///
    /// A behavioural test cannot see this: sweeping a table the query never named changes no answer,
    /// only cost. That is exactly how a cost bound rots quietly.
    #[test]
    fn the_sweep_enumerates_only_the_tables_the_query_named() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(100, 0, "5")], 100, 100).unwrap();
        seal_range(dir.path(), &[approval(101, 0)], 101, 101).unwrap();
        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(manifest.tables.len(), 2, "two tables, one segment each");

        let named = |t: &str| -> Vec<String> {
            segments_to_verify(dir.path(), &[t.to_string()].into_iter().collect())
                .into_iter()
                .map(|(table, _)| table)
                .collect()
        };
        assert_eq!(
            named("usdc__transfer"),
            vec!["usdc__transfer".to_string()],
            "a query over one table must not make the other one's segments readable"
        );
        assert_eq!(named("usdc__approval"), vec!["usdc__approval".to_string()]);
        // DuckDB matches identifiers case-insensitively and the AST reports the name as written, so
        // a shouted table name must still find its own segments and no others.
        assert_eq!(named("USDC__TRANSFER"), vec!["usdc__transfer".to_string()]);
        assert!(
            named("no_such_table").is_empty(),
            "a name the nest does not have reaches nothing"
        );
        assert!(
            segments_to_verify(dir.path(), &BTreeSet::new()).is_empty(),
            "a query that names no table at all - the 27-byte `SELECT CAST('x' AS INTEGER)` the \
             review measured - must reach no segment whatsoever"
        );
    }

    /// **Issue #433.** The serving-path discriminator must catch page corruption that every cheap
    /// DuckDB probe waves through, and must not accuse a healthy segment.
    ///
    /// The fixture asserts its own premise. Corrupting the data region while leaving the footer and
    /// magic bytes intact is the whole condition - if the file stopped binding it would be #430's
    /// case, already covered, and this test would be proving something else under this name. So it
    /// checks `read_parquet` still binds the corrupt file before asserting the hash catches it. That
    /// gap between "binds" and "hashes wrong" is exactly the ground #433 sits on.
    #[test]
    fn segments_failing_verification_catches_page_corruption_that_still_binds() {
        let dir = tempfile::tempdir().unwrap();
        // One segment per seal: this is about a damaged file, not about the table floor (#1150).
        test_set_table_floor(dir.path(), 0);
        seal_range(dir.path(), &[transfer(100, 0, "5")], 100, 100).unwrap();
        seal_range(dir.path(), &[transfer(101, 0, "7")], 101, 101).unwrap();
        let segs = load_manifest(dir.path()).unwrap().tables["usdc__transfer"].clone();
        assert_eq!(segs.len(), 2);

        // Nothing is wrong yet, and saying so is half the test: a discriminator that always answered
        // "corrupt" would reduce every table on the first execution error in a healthy nest.
        assert!(
            segments_failing_verification(dir.path(), &usdc(), None).is_empty(),
            "a healthy tree must accuse nothing"
        );

        // Destroy the pages of the block-101 segment, keeping footer and magic bytes.
        let victim = segs.iter().find(|s| s.from_block == 101).unwrap();
        let path = segment_path(dir.path(), &victim.file, &victim.hash);
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        let footer_len = u32::from_le_bytes(bytes[len - 8..len - 4].try_into().unwrap()) as usize;
        let end = len - 8 - footer_len;
        assert!(end > 4, "the fixture needs a data region to corrupt");
        bytes[4..end].fill(0xFF);
        std::fs::write(&path, &bytes).unwrap();

        // **The fixture is the condition it names.** Still a Parquet file as far as binding goes.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        assert!(
            conn.prepare(&format!(
                "SELECT 1 FROM read_parquet(['{}'], union_by_name=true) LIMIT 0",
                path.display()
            ))
            .is_ok(),
            "if this no longer binds, the fixture has become #430's footer-corrupt case and this \
             test has stopped testing #433"
        );

        let bad = segments_failing_verification(dir.path(), &usdc(), None);
        assert_eq!(
            bad.len(),
            1,
            "exactly the corrupt segment, and not its healthy sibling"
        );
        assert!(bad.contains(&victim.hash), "and it is the one we corrupted");
    }

    /// **Issue #433, and the reason there is no cache in `segments_failing_verification`.**
    ///
    /// A verdict about a segment's bytes must not be keyed on the segment's timestamp. Measured on
    /// this box (btrfs, Linux 6.12): rewriting a file in place with the same length inside one kernel
    /// timer tick leaves `st_mtime_ns` **byte-identical**, because writes are stamped from a per-tick
    /// cached clock. So a `(mtime, len)` memo cannot see an in-place same-length overwrite - exactly
    /// the corruption this is for.
    ///
    /// I know this because I wrote that memo, and then wrote in its doc comment that the test above
    /// would catch it coming back. **It would not**: mutating the memo back in left the whole suite
    /// green, because the two sweeps in that test happen to straddle a tick. A claim about a
    /// mechanism, with nothing behind it, in the fix for claims with nothing behind them.
    ///
    /// So this constructs the condition deterministically instead of hoping for it: corrupt the bytes
    /// and put the original timestamp back, which is what a same-tick overwrite does anyway. Linux
    /// only - it needs GNU `touch -d @<epoch.nanos>` - and CI is Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_corrupt_segment_is_caught_even_when_its_mtime_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(200, 0, "9")], 200, 200).unwrap();
        let seg = load_manifest(dir.path()).unwrap().tables["usdc__transfer"][0].clone();
        let path = segment_path(dir.path(), &seg.file, &seg.hash);

        // Ask once while it is healthy: this is what would populate any cache.
        assert!(segments_failing_verification(dir.path(), &usdc(), None).is_empty());

        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        let footer_len = u32::from_le_bytes(bytes[len - 8..len - 4].try_into().unwrap()) as usize;
        bytes[4..len - 8 - footer_len].fill(0xFF);
        std::fs::write(&path, &bytes).unwrap();

        // Put the clock back, so the file is byte-different and timestamp-identical.
        let d = before.duration_since(std::time::UNIX_EPOCH).unwrap();
        let ok = std::process::Command::new("touch")
            .arg("-d")
            .arg(format!("@{}.{:09}", d.as_secs(), d.subsec_nanos()))
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "the fixture needs GNU touch to restore the timestamp");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "the whole point of this test is that the timestamp did not move"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            len as u64,
            "and neither did the length"
        );

        assert_eq!(
            segments_failing_verification(dir.path(), &usdc(), None).len(),
            1,
            "corruption must be caught from the bytes - a verdict cached on (mtime, len) would call \
             this segment intact, which is the failure this whole issue is about"
        );
    }

    /// **Issue #476, tightened for #529.** Concurrent callers naming the same tables must share one
    /// sweep, not each pay to hash the same segments - the amplifier the issue names:
    /// `SQL_MAX_CONCURRENCY` permits held by identical repeated requests, each currently paying the
    /// full cost on its own.
    ///
    /// #529: the original made this deterministic with a fixed 150ms per-segment delay, keeping the
    /// leader busy long enough - it hoped - for the three followers to be scheduled and find its slot
    /// still in the map. That raced real thread scheduling rather than the filesystem, and at load
    /// average 34 a follower can sit unscheduled well past 150ms, arrive to find the leader already
    /// gone, and start an uncoalesced sweep of its own - `cargo test --lib` was red on a busy dev box
    /// and green on a quiet one. A registration barrier (`test_set_sweep_registration_barrier`)
    /// removes the race structurally instead of widening the margin: every one of the four callers
    /// blocks inside `segments_failing_verification` until all four have made their leader-or-follower
    /// decision, so there is no wall-clock window in which a follower can miss the leader's slot no
    /// matter how the threads are scheduled.
    #[test]
    fn concurrent_identical_sweeps_coalesce_into_one() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(100, 0, "5")], 100, 100).unwrap();

        test_set_sweep_registration_barrier(dir.path(), 4);

        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let dir = dir.path().to_path_buf();
                let tables = usdc();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    segments_failing_verification(&dir, &tables, None)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        test_clear_sweep_registration_barrier(dir.path());

        for r in &results {
            assert_eq!(
                r, &results[0],
                "coalesced callers must all see the same answer"
            );
        }
        assert_eq!(
            test_sweep_start_count(dir.path()),
            1,
            "4 concurrent callers naming the same tables must pay for one sweep, not 4"
        );
    }

    /// **Issue #476, the other half - tightened for #529.** A sweep must stop at its own deadline
    /// rather than hash every named segment regardless of how long that takes - so a query's time
    /// budget bounds the sweep it triggers, not just the query execution either side of it.
    ///
    /// #529: the original raced an 80ms-per-segment `thread::sleep` against a 100ms deadline and then
    /// asserted `elapsed < 220ms` to prove the loop actually stopped rather than merely returning an
    /// empty set by chance. At load average 34 that margin (one segment's worth) evaporates: a
    /// descheduled thread can wake up arbitrarily later than the 80ms it asked for, and the elapsed
    /// wall-clock assertion above went red on its own even when the early exit itself was correct.
    /// This pins the same property without a clock in it at all: `test_set_sweep_expire_after_checks`
    /// tells the sweep loop directly that its budget is spent after the first check, and
    /// `test_sweep_segments_processed_count` proves by construction (not by inference from elapsed
    /// time) that only the first of the three segments was ever read - the corrupt one, last in block
    /// order, is never reached.
    #[test]
    fn sweep_stops_at_its_deadline_instead_of_running_to_completion() {
        let dir = tempfile::tempdir().unwrap();
        // Three segments, so the deadline has something to stop short of (#1150).
        test_set_table_floor(dir.path(), 0);
        for (block, value) in [(100u64, "1"), (101, "2"), (102, "3")] {
            seal_range(dir.path(), &[transfer(block, 0, value)], block, block).unwrap();
        }
        let segs = load_manifest(dir.path()).unwrap().tables["usdc__transfer"].clone();
        assert_eq!(segs.len(), 3);
        let victim = segs.iter().max_by_key(|s| s.from_block).unwrap();
        let path = segment_path(dir.path(), &victim.file, &victim.hash);
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        let footer_len = u32::from_le_bytes(bytes[len - 8..len - 4].try_into().unwrap()) as usize;
        let end = len - 8 - footer_len;
        assert!(end > 4, "the fixture needs a data region to corrupt");
        bytes[4..end].fill(0xFF);
        std::fs::write(&path, &bytes).unwrap();

        test_set_sweep_expire_after_checks(dir.path(), 1);
        // Still a real, non-expired deadline - a mutation that dropped the deadline argument
        // entirely (passed `None`) would let the sweep run unconstrained and reach the corrupt
        // segment regardless of the check-count override, which `bad.is_empty()` below would catch.
        let deadline = Some(Instant::now() + Duration::from_secs(3600));
        let bad = segments_failing_verification(dir.path(), &usdc(), deadline);
        test_set_sweep_expire_after_checks(dir.path(), -1);

        assert_eq!(
            test_sweep_segments_processed_count(dir.path()),
            1,
            "the budget allows exactly one check to pass; a second segment being read means the \
             early exit did not actually happen"
        );
        assert!(
            bad.is_empty(),
            "the deadline must land before the corrupt (last) segment is ever read, or this proves \
             nothing about stopping early"
        );
    }

    #[test]
    fn verify_quarantines_a_corrupt_segment_and_leaves_intact_ones() {
        let dir = tempfile::tempdir().unwrap();
        // One segment per seal: this is about a damaged file, not about the table floor (#1150).
        test_set_table_floor(dir.path(), 0);
        seal_range(dir.path(), &[transfer(100, 0, "5")], 100, 100).unwrap();
        seal_range(dir.path(), &[transfer(101, 0, "7")], 101, 101).unwrap();
        let manifest = load_manifest(dir.path()).unwrap();
        let segs = &manifest.tables["usdc__transfer"];
        assert_eq!(segs.len(), 2);

        // A clean tree quarantines nothing.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 0);

        // Corrupt the first segment's bytes (simulate disk rot / tampering).
        let bad = dir.path().join(SEGMENTS_DIR).join(&segs[0].file);
        std::fs::write(&bad, b"not a parquet file anymore").unwrap();

        // Verify quarantines exactly the corrupt one; the intact one stays put.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 1);
        assert!(!bad.exists(), "corrupt file moved out of segments/");
        assert!(
            dir.path().join("quarantine").join(&segs[0].file).exists(),
            "corrupt file is in quarantine/"
        );
        assert!(
            dir.path().join(SEGMENTS_DIR).join(&segs[1].file).exists(),
            "intact segment untouched"
        );
        // Idempotent: the already-quarantined (now-missing) segment isn't re-counted.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 0);
    }

    #[test]
    fn seals_each_table_to_its_own_segment() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            transfer(100, 0, "5"),
            transfer(100, 1, "7"),
            approval(101, 0),
        ];
        let summary = seal_range(dir.path(), &rows, 100, 101).unwrap().unwrap();
        assert_eq!(summary.tables, 2); // transfer + approval
        assert_eq!(summary.rows, 3);

        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(manifest.tables["usdc__transfer"].len(), 1);
        assert_eq!(manifest.tables["usdc__transfer"][0].rows, 2);
        assert_eq!(manifest.tables["usdc__approval"][0].rows, 1);

        // The transfer segment reads back with the right rows.
        let seg = &manifest.tables["usdc__transfer"][0];
        let file = File::open(dir.path().join(SEGMENTS_DIR).join(&seg.file)).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap();
        let total: usize = reader.map(|b| b.unwrap().num_rows()).sum();
        assert_eq!(total, 2);
    }

    /// #842 - the RFC-0033 §11a shared-store arm had no test at all.
    ///
    /// Recovered from the 2026-08-24 nightly mutation artifact, which found `delete ! in
    /// seal_range_with_snapshot` surviving and was then cancelled before it could report it. The
    /// guard is `if !shared.exists()`; with the `!` removed a *new* shared segment is never written
    /// and only an already-present one is rewritten. Every test passed, because nothing sealed into
    /// a shared store and then looked for the file.
    ///
    /// This is the arm two mounts of one NID depend on, so the failure it admits is a manifest
    /// listing a segment with no bytes behind it, on the dataset that by definition has more than
    /// one nest reading it.
    #[test]
    fn a_shared_store_receives_the_segment_bytes() {
        // `shared_store` is derived by convention: a dataset at `<root>/data/<nid>` shares
        // `<root>/segments`. A solo nest is not under a `data/` parent and takes the other arm.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(crate::runtime::DATA_DIR).join("nid0");
        std::fs::create_dir_all(&dir).unwrap();
        let store = shared_store(&dir).expect("a dataset under data/ has a shared store");
        assert_eq!(store, root.path().join(SEGMENTS_DIR));
        assert!(
            !store.exists(),
            "the store does not exist before the first seal"
        );

        seal_range(&dir, &[transfer(100, 0, "5")], 100, 100).unwrap();

        // The manifest names one segment; its bytes must be in the shared store.
        let manifest = load_manifest(&dir).unwrap();
        let seg = &manifest.tables["usdc__transfer"][0];
        let shared = store.join(format!("{}.parquet", seg.hash));
        assert!(
            shared.is_file(),
            "the shared store holds no {}.parquet - the Some(store) arm wrote nothing. \
             Store contains: {:?}",
            seg.hash,
            std::fs::read_dir(&store)
                .map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
        );
        assert!(
            std::fs::metadata(&shared).unwrap().len() > 0,
            "the shared segment is empty"
        );

        // And it went there *instead* of beside the nest - the two arms are exclusive.
        assert!(
            !dir.join(SEGMENTS_DIR).join(&seg.file).exists(),
            "a shared dataset must not also write the per-nest copy"
        );

        // The reader agrees with the writer about where the bytes are.
        assert_eq!(segment_path(&dir, &seg.file, &seg.hash), shared);
    }

    /// The other half of the same guard: re-sealing identical rows must not disturb the stored copy.
    /// This is what the `!` buys, and it is worth pinning separately so a fix for the test above
    /// cannot simply drop the condition.
    #[test]
    fn re_sealing_identical_rows_leaves_the_shared_segment_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(crate::runtime::DATA_DIR).join("nid0");
        std::fs::create_dir_all(&dir).unwrap();
        seal_range(&dir, &[transfer(100, 0, "5")], 100, 100).unwrap();

        let manifest = load_manifest(&dir).unwrap();
        let seg = manifest.tables["usdc__transfer"][0].clone();
        let shared = shared_store(&dir)
            .unwrap()
            .join(format!("{}.parquet", seg.hash));
        let before = std::fs::read(&shared).unwrap();

        // Content-addressed idempotency: the same rows again are the same segment.
        seal_range(&dir, &[transfer(100, 0, "5")], 100, 100).unwrap();

        assert_eq!(
            std::fs::read(&shared).unwrap(),
            before,
            "a re-seal changed the shared segment's bytes"
        );
        assert_eq!(
            load_manifest(&dir).unwrap().tables["usdc__transfer"].len(),
            1,
            "a re-seal double-listed the segment in the manifest"
        );
    }

    #[test]
    fn empty_range_seals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(seal_range(dir.path(), &[], 1, 2).unwrap().is_none());
    }

    #[test]
    fn content_address_is_deterministic() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let rows = vec![transfer(1, 0, "1")];
        seal_range(dir1.path(), &rows, 1, 1).unwrap();
        seal_range(dir2.path(), &rows, 1, 1).unwrap();
        let a = &load_manifest(dir1.path()).unwrap().tables["usdc__transfer"][0].hash;
        let b = &load_manifest(dir2.path()).unwrap().tables["usdc__transfer"][0].hash;
        assert_eq!(a, b); // same rows in → same content address
    }

    /// RFC-0004 §1 path-equivalence: rows sealed *directly* (the seal-direct backfill path) and the
    /// same rows sealed *after a redb round-trip* (the hot-then-seal path) yield byte-identical
    /// segments. `seal_range` is the one shared writer, so the two backfill paths are provably the
    /// same bytes - the determinism claim the optimisation rests on.
    #[test]
    fn seal_direct_matches_seal_via_hot_store() {
        use crate::store::Store;
        let rows = vec![
            transfer(100, 0, "5"),
            transfer(100, 1, "7"),
            approval(101, 0),
            transfer(102, 0, "9"),
        ];

        // Path A - direct: seal the decoded rows as-is.
        let da = tempfile::tempdir().unwrap();
        seal_range(da.path(), &rows, 100, 102).unwrap();

        // Path B - via hot store: write to redb, read the range back, then seal.
        let db = tempfile::tempdir().unwrap();
        let store = Store::open(&db.path().join("hot.redb")).unwrap();
        for r in &rows {
            let v: Value = serde_json::from_str(r).unwrap();
            let key = Store::entity_key(
                v["block_number"].as_u64().unwrap(),
                v["log_index"].as_u64().unwrap(),
            );
            store.put_entity(&key, r).unwrap();
        }
        let readback = store.entities_in_range(100, 102).unwrap();
        seal_range(db.path(), &readback, 100, 102).unwrap();

        // Same tables, same per-table content hashes.
        let ma = load_manifest(da.path()).unwrap();
        let mb = load_manifest(db.path()).unwrap();
        assert_eq!(
            ma.tables.keys().collect::<Vec<_>>(),
            mb.tables.keys().collect::<Vec<_>>()
        );
        for (table, segs) in &ma.tables {
            assert_eq!(
                segs[0].hash, mb.tables[table][0].hash,
                "segment hash differs for {table} between direct and via-hot-store paths"
            );
        }
    }

    // ---------------------------------------------------------------------------------------
    // #1150 - a table under the floor is sealed provisionally and folded into its own next cut.
    //
    // Every test here seals through `seal_range` into a real directory and reads the manifest and
    // the files back, because the property is about what is on disk: how many files a sparse table
    // gets, whether a busy one is untouched, and whether the folded bytes are the bytes a single
    // seal would have written.
    // ---------------------------------------------------------------------------------------

    fn transfers(from_block: u64, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| transfer(from_block + (i / 4) as u64, (i % 4) as u64, &i.to_string()))
            .collect()
    }

    fn only(manifest: &Manifest, table: &str) -> Segment {
        let segs = &manifest.tables[table];
        assert_eq!(
            segs.len(),
            1,
            "{table} has {} segments where exactly one was expected: {segs:?}",
            segs.len()
        );
        segs[0].clone()
    }

    fn parquet_files(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir.join(SEGMENTS_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".parquet"))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn a_table_under_the_floor_is_sealed_provisionally_and_still_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            transfer(10, 0, "1"),
            transfer(10, 1, "2"),
            transfer(11, 0, "3"),
        ];
        assert!(rows.len() < SEAL_TABLE_FLOOR);
        let summary = seal_range(dir.path(), &rows, 10, 11).unwrap().unwrap();
        assert_eq!((summary.tables, summary.rows), (1, 3));

        let seg = only(&load_manifest(dir.path()).unwrap(), "usdc__transfer");
        assert!(
            seg.provisional,
            "three rows is under SEAL_TABLE_FLOOR={SEAL_TABLE_FLOOR}, so this segment must be \
             marked for folding rather than sealed as final"
        );
        assert_eq!((seg.from_block, seg.to_block, seg.rows), (10, 11, 3));
        // Queryable exactly as a final segment is: the typed reader sees it.
        let back = read_segment_rows(&dir.path().join(SEGMENTS_DIR).join(&seg.file)).unwrap();
        assert_eq!(
            back.iter()
                .map(|r| r["block_number"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![10, 10, 11]
        );
    }

    #[test]
    fn a_provisional_segment_folds_into_the_tables_next_cut() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(10, 0, "1")], 10, 10).unwrap();
        let first = only(&load_manifest(dir.path()).unwrap(), "usdc__transfer");
        let summary = seal_range(
            dir.path(),
            &[transfer(11, 0, "2"), transfer(12, 0, "3")],
            11,
            12,
        )
        .unwrap()
        .unwrap();

        let m = load_manifest(dir.path()).unwrap();
        let seg = only(&m, "usdc__transfer");
        assert!(seg.provisional, "still under the floor, still folding");
        assert_eq!(
            (seg.from_block, seg.to_block, seg.rows),
            (10, 12, 3),
            "the folded segment spans from the first provisional cut to this one and holds every row"
        );
        assert_ne!(seg.hash, first.hash);
        assert_eq!(
            summary.rows, 2,
            "rows sealed counts the rows that arrived, not the rows re-written: the metric would \
             otherwise double-count every fold"
        );
        assert_eq!(
            parquet_files(dir.path()),
            vec![seg.file.clone()],
            "the file the fold replaced must be gone from disk, or the fold only moved the small \
             files problem from the manifest to the directory"
        );
        let back = read_segment_rows(&dir.path().join(SEGMENTS_DIR).join(&seg.file)).unwrap();
        assert_eq!(
            back.iter()
                .map(|r| r["block_number"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn a_folded_segment_is_byte_identical_to_sealing_all_its_rows_at_once() {
        // The fold reads the provisional Parquet back and re-seals; if that round trip lost or
        // reshaped anything, the two operators - one who sealed in two cuts, one who sealed in
        // one - would hold different content addresses for the same rows.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = vec![
            transfer(10, 0, "1"),
            transfer(10, 1, "2"),
            approval(10, 2),
            transfer(11, 0, "3"),
        ];
        let second = vec![approval(12, 0), transfer(12, 1, "4")];
        seal_range(a.path(), &first, 10, 11).unwrap();
        seal_range(a.path(), &second, 12, 12).unwrap();
        let all: Vec<String> = first.iter().chain(second.iter()).cloned().collect();
        seal_range(b.path(), &all, 10, 12).unwrap();

        let ma = load_manifest(a.path()).unwrap();
        let mb = load_manifest(b.path()).unwrap();
        for table in ["usdc__transfer", "usdc__approval"] {
            let folded = only(&ma, table);
            let once = only(&mb, table);
            assert_eq!(
                folded.hash, once.hash,
                "{table}: two cuts then a fold != one seal of the same rows"
            );
            assert_eq!(
                (folded.from_block, folded.to_block, folded.rows),
                (once.from_block, once.to_block, once.rows)
            );
        }
    }

    #[test]
    fn crossing_the_floor_makes_the_segment_final_and_the_next_cut_starts_afresh() {
        let dir = tempfile::tempdir().unwrap();
        let half = SEAL_TABLE_FLOOR / 2;
        seal_range(dir.path(), &transfers(1_000, half), 1_000, 1_999).unwrap();
        assert!(only(&load_manifest(dir.path()).unwrap(), "usdc__transfer").provisional);

        seal_range(dir.path(), &transfers(2_000, half), 2_000, 2_999).unwrap();
        let m = load_manifest(dir.path()).unwrap();
        let sealed = only(&m, "usdc__transfer");
        assert!(
            !sealed.provisional,
            "{} rows is the floor, so the folded segment is final",
            SEAL_TABLE_FLOOR
        );
        assert_eq!(
            (sealed.from_block, sealed.to_block, sealed.rows),
            (1_000, 2_999, SEAL_TABLE_FLOOR)
        );

        // A final segment is never reopened: the next handful of rows starts a new provisional
        // beside it, which is what keeps the sealed layer append-only.
        seal_range(dir.path(), &[transfer(3_000, 0, "x")], 3_000, 3_000).unwrap();
        let m = load_manifest(dir.path()).unwrap();
        let segs = &m.tables["usdc__transfer"];
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert_eq!(segs[0].hash, sealed.hash, "the final segment is untouched");
        assert!(segs[1].provisional);
        assert_eq!(
            (segs[1].from_block, segs[1].to_block, segs[1].rows),
            (3_000, 3_000, 1)
        );
        assert_eq!(parquet_files(dir.path()).len(), 2);
    }

    #[test]
    fn a_busy_table_is_sealed_exactly_as_before_whatever_its_co_tenants_do() {
        // The identity guarantee the floor must not touch: a table that clears the floor at a cut
        // gets the same file, hash and range with a sparse table beside it as without one.
        let with = tempfile::tempdir().unwrap();
        let without = tempfile::tempdir().unwrap();
        let busy = transfers(500, SEAL_TABLE_FLOOR + 7);
        let mut mixed = busy.clone();
        mixed.push(approval(600, 0));
        seal_range(with.path(), &mixed, 500, 600).unwrap();
        seal_range(without.path(), &busy, 500, 600).unwrap();

        let a = only(&load_manifest(with.path()).unwrap(), "usdc__transfer");
        let b = only(&load_manifest(without.path()).unwrap(), "usdc__transfer");
        assert!(!a.provisional && !b.provisional);
        assert_eq!(
            (a.hash, a.file, a.from_block, a.to_block, a.rows),
            (b.hash, b.file, b.from_block, b.to_block, b.rows)
        );
        assert!(
            only(&load_manifest(with.path()).unwrap(), "usdc__approval").provisional,
            "the one-row co-tenant is the one that folds"
        );
    }

    #[test]
    fn re_sealing_identical_rows_into_a_provisional_table_is_still_a_no_op() {
        // The idempotency rule `nuthatch screen` re-audits rely on, checked before the fold: the
        // same rows again must not be folded in a second time as if they were new.
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![transfer(10, 0, "1"), transfer(10, 1, "2")];
        seal_range(dir.path(), &rows, 10, 10).unwrap();
        let before = only(&load_manifest(dir.path()).unwrap(), "usdc__transfer");
        let again = seal_range(dir.path(), &rows, 10, 10).unwrap().unwrap();
        assert_eq!((again.tables, again.rows), (0, 0));
        let after = only(&load_manifest(dir.path()).unwrap(), "usdc__transfer");
        assert_eq!((after.hash, after.rows), (before.hash, 2));
        assert_eq!(parquet_files(dir.path()).len(), 1);
    }

    #[test]
    fn a_fold_that_cannot_install_its_manifest_leaves_the_provisional_file_in_place() {
        // The crash window Jules named on #1153: the replacement file written, the old file
        // removed, and then the process dies before the manifest is installed. The manifest on
        // disk would name a file that is gone and the folded rows would read as missing. So the
        // old file goes only after `save_manifest` returns - and this makes `save_manifest` fail
        // in the middle of a fold, which is the only order in which that property is observable.
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(10, 0, "1")], 10, 10).unwrap();
        let first = only(&load_manifest(dir.path()).unwrap(), "usdc__transfer");
        let first_path = dir.path().join(SEGMENTS_DIR).join(&first.file);
        assert!(first_path.exists());

        // `save_manifest` writes `manifest.json.tmp` then renames it; a directory in the way of
        // the temp file fails the write before anything is installed.
        let tmp = dir.path().join(SEGMENTS_DIR).join("manifest.json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        let err = seal_range(dir.path(), &[transfer(11, 0, "2")], 11, 11).unwrap_err();
        assert!(
            format!("{err:#}").contains("manifest"),
            "the fixture must fail at the manifest, not earlier: {err:#}"
        );
        assert!(
            first_path.exists(),
            "the provisional file was removed before the manifest that drops it was installed - \
             a crash here loses every row in it"
        );
        assert_eq!(
            only(&load_manifest(dir.path()).unwrap(), "usdc__transfer").hash,
            first.hash,
            "the on-disk manifest still names the file that is still there"
        );

        // With the way clear the same fold completes, and only then is the old file gone.
        std::fs::remove_dir(&tmp).unwrap();
        seal_range(dir.path(), &[transfer(11, 0, "2")], 11, 11).unwrap();
        let folded = only(&load_manifest(dir.path()).unwrap(), "usdc__transfer");
        assert_eq!(folded.rows, 2);
        assert!(!first_path.exists());
        assert_eq!(parquet_files(dir.path()), vec![folded.file]);
    }

    #[test]
    fn a_fold_in_a_shared_store_leaves_the_replaced_bytes_for_prune() {
        // RFC-0033 §11a: bytes in the shared store may be another dataset's too, so a fold writes
        // the new file and deletes nothing; the manifest stops referencing the old hash and
        // `nuthatch prune` reclaims it from that.
        let root = tempfile::tempdir().unwrap();
        let nest = root.path().join(crate::runtime::DATA_DIR).join("nid-1");
        std::fs::create_dir_all(&nest).unwrap();
        seal_range(&nest, &[transfer(10, 0, "1")], 10, 10).unwrap();
        let first = only(&load_manifest(&nest).unwrap(), "usdc__transfer");
        seal_range(&nest, &[transfer(11, 0, "2")], 11, 11).unwrap();
        let folded = only(&load_manifest(&nest).unwrap(), "usdc__transfer");
        assert_ne!(first.hash, folded.hash);
        let store = root.path().join(SEGMENTS_DIR);
        assert!(store.join(format!("{}.parquet", first.hash)).exists());
        assert!(store.join(format!("{}.parquet", folded.hash)).exists());
        assert_eq!(folded.rows, 2);
    }

    #[test]
    fn the_floor_is_measured_in_rows_a_table_holds_not_rows_a_cut_carries() {
        // A cut of SEAL_DIRECT_BATCH rows where one table has all but three of them: the busy
        // table is final at that cut, the sparse one is provisional, and stays provisional across
        // as many cuts as it takes to reach the floor - one file, not one per cut.
        let dir = tempfile::tempdir().unwrap();
        let cuts = 5u64;
        for c in 0..cuts {
            let from = 1_000 * (c + 1);
            let mut rows = transfers(from, SEAL_TABLE_FLOOR);
            rows.push(approval(from + 500, 0));
            rows.push(approval(from + 500, 1));
            rows.push(approval(from + 501, 0));
            rows.sort_by_key(|r| {
                let v: Value = serde_json::from_str(r).unwrap();
                (
                    v["block_number"].as_u64().unwrap(),
                    v["log_index"].as_u64().unwrap(),
                )
            });
            seal_range(dir.path(), &rows, from, from + 999).unwrap();
        }
        let m = load_manifest(dir.path()).unwrap();
        assert_eq!(
            m.tables["usdc__transfer"].len(),
            cuts as usize,
            "the busy table gets one final segment per cut, as it always did"
        );
        assert!(m.tables["usdc__transfer"].iter().all(|s| !s.provisional));
        let sparse = only(&m, "usdc__approval");
        assert!(sparse.provisional);
        assert_eq!(
            (sparse.from_block, sparse.to_block, sparse.rows),
            (1_000, 5_999, 3 * cuts as usize),
            "five cuts of three rows is one fifteen-row file, not five three-row files"
        );
        assert_eq!(parquet_files(dir.path()).len(), cuts as usize + 1);
    }
}
