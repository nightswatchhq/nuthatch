# RFC-0052: The mirrored nest - publishing the sealed directory to object storage for external engines

- Status: **Draft. Design only.** S0 (#1258) answered 2026-09-12 against the tree; remaining
  slices wait. Proposed as the delivery half of RFC-0047: that RFC makes the sealed directory a
  contract; this one makes the contract reachable over a network. Every slice is additive - no
  query-syntax change, no segment-format change, no new source of truth. The freeze question is
  §9.
- Author: Pete (drafted 2026-09-09). S0 filled the `[VERIFY]` items that gate S1; remaining
  `[VERIFY]` marks (schema.json shape, Dune namespace) stay in §10.
- Date: 2026-09-09
- Origin: a board research report, "Integrating nuthatch with Dune Analytics" (2026-09-08),
  which ranked a row-insert sink into Dune's table API as the highest-value integration. §1
  records why this RFC disagrees with that ranking and takes the report's own footnote - an
  object-store publish target - as the headline instead.
- Depends on: RFC-0047 (the sealed directory as a contract; C2 catalogue versioning is the
  enrichment this RFC carries, not a prerequisite), [reading-segments.md](../reading-segments.md)
  (the consumer contract, reused verbatim), RFC-0019 §1/§3 (`object_store`, `AWS_*`, credential
  kind **a**, "never in a bundle"), RFC-0009 (content-addressed segments and the catalogue),
  RFC-0012 (the shared store layout, NID), RFC-0033 slice 5 (data identity), RFC-0010 Part B
  (delivery-engine gauges and the "never blocks indexing" rule), RFC-0045 §3 (host-side and
  out-of-band, never in the data path), RFC-0028 (deterministic seal boundaries).
- Blocks: nothing. Enables a later "cold-start from a mirror" RFC (§8) and the Dune-facing view
  emitter (§8).

## Abstract

A sealed segment is plain Parquet, content-addressed, past finality, and never rewritten. A
catalogue names them and is installed atomically. RFC-0047 commits to that as a contract so that
"point another engine at the same directory" is true by design. The directory is still on one
box.

This RFC proposes **mirroring the sealed directory to an object-store prefix** - the catalogue
and the segments it names, nothing else, no new format - so that any engine that reads Parquet
from a bucket (DuckDB, Trino and therefore DuneSQL, Snowflake, BigQuery, Databricks, Spark,
DataFusion) reads a nest's history as an external table, with no row-insert API, no per-row
credits, no upload caps, and no reorg handling on the consumer's side, because nothing unsealed
ever leaves the box.

The mirror is **opt-in, additive, append-only, and idempotent**. With no target configured the
binary makes no network call it does not make today. The publisher is a level-triggered
reconciler (local catalogue versus remote catalogue), woken by each seal, that never sits in the
seal path. The consumer-facing contract is `reading-segments.md` plus one page of deltas.

## §0 - Why this and not a Dune sink

The research report's first recommendation was a nuthatch → Dune sink over `POST
/api/v1/table/{ns}/{name}/insert`. It is feasible - `dune-sync` and `node-indexer` prove the
path - and it is the wrong first move, for four reasons this RFC is built on.

**It is the integration where nuthatch's architecture is the advantage.** Every other indexer
that pushes rows into Dune re-serialises mutable rows out of Postgres. Nuthatch already writes
immutable, content-addressed Parquet at a data-chosen block boundary past finality. Copying an
object is the cheapest thing a computer does; converting it to CSV and posting it through a
1.2 GB-capped, credit-metered, all-or-nothing endpoint is the most expensive way to move the
same bytes.

**It is engine-neutral.** A prefix of Parquet and a JSON catalogue is readable by Dune's Trino,
by Snowflake's external tables, by BigQuery's external tables, by Databricks' `read_files`, and
by another DuckDB. The row-insert sink is readable by Dune. RFC-0047 §0's "escape hatch should be
real" is exactly this: a mirror is the escape hatch with a network in front of it.

**It preserves the non-negotiables.** No phone-home: the publisher talks to one bucket the
operator named, and to nothing when none is named. No mandatory third-party dependency: the
mirror is a *producer* surface; nuthatch never reads from it on the happy path. No gated data
service: `CLAUDE.md` §3 binds the artefact, and a self-hoster who deletes `[publish]` loses
nothing (the RFC-0046 slice-0 test, applied here).

**The sink becomes a thin adapter afterwards.** A team without an Enterprise Dune plan can still
want rows in `dune.<team>.<table>`. Once the mirror exists, that adapter is "read the published
catalogue, post the delta since the last cursor" - a sidecar, not a core feature, and it is out
of scope here (§8).

One more observation, for the record: the report's other bullet-point ideas that consumed Dune
data (Sim balances, Sim IDX) are dead - Dune sunset Sim on 2026-08-01. The Dune → nuthatch
direction is also against RFC-0045 §3's rule that fetching is never in the data path. Nothing in
this RFC pulls from Dune.

## §1 - What is already true

Read against the tree, the same way RFC-0047 §1 did.

**The store abstraction exists.** RFC-0019 §1 shipped `BundleStore` with `FsStore` and an
`object_store`-backed `ObjStore` (S3/MinIO/R2/GCS), configured by `AWS_*` env (`AWS_ENDPOINT`
for non-AWS), on by default since 2026-07-28, verified live against Hetzner Object Storage. It
stores immutable blobs by content address and a thin index of movable pointers. **S0: do not
reuse the trait.** `BundleStore` (`src/distribution.rs`) is `put_blob`/`get_blob` of a whole
`Vec<u8>` under `blobs/<hash>.bundle`, plus `set_ref`/`get_ref` for registry pointers. A
mirror needs streamed Parquet puts, HEAD, checksum-on-put, and `PutMode::Update` on
`manifest.json` at `<dataset>/<table>/<hash>.parquet`. Share `ObjStore::from_locator` and the
`AWS_*` lowercasing; write a thin `object_store` wrapper for the publisher. Credential
plumbing is kind (a). The blob registry stays the blob registry.

**The catalogue is the commit point.** `seal.rs::save_manifest` writes `segments/manifest.json.tmp`,
fsyncs, renames. A file not named by the catalogue is not a sealed segment. A named file that is
missing or hash-mismatched is quarantined at startup. Additive fields use serde defaults. The
catalogue is therefore both the thing to copy and the thing whose install order defines "the
mirror is at sealed block N".

**The shared-store layout is already keyed by hash.** In a runtime, bytes live at
`<runtime>/segments/{hash}.parquet`, shared across every mount that names them, and the
per-dataset catalogue at `data/<nid>/segments/manifest.json` resolves to them by rule. A bucket
prefix can be a runtime root.

**The external-reader contract exists and is gated.** `reading-segments.md` specifies layout,
catalogue schema, ordering, and the 256-bit contract (canonical decimal `Utf8`, `c_dec` /
`c_overflow` never written to Parquet, no silent narrowing). Its resolver is eleven lines of
Python. Two red tests guard it (`sealed_parquet_writes_uint64_counters_and_utf8_uint256_without_dec_companions`,
`bigint_columns_get_decimal_and_overflow_views`). This RFC adds a remote branch to the resolver
and nothing to the types.

**A delivery engine exists and has the right rule.** RFC-0010 Part B's outbox is host-side,
at-least-once, and "a slow endpoint never blocks indexing", with outbox depth and dead-letter
gauges. The publisher is not a webhook and does not ride the outbox (§3.4 says why), but it
inherits the rule and the gauge naming.

**Two catalogue behaviours constrain the mirror.** `provisional: true` marks a segment with fewer
than 1,000 rows at the cut that "the next seal will fold". The catalogue appends, and a later
seal of an earlier range can sit after a newer one. **S0: a non-provisional entry is never
removed.** The only `segments.remove` in the tree is `src/seal.rs` folding a `provisional:
true` row into the next cut (test: "A final segment is never reopened"). `save_manifest` is
tmp + fsync + rename and does not edit the list. `prune.rs` deletes unmounted trees and
unreferenced parquet, never a surviving catalogue entry. v1 does not need generations or
tombstones. A future compaction RFC still cannot delete in place.

## §2 - Goals and non-goals

**Goals**

1. A nest's sealed history is readable by an external Parquet engine from an object-store prefix,
   with correctness equal to reading the local sealed directory, and with the same contract page.
2. Publishing never delays a seal, never holds the single writer, and fits inside the ≤2 GB
   per-cursor budget with a named, bounded reservation.
3. The mirror is append-only and every write is idempotent, so a crash, a retry, a second run of
   the same command, or a re-run from an empty local state after `nest load` converges to the
   same bucket contents.
4. A consumer can tell, from the bucket alone, which nest produced the data (bundle hash, data
   identity, chain), through which block it is complete, and by which nuthatch version.
5. Solo (`nuthatch dev`) and roost (`serve`, RFC-0027/0032) both publish; a roost publishes each
   mounted nest's dataset under its own prefix.

**Non-goals**

- A table format (Iceberg, Delta, Hudi). RFC-0047 §5 rejected these for the local catalogue;
  the same reasons apply remotely, plus one: their Rust write paths are the heaviest dependency
  this could drag into a binary that must stay small. A mirror of today's catalogue is a
  translation away from Iceberg metadata if a consumer ever needs it (§8).
- Publishing the hot tip. Nothing unsealed leaves the box. The mirror's freshness is sealed
  freshness; RFC-0040's dial governs that, not this RFC.
- Publishing authored views, DBSP entities, or `/derived` output. Those are query-layer
  (RFC-0018 §1, RFC-0041), evaluated over hot ∪ cold; the mirror carries only what is sealed.
  Materialising them into the mirror is a later decision with its own reorg story.
- Deleting, compacting, or rewriting anything in the bucket. v1 has no delete path and the
  recommended IAM policy has no `DeleteObject` (§3.7).
- A Dune-specific format, endpoint, or client. Dune is one consumer of a neutral layout (§5).
- Reading from a mirror. Cold-start-from-mirror is §8, a separate RFC.
- Multi-writer. One publisher per dataset prefix, enforced by conditional put (§3.3), same
  single-writer principle the seal loop already has.

## §3 - Design

### 3.1 - The layout is the runtime layout, per table

```
<prefix>/                                 # s3://bucket/path or a directory (FsStore)
  <dataset>/                              # data identity (RFC-0033 s5) - not the NID; see below
    publish.json                          # provenance envelope, §3.5
    manifest.json                         # the catalogue, byte-identical to segments/manifest.json
    schema.json                           # logical types (which Utf8 columns are big ints) [VERIFY name/shape]
    <table>/
      <hash>.parquet                      # one object per catalogued, non-provisional segment
```

Three decisions, and the reasons.

**Per-table prefixes, not one shared content-addressed pool.** The local shared store keys every
segment of every nest by hash in one directory because nuthatch resolves through the catalogue
and never globs. External engines glob: Trino's Hive connector takes a table *location*,
Snowflake an external stage plus pattern, BigQuery a URI wildcard, Databricks a path. A directory
that holds every table's files is unreadable to all of them. The cost is that two nests decoding
the same contract publish the same segment twice, under two prefixes. That is a storage cost, not
a correctness cost, and it is the cost a consumer would pay to copy it anyway.

**No Hive partition directories.** The obvious `block_range=…/` or `block_day=…/` layout is
rejected because a seal range is not a partition value: a segment spans whatever block range the
data-chosen cut produced (RFC-0028), and assigning it to a bucket by `from_block` makes a
pruning engine skip rows that straddle the boundary. That is silent data loss dressed as an
optimisation. Pruning comes from what is already written on every column - Parquet footer
min/max statistics (RFC-0047 §1, `segment-layout.md`) - and from the catalogue's
`from_block`/`to_block`, which every engine in §5 uses. A `chain_id=` directory is not needed
either: a dataset is one chain.

**Keyed by data identity, not by NID.** RFC-0033 slice 5 gave a nest a data identity beside its
package identity so that a cosmetic edit moves the NID and adopts the existing dataset. The
mirror must not fork on a comment change. The prefix is the data identity; `publish.json` carries
the current NID and bundle hash for provenance. **S0:** `blob::Manifest::data_identity()` is
SHA-256 of domain `nuthatch-data-identity-v1\0` plus `schema_version`, `registry_hash`, and
every authored file that `affects_data`, encoded as 64 lowercase hex characters. It is not a
file on disk. A solo `nuthatch dev` nest has `AppState.nid = None` and is not identity-keyed
(`PreparedDataset::without_nid`); the publisher still computes the prefix with
`blob::build_manifest(dir, None)?.data_identity()` from the project directory, no mount
required.

### 3.2 - What is published: catalogued, non-provisional, append-only

The publisher's unit is a **catalogue entry**, never a file it found on disk. Rule, in order:

1. An entry with `provisional: true` is **not published**. The local catalogue promises the next
   seal folds it; publishing it and then folding would leave a globbing consumer double-counting
   or require a delete. Waiting costs at most one seal of freshness for a table with under 1,000
   rows at the cut.
2. Every other entry is published exactly once, at `<dataset>/<table>/<hash>.parquet`. Same hash,
   same key, so re-publishing is a no-op by construction (RFC-0019 §1's dedup argument).
3. An entry is never removed from the mirror. The mirror inherits the catalogue's append-only
   property.

S0 closed the catalogue question: a non-provisional entry is not dropped. If a future
compaction RFC ever does drop one, an append-only mirror would serve stale files to globbing
engines forever. Two acceptable resolutions, neither in v1:

- **Generations.** A catalogue that drops entries publishes under `<dataset>/gen-<n>/…` and
  `publish.json` names the live generation; old generations are left in place (or removed by an
  operator with delete rights). Consumers point at the live generation.
- **Tombstones.** `publish.json` carries a `superseded` list; catalogue-driven readers skip them,
  and the docs state that globbing readers are unsafe on this dataset.

The recommendation, if the verify comes back "entries can be dropped", is generations, because
it keeps the globbing engines - the whole point of §3.1 - correct. A future compaction RFC must
therefore version the published prefix; it cannot delete in place. That constraint is recorded
here so compaction inherits it.

### 3.3 - The commit protocol: files, then verify, then catalogue

A publish run is a reconciliation:

```
local  = read segments/manifest.json            (the atomically installed one)
remote = get <dataset>/manifest.json            (absent on first run)
want   = local entries − provisional
have   = remote entries                          (or empty)
for each entry in want − have, by (from_block, to_block, hash):
    put <dataset>/<table>/<hash>.parquet         streamed from disk, sha256 attached as the
                                                 object checksum where the store supports it,
                                                 else HEAD size check after put
if every put succeeded:
    put <dataset>/schema.json                    (only when changed)
    put <dataset>/manifest.json                  conditional on the remote version (ETag /
                                                 object_store PutMode::Update); refuse on mismatch
    put <dataset>/publish.json                   after the catalogue, with its sha256 and
                                                 sealed_through = max to_block of want
```

Properties this buys:

- **A reader sees a consistent mirror at every instant.** A single-object put is atomic on
  every store in scope; a reader gets the old catalogue or the new one. A file that exists but
  is not yet catalogued is a *real sealed segment* - past finality, immutable - so a globbing
  reader that sees it early sees fresher data, not wrong data. This is the one place the
  local "do not glob" rule can be relaxed, and only because §3.2 guarantees a per-table prefix
  holds nothing but catalogued, non-provisional segments (plus at most one in-flight batch of
  the same). The catalogue is committed before its provenance envelope, so a crash can only
  understate `sealed_through`, never advertise a catalogue that does not yet exist. A consumer
  accepts `publish.json` only when its `catalogue_sha256` matches the `manifest.json` it read;
  any mismatch is the ordinary short publication window, and it re-reads both objects.
- **Idempotent and self-healing.** There is no queue to lose. A crash mid-run leaves files the
  next run finds by hash and skips. A bucket wiped by an operator is rebuilt by the next run.
  `nest load` on a fresh box followed by a backfill converges to the same keys (F-D3 caveat:
  same binary, same bytes; across arrow-rs versions the hash can differ, so a mirror built by
  two nuthatch versions can hold two files for one range only if the local catalogue does -
  which is the §1 verify again).
- **Single writer, enforced.** The conditional put on `manifest.json` makes a second publisher
  for the same dataset fail loudly, the way a fenced write fails in RFC-0022. Two roosts
  publishing one dataset is a configuration error and is reported as one.
- **The remote catalogue is byte-identical to the local one.** So `doctor` compares by hash, a
  consumer can check what it holds against what the operator has, and RFC-0047 C2's
  enrichments (`manifest_version`, `sort_order`, `logical_type`, stats) arrive in the mirror
  the day they land locally, with no change here. The `file` field's `{table}-{hash}.parquet`
  is resolved by the remote rule (§3.6), the same way the shared-store rule already re-maps it.

What it does not do: multipart uploads are streamed from disk with a bounded part buffer;
the publisher never holds a segment in memory. It also never reads the hot store.

### 3.4 - Trigger: level-triggered reconciler, woken by seals

The publisher is a host-side task: `interval` (default 60 s) plus a wake-up on every successful
`save_manifest`. It does not ride the RFC-0010 outbox, deliberately: the outbox carries
*messages* - edges - and a missed edge is a lost delivery. The mirror's state is a *level*: the
catalogue. A reconciler that compares levels recovers from any missed wake-up, any crash, any
prior partial run, for free, and backfill (`nuthatch publish sync`) is the same code with no
timer. It borrows the outbox's operational shape: gauges for pending count and lag, a
dead-letter state after N consecutive failures on one object, and the rule that a slow bucket
never blocks a seal.

Concurrency is `publish.parallelism` (default 2) objects in flight. The reservation it needs is
named, not inferred: `parallelism × part_buffer` (default 2 × 8 MiB) is the `publish_headroom`
term that joins RFC-0047 §2.4's budget arithmetic. It is small on purpose; the mirror is not a
throughput product, and the acceptance test in §7 is that ingestion throughput is unchanged
while publishing to a throttled store.

### 3.5 - `publish.json`: the provenance envelope

The catalogue says which files. This says whose, and how complete:

```json
{
  "layout_version": 1,
  "nuthatch_version": "2.6.0",
  "chain_id": 1,
  "data_identity": "…",
  "nid": "…",
  "bundle_hash": "…",
  "sealed_through": 20123456,
  "published_at": "2026-09-09T12:00:00Z",
  "catalogue_sha256": "…",
  "schema_sha256": "…",
  "tables": ["usdc__transfer", "usdc__approval"]
}
```

`layout_version` is this RFC's version of the *prefix layout*, separate from RFC-0047's
`manifest_version` for the *catalogue*. `sealed_through` is the consumer's freshness number:
"this mirror is complete through block N", the same fact the `/sql` caveat carries for a
degraded nest. `published_at` is the only non-deterministic field and is the reason
`publish.json` is not content-addressed; it is a pointer, like `index/…/latest` in RFC-0019 §2.
Its `catalogue_sha256` is a commit check, not merely provenance: readers compare it with the
catalogue bytes they read and retry on a mismatch. A publisher writes the catalogue first and
this envelope second, so an interruption can leave freshness understated but cannot make it
overstate an absent catalogue.

### 3.6 - The consumer contract: one page of deltas

`reading-segments.md` stays the contract. A new page, *Reading a published nest*, is a
diff against it, not a second spec:

- Resolution rule 0, before the two existing ones: if the dataset is a published prefix, a
  catalogue entry's file is `<dataset>/<table>/<hash>.parquet`.
- Globbing `<dataset>/<table>/*.parquet` **is** permitted on a published prefix, by §3.2/§3.3.
  It remains forbidden locally. The page says both, and why.
- Ordering, `union_by_name`, the 256-bit contract, `c_dec`/`c_overflow` not in the file: unchanged,
  by reference.
- `publish.json` fields and `sealed_through`.
- The eleven-line resolver gains an `s3://` branch.

### 3.7 - Configuration, credentials, and the boundary

```toml
# operator config - mounts.toml for a roost, never nuthatch.toml
[publish]
target      = "s3://my-bucket/nuthatch"   # or "/mnt/mirror" for FsStore; absent = never publish
interval    = "60s"
parallelism = 2
tables      = ["*"]                        # or an explicit list
```

- **Absent means off.** No `[publish]`, no task, no client constructed, no DNS lookup. The
  no-phone-home rule is satisfied by absence, not by a flag.
- **Credentials are kind (a), RFC-0019 §3.** `AWS_*` env, the bucket's own auth, never in
  `nuthatch.toml`, never in a bundle. **S0: `[publish]` is not excluded from the NID.**
  `blob.rs` hashes every authored file except `nuthatch.redb`, `segments`, `.git`, `.DS_Store`.
  `nuthatch.toml` is in that set and in `data_identity()`. Putting the target there would fork
  both identities. The table lives in `mounts.toml` (per mount) or on the CLI
  (`--publish-target`, same pattern as `--state-rpc` / `--ipfs`). A solo nest has no
  `mounts.toml`, so the flag is the solo path.
- **Minimal IAM, and it is a feature.** The publisher needs `PutObject`, `GetObject`,
  `HeadObject`, `ListBucket` on the prefix. It does not need `DeleteObject`, and the docs
  recommend not granting it: a credential that cannot delete makes the mirror immutable by
  policy, which is the property the consumer is being sold.
- **Public or private is the operator's call.** A public bucket is a public dataset. The docs
  say so in the same voice RFC-0019 uses for private nests, and `publish status` prints the
  target so nobody mirrors a private nest to the wrong prefix by accident.

### 3.8 - CLI, metrics, admin

- `nuthatch publish sync [--target …] [--dry-run]` - one reconciliation, exits non-zero on any
  failed put or a refused catalogue put. Backfill and repair are this command.
- `nuthatch publish status` - target, local `sealed_through`, remote `sealed_through`, pending
  segments and bytes, last error.
- `nuthatch publish verify [--deep]` - walk the remote catalogue; HEAD every file (size,
  checksum where the store returns it); `--deep` downloads and re-hashes. The remote twin of
  RFC-0047's `doctor` catalogue check, and `doctor` gains a `--publish` that runs the shallow
  form.
- Metrics, in the RFC-0010 naming: `nuthatch_publish_sealed_through`, `nuthatch_publish_lag_blocks`
  (local − remote), `nuthatch_publish_pending_segments`, `nuthatch_publish_bytes_total`,
  `nuthatch_publish_errors_total`, `nuthatch_publish_dead_letter`. Operators alert on lag
  first, the way RFC-0010 §"open questions" predicted for outbox depth.
- Admin UI (`/_admin/`): one row per mounted nest - target, lag, last success.

## §4 - Correctness argument

The claim to defend is: *a consumer reading the mirror sees exactly the rows a local reader of
the sealed directory sees, through `sealed_through`, at every instant, under crashes and
retries.* It follows from four existing facts and one new rule:

1. Segments are immutable and content-addressed (RFC-0009). A key is a file's bytes; a put of
   the same key is the same bytes or a no-op.
2. Segments are past finality (RFC-0028). No published row is ever retracted by a reorg, so
   the consumer needs no reorg handling - the thing every RPC-fed warehouse pipeline, Dune's
   included, spends most of its complexity on.
3. The catalogue is installed atomically (`save_manifest`) and its remote copy is a single
   atomic put, conditional on version.
4. `reading-segments.md`'s ordering and `union_by_name` rules are unchanged.
5. New: per-table prefixes hold only catalogued, non-provisional segments (§3.2), so globbing
   and catalogue-driven reads agree. `publish.json` is accepted only when its
   `catalogue_sha256` names the `manifest.json` the reader has; the catalogue-first publication
   order makes a mismatched envelope stale rather than ahead.

S0: a non-provisional entry is never removed from the local catalogue. Rule 5 holds for v1.
A future compaction RFC that drops entries must take §3.2's generations path before it ships.

## §5 - Consumer recipes (the acceptance targets, not documentation)

Each is a recipe to be run once against a real bucket and recorded, the way RFC-0019 recorded
Hetzner. The first two are CI; the rest are hand-verified.

**DuckDB** (CI, MinIO):

```sql
SELECT * FROM read_parquet('s3://bucket/nuthatch/<dataset>/usdc__transfer/*.parquet',
                           union_by_name = true);
```

with the `TRY_CAST … DECIMAL(38,0)` companion from `reading-segments.md` when needed. This is the
row-for-row parity gate in §7.

**Trino, and therefore DuneSQL** (nightly, container): Hive connector external table over the
per-table location, `format = 'PARQUET'`. One detail that would otherwise be found in production:
the Hive connector maps Parquet columns **by index** unless `hive.parquet.use-column-names=true`
(or the equivalent session property); a table whose columns drifted across seals (`BTreeSet`
column order, `reading-segments.md` §Ordering) is silently wrong without it. The recipe sets it
and the test has a drifted table. For Dune specifically: DuneSQL has a native `UINT256` and
accepts a cast from decimal text `[VERIFY the exact cast]`, which is why the exact-text contract
is the right thing to have published and why FLBA32 (#1222) is not a prerequisite. Addresses
are `0x` lowercase hex text; Dune's `varbinary` convention needs `from_hex(substr(a, 3))`. That
view-level mapping is the Dune-facing emitter in §8, not this RFC.

**Snowflake** (hand): external stage on the prefix, `CREATE EXTERNAL TABLE … AUTO_REFRESH`
(event-notification driven) or `ALTER EXTERNAL TABLE … REFRESH` from a cron; `INFER_SCHEMA`
against `schema.json`'s intent. Utf8 big ints stay `VARCHAR`; `TRY_TO_DECIMAL(c, 38, 0)` is the
checked cast.

**BigQuery** (hand): external table over a GCS prefix (the `object_store` GCS backend is in
scope by RFC-0019 §1) or BigLake over S3. `SAFE_CAST(c AS BIGNUMERIC)` is checked and holds
38 digits.

**Databricks / Spark** (hand): `read_files('s3://…/<table>/', format => 'parquet',
schemaEvolutionMode => 'addNewColumns')`; `try_cast(c AS DECIMAL(38,0))`.

**Dune, the ingestion side.** What the Data Foundation pipeline needs from a third-party
producer is a stable prefix of Parquet, a catalogue to know what is complete, and no reorgs to
handle. This RFC produces exactly that and stops. How Dune registers such a prefix into a
namespace is Dune's mechanism, to be confirmed internally (`[VERIFY]`); the RFC commits to the
producer contract, not to the consumer's import path. Datashare is the opposite direction
(Dune → warehouses) and is not relevant.

## §6 - Drawbacks

- **Many small objects.** Pre-#1067 segments are 6 KB and there is no compaction anywhere in
  the tree (RFC-0047 §1). A long-lived nest mirrors thousands of small files; Trino and
  Snowflake listing cost is per object. This RFC does not fix that - compaction is a catalogue
  change with the §3.2 generations constraint - but it makes the cost visible in
  `publish status` (object count) so the compaction RFC has a number.
- **Duplicate bytes across nests** for shared contracts (§3.1). Accepted for engine
  compatibility.
- **Freshness is sealed freshness.** A consumer gets history through `sealed_through`, which on
  a chain with a slow `finalized` tag is minutes behind tip (RFC-0050's seven measured minutes).
  The number is published; the expectation is set; LiveFetch-style tip reads stay out of scope.
- **A new host-side task and a new dependency surface** (`object_store` was already in;
  checksum-on-put and conditional put may need feature flags per backend). Bounded by the
  reservation in §3.4 and by the §7 throughput gate.
- **Provenance beyond hash.** A consumer trusts the bucket owner. The mirror carries
  `bundle_hash` so a sceptical consumer can `nest load` the same nest and re-index a range to
  compare rows; it does not carry a signature. Signed `publish.json` (the RFC-0008 audit-manifest
  key) is an open question, not a v1 feature.

## §7 - Slices, each with an acceptance criterion that can fail

| Slice | Delivers | Fails if |
| --- | --- | --- |
| S0 - verify | The §1 items answered in the tree (2026-09-12, #1258): catalogue is append-only for non-provisional entries; `data_identity()` is 64 hex and computable on a solo nest; `[publish]` is *not* outside the NID so it moves to `mounts.toml` / `--publish-target`; `BundleStore` is a wrapper over `object_store`, not the trait | Generations in v1: the catalogue answer did not force them |
| S1 - `publish sync` | Reconciler over `FsStore` and S3 (MinIO in CI); §3.1 layout; §3.3 protocol; `verify` | After `sync`, remote `manifest.json` is not byte-equal to local; or for any table, DuckDB `count(*)` and `sum(c_dec)` over `s3://…/<table>/*.parquet` differ from the local sealed rows; or a second `sync` performs any put other than `publish.json` |
| S2 - continuous | Seal-triggered wake-up + interval in `dev` and `serve`; per-mount prefixes in a roost; dead-letter; metrics | Publish lag exceeds one seal plus one interval on a live nest; or RFC-0004's backfill harness measures ingestion throughput outside noise while publishing to a MinIO throttled to 1 MB/s; or a seal is observed waiting on a put |
| S3 - contract | `publish.json`, `schema.json`, *Reading a published nest*, resolver `s3://` branch; Trino container test | Trino over the prefix returns different counts or `sum` from local DuckDB on a nest with a drifted table; or the page and `seal.rs` disagree (same red-test rule as `reading-segments.md`) |
| S4 - operator surface | `publish status`, `doctor --publish`, admin row, IAM doc | `doctor --publish` passes on a bucket where one object was replaced by hand with different bytes |
| S5 - recipes | Snowflake, BigQuery, Databricks run once by hand and recorded; Dune ingestion path confirmed | A recipe that was never run is listed as supported |

S1 is the whole thing; S2-S4 make it live and honest; S5 is evidence. Nothing in S1-S4
changes a segment, a catalogue field, or a query.

## §8 - Follow-ons this RFC enables and does not do

- **Cold-start from a mirror.** `nuthatch init … --from-mirror s3://…` seeds a nest's sealed
  history from a published prefix instead of an RPC backfill, verifying every file against the
  catalogue hash and pinning `bundle_hash` so the decode is known. Operators would share
  indexed history the way they share bundles. This is the adoption feature; it is also the first
  time nuthatch would *read* a third-party data source and needs its own trust design - hence
  its own RFC.
- **The Dune-facing view emitter.** From `semantic.toml` (RFC-0016) and `schema.json`, emit
  DuneSQL/dbt models over the mirrored tables: `UINT256` casts, `varbinary` addresses, `_dec`
  companions, documented columns. That is where "push interpretation, not just rows" lives.
- **The row-insert sidecar** for teams without Enterprise ingestion: read the published
  catalogue, post the delta since a cursor to `/table/{ns}/{name}/insert`. A few hundred lines
  outside the binary.
- **Iceberg metadata as a translation** of the catalogue, if a consumer needs snapshot
  isolation or a catalog-registered table. The catalogue already has the shape (RFC-0047 §5).
- **Compaction**, constrained by §3.2 to version the published prefix rather than delete.

## §9 - Freeze position

RFC-0047 was unfrozen on 2026-09-08 and this is its network half. Every slice is additive:
no format change, no catalogue field, no query surface. S1's `sync` is a new command and S2 is a
new host-side task, which is a feature by any honest reading; it is proposed under the same
"to be built in full" disposition 0047 received, with `publish_headroom` measured before S2
is on by default in a roost, per RFC-0047 §2.4's refusal rule.

## §10 - Unresolved questions

- Whether `publish.json` should be signed with the RFC-0008 audit key, so a consumer can verify
  the publisher and not only the bytes.
- Whether a roost should publish one prefix per mount (this RFC) or one per data identity
  shared across mounts that adopted the same dataset. Per data identity is more correct; per
  mount is simpler to reason about when two mounts on one runtime differ in `tables`.
- The exact `schema.json` name and shape as it exists today, and whether it is the right artefact
  to publish or whether RFC-0047 C2's `logical_type` in the catalogue supersedes it, at which
  point `schema.json` is redundant and should not be published at all.
- `object_store` support for checksum-on-put and conditional put across S3, R2, MinIO, GCS
  (R2's `If-Match` semantics in particular). Where a backend lacks one, the fallback is a HEAD
  size check and a documented "last writer wins" for that backend, printed by `publish status`.
- Whether `tables = […]` selection should exist at all in v1, given that a partial mirror is a
  dataset a consumer cannot distinguish from a complete one without reading `publish.json`.

## §11 - Prior art

`dune-sync` and `duneanalytics/node-indexer` for the row-push shape this RFC declines;
RFC-0019 for the bucket-as-registry pattern and its credential boundary; RFC-0010 for the
delivery rule and gauges; RFC-0045 for "host-side, out-of-band, never in the data path";
RFC-0047 for the contracts being mirrored; Amp (RFC-0043) for the reminder that Parquet in a
bucket is only a product if the catalogue is specified; Iceberg and Delta for the
manifest-then-commit protocol §3.3 reproduces with one JSON file and a conditional put.
Nuthatch's contribution is that the mirror is *nothing but a copy* - the format, the catalogue
and the contract already exist, and the mirror adds a network and a provenance envelope.
