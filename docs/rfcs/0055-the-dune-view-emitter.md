# RFC-0055: The Dune view emitter - generated DuneSQL models over a mirrored nest

**Status:** **Draft - design only.** Written for Chief's acceptance before any code (#1344). It is new
binary capability, so no slice starts until this is accepted.

**Date:** 2026-09-13

**Author:** Pete (cargopete)

**Depends on:** RFC-0052 (the mirrored nest, and §8, which names this follow-on and the row-insert
sidecar §4 depends on), RFC-0016 (`semantic.toml`, the descriptions a model carries), RFC-0047 C1 and
[reading-segments.md](../reading-segments.md) (the physical types every cast starts from),
[reading-published-nest.md](../reading-published-nest.md) (the consumer contract), RFC-0044 S2
(`port_emit.rs`, the emitter this deliberately does not share; §7).

**Provenance:** public sources only, by Chief's rule for this work: no Dune-internal knowledge shapes
it. Every claim about DuneSQL or Dune's products cites a public page read on 2026-09-13, from research
briefs 7 and 8 (§10 lists the sources). What the public record does not show is marked unverified,
and nothing depends on it without a slice that runs it first.

## Abstract

A published nest is Parquet that any engine can read, but its columns are nuthatch's physical types:
exact decimal text for wide integers, `0x` hex text for addresses and hashes, text for everything but
four counters. A Dune user reading those rows raw must rediscover each rule per column, and gets silently
wrong answers for the ones they miss. `nuthatch emit dune` reads a nest's `schema.json` and
`semantic.toml`, which already record what every column is and which ones are dangerous, and writes one
DuneSQL query per table that casts each column to its native DuneSQL type, names it the way Dune's own
decoded tables do, and carries the author's descriptions. It is deterministic and offline. It pushes
interpretation, not rows (RFC-0052 §8), and it does not by itself put data in Dune (§4).

## §0 - What is already true

**The physical types are fixed and documented.** `rows_to_batch` writes exactly four columns as `UInt64`
(`block_number`, `log_index`, `_seq`, `block_timestamp`) and every other column as nullable `Utf8`, gated
by `sealed_parquet_writes_uint64_counters_and_utf8_uint256_without_dec_companions`
(`reading-segments.md`). A `uint24` event parameter is text. The DuckDB `<col>_dec` and
`<col>_overflow` companions are **not in the file**: they are view columns computed at query time, so a
Dune model has only the exact text to cast from.

**Every column already names its kind.** `schema.json` records, per table, each column's `name`,
`sol_type`, `storage` and `indexed`. The `storage` vocabulary is closed, from `StorageKind::as_str` in
`decode/src/registry.rs` plus the implicit columns: `u64`, `i64`, `word16`, `word32`, `address`, `bool`,
`fixed_bytes`, `bytes`, `string`, `json`, `hash32`, and `bytes32` for the implicit `block_hash` and
`tx_hash`. The map in §3 is a function of `storage` and `sol_type`, never of a contract, which is what
makes it generatable.

**The dangerous columns are already derived.** `semantic.toml`'s `[table.*.footguns]` are computed from
the registry, not authored: `reserved_words`, `big_ints`, `overflows_dec`, `bools`. The emitter uses
them as a cross-check against `storage`, not as a second source of truth.

## §1 - Goals and non-goals

Goals:

- One query per table, casting every column from its physical type to the DuneSQL type §3.2 names,
  with the table and column descriptions from `semantic.toml`.
- Byte-identical output for identical inputs, with no network and no model in the output path.
- A refusal, named per column or view, for everything that does not translate exactly (§5).

Non-goals:

- **Getting data into Dune.** §4: there is no public way to point Dune at the mirror, and the one
  public route, a row-insert sidecar, is its own work.
- Translating authored DuckDB SQL (`views/*.sql`). §5 and slice S3.
- Contributing to Spellbook, or borrowing its code (§3.3).
- Any change to what nuthatch stores or publishes.

## §2 - Inputs and output

Input: a nest directory, read-only. `schema.json` (column kinds), `semantic.toml` (descriptions, grain,
footguns), and the nest's name and chain from `nuthatch.toml`.

Output: one `<model>.sql` file per table plus a `README.md` listing what was and was not emitted, in a
fixed order with a fixed layout (§3.3).

`nuthatch emit dune --dir <nest> --out <dir> --source <namespace>`. `--source` is the Dune namespace the
rows were loaded into (§4). It is the operator's, not the nest's, so it is never inferred.

## §3 - The type map

### 3.1 - The physical side

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

The source columns are `varchar` except the four counters (§4). Citation keys are in §10.

| from | DuneSQL type | expression | on bad input | cite |
| --- | --- | --- | --- | --- |
| `block_number`, `log_index` | `bigint` | `cast(c as bigint)` | cannot exceed `bigint` on any chain in scope | D1, D10 |
| `block_timestamp` | `timestamp`, and `date` | `cast(from_unixtime(c, 'UTC') as timestamp)`; `cast(... as date)` | not documented | D4, S2, D10 |
| `bytes32` / `address` / `fixed_bytes` / `bytes` / `hash32` | `varbinary` | `from_hex(substr(c, 3))`, stripping the `0x`; never `cast(c as varbinary)`, which encodes the string's bytes | not documented; unverified | D3, T1 |
| `u64` with `sol_type` up to `uint56`; `i64` | `bigint` | `cast(c as bigint)` | not documented | D2 |
| `u64` with `sol_type` `uint64` | `uint256` | `cast(c as uint256)`, since `uint64` values can exceed `bigint`'s maximum | unverified, below | D1, D2 |
| `word16` / `word32`, unsigned | `uint256` | `cast(c as uint256)` | unverified, below | D1, D2 |
| `word16` / `word32`, signed | `int256` | `cast(c as int256)` | unverified, below | D1, D2 |
| `bool` | `boolean` | `case c when 'true' then true when 'false' then false end` | NULL for any other text, by `CASE`'s definition | D1 |
| `string` | `varchar` | `c` | n/a | |
| `json` | `varchar` | `c`, read with `json_value(c, 'strict $.field')` | `try` handles path errors | D12, T2 |

Four rules sit around the table:

- **Every wide integer keeps its exact text beside the typed column**, as `<name>_raw` `varchar`, the
  suffix Spellbook's seeds use for raw amounts (S11). A user can always reach the exact value, and a
  name collision with a real ABI parameter is a refusal (§5), not a rename.
- **Casts are `cast`, not `try_cast`.** `try_cast` "returns null if the cast fails" (D2). A silent NULL
  in a wide-integer column undercounts every sum over it, which is a wrong number presented as a right
  one. A cast that fails should fail the query.
- **The `0x` prefix is stripped before `from_hex`.** Dune's own example passes a prefixed string,
  `from_hex('0x6574686275696c646572')` (D3), but Trino's `from_hex` documents hex digits only (T1),
  and RFC-0052 §5 already wrote `from_hex(substr(a, 3))` for Dune. `substr(c, 3)` is correct under both
  readings, so the emitter does not depend on Dune accepting the prefix.
- **The wide-integer cast is the load-bearing unverified claim.** D2 says `cast` "can be used to cast a
  varchar to a numeric value type". Every `UINT256` example in D1, D2 and D3 starts from a literal, an
  integer or `varbinary`, never from decimal text. No documented intermediate step exists from decimal
  text: the `varbinary_to_uint256` route needs hex. S0 runs `cast('<2^256-1>' as uint256)` and the signed
  extremes on Dune before any other slice depends on them.

### 3.3 - The output target and naming

**Plain DuneSQL, one `SELECT` per table, not `CREATE VIEW` and not dbt.** A saved query works on any
plan and can be read as `query_<id>` (D15). A materialised view of it works on any plan within that
plan's storage (D16). `CREATE OR REPLACE VIEW` over the Trino endpoint and the dbt connector are
Enterprise-only, with Data Transformations enabled (D17, D18). Plain SQL is the target that reaches
every Dune user. A dbt wrapper in Spellbook's shape is a later slice for Enterprise users, if asked for.

**Names follow Dune's decoded tables, as a familiarity choice.** Dune documents
`[projectname_blockchain].[contractName]_evt_[eventName]` and the columns `contract_address`,
`evt_tx_hash`, `evt_index`, `evt_block_time` and `evt_block_number` (D8, S9). That convention is
documented **only for tables Dune decodes itself**, from submitted ABIs (D9). Nothing documents a
convention for third-party or uploaded tables, and a third party cannot create a `<project>_<chain>`
schema: it writes only into its own team namespace (D18, D19). So the emitter borrows the names for
familiarity, not compliance:

- the query is named `<nest>_<chain>_<alias>_evt_<event>`, lowercase, since Dune stores table names
  lowercase (D13);
- `address` becomes `contract_address`, `tx_hash` becomes `evt_tx_hash`, `log_index` becomes
  `evt_index`, `block_number` becomes `evt_block_number`, `block_timestamp` becomes `evt_block_time`,
  and `evt_block_date` is added;
- `block_hash` becomes `evt_block_hash`, which Dune's decoded tables do not document, and `_seq`, an
  ordering column of nuthatch's own, is not emitted;
- event parameters keep their ABI names, camelCase and all, as Dune's decoded tables do (S9).

The `evt_*` column **types** are not documented anywhere. They follow `ethereum.logs`, which is: `bigint`
block number, `timestamp` block time, `varbinary` hashes and addresses, `date` block date (D10).

**Every ABI-derived identifier is double-quoted.** Only some ABI names are reserved words in DuneSQL
(`from`, `order`, `group`; D6), and quoting all of them is the deterministic rule. Descriptions from
`semantic.toml` go into a comment header, table first, then one line per column, since plain DuneSQL
has no column comments.

**Spellbook's code is not borrowed.** Spellbook is Business Source License 1.1, with an Additional Use
Grant that excludes use for "a Data or Analytics Platform", converting to GPL-3.0-or-later on
2027-03-03 (S1). Its conventions are public in Dune's own docs; its macros and model SQL are not used,
before or after that date.

## §4 - How the models meet the data

**There is no public way to point Dune at a Parquet prefix.** Brief 7 found no external table, no bucket
connection and no way to attach a catalog on any plan, and Parquet is not an accepted upload format
anywhere in the docs. Every documented route copies rows into Dune-managed storage (D18, D19; [Trino
Connector Overview](https://docs.dune.com/api-reference/connectors/trino/overview.md)). The only public
mention of object storage going into Dune is the sales-led [Dune for
chains](https://dune.com/dune-for-chains) offer, which this RFC does not rely on. Datashare and S3
Export go the other way.

So RFC-0052's mirror does not reach Dune by itself. **The one public route is a sidecar**, RFC-0052 §8's
row-insert follow-on. It creates one table per nuthatch table with `POST /v1/uploads` (10 credits each),
watches the published `manifest.json`, and appends each new segment's rows as NDJSON through
`POST /v1/uploads/:namespace/:table/insert`. Each insert is atomic, at most 1.2 GB, and billed at 3
credits per GB with a minimum of 1 credit per request ([Insert
Data](https://docs.dune.com/api-reference/tables/endpoint/uploads-insert.md),
[Billing](https://docs.dune.com/api-reference/overview/billing.md)).

The per-request minimum makes cadence the price. One insert per table every five minutes is about 8,640
credits a month per table; hourly is about 720. That is arithmetic on those two pages, not a published
figure. Storage is capped at 100 MB on Free, 1 GB on Analyst and 15 GB on Plus, and uploaded data is
public unless the operator is on Enterprise ([How Credits
Work](https://docs.dune.com/resources/credits-billing/how-credits-work.md)).

What this means for the emitter:

- **The queries read the sidecar's tables, not the mirror.** Their source is `dune.<namespace>.<table>`,
  the create API's `full_name` form ([List Uploaded
  Tables](https://docs.dune.com/api-reference/tables/endpoint/uploads-list.md)), and `--source` names the
  namespace.
- **The queries assume the sidecar loads every column as `varchar`** except the four counters, and cast
  at query time. The column types `create` accepts are not published; `varchar` is the one choice the
  docs support, and it keeps the exact-text contract intact across the upload.
- **The emitter is useful without Dune's cooperation.** It emits correct DuneSQL for anyone holding the
  rows, through the sidecar or a private arrangement, and does not claim a Dune integration by existing.
- **The sidecar is a prerequisite for S4 and is not this RFC.** It is a few hundred lines outside the
  binary (RFC-0052 §8) and needs its own acceptance.

## §5 - What it refuses

- **`hash32` columns are emitted as the hash they are**, named as such in the column comment, never as
  the value they hash.
- **`json` columns stay text.** Typing a tuple's fields needs each component's ABI type, and that is a
  later slice if asked for, not a guess now.
- **Authored views (`views/*.sql`) are not translated in v1.** They are DuckDB SQL over the nest's own
  `_dec` companions, which do not exist on the Dune side. Each is listed in the output `README.md` as
  not emitted, with the reason.
- **A `storage` value outside §3.1 fails the run** and names the column. A new `StorageKind` extends the
  map deliberately.
- **A disagreement between `storage` and the footguns fails the run**, for example a `word32` column
  missing from `big_ints`. One of the two files is stale, and the output would be wrong either way.
- **A name collision fails the run**: a parameter named `contract_address`, `evt_index` or another
  emitted name, or `<name>_raw` colliding with a real parameter. Renaming silently would make a model
  disagree with its ABI.

## §6 - Correctness and tests

- **Golden:** a fixed `schema.json` and `semantic.toml` covering every §3.1 row, a signed and an
  unsigned wide integer, a `uint64`, a reserved-word parameter, a `hash32` and a `0x`-prefixed address go in, and exact output
  files come out, compared byte for byte.
- **Vocabulary pin:** a test enumerates `StorageKind` and fails if a variant has no §3.2 row, so the map
  cannot fall behind the decoder.
- **Refusal tests:** an unknown kind, a storage/footgun disagreement and a name collision each fail with
  the column named.
- **What offline tests cannot prove** is that DuneSQL accepts the emitted casts. S0 runs the unverified
  ones on Dune, and S4 runs whole queries over real rows, or records that it could not.

## §7 - Relationship to `port_emit.rs`

Deliberately separate. `port_emit.rs` (RFC-0044 S2) consumes a subgraph port report, classified fields
with mapping citations, and writes nuthatch artefacts (`[[calls]]`, `views/*.sql`, entities). This
consumes a nest's own `schema.json` and `semantic.toml` and writes another engine's dialect. The inputs,
the target and the failure modes have nothing in common but the table names. They share only the
existing parsers for `semantic.toml` (`semantic::Semantic`) and `schema.json`, which this reuses rather
than duplicating.

## §8 - Slices, each with a criterion that can fail

| slice | delivers | fails if |
| --- | --- | --- |
| S0 - verify | the §3.2 casts marked unverified, run once on Dune with literals, recorded with the query and its output, including `from_hex(substr('0x…', 3))` against the prefixed form; a free account suffices because no upload is involved | `cast` from decimal text to `uint256` or `int256` is refused, or loses a digit, at `2^256-1`, `-2^255` or `2^255-1`; then §3.2 is redesigned before S1 |
| S1 - per-table queries | `nuthatch emit dune`, §3 casts and names, §5 refusals, the golden test | the golden output changes between two runs, or a `StorageKind` variant has no row |
| S2 - descriptions | table and column descriptions and grain from `semantic.toml` in each query's comment header | a description in `semantic.toml` is missing from the emitted query |
| S3 - authored views | the subset of `views/*.sql` that translates exactly, the rest named | a translated view returns a different result from the DuckDB original on the fixture |
| S4 - recorded run | the emitted queries run in Dune over a real nest's rows loaded by the RFC-0052 §8 sidecar, recorded; blocked until that sidecar exists (§4) | a query is listed as working that was never run |

## §9 - Unresolved

Ingestion, from brief 7. None of these is publicly documented, and each blocks S4 until a real run answers
it:

1. The column types `POST /v1/uploads` accepts, and whether `uint256`, `int256` or `varbinary` are among
   them. §4 assumes `varchar` for that reason.
2. Whether an NDJSON insert can omit a nullable column, and whether an uploaded table can gain one. A
   nest's columns drift between seals (`reading-segments.md` §Ordering), so the sidecar may need a new
   table per column set.
3. The upsert endpoint named in the March 2026 changelog: its path, key semantics and cost.
4. Which credit rule is enforced. Billing says 3 credits per GB with a 1-credit minimum; the older
   Tables Overview still describes data points per credit.

Types and conventions, from brief 8:

5. Whether `cast(varchar as uint256)` and `cast(varchar as int256)` accept the full ranges. S0 answers it.
6. Whether Dune's `from_hex` accepts a `0x` prefix, which §3.2 avoids depending on, and what it does with malformed input. Also what `cast` does with out-of-range text into `bigint`.
   The source text is canonical, so neither should occur, but neither is documented.
7. The types of the decoded `evt_*` columns, and whether `evt_tx_from`, `evt_tx_to` or `evt_block_date`
   exist in Dune's own tables. §3.3 follows `ethereum.logs` and says so.
8. Dune's session time zone. §3.2 passes `'UTC'` explicitly for that reason.

## §10 - Sources

All read 2026-09-13. Spellbook at `c73960eb` on `main`.

- D1 https://docs.dune.com/query-engine/datatypes
- D2 https://docs.dune.com/query-engine/Functions-and-operators/conversion
- D3 https://docs.dune.com/query-engine/Functions-and-operators/varbinary
- D4 https://docs.dune.com/query-engine/Functions-and-operators/datetime
- D6 https://docs.dune.com/query-engine/reserved-keywords
- D8 https://docs.dune.com/data-catalog/evm/ethereum/decoded/event-logs
- D9 https://docs.dune.com/data-catalog/evm/ethereum/decoded/overview
- D10 https://docs.dune.com/data-catalog/evm/ethereum/raw/logs
- D12 https://docs.dune.com/web-app/decoding/multichain-decoding
- D13 https://docs.dune.com/web-app/decoding/best-practices
- D15 https://docs.dune.com/query-engine/query-a-query
- D16 https://docs.dune.com/query-engine/materialized-views
- D17 https://docs.dune.com/api-reference/connectors/dbt/overview
- D18 https://docs.dune.com/api-reference/connectors/sql-operations
- D19 https://docs.dune.com/web-app/upload-data
- S1 github.com/duneanalytics/spellbook `LICENSE`
- S2 github.com/duneanalytics/spellbook `AGENTS.md`
- S9 github.com/duneanalytics/spellbook `sources/aave/aave_sources.yml`
- S11 github.com/duneanalytics/spellbook `dbt_subprojects/dex/seeds/_project/zeroex/ethereum/_schema.yml`
- T1 github.com/trinodb/trino `docs/src/main/sphinx/functions/binary.md` (`from_hex`)
- T2 github.com/trinodb/trino `docs/src/main/sphinx/functions/conditional.md`
