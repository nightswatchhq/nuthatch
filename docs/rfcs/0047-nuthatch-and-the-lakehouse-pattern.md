# RFC-0047: Nuthatch and the lakehouse pattern

- Status: **Draft. Design only.** Under the 2026 feature freeze this is a document to argue with,
  not work to start, until the board takes named slices. The four commitments below are
  documentation, specification, writer-config, and making existing DuckDB caps operator-visible.
  That is freeze-compatible in spirit. One item is not: changing the physical Parquet type of
  256-bit values. That is a segment-format version and is named here rather than smuggled in as
  "no new features".
- Author: Jenny
- Date: 2026-09-02
- Origin: a board draft of this title, read against the tree the same day. §1 records where the
  draft and the tree disagree.
- Depends on: RFC-0013 §3 (sealed Parquet, DuckDB over hot ∪ cold), RFC-0009 (content-addressed
  segments and the catalogue), RFC-0001 §2 (`*_dec` / `*_overflow`), RFC-0028 (deterministic seal
  boundaries), RFC-0035 (sealed segments were not a 2.0 break), RFC-0042 §14 (KEEP DuckDB;
  `SQL_MAX_CONCURRENCY` is the remaining named revisit), RFC-0043 §7 / §10 (writer defaults,
  Bloom, file count), #889 / [segment-layout.md](../bench/segment-layout.md), #1067 (tip-path
  batching, landed 2026-09-02).
- Blocks: nothing. It does not reopen RFC-0042, and it does not change query syntax.

## Abstract

Nuthatch's storage is a single-process lakehouse: a mutable hot store (redb), sealed immutable
Parquet as cold, and an embedded engine (DuckDB) federating the two. The escape hatch that
implies, "point something else at the same directory", is true by accident today rather than by
contract.

This RFC does not add a query engine, a catalog, or a table format. It proposes that we **commit
to the pattern explicitly** in four places where the implementation is implicit, under-specified,
or chosen by a crate default that then freezes into every sealed file:

1. A documented, normative representation for 256-bit integers across ingest, storage, and query.
2. A versioned specification of the **segment catalogue that already exists**, so the sealed
   directory is a stable interface.
3. An audited set of **Parquet writer settings**, because those are write-once.
4. Explicit **resource governance** for the embedded engine, so a heavy query cannot take
   ingestion with it.

## §0 - Why write this down at all

Three observations, none of them new capability.

**The escape hatch should be real.** A sealed segment is supposed to be plain Parquet: an
operator who outgrows the embedded engine points DataFusion, Trino, Spark, or another DuckDB at
the same directory. Today that is true because the files happen to be readable. Without a
specified catalogue and documented encodings, an external reader reverse-engineers segment
boundaries, ordering, and types from `seal.rs`. A migration path that requires reading the
migrator's source is not a migration path.

**Write-time decisions are forever.** Segments are immutable. Row-group sizing, column order,
sort, statistics, bloom filters, and compression are fixed at seal and cannot be improved for
data already sealed. Every month the writer runs on unaudited defaults is a month of segments
that will carry those defaults permanently. That is the strongest argument for doing the writer
audit now, mid-freeze, rather than after.

**Embedded inverts the blast radius.** A runaway `SELECT` shares a process, a memory budget, a
file-descriptor table, and an OOM-kill fate with ingestion. That is a reliability bug in all but
name. Some of the wall is already built (512 MB / 2 threads / 2 concurrent queries). It is not
operator-visible, it is not a named split with the ingestion reservation, and it has no spill
bound.

## §1 - What is already true

The board draft treated all four as greenfield. They are not. Nuthatch has shipped a lakehouse.
This RFC specifies it.

### 256-bit values are exact text, with a checked decimal companion

`seal.rs::rows_to_batch` writes `block_number`, `log_index`, `_seq` and `block_timestamp` as
`UInt64` and **everything else as `Utf8`**. A `uint256` lands as its canonical decimal text. That
is lossless. It is also not `FIXED_LEN_BYTE_ARRAY(32)`.

The query layer already does the projection the draft asked `semantic.toml` to grow. From
`analytics.rs`, RFC-0001 §2:

> each such column `c` gets two derived view columns: `c_dec` - the value as `DECIMAL(38,0)` when
> it fits, else NULL - and `c_overflow` - true when the exact value exceeds 38 digits.

`TRY_CAST` is the conversion. Overflow is flagged, not sealed-as-failure. `SUM(c_dec)` works;
`SUM(c)` is the footgun `views.md` and the MCP error hints already name. **No silent narrowing on
the query path.** An external reader of the raw Parquet does not get `_dec` or `_overflow`; those
are DuckDB view columns. That is the actual gap: the contract is engine-local.

Unpadded decimal text is also **not** bytewise-sortable in numeric order (`"9"` > `"10"`). The
draft's FLBA32-BE claim (sortable, stats-meaningful) is a real property we do not have.

### The catalogue already exists, and it is already the source of truth

`seal.rs` writes `manifest.json` next to `segments/`, atomically (`rename` over a fsynced temp,
COR-9). A segment is a content-addressed Parquet file, sha256 of the bytes, catalogued per table:

```
{ hash, from_block, to_block, rows, file, registry_snapshot? }
```

A file not in the catalogue is not a sealed segment. A catalogue entry whose file is missing or
hash-mismatched is quarantined at startup (`seal.rs` integrity walk). Additive fields already
use serde defaults (`registry_snapshot` is the precedent).

What the catalogue does **not** carry: `manifest_version`, a schema fingerprint, a `sort_order`
promise, logical types (`uint256` / `address` / `hash32`), or segment-level column statistics.
Those are the enrichments, not a second artefact.

The draft's "a segment without a manifest is by definition unsealed" is the wrong object. Unsealed
means not catalogued. A per-file sidecar would be a second source of truth the existing atomic
rename was written to avoid.

### The writer sets compression and nothing else

From the code, and from the 2026-08-29 footer read in `docs/bench/segment-layout.md`:

| property | actual |
| --- | --- |
| compression | **SNAPPY** (`write_parquet`) |
| column statistics | written, every column |
| Bloom filters | none |
| row groups per file | always 1 |
| everything else | `parquet` crate default (`parquet-rs 58.3.0` at the measurement) |

There is no compaction anywhere in the tree. Small files were a tip-path batching hole, not a
missing Iceberg spec: #1067 gave the tip path the same 20,000-row, data-chosen cut the backfill
path already had. New seals are larger. Old ones stay 6 KB. That is the immutability argument
working, and it is why writer settings still want an audit for everything sealed from here.

RFC-0043 §7.4 already named `FixedSizeBinary(20/32)` and `Decimal128(38,0)` as "a sane convergent
answer worth comparing our columns against the next time the schema is opened. Not urgent." This
RFC is that opening, and the comparison has a cost the Amp note did not price.

### DuckDB is already capped; the caps are not config

`analytics.rs`: `MEM_LIMIT = "512MB"`, `MAX_THREADS = 2`. `serve.rs`: `SQL_MAX_CONCURRENCY = 2`,
ceiling 16, overridable via `NUTHATCH_SQL_MAX_CONCURRENCY` because #1006 needed to measure it
without five builds. Unguarded `/sql` also has a result-byte cap, because row materialisation
lives in Rust outside DuckDB's `memory_limit`. Trusted internal queries are not byte-capped.

There is no `temp_directory`, no `max_temp_size`, no named `ingestion_reservation`. The 2 GB
per-cursor budget is the non-negotiable; the split between ingestion and analytics is implicit
(512 MB of it is DuckDB, plus however many connections the permit count opens). RFC-0042 §14
already listed `SQL_MAX_CONCURRENCY` as a remaining revisit. This RFC does not reopen the engine
decision.

`nuthatch doctor` probes RPC endpoints. It does not validate the catalogue against files.

## §2 - The four commitments

### 1. 256-bit representation (normative documentation first)

**What lands as documentation, freeze-legal:**

- Storage is canonical decimal text in `Utf8`. That is the external-reader contract today.
- Query convenience is `c_dec` / `c_overflow`, derived at view definition, never written to
  Parquet.
- The rule: **no silent narrowing.** Every conversion out of the exact text is either checked
  (`TRY_CAST`, NULL + `c_overflow`) or not offered. The documentation states this as a contract,
  not as a DuckDB trick, because it is the difference between a correct blockchain indexer and a
  plausible-looking one.
- A page, **"Reading Nuthatch segments without Nuthatch"**, specifies directory layout, the
  catalogue schema, ordering, and this encoding.

**What is a format version, and is not implied by the rest:**

- Physical type `FIXED_LEN_BYTE_ARRAY(32)` big-endian. Lossless, bytewise-sortable for unsigned
  values, stats-meaningful. Amp converged there. We did not. Switching it means new seals do not
  byte-compare to old ones (already true across arrow-rs bumps, see `seal.rs` F-D3), and every
  external reader plus `read_table_rows` must accept both. That is a `manifest_version` bump, a
  dual-read, and a migration note. It is not "the writer currently does this, write it down".

**What stays out:**

- Per-column `exact-bytes` / `decimal-checked` / `double-lossy` in `semantic.toml`. Today every
  big-int column gets both the exact text and the checked decimal. Making the author pick is new
  schema surface. `double-lossy` as a default is forbidden; as an opt-in it is a feature.
- Seal-time failure when a value exceeds DECIMAL(38). Today we flag overflow at query time and
  still seal the exact text. Failing the seal is a behaviour change on a path that currently
  succeeds, and it would refuse to archive a real uint256 that happens to be wide. The unresolved
  question is whether that refusal is ever wanted.

### 2. Version the catalogue, do not replace it

`manifest.json` grows, additively, still the one atomic file:

- `manifest_version: 1` (absent today; readers treat missing as 0).
- `schema_fingerprint` (the decode-registry hash we already have a name for).
- per-segment `sort_order` as a **promise**, validated at seal. Current write order is block
  order, then content address within a range (`seal.rs`). Name that. External engines may merge
  and prune from the promise without opening footers.
- per-column `logical_type` (`uint256`, `address`, `hash32`, …) next to the physical Parquet
  type, because Parquet cannot say "this Utf8 is a uint256".
- segment-level stats (min/max/null_count) so pruning can start from the catalogue. Duplication
  with Parquet footers is deliberate; the footer remains authoritative, `doctor` checks they
  agree.

Compatibility: additive changes bump nothing; breaking changes bump `manifest_version`. Readers
ignore unknown fields. That is the serde-default policy `registry_snapshot` already uses.

A top-level `segments.json` (id + block range, rewritten on each seal) is a cache. Its loss is
recoverable by walking the catalogue. Do not make it a second source of truth.

`nuthatch doctor` gains a catalogue check: every entry's file exists, hashes, and, once stats
are present, agrees with the footer. That is the existing integrity walk with a CLI.

### 3. Parquet writer audit

Deliverable: a table in the segment-format spec that matches the implementation, plus a one-time
footer audit on a real nest (the #889 method). Any deviation is a writer-config bug.

**Current, to write down:**

| Setting | Value |
| --- | --- |
| Compression | SNAPPY |
| Statistics | all columns |
| Bloom filters | none |
| Row groups | 1 per file |
| Dictionary / page checksums / sort metadata | crate default, unverified in the spec |

**Proposed for new seals only**, justified by engine-agnostic readability, not by DuckDB:

| Setting | Value | Rationale |
| --- | --- | --- |
| Compression | zstd, level 3 | size/speed, universal readers; SNAPPY stays on already-sealed files |
| Bloom filters | address, topic, and hash columns | point-lookup columns; keep file overhead bounded |
| Page checksums | enabled | cheap, segments are forever |
| Dictionary | on, fall back on near-unique 32-byte columns | defaults waste space on hashes |
| Sort | `(block_number, tx_index, log_index)` where those columns exist | dominant predicate; must match the catalogue promise |
| Row group size | not a 128 MiB target | we seal on a row threshold at a data-chosen block boundary (`SEAL_DIRECT_BATCH`). One group per seal is the current shape. Changing it is a second decision, priced against #889's per-file cost, not copied from a lakehouse cookbook |

Each proposed change is a new-seal-only writer-config fix. None rewrites history.

### 4. Resource governance, as config over existing walls

New keys, conservative defaults equal to what the binary already does:

- `analytics.memory_limit` - default `512MB`.
- `analytics.threads` - default `2`.
- `analytics.temp_directory` + `analytics.max_temp_size` - spill-to-disk, bounded. Not set today.
  A memory-limited query should become slow, not dead.
- `ingestion_reservation` - the named floor for the ingest path, so the 2 GB per-cursor budget
  is arithmetic in one place rather than a comment in `serve.rs`. Exact default is an unresolved
  measurement.

Before any of those keys becomes operator-visible, startup must reject the configuration unless:

```text
(sql_permits × analytics.memory_limit) + ingestion_reservation + runtime_headroom ≤ 2 GiB
```

`sql_permits` is the one cursor-wide gate, not a per-nest value. `runtime_headroom` is the
measured high-water mark for Rust, DBSP, decode and result materialisation outside DuckDB; it is
not a hand-waved remainder. `analytics.max_temp_size` is a separate disk cap and does not buy RAM
in that equation. The defaults and hard maxima wait for that measurement, and the validator ships
with them. A knob without this refusal is not resource governance, it is an invitation to find the
2 GB limit by falling over.

`SQL_MAX_CONCURRENCY` stays the permit count it is. This RFC does not raise it, and its existing
benchmark override is not promoted to an unconstrained configuration key by this RFC.

**Normative principle, and it belongs in the docs:** ingestion liveness outranks query
completion. A query that cannot run in its budget fails with a clear error naming the keys. It
never degrades block processing. That is a real difference from a dedicated analytical database,
where the query is the whole point, and it is already how the 512 MB / 2 thread wall behaves.
Making it config makes the wall visible.

## §3 - Freeze position

| Slice | Kind | Carve-out needed? |
| --- | --- | --- |
| "Reading segments without Nuthatch" + 256-bit contract as it ships | docs | no |
| `manifest_version` and additive catalogue fields | spec + small writer | no, if additive and dual-readable |
| Writer-settings table matching the code | docs | no |
| zstd / bloom / checksums on **new** seals | writer config, performance | no, if measured and not a format break |
| `analytics.*` config keys plus the startup budget validator | config over existing behaviour | no, only with measured headroom and the 2 GiB refusal above |
| `doctor` catalogue check | CLI over the existing integrity walk | no |
| Utf8 → FLBA32, or seal-fail on wide uint256, or `semantic.toml` projection policies | format / behaviour / schema surface | **yes, or a named exception in this RFC once the board takes it** |

A proposal to start the last row is a proposal for a carve-out, same rule as RFC-0042.

## §4 - Drawbacks

- Enriching the catalogue adds seal-time work and a checkable derived artefact. Mitigated: the
  Parquet file remains authoritative; `doctor` validates; the atomic rename already exists.
- Committing to a public segment spec constrains storage refactors. That is the point. The
  pressure valve is `manifest_version`.
- FLBA32, if taken, splits the corpus by physical type until a dual-read is done. The cost is
  why it is not in the freeze-legal set.
- Naming `ingestion_reservation` without measuring it produces a number nobody should act on.
  The key can exist; the default waits on a high-water mark per chain profile.

## §5 - Rationale and alternatives

- **Adopt Iceberg or Delta instead of enriching the catalogue.** Rejected for now: those formats
  assume a catalog and multi-writer coordination we do not have, and their Rust write paths are
  the heaviest dependency this would drag in. The catalogue already mirrors their *shape*
  (segment-level records, atomic commit, content address). A future migration is a translation.
  Revisit if multi-writer or an external catalog ever materialises. RFC-0043 made the same call
  about Amp's metadata store.
- **Do nothing until after the freeze.** Rejected for the writer settings: immutability means
  deferral has a permanent cost. Catalogue versioning and config-over-constants could wait;
  batching them here is cheaper than three small RFCs.
- **Native 256-bit arithmetic in the query layer.** Out of scope. That is engine work, and
  RFC-0042 parked the engine. Documenting `_dec` is not implementing HUGEINT.

## §6 - Unresolved questions

- Exact default for `ingestion_reservation`. Needs a measured ingest RSS high-water mark per
  chain profile, on the box that enforces the 2 GB budget, not on a laptop.
- Whether a `decimal-checked` *seal* failure is ever wanted, versus today's query-time
  `c_overflow`. Convenient fallback to exact-text-only would be silent narrowing's cousin.
- Whether the catalogue should flag per-column bloom presence so an external reader can plan
  point lookups without probing files. Vacuous until blooms exist.
- Whether `created_by` in the Parquet footer should be pinned (F-D3) as part of making segment
  identity a contract across nuthatch versions. That is adjacent, not the same issue.
- Whether FLBA32 is worth a format version at all, given `_dec` already covers the query path
  and the escape hatch is "read the Utf8". Amp's type choices are a comparison, not an order.

## §7 - Prior art

Lakehouse table formats (Iceberg, Delta, Hudi) for the catalogue/statistics shape; distributed
SQL engines' per-query memory limits for the resource model; our own `_dec` / `*_overflow` for
the checked projection. Amp (RFC-0043) for the writer-default warning and the Arrow type
comparison. Nuthatch's contribution is collapsing the pattern into one process while keeping the
storage contracts engine-agnostic, which is only true if we write them down.
