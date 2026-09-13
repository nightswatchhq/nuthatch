# RFC-0055: The Dune view emitter - generated DuneSQL models over a mirrored nest

**Status:** **Draft - design only.** Written for Chief's acceptance before any code (#1344). It is new
binary capability, so no slice starts until this is accepted.

**Date:** 2026-09-13

**Author:** Pete (cargopete)

**Depends on:** RFC-0052 (the mirrored nest: the published prefix these models read, and §8, which
names this follow-on), RFC-0016 (`semantic.toml`, the descriptions a model carries), RFC-0047 C1 and
[reading-segments.md](../reading-segments.md) (the physical types every cast starts from),
[reading-published-nest.md](../reading-published-nest.md) (the consumer contract), RFC-0044 S2
(`port_emit.rs`, the emitter this deliberately does not share; §7).

**Provenance:** public sources only, by Chief's rule for this work: no Dune-internal knowledge shapes
it. Every claim about DuneSQL or Dune's products below cites a public page read on 2026-09-13, from
research briefs 7 and 8. A claim the public record does not support is marked `[unverified]` and
promises nothing.

## Abstract

A published nest is Parquet that any engine can read, but its columns are nuthatch's physical types:
exact decimal text for wide integers, `0x` hex text for addresses and hashes, text for everything but
four counters. A Dune user reading it raw has to rediscover every one of those rules, per column, and
gets silently wrong answers for the ones they miss. `nuthatch emit dune` reads a nest's `schema.json`
and `semantic.toml`, which already record what every column is and which ones are dangerous, and writes
DuneSQL models that cast each column to its native DuneSQL type and carry the author's descriptions.
It is deterministic and offline, and it pushes interpretation, not rows (RFC-0052 §8).

## §0 - What is already true

**The physical types are fixed and documented.** `rows_to_batch` writes exactly four columns as
`UInt64` (`block_number`, `log_index`, `_seq`, `block_timestamp`) and every other column as nullable
`Utf8`, gated by `sealed_parquet_writes_uint64_counters_and_utf8_uint256_without_dec_companions`
(`reading-segments.md`). A `uint24` event parameter is text. The DuckDB `<col>_dec` and
`<col>_overflow` companions are **not in the file**: they are view columns computed at query time, so a
Dune model has only the exact text to cast from.

**Every column already names its kind.** `schema.json` records, per table, each column's `name`,
`sol_type`, `storage` and `indexed`. The `storage` vocabulary is closed, from `StorageKind::as_str` in
`decode/src/registry.rs` plus the implicit columns: `u64`, `i64`, `word16`, `word32`, `address`,
`bool`, `fixed_bytes`, `bytes`, `string`, `json`, `hash32`, and `bytes32` for the implicit
`block_hash`/`tx_hash`. The mapping is a function of that vocabulary, never of a contract, which is
what makes it generatable.

**The dangerous columns are already derived.** `semantic.toml`'s `[table.*.footguns]` are computed from
the registry, not authored: `reserved_words`, `big_ints`, `overflows_dec`, `bools`. The emitter uses
them as a cross-check against `storage`, not as a second source of truth.

## §1 - Goals and non-goals

Goals:

- One model per table, casting every column from its physical type to the DuneSQL type §3 names, with
  the table and column descriptions from `semantic.toml`.
- Byte-identical output for identical inputs, with no network and no model in the output path
  (non-negotiable 4 does not strictly bind presentation, but a generator that drifts between runs
  cannot be golden-tested).
- A refusal, named per column or view, for everything that does not translate exactly (§5).

Non-goals:

- **Getting data into Dune.** How a published prefix becomes a Dune table is §4, and this RFC promises
  only what the public record shows. If there is no public path, the emitter still produces correct
  models for whoever has one, and says so.
- Translating arbitrary authored DuckDB SQL (`views/*.sql`) into DuneSQL. §5 and slice S3.
- Contributing to Spellbook, or copying its code (§3.3).
- Any change to what nuthatch stores or publishes.

## §2 - Inputs and output

Input: a nest directory, read-only. `schema.json` (column kinds), `semantic.toml` (descriptions, grain,
footguns), and the `data_identity` the mirror publishes under, computed the way `publish` computes it.

Output: a directory of files, deterministic in content and order. The shape (plain DuneSQL or dbt-style
models with a `schema.yml`) is decided in §3.3 from brief 8's findings.

`nuthatch emit dune --dir <nest> --out <dir> [--source <namespace.prefix>]`, where `--source` names how
the consumer's Dune tables are addressed, since that addressing is theirs, not the nest's (§4).

## §3 - The type map

### 3.1 - The physical side, settled

| `storage` | columns | physical |
| --- | --- | --- |
| `u64` (implicit) | `block_number`, `log_index`, `_seq`, `block_timestamp` | `UInt64` |
| `bytes32` (implicit) | `block_hash`, `tx_hash` | `Utf8`, `0x` lowercase hex |
| `address` | `address` and address parameters | `Utf8`, `0x` lowercase hex |
| `u64` / `i64` | parameters of 64 bits or fewer | `Utf8`, decimal text |
| `word16` / `word32` | 65- to 256-bit parameters | `Utf8`, canonical decimal text, `-` when negative |
| `bool` | | `Utf8`, `'true'` / `'false'` |
| `fixed_bytes` | `bytes1` to `bytes32` | `Utf8`, `0x` lowercase hex |
| `bytes` | dynamic `bytes` | `Utf8`, `0x` lowercase hex |
| `hash32` | indexed dynamic types | `Utf8`, `0x` hex of the keccak, **not the value** |
| `string` | | `Utf8` |
| `json` | arrays and tuples | `Utf8`, JSON text |

### 3.2 - The DuneSQL side

`[pending brief 8]` One row per `storage` kind above: the DuneSQL type, the exact documented expression,
what a value that does not fit does (error or NULL), and the citation.

### 3.3 - The output target and naming

`[pending brief 8]` Plain DuneSQL or dbt-style models; Dune's decoded-column naming, and whether it is
documented for third-party tables at all; Spellbook's licence and what that permits borrowing.

## §4 - How the models meet the data

**There is no public way to point Dune at a Parquet prefix.** Brief 7 found no external table, no bucket
connection and no way to attach a catalog on any plan. Parquet is not an accepted upload format
anywhere in the docs. Every documented route copies rows into Dune-managed storage ([Trino Connector
Overview](https://docs.dune.com/api-reference/connectors/trino/overview.md), [Upload
Data](https://docs.dune.com/web-app/upload-data.md), read 2026-09-13). The only public mention of
object storage going into Dune is the sales-led [Dune for chains](https://dune.com/dune-for-chains)
offer, which this RFC does not rely on. Datashare and S3 Export go the other way.

So RFC-0052's mirror does not reach Dune by itself. **The one public route is a sidecar**, RFC-0052 §8's
row-insert follow-on. It creates one table per nuthatch table with `POST /v1/uploads` (10 credits each),
watches the published `manifest.json`, and appends each new segment's rows as NDJSON through
`POST /v1/uploads/:namespace/:table/insert`. Each insert is atomic, at most 1.2 GB, and billed at
3 credits per GB with a minimum of 1 credit per request ([Insert
Data](https://docs.dune.com/api-reference/tables/endpoint/uploads-insert.md),
[Billing](https://docs.dune.com/api-reference/overview/billing.md)).

The per-request minimum makes cadence the price. One insert per table every five minutes is about 8,640
credits a month per table; hourly is about 720. That is arithmetic on those two pages, not a published
figure. Storage is capped at 100 MB on Free, 1 GB on Analyst and 15 GB on Plus, and uploaded data is
public unless the operator is on Enterprise ([How Credits
Work](https://docs.dune.com/resources/credits-billing/how-credits-work.md)).

What this means for the emitter:

- **The models read the sidecar's tables, not the mirror.** Their source is `dune.<namespace>.<table>`,
  the create API's `full_name` form ([List Uploaded
  Tables](https://docs.dune.com/api-reference/tables/endpoint/uploads-list.md)), and `--source` names
  the namespace because it is the operator's, not the nest's.
- **The models assume the sidecar loads every column as `varchar`**, except the four `UInt64` counters,
  and cast at query time. The column types `create` accepts are not published. `varchar` is the one
  choice the docs support, and it keeps the exact-text contract intact across the upload.
- **The emitter is useful without Dune too.** It emits correct DuneSQL for anyone holding the rows,
  whether through the sidecar or a private arrangement. It does not claim a Dune integration by
  existing.
- **The sidecar is a prerequisite for S4 and is not this RFC.** It is a few hundred lines outside the
  binary (RFC-0052 §8) and needs its own acceptance.

## §5 - What it refuses

- **`hash32` columns are emitted as the hash they are**, named as such in the column description, and
  never as the value they hash.
- **`json` columns stay text.** Parsing a tuple into typed DuneSQL columns needs the ABI's component
  types per field; that is a later slice if asked for, not a guess now.
- **Authored views (`views/*.sql`) are not translated in v1.** They are DuckDB SQL over the nest's own
  `_dec` companions, which do not exist on the Dune side; each is listed as not emitted, with the reason.
- A column whose `storage` is not in §3.1's vocabulary fails the run and names the column. A new
  `StorageKind` must extend the map deliberately.
- A disagreement between `storage` and the footguns (a `word32` column missing from `big_ints`) fails
  the run: one of the two files is stale, and the output would be wrong either way.

## §6 - Correctness and tests

- **Golden:** a fixed `schema.json` and `semantic.toml` covering every §3.1 row in, exact output files
  out, compared byte for byte.
- **Vocabulary pin:** a test enumerates `StorageKind` and fails if a variant has no §3 row, so the map
  cannot silently fall behind the decoder.
- **Refusal tests:** an unknown kind and a storage/footgun disagreement each fail with the column named.
- Whether the emitted SQL runs on DuneSQL is not provable offline. S4 records a real run, or records
  that none was possible.

## §7 - Relationship to `port_emit.rs`

Deliberately separate. `port_emit.rs` (RFC-0044 S2) consumes a subgraph port report, classified fields
with mapping citations, and writes nuthatch artefacts (`[[calls]]`, `views/*.sql`, entities). This
consumes a nest's own `schema.json` and `semantic.toml` and writes another engine's dialect. The inputs,
the target and the failure modes share nothing but the nest's table names. What they do share is
reading `semantic.toml` (`semantic::Semantic`) and `schema.json`, and both keep using the existing
parsers rather than new ones.

## §8 - Slices, each with a criterion that can fail

| slice | delivers | fails if |
| --- | --- | --- |
| S0 - verify | briefs 7 and 8 answered from public sources, §3.2 to §4 filled in with citations | any §3.2 row has no citation, or §4 promises a path the findings did not show |
| S1 - per-table models | `nuthatch emit dune`, §3 casts, §5 refusals, the golden test | the golden output changes between two runs, or a `StorageKind` variant has no row |
| S2 - descriptions | table and column descriptions and grain from `semantic.toml` in the output | a description in `semantic.toml` is missing from the emitted model |
| S3 - authored views | the subset of `views/*.sql` that translates exactly, the rest named | a translated view returns a different result from the DuckDB original on the fixture |
| S4 - recorded run | the emitted models run in Dune over a real nest's rows, loaded by the RFC-0052 §8 sidecar, and recorded; blocked until that sidecar exists (§4) | a model is listed as working that was never run |

## §9 - Unresolved

Ingestion, from brief 7. None of these is publicly documented, and each blocks S4 until answered by a
real run:

1. The column types `POST /v1/uploads` accepts, and whether `uint256`, `int256` or `varbinary` are among
   them. §4 assumes `varchar` for that reason.
2. Whether an NDJSON insert can omit a nullable column, and whether an uploaded table can gain one.
   A nest's columns can drift between seals (`reading-segments.md` §Ordering), so the sidecar may need
   a new table per column set.
3. The upsert endpoint named in the March 2026 changelog: its path, key semantics and cost.
4. Which credit rule is enforced. Billing says 3 credits per GB with a 1-credit minimum; the older
   Tables Overview still describes data points per credit.

`[pending brief 8]` Type-map and convention gaps.
