# RFC-0056: The Dune row-insert sidecar - a published nest's rows into Dune through the upload API

**Status:** **Draft** - awaiting Chief's decision (#1362). New binary capability, so it is written and
decided before any code; the build slices in §9 are filed on acceptance.

**Date:** 2026-09-15

**Author:** Pete (cargopete)

**Depends on:** RFC-0052 (the published layout and reader contract this reads, and §8, which names this
sidecar), RFC-0055 (the queries that read what this loads: §3.3 naming, §4 the route, §9 the open
questions), RFC-0046 §1 (the deletion test), [reading-segments.md](../reading-segments.md) (ordering,
column sets, physical types), [reading-published-nest.md](../reading-published-nest.md) (the consumer
contract).

**Provenance:** public sources only, by the rule this work runs under. Every claim about Dune cites a
public page read on 2026-09-15 (§11). Where the public record is silent, §10 says so and the design
either avoids depending on it or gives a slice that runs it first. Nothing here called the Dune API.

## Abstract

RFC-0055 writes DuneSQL over a nest's tables, and its §4 found that nothing public lets Dune read a
Parquet prefix. Every documented route copies rows into Dune-managed storage, and the only
append-capable one is `POST /v1/uploads` to create a table, then `POST /v1/uploads/:namespace/:table/insert`
with CSV or NDJSON. This RFC is that route.

A `dune` subcommand group under `nuthatch publish` reads a published dataset's catalogue, transcodes
each catalogued segment not yet loaded from Parquet to NDJSON, and appends it to one Dune table per
nuthatch table in the operator's namespace, named exactly as `nuthatch emit dune` already reads them. A
cursor file keyed by segment content hash records what landed. Every row carries its segment and its
row ordinal, so a duplicate is always detectable, and a request whose outcome is unknown blocks its
table until an explicit reconcile has counted it in Dune. It runs only when an operator invokes it with
a key. It is not part of `dev` or `serve`, and it has no configuration key.

Worked through on a real nest (§5), the per-request credit minimum makes batching the whole credit
question for a backfill, and Dune's storage caps, not credits, are what bound a nest with real history.

## §0 - The non-negotiable this appears to strain, and why it does not

`CLAUDE.md` non-negotiable 3: *"No phone-home. No telemetry, no mandatory API tokens, no gated data
services."* A nuthatch command that sends a nest's rows to a third-party service, and needs an API
token to do it, reads at a glance as all three. It is none of them, for the reasons RFC-0052 §0 gave
for publishing to a bucket, each of which carries over.

- **It sends nothing the operator did not choose to send.** The destination is the operator's own Dune
  namespace, reached with the operator's own key, and the payload is the rows of tables they named.
  Nothing about the operator, the binary or its use goes anywhere. It is `nuthatch publish sync` with a
  different kind of target.
- **The token is mandatory only for the command whose one purpose is to use it.** `AWS_*` credentials
  are mandatory for an `s3://` publish target in exactly this sense. No other command reads
  `DUNE_API_KEY`, and without it the commands that talk to Dune refuse before they build an HTTP client
  (§4.8).
- **No data service is gated.** No capability of nuthatch requires Dune. The rows in Dune are a copy of
  a published mirror that any engine can still read (RFC-0052), and the nest that produced them is the
  same whether or not the copy exists.
- **The deletion test holds by construction.** RFC-0046 §1: delete the feature and a self-hoster loses
  nothing; enable it and the binary still runs for anyone who did not. There is no `nuthatch.toml` or
  `mounts.toml` key, no task in `dev` or `serve`, and no call site outside the subcommand (§3). S1
  records the test suite passing with the module deleted.

Two neighbouring rules hold as well. **Non-negotiable 4:** nothing read back from Dune enters stored
state. The one read, reconcile's row count (§4.6), goes into the sidecar's own cursor file and nowhere
else. **RFC-0045 §3:** nothing is fetched from Dune into a nest. The direction is one way.

## §1 - What is already true

**The published dataset is a contract, and it holds only what is safe to copy.** RFC-0052 publishes
`<dataset>/manifest.json`, `publish.json`, `schema.json` and `<dataset>/<table>/<hash>.parquet` for
every catalogued, non-provisional segment. The mirror is append-only: an object is never removed.
`publish.json` carries `data_identity`, `schema_sha256` and `catalogue_sha256`, and a reader checks the
last against the manifest it fetched before trusting either
([reading-published-nest.md](../reading-published-nest.md)).

**The catalogue appends in seal order, not block order.** A later seal of an earlier range, or a
re-seal, can sit after a newer segment ([reading-segments.md](../reading-segments.md) §Ordering). A
reader that records "loaded through block N" skips it.

**A segment's columns are the keys its rows carried.** `rows_to_batch` (`src/seal.rs`) takes the sorted
union of every row's JSON keys, so a column no row in the file carries is absent, not null-filled, and
one table's segments differ in their column sets. Every key becomes a column, including two that
`schema.json` does not declare: `table`, the routing key, which holds the table's own name, and `_seq`,
nuthatch's ordering counter. Measured: the newest segment of the `dips-nest` nest on the maintainer's
MacBook, sealed 2026-08-28, carries both. `block_number`, `log_index`, `_seq` and `block_timestamp` are
`UInt64`; every other column is nullable `Utf8`.

**A nest's columns can start with an underscore.** ABI parameter names become column names unchanged.
Measured in `schema.json` on the same machine: the `gns` tables of the `graph-network` nest carry
`_counterpart`, `_l1SubgraphId`, `_l2SubgraphID`, `_l2Curator` and `_tokens`, and the `staking_legacy`
tables of `graph-allocations-nest` carry `__DEPRECATED_cooldownBlocks`. Every table carries `_seq`.

**The emitted queries already name their source.** `nuthatch emit dune` writes
`FROM dune.<source>.<table>`, where `<table>` is the nuthatch table name unchanged and `<source>` is the
namespace (`src/dune_emit.rs`). It never selects `_seq` or `table`. RFC-0055 §4 assumes the loaded
columns are `varchar` except the counters.

**The binary already holds every part but the Dune client:** a Parquet reader (`parquet`, `arrow`), the
published-dataset reader (the `Mirror` in `src/publish.rs`, over a directory or `object_store`), and an
HTTP client with rustls and streamed bodies (`reqwest`).

**What Dune documents about its upload route** (keys refer to §11):

- `POST /v1/uploads` creates a table from `namespace`, `table_name`, `schema` (a list of `name`, `type`
  and `nullable`, which defaults to true), and optional `description` and `is_private`. It fails if the
  table exists, and costs 10 credits [U1].
- "Column names in the table can't start with a special character or a digit." [U1]
- The create page's examples use two column types, `timestamp` and `double`. No page read lists the
  accepted types, and the published OpenAPI specification defines no `/v1/uploads` path [U1, O1]. The
  August 2026 changelog says uploaded tables "now support complex column types such as
  `array<struct<...>>`" [C1].
- `POST /v1/uploads/{namespace}/{table_name}/insert` takes CSV or NDJSON, at most 1.2 GB. "Either all of
  the data in the request was inserted, or none of it was." The data "must conform to the schema, and
  must use the same column names as the schema". It costs "3 credits per GB written, minimum charge is 1
  credit". "A limited number of concurrent insertion requests per table is supported", with 5 to 10
  recommended. The response carries `bytes_written` and `rows_written` [U2].
- Clear removes every row and keeps the schema [U3]. Delete removes the table, costs no credits, and
  needs an admin [U4].
- RFC-0052 §8 names `/table/{ns}/{name}/insert`. Dune deprecated the `/v1/table` paths in November 2025
  and set their removal for 2026-03-01 [U5, C1]. This RFC uses the `/v1/uploads` paths.
- Insert and create are low-limit, write-heavy endpoints: 15 requests a minute on Free, 70 on Plus [R1].
- Uploads need a key with the `Read/Write` scope, sent as `X-DUNE-API-KEY` [A1, U1].
- Storage is a per-plan cap, not charged per credit: 100 MB on Free, 1 GB on Analyst, 15 GB on Plus
  [B1]. "Once you hit your plan's limit, you'll need to free space or upgrade to continue writing new
  data." Analyst is $75 a month for 4,000 credits, and Plus is $399 for 25,000 [B2].
- "All data uploaded is public and can be accessed by anyone." Private uploads need Enterprise [W1],
  although the create API accepts `is_private` on any request [U1].
- An upsert endpoint "for merging data and preventing duplicates" was announced in March 2026 [C1]. No
  page read documents its path, its key or its cost.
- `POST /v1/sql/execute` runs SQL with a `Read` key and is charged by the compute it uses [E1].

## §2 - Goals and non-goals

Goals:

- Every catalogued segment of the selected tables of one published dataset lands in Dune exactly once,
  as rows that the queries `nuthatch emit dune` writes read without change.
- A crash, a timeout or a retry can never insert a segment twice silently. Where the outcome cannot be
  known, the table stops and says so.
- Cost is predictable before a request is sent, and batching keeps the per-request minimum from setting
  it.
- No network call, no task and no configuration unless the command is invoked.

Non-goals:

- Reading a nest's own directory (§4.1).
- Any change to what nuthatch stores, seals or publishes, or to `dev` and `serve`.
- Transforming values. Casting belongs to RFC-0055, at query time. The sidecar passes exact text
  through.
- Removing or rewriting rows in Dune, apart from an operator's deliberate clear and reload (§7).
- Enterprise ingestion, Datashare, and Dune's upsert endpoint while it is undocumented (§8).
- Scheduling. The operator's timer runs it.

## §3 - Where it lives

**Decision: a `dune` subcommand group under `nuthatch publish`, compiled into the default binary, in one
module reachable from that subcommand and nowhere else.** RFC-0052 §8 and RFC-0055 §4 both describe the
sidecar as sitting outside the binary. This RFC overrides that description, for these reasons.

| option | verdict |
| --- | --- |
| A Python script | Refused. Chief's rule of 2026-09-05: no Python in nightswatchhq. |
| A bash script over `jq` and `curl` | Cannot work alone. Bash cannot read Parquet, so the script needs another engine to transcode, and the obvious one, the DuckDB CLI, is a second install the operator must trust to pass exact text through unchanged. |
| A separate Rust crate and binary | Works, and costs the most. It relinks `parquet`, `arrow`, `object_store` and `reqwest`, and ships a second release artefact beside the single binary of non-negotiable 1. Worse, the column-name mapping of §4.2 must agree with `nuthatch emit dune`, and two binaries released separately can disagree. |
| In the binary, behind a non-default cargo feature | Keeps it out of the default artefact, as the `counter` feature does for payment (#1217). That was right for payment, whose absence from the default binary is the property RFC-0046 protects. Here it would mean the released binary lacks a documented command, which is the failure `Cargo.toml` records for the `object-store` feature before 2026-07-28. |
| **In the default binary, reachable only from its subcommand** | **Chosen.** It reuses the reader, the Parquet stack and the HTTP client, shares the name mapping with `emit dune` through one function, and adds no artefact. |

What makes the chosen option safe is structural, not a promise. The module adds no `nuthatch.toml` or
`mounts.toml` key, spawns nothing in `dev` or `serve`, and is referenced only from CLI dispatch. S1 adds
a gate in the shape of `tests/payment_absent.rs` that fails if any source file other than the module and
its dispatch arm names the module, or if `dev`, `serve` or `mounts.toml` gain a Dune option. S1 also
records one run of the full suite with the module and its subcommand deleted.

## §4 - Design

### 4.1 - Source: a published dataset, never the nest's directory

The sidecar reads `--dataset <dir | s3://bucket/prefix/<dataset>>` through the same `Mirror` reader
`nuthatch publish verify` uses, and follows reading-published-nest.md: it fetches `manifest.json` and
`publish.json`, and retries until `sha256(manifest.json)` equals `catalogue_sha256`, or refuses.

It does not read a nest's own `segments/`, because that directory holds **provisional** segments, which
the next seal folds into a new segment. Loading a provisional segment and then its fold inserts the same
rows twice, and no cursor keyed on content can see it, because the two segments have different hashes. A
published dataset holds only catalogued, non-provisional segments, which never change. An operator who
wants no bucket publishes to a directory (`nuthatch publish sync --target /path`) and points the sidecar
there.

Reading the published form also means the sidecar can run on a different host from the nest, needs no
access to the nest's directory, and can load any dataset its operator can read.

### 4.2 - The Dune tables

- **One Dune table per selected nuthatch table**, named with the nuthatch table name unchanged, in the
  namespace given by `--namespace`. That is the name `nuthatch emit dune --source <namespace>` already
  reads, so the shipped emitter needs no change for it.
- **Tables are selected explicitly**, with `--tables <list>` or `--tables '*'`. There is no default. A
  nest's decoded rows are public chain data, but an offchain table (RFC-0045) may not be, and uploads
  below Enterprise are public [W1].
- **Visibility is required**, as `--visibility public|private`. The sidecar sends `is_private` to match
  and never chooses for the operator. What Dune does with `private` below Enterprise is not documented
  (§10).
- **Columns**, in order: every column `schema.json` declares for the table except `_seq`; then
  `nuthatch_segment` and `nuthatch_row` (§4.6). `table` and `_seq` are not uploaded: `table` is constant
  within a table, and the emitted queries read neither.
- **Types.** `block_number`, `log_index`, `block_timestamp` and `nuthatch_row` are created as `bigint`
  if S0 records Dune accepting that name, and otherwise as `double`, which the create page's own example
  uses. A `double` holds every integer below 2^53 exactly, and no counter in scope approaches that: block
  numbers are below 10^9, timestamps below 2 × 10^9, and log indices below 10^6. RFC-0055's
  `cast(c as bigint)` and `from_unixtime(c, 'UTC')` accept either. Every other column is `varchar`, which
  RFC-0055 §4 already assumes. Every column is nullable.
- **Names that start with a non-letter.** Dune refuses column names that start with "a special character
  or a digit" [U1], and whether an underscore counts is not documented. S0 creates a column named `_x`
  and records the answer. If it is accepted, names pass unchanged. If it is refused, the sidecar and
  `nuthatch emit dune` both map every column name that starts with a non-letter to `nh` followed by the
  name, so `_counterpart` becomes `nh_counterpart`, through one shared function. A mapped name that
  collides, case-insensitively, with another column of the table refuses the table. S3 makes that change
  to the emitter.
- **The table description** records the dataset's `data_identity` and `schema_sha256`, so a table's
  origin can be read in Dune without the cursor.
- **Creation.** The sidecar creates a table only when its cursor has no entry for it. A create that fails
  because the table exists refuses, naming the table, rather than adopting a table the sidecar cannot
  prove it wrote.

### 4.3 - The row

Each Parquet row becomes one NDJSON line. `UInt64` columns become JSON numbers. `Utf8` columns become
JSON strings, byte for byte, and a null becomes JSON `null`. Nothing is cast, trimmed or reformatted, so
the exact text RFC-0055 casts from survives the upload.

**Every uploaded column is present on every line.** A column the segment does not carry is written as
`null`. Whether Dune accepts an omitted nullable column is not documented [U2], so the design never asks
it to.

The format is NDJSON, not CSV, because JSON tells a null from an empty string without a convention, and
needs no quoting rule for text that holds commas, quotes or newlines, which `string` and `json` columns
do.

### 4.4 - Batching and memory

The minimum charge of 1 credit per request [U2] makes the request count the price of small segments
(§5), so one request carries many segments. Per table, the segments not yet loaded are taken in the
catalogue's block order, `(from_block, to_block, hash)`, and packed into one request until the next
segment would take the body past `--batch-bytes`, which defaults to 1 GiB, below the 1.2 GB cap. The
sidecar transcodes a batch into a spool file in a temporary directory, so the request has an exact
`content-length`, the body streams from disk, and memory holds one segment's decoded rows at a time
rather than the batch. That keeps a sidecar running beside a nest out of the nest's memory budget
(non-negotiable 2); S1 bounds it.

A single segment whose NDJSON alone exceeds the budget refuses its table, naming the segment. A segment is
the unit of the cursor, and splitting one across two requests would break that. Since #1391 a seal cut is
bounded at 64 MiB of row JSON (`SEAL_DIRECT_BYTES`), so a segment sealed by a current binary cannot reach
1 GiB. The largest segment in the nest §5 measures is 1.43 MB of Parquet.

Requests run one table at a time and one request at a time, inside every documented limit [U2, R1].
Parallelism can be added if a recorded run shows it is needed.

### 4.5 - The cursor

The cursor is a JSON file at `--state <file>`. The flag is required, because the cursor is the only record
of what was loaded and its location is the operator's decision. It never sits inside a nest directory,
where authored files feed the NID, and it is never kept in Dune. It holds:

- the `namespace`, the dataset's `data_identity` and `schema_sha256`, the visibility chosen, and the
  `--from-block` value, if one was given;
- per table: the uploaded column names and types, and the set of **segment hashes** loaded;
- per table: at most one **in-flight batch**, with its segment hashes and the time it was sent.

The cursor is keyed by segment content hash, never by block number, because the catalogue appends in seal
order (§1). A segment appended later for an earlier range is a hash the cursor has not seen, and it is
loaded like any other.

`--from-block <n>` loads only segments whose `to_block` is at or above `n`. Segments are loaded whole, so
a segment that straddles `n` brings its earlier rows too. A run with a different value from the cursor's
refuses.

The cursor is replaced atomically, by writing a temporary file and renaming it over the old one, and it is
flushed before any request that depends on it.

### 4.6 - The dedup key, and the request whose outcome is unknown

**The dedup key is `(nuthatch_segment, nuthatch_row)`.** `nuthatch_segment` is the first 16 hex
characters of the segment's hash, and `nuthatch_row` is the row's 0-based position in its Parquet file.
Both are deterministic: the file is content-addressed and immutable, so its row order is fixed. Sixteen
characters rather than 64 save 48 bytes a row, 1.6 GB on the nest in §5. A collision would need two
segments of one table to share a 64-bit prefix, and the sidecar checks the whole catalogue for one,
offline, before any request. A collision refuses the table.

An insert is atomic [U2], so after any one request a segment is either wholly in Dune or wholly absent.
What an insert does not give is an idempotent retry. Nothing public documents deduplication on `/insert`,
and the upsert endpoint that might provide it is undocumented (§10). The protocol:

1. Record the batch as in flight in the cursor, and flush.
2. Send the request.
3. On `200`, move the batch's hashes to the loaded set and clear the in-flight record.
4. On any other status, Dune has inserted nothing [U2]. Clear the in-flight record, and fail the run with
   the response.
5. On a timeout, a reset connection or a crash, the outcome is unknown, and the in-flight record stays.

**A table with an in-flight record refuses to sync.** It is resolved only by `publish dune reconcile`,
which is an explicit command because it spends query credits. It sends one query through
`POST /v1/sql/execute` [E1]:
`SELECT nuthatch_segment, count(*) FROM dune.<namespace>.<table> WHERE nuthatch_segment IN (...) GROUP BY 1`.
For each in-flight segment, a count equal to the catalogue's `rows` means it landed, and zero means it
did not. Any other number refuses, because an atomic insert of whole segments cannot produce it.

A zero is trusted only once the in-flight record is older than `--settle`, which defaults to 10 minutes.
Whether a row is visible to a query as soon as its insert returns is not documented (§10), and trusting
an early zero would re-send a batch that did land. S0 measures the delay, and the default is set from
that measurement.

`publish dune verify` runs the same count over every loaded segment and compares each count with the
catalogue. It is the remote twin of `nuthatch publish verify`, and it is the check S4 records.

### 4.7 - Column drift and schema change

**Drift between segments never reaches Dune.** A Dune table's columns are fixed at creation from
`schema.json`, and every line carries every column (§4.3). A segment that lacks a column contributes
nulls for it.

**A segment that carries a column `schema.json` does not declare refuses its table**, naming the table,
the column and the segment, unless the column is `table` or `_seq`. Either the published schema is stale
or the catalogue is not the one the schema describes, and loading the rows would drop a value silently. A
row whose `table` value is not the table it was catalogued under refuses the same way.

**A schema never changes under a table the sidecar created.** A dataset's `schema.json` is covered by its
`data_identity`, which hashes the registry hash and every data-affecting input (`src/blob.rs`), so a new
ABI or a new contract makes a new dataset. The cursor pins `data_identity` and `schema_sha256`, and a
mismatch refuses the whole run. Whether an uploaded table can gain a column is not documented [U2], and
the sidecar never tries.

The consequence is operational, and it is stated here rather than discovered. Table names carry no
dataset identity, so an emitted query, and every dashboard built on one, keeps working across a
re-index. A new dataset for the same nest therefore cannot share a namespace with the old tables. The
operator deletes them, which costs no credits and needs an admin [U4], and starts a new cursor. Suffixing
each table with its identity would avoid the delete and break every emitted query's source at each
re-index; §8 rejects it.

### 4.8 - Credentials and the boundary

- The key is read from `DUNE_API_KEY` in the environment and nowhere else. It is never a flag, so it
  stays out of shell history, and it is never written to the cursor, a spool file or a log line. A Dune
  error message is printed with the key redacted if the key appears in it.
- Without the variable, `sync`, `reconcile` and `verify` refuse before constructing an HTTP client.
  `status` and `sync --dry-run` never use the network and need no key.
- The only host contacted is Dune's API. Redirects to another host are refused. No call happens at
  startup or from any other command.

### 4.9 - Commands

The subcommands of `dune`, under `nuthatch publish`:

| command | network | does |
| --- | --- | --- |
| `publish dune status` | none beyond reading the dataset | per table: segments loaded, pending and in flight; pending bytes; the estimated credits of a sync |
| `publish dune sync` | Dune, with a key | creates missing tables, loads pending segments in batches, and refuses tables with an in-flight record |
| `publish dune sync --dry-run --out <dir>` | none beyond reading the dataset | writes each batch it would send as an NDJSON file and each create request as JSON, and changes no cursor |
| `publish dune reconcile` | Dune, spends query credits | resolves in-flight records (§4.6) |
| `publish dune verify` | Dune, spends query credits | counts every loaded segment in Dune against the catalogue |

Every command takes `--dataset`, `--namespace` and `--state`. The exit status is non-zero if any table
refused, and every refusal names its table and its cause.

## §5 - Cost, worked through for a real nest

**The nest.** `spookyswap-nest`: the SpookySwap V2 factory and its pairs on Fantom (chain id 250), 6
tables and 8,936 catalogue entries over blocks 3,795,376 to 17,548,833, which is 2021-04-17 to
2021-09-23, or 159 days. Measured on 2026-09-15 against the copy on the maintainer's MacBook: file sizes
with `stat` over the segment files, and rows and NDJSON size with the DuckDB CLI over
`read_parquet(..., union_by_name=true)` per table. NDJSON size is the sum over every row of
`strlen(to_json(row)) + 1`, compact JSON plus a newline, which is the shape the sidecar writes.

| table | segments | rows | Parquet bytes | NDJSON bytes |
| --- | ---: | ---: | ---: | ---: |
| `factory__pair_created` | 601 | 1,985 | 3,311,234 | 1,017,077 |
| `pair__burn` | 1,667 | 78,310 | 22,976,149 | 38,770,780 |
| `pair__mint` | 1,667 | 2,519,327 | 429,816,348 | 1,117,746,879 |
| `pair__swap` | 1,667 | 9,659,101 | 1,021,504,550 | 5,126,479,325 |
| `pair__sync` | 1,667 | 12,256,767 | 1,169,703,255 | 4,941,920,090 |
| `pair__transfer` | 1,667 | 8,835,784 | 634,626,363 | 4,060,033,235 |
| **total** | **8,936** | **33,351,274** | **3,281,937,899** | **15,285,967,386** |

The NDJSON is 4.66 times the Parquet. The measurement includes `_seq` and `table`, which the sidecar
drops, and lacks `nuthatch_segment` and `nuthatch_row`, which it adds. On this nest that nets to about 22
more bytes a row, so the sidecar's NDJSON would be about 16.0 GB. That adjustment is arithmetic on the
column widths, not a measurement; S1's golden test and S4's `bytes_written` replace it.

**The backfill.** An average segment is about 1.8 MB of NDJSON, which is 0.005 credits by size, so sent
one request per segment the 1-credit minimum sets the price.

| strategy | requests | credits |
| --- | ---: | ---: |
| one insert per segment | 8,936 | at least 8,936, plus about 48 by size |
| batched at 1 GiB | about 20 | about 50 |

Unbatched, the backfill costs more than twice Analyst's monthly 4,000 credits. Batched, it costs about
50: 3 credits per GB over 16.0 GB, plus the minimum on the few small requests, before any rounding Dune
applies (§10). At 15 requests a minute, the Free rate limit, the batched backfill sends in under two
minutes of request budget.

**Storage, not credits, is the binding limit.** 16.0 GB of NDJSON written exceeds every self-serve cap:
Free's 100 MB, Analyst's 1 GB and Plus's 15 GB [B1]. Whether a cap counts bytes written or bytes stored
after Dune's own compression is not documented (§10). If it counts something near the Parquet size, 3.28
GB, the history fits Plus and no plan below it. On either reading this nest's full history does not fit
Analyst, and the operator's levers are `--tables` and `--from-block`.

**Ongoing cost follows the seal rate, not how often the sidecar runs.** A run makes no request for a
table with no new segment. On this nest `pair__sync` sealed 28 segments in the last 30 days of its
range. That is the cut of a backfill over 2021 history, not a live tip's cadence, which follows the
chain's seal span, so it is an order of magnitude and not a forecast. If all six tables sealed at that
rate, which over-counts `factory__pair_created` with its 601 segments against 1,667, a daily run would
make at most 168 requests a month: about 168 credits by the minimum, plus about 9 by size, since 16.0 GB
over 159 days is about 3.0 GB a month. Running every five minutes would cost the same, because there
would be nothing more to send. This replaces RFC-0055 §4's figure of 8,640 credits a month per table at
a five-minute cadence, which assumed a request on every run whether anything had sealed or not. Growth of
about 3 GB a month passes Plus's cap in about five months from empty, if bytes written are what the cap
counts.

**Table count multiplies the floor.** Every table with a new segment costs at least 1 credit per run.
`graph-allocations-nest` has 81 decoded tables ([pricing-the-lodestar-nests.md](../pricing-the-lodestar-nests.md)),
so a run in which every table had sealed costs at least 81 credits there. That nest was not measured for
this RFC.

## §6 - Correctness argument

The invariant, for each selected table after a `sync` that exits zero and leaves no in-flight record: the
rows of `dune.<namespace>.<table>` are exactly the rows of the catalogued segments whose `to_block` is at
or above the cursor's `--from-block`, each row once, with every value's text unchanged.

- **Each row once.** A segment enters the loaded set only on a `200`, which means all its rows are in
  Dune [U2], and a segment in the loaded set is never sent again. A request whose outcome is unknown
  blocks its table until a count proves which way it went (§4.6).
- **Nothing skipped.** The cursor is keyed by hash, so a segment appended late for an earlier range is
  pending like any other (§4.5). The source is the published catalogue, which never removes an entry
  (RFC-0052 §3.2).
- **Nothing doubled at the source.** Provisional segments are never published, so a fold cannot deliver
  the same rows under a second hash (§4.1).
- **Values unchanged.** The transcode is a type-preserving copy of `UInt64` and `Utf8` (§4.3). A drifted
  column is null in exactly the segments that lack it, as it is in DuckDB's `union_by_name` scan.
- **A violation is detectable.** `(nuthatch_segment, nuthatch_row)` is unique per table by construction,
  so `verify` finds a duplicated or missing segment by counting.

## §7 - What it refuses

- A dataset whose `publish.json` never matches its manifest, or whose `data_identity`, `schema_sha256` or
  `--from-block` differs from the cursor's.
- A table with an in-flight record, until `reconcile`.
- A table that exists in Dune but not in the cursor.
- A segment column that `schema.json` does not declare, other than `table` and `_seq`, and a row whose
  `table` value is not its table.
- An added or mapped column name that collides with a declared one: `nuthatch_segment`, `nuthatch_row`,
  or an `nh`-mapped name.
- Two segments of one table that share the 16-character prefix.
- A segment whose NDJSON alone exceeds `--batch-bytes`.
- A reconcile count that is neither zero nor the catalogue's row count.
- A missing `DUNE_API_KEY`, for any command that talks to Dune.

There is no repair command. An operator who needs a table rebuilt clears it [U3] and removes the table's
entry from the cursor, both of which are deliberate acts.

## §8 - Alternatives rejected

- **Dune's upsert endpoint as the dedup mechanism.** It would make a retry idempotent, and
  `(nuthatch_segment, nuthatch_row)` is the natural key for it. It is announced [C1] and not documented,
  so its path, its key semantics and its cost are unknown. If it is documented, an amendment can replace
  §4.6's reconcile with it.
- **A block high-water mark as the cursor.** It skips a late seal of an earlier range (§1).
- **Reading the nest's own directory.** It double-loads provisional segments (§4.1).
- **One insert per segment.** The simplest design, and on the §5 nest about 180 times the credits of
  batching.
- **A new Dune table per column set**, which RFC-0055 §9 item 2 raised. Null-filling to the declared
  schema makes it unnecessary, and it would multiply tables and every query over them.
- **Table names suffixed with the dataset identity.** It survives a re-index without a delete, and it
  breaks every emitted query and dashboard at each re-index (§4.7).
- **A task in `dev` or `serve`, woken by seals.** It would put a third-party client in the runtime and
  fail the structural half of the deletion test (§3). A timer that runs `sync` achieves the same and costs
  the same, because cost follows seals (§5).
- **CSV.** It needs a null convention and a quoting rule for text, and NDJSON has both natively (§4.3).
- **Casting to native types before upload.** It needs create types that are not documented, and it moves
  RFC-0055's casts from query time to load time, where a wrong cast is stored rather than visible.

## §9 - Slices, each with a criterion that can fail

| slice | delivers | fails if |
| --- | --- | --- |
| S0 - verify | One recorded run on a real account against a scratch table, with no nest data: a create with `varchar`, `bigint`, `double` and a column named `_x`; a table name containing `__`; an NDJSON insert with an explicit `null` and one that omits a nullable column; the delay before an inserted row is visible, measured by polling a count through `POST /v1/sql/execute`; the credits charged per request, from the account's usage; `is_private: true` on the account's plan; and a delete. Recorded on the issue, with the key never shown | `varchar` is refused, an explicit `null` is refused for a nullable column, or a table name with `__` is refused. Any of those sends §4.2 or §4.3 back for redesign before S1 |
| S1 - the offline core | The module, `publish dune status` and `publish dune sync --dry-run`: the reader, the transcode, batching, the cursor, the name mapping shared with `emit dune`, and every §7 refusal that needs no network. A golden NDJSON test over the RFC-0055 fixture nest published to a directory. The structural gate of §3, and one recorded run of the suite with the module deleted | the golden bytes change between two runs; a segment appended later for an earlier range is not pending; a segment that lacks a column does not emit `null` for it; a run with no `DUNE_API_KEY` opens a socket; peak RSS passes 256 MiB during a dry run over the §5 nest; or any test outside the module fails with the module deleted |
| S2 - the network half | `sync`, `reconcile` and `verify` against a mock Dune server in tests, in the style of the mock servers in `src/rpc.rs` | a failure injected after a request is sent and before the cursor records it lets a later `sync` send that segment again without `reconcile`; `reconcile` marks a segment loaded on any count other than its catalogue rows; or `reconcile` trusts a zero younger than `--settle` |
| S3 - emitter alignment | Only if S0 records a leading underscore as refused: `nuthatch emit dune` reads mapped names through the shared function, with its golden output updated | a column name the sidecar would upload differs from the name the emitted query reads, checked across every fixture column. If S0 records the underscore as accepted, S3 closes citing S0 |
| S4 - the recorded run | A real public nest loaded into a real account with `sync`, then checked with `verify`, with the credits charged and `bytes_written` recorded against §5's estimate. This unblocks RFC-0055 S4 (#1360) | `verify` finds a segment whose Dune count differs from its catalogue rows; a table is listed as loaded that `verify` did not check; or the recorded credits differ from the estimate by more than a factor of two and §5 is not amended in the same change |

## §10 - Unresolved

Each item is unsupported until a recorded run answers it. None is assumed.

1. **The column types `POST /v1/uploads` accepts.** Only `timestamp` and `double` appear in its examples
   [U1], and the OpenAPI specification does not describe the endpoint [O1]. S0 answers `varchar` and
   `bigint`.
2. **Whether a leading underscore counts as a special character** in a column name [U1]. S0 answers it.
3. **Whether NDJSON may omit a nullable column** [U2]. The design does not depend on it; S0 records it
   anyway.
4. **Whether an uploaded table can gain a column.** The design never asks (§4.7).
5. **The upsert endpoint**: its path, its key semantics and its cost [C1].
6. **The read-after-write delay**: how long before an inserted row is visible to
   `POST /v1/sql/execute`. S0 measures it and sets `--settle`.
7. **The credit arithmetic**: whether a GB is 10^9 or 2^30 bytes, whether a request's charge rounds up to
   a whole credit, and whether the charge follows the request's size or `bytes_written`. The Tables
   overview still prices CSV uploads at "1,000 data points per credit" [U6], against Billing's per-GB
   rate [B1]. S0 and S4 record what is charged.
8. **What the storage cap counts**: bytes written, or bytes stored [B1]. §5 gives both readings, and S4
   records which holds.
9. **`is_private` below Enterprise.** The upload page says private uploads need Enterprise [W1], and the
   create API accepts the field on any request [U1]. S0 records what happens on the account's plan.
10. **The concurrent-insert limit per table**, described as "limited" without a figure [U2]. v1 sends one
    request at a time.
11. **The rate-limit category and cost of Execute SQL** for a grouped count [E1, R1]. S0 records the cost
    of one.

## §11 - Sources

All read 2026-09-15.

- U1 https://docs.dune.com/api-reference/tables/endpoint/uploads-create
- U2 https://docs.dune.com/api-reference/tables/endpoint/uploads-insert
- U3 https://docs.dune.com/api-reference/tables/endpoint/uploads-clear
- U4 https://docs.dune.com/api-reference/tables/endpoint/uploads-delete
- U5 https://docs.dune.com/api-reference/tables/endpoint/migration
- U6 https://docs.dune.com/api-reference/tables/endpoint/overview
- B1 https://docs.dune.com/api-reference/overview/billing
- B2 https://docs.dune.com/resources/credits-billing/how-credits-work
- R1 https://docs.dune.com/api-reference/overview/rate-limits
- A1 https://docs.dune.com/api-reference/overview/authentication
- E1 https://docs.dune.com/api-reference/executions/endpoint/execute-sql
- W1 https://docs.dune.com/web-app/upload-data
- C1 https://docs.dune.com/docs/changelog (the entries of November 2025, and of February, March and
  August 2026)
- O1 https://docs.dune.com/analytics-openapi.json (defines no `/v1/uploads` path)
