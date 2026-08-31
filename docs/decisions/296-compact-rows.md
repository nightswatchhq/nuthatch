# #296: compact binary rows, measured

**This document contains no decision.** #296 is a storage-format change, and the sprint brief adds
the constraint that matters: *"Do not trade away the no-resync promise by implication."* That is a
product commitment, so this measures the cost, prices the options, and stops.

Measured 2026-08-31 against the **live Lodestar deployment**, not a fixture.

## The hot store is most of the budget - in RSS, which is what the budget is about

**Corrected after review.** The first version of this document quoted `nuthatch_hot_store_bytes` and
called it budget usage. That gauge is the **redb file on disk**, and non-negotiable 2 is **resident
memory per cursor**. Equating them was the error this sprint keeps finding, made in the headline
argument of a decision document. Both are now measured.

Read live, 2026-08-31 - `nuthatch_hot_store_bytes` beside the process's `VmRSS`:

| unit | nest | hot-store file | **RSS** | RSS/file |
| --- | --- | ---: | ---: | ---: |
| `nuthatch` | graph-staking-nest | 1,084,432,384 B (1.08 GB) | **1,447,247,872 B (1.45 GB)** | 1.33 |
| `nuthatch-gns` | graph-gns-nest | 1,140,813,824 B (1.14 GB) | **1,415,196,672 B (1.42 GB)** | 1.24 |
| `horizon-nest` | horizon | 334,966,784 B (335 MB) | 439,259,136 B (439 MB) | 1.31 |
| `nuthatch-dips` | dips-nest | 48,607,232 B (49 MB) | 72,073,216 B (72 MB) | 1.48 |

**RSS is consistently *above* the file, by 1.24-1.48x.** So the file was not merely an imperfect
proxy - it *understated* the thing the budget bounds, and the corrected figures are worse than the
claim they replace: two cursors at **72% and 71% of their 2 GB**, not "over half".

Each of these is a separate process and therefore its own cursor today. Mounting a second nest beside
either of the two large ones does not fit.

### What that implies for the saving - a hypothesis, not a forecast

**Corrected twice, and the second correction is the more important one.** An earlier version of this
section fitted `RSS ≈ 7 MB + 1.33 x hot-store file` across the four points and used it to predict
roughly 600 MB after a 59% payload cut. That fit does not support a forecast, for a reason worth
writing down rather than softening:

**`VmRSS` is the whole process.** It includes the allocator, ingestion and RPC state, HTTP serving,
DuckDB's own working set, and every buffer in flight. The four samples have no baseline and no
controlled workload, so they do not isolate what redb contributes, and nothing here measures how RSS
moves *after* a re-encode. Reducing payload bytes plausibly reduces resident pages; **by how much is
unmeasured.**

So the honest form is:

- **Measured:** two cursors sit at 1.45 GB and 1.42 GB RSS against a 2 GB budget - 72% and 71%. That
  is a fact about today and it does not depend on any model.
- **Measured:** row payloads are 2.45-2.49x larger as JSON than as a schema-driven binary encoding,
  on two independent table shapes.
- **Hypothesis, unvalidated:** shrinking payloads by ~59% would bring those cursors materially back
  under budget. **A prototype encoder measured against a real store is the only thing that settles
  it**, and nothing in this document should be read as having done so.

The size argument is settled. The RSS argument is not, and this project has published enough
asserted numbers this month.

## The prototype was built, and it settles it the other way

**Measured 2026-08-31, `tests/bench_compact_rows.rs`.** Two redb stores, 1,600,000 identical
synthetic rows each - one holding today's JSON strings, one holding the compact encoding modelled
above - written in 5,000-row windows as `commit_window` writes them. Each configuration is then read
back **in its own process**, because `VmRSS` is process-wide and that is exactly the correction
review made to the section above. Full scan to fill the cache, then 50,000 random point reads that
actually decode the row.

First, the file. The 2.45x is a *payload* ratio and the file does not inherit it:

| | file | per row | ratio |
| --- | ---: | ---: | ---: |
| JSON | 2.16 GB | 1,348 B | |
| compact | 1.08 GB | 674 B | **2.00x** |

redb's per-row overhead is unchanged by the encoding, so a 3.1x payload cut is a 2.00x file cut.

Then the thing the whole issue rests on. **RSS does not track the file. It tracks redb's cache
setting, which nuthatch has never set:**

**macOS** (M-series, MacBook):

| cache | JSON (2.16 GB file) | compact (1.08 GB file) | JSON point-read | compact point-read |
| --- | ---: | ---: | ---: | ---: |
| 1 GiB (today's default) | 1.28 GB | 0.89 GB | 3.0 us | 1.1 us |
| 512 MiB | **0.64 GB** | **0.64 GB** | 3.4 us | 1.5 us |
| 256 MiB | **0.33 GB** | **0.33 GB** | 3.8 us | 2.0 us |
| 128 MiB | **0.17 GB** | **0.17 GB** | 3.7 us | 2.2 us |

**Linux** (Debian 13, 6.12.94, 32 core - the platform production runs), same commit, byte-identical
stores:

| cache | JSON (2.16 GB file) | compact (1.08 GB file) | JSON point-read | compact point-read |
| --- | ---: | ---: | ---: | ---: |
| 1 GiB (today's default) | 1.00 GB | 0.69 GB | 4.2 us | 1.5 us |
| 512 MiB | **0.50 GB** | **0.50 GB** | 4.9 us | 2.0 us |
| 256 MiB | **0.25 GB** | **0.25 GB** | 4.9 us | 2.6 us |
| 128 MiB | **0.13 GB** | **0.13 GB** | 5.0 us | 2.9 us |

**At every cache size smaller than both files, on both platforms, the two encodings land on
identical RSS** - 0.50 against 0.50, 0.25 against 0.25, 0.13 against 0.13 on Linux - across a 2.00x
difference in file size. The encoding is not what is buying the memory. The cache size is, and it is
the same lever in both columns.

Linux is the cleaner of the two: RSS tracks the cache setting almost one-for-one, where macOS's
allocator adds a roughly 1.25x overhead on top. The conclusion does not depend on which, but the
Linux figures are the ones to reason about, because that is where the nests run. Its point-reads are
slower in absolute terms (4.2 us against 3.0) on a different CPU under load average 20-30; the
*ratios* are what transfer, and they agree to within 0.1x.

`Builder::new()` calls `set_cache_size(1 GiB)` (split 90% read / 10% write) and `store.rs` never
overrides it at any of its three open sites. redb 2.6.3 does not mmap - `file_backend/unix.rs`
preads into `Vec<u8>` - so every cached page is heap and counts in RSS. That 1 GiB is a real
per-process heap ceiling that nobody chose.

It also explains the production table above better than the fit that was withdrawn from it. Both
large nests sit just over the ceiling and read 1.44 and 1.42 GB, and the **larger** store (1.14 GB)
has the **smaller** RSS - which `RSS = k x file` cannot produce and a ceiling can.

### So the recommendation changes

The table below gains a fifth row that was missing because nobody knew the cache was a default:

| option | cost | RSS saving, measured |
| --- | --- | --- |
| **Set redb's cache size** | one argument at three call sites | 1.00 -> 0.25 GB on Linux; **~750 MB/cursor** |
| Compact encoding | a storage format, a migration, and part of RFC-0020 | **nothing, once the cache is set** |

The memory case for #296 is answered, and the answer is no: the saving it was justified by is
already available, is larger, costs no format change, spends no part of the no-resync promise, and
is one line. Per the sprint's own constraint - *do not trade away the no-resync promise by
implication* - there is now nothing to trade it for.

**What the encoding does own, and it is real:** point-read decode is **2.7x faster on macOS and 2.8x
on Linux** (4.2 -> 1.5 us at 1 GiB, 4.9 -> 2.6 us at 256 MiB - a consistent 1.7-2.7 us saved per
read). It is the one expectation the prototype confirmed. That is a latency result,
not a memory one. #296 asked for memory. If the decode win is worth wanting it should be argued on
its own terms, against the cost of a migration, and it is a much weaker case than the one this
document set out to make.

Note the second column of that table for the cache option: on Linux a smaller cache costs **0.7 us**
per point read (4.2 -> 4.9 us) for **750 MB**. And because the budget is per *cursor*, an unset cache
means N cursors reserve N GiB - so this is worth more to multi-nest density than the encoding ever
was.

### What this measurement does not establish

- **The latency figures are optimistic about disk.** Both boxes had the store in the OS page cache
  (the Linux box has 62 GB of RAM and 38 GB of it in buff/cache),
  so a redb cache miss cost a memcpy, not a read. Under real memory pressure a 256 MiB cache would
  fault to disk and the 3.8 us would be worse. Pulling the cache lever needs a production
  measurement before a value is picked, and 256 MiB should not be assumed to be it.
- **The point-read workload is uniformly random over 1.6M rows**, which is worse locality than a
  nest serving a finality-bounded tail. That pushes the same numbers the other way. The two effects
  are not netted here because neither is measured.
- **The rows are synthetic** - an ERC-20 transfer shape, 1,348 B/row as JSON against production's
  651 B/row payload. The *ratios* are what this establishes; the absolute per-row bytes are not
  production's.

## What the format costs, on real rows

Rows are stored as JSON strings: `ENTITIES: TableDefinition<&str, &str>` in `store.rs`, written by
`DecodedRow::to_json().to_string()`.

Sampled from two differently-shaped production tables and modelled against a schema-driven binary
encoding - field names dropped (the schema has them), hashes as 32 raw bytes rather than 66-char hex,
addresses as 20 rather than 42, `uint256` as the 32-byte word rather than a decimal string, block
numbers and timestamps as varints, and `_seq` **not stored at all** because it is derived from
`(block << 20) | log_index`:

| table | rows | JSON | compact | ratio | saving |
| --- | ---: | ---: | ---: | ---: | ---: |
| `staking_legacy__stake_delegated` | 2,000 | 651 B/row | 266 B/row | **2.45x** | 59% |
| `staking__tokens_delegated` | 345 | 713 B/row | 287 B/row | **2.49x** | 60% |

Two independent shapes agreeing to within 0.04x. **Field names alone are 30% of the stored bytes in
both** - the schema is re-transmitted once per row.

Applied to the measured hot stores, 1.08 GB would become roughly **440 MB**.

### One saving that is not the format's, and must not be counted as it

Both tables carry `shares`/`shares_dec` and `tokens`/`tokens_dec` holding **identical strings**.
Dropping the duplicate takes `staking__tokens_delegated` from 713 to 223 B/row - a 3.20x ratio - but
that is schema redundancy rather than encoding. It belongs to its own issue and is excluded from
every figure above.

## What is not measured

*(Written before the prototype. All three are now measured - see "The prototype was built" above -
and kept here because what they were expected to show is part of the record.)*

- ~~**Decode cost on point-read.** #296 names it; this does not measure it.~~ **Measured: 2.7x
  faster.** It was the one expectation that held, and it is the only benefit left standing.
- ~~**redb's own overhead, exactly.**~~ **Measured: it does not shrink with the payload.** A 3.1x
  payload cut is a 2.00x file cut, and a 0x RSS cut once the cache is set.
- ~~**Any prototype.** No encoder exists.~~ One exists now, in `tests/bench_compact_rows.rs`, and it
  falsified the hypothesis it was built to confirm.

## The contracts on the table

RFC-0020's promise is that a version upgrade is a binary swap - proven in production, 0.3.0 to 0.6.0
with no data migration. That promise is what this change spends.

| option | cost | what it forecloses |
| --- | --- | --- |
| **Versioned read path** - write v2, read v1 and v2, convert lazily | most work: two decoders live in the tree indefinitely, and every reader - hot path, reorg rollback, `from_stored`, the entity circuit - handles both | nothing. The no-resync promise holds intact |
| **Rebuild on upgrade** - refuse a v1 store, tell the operator to re-index | least work: one guard, one message | **the no-resync promise, explicitly.** `horizon-nest` backfills from block 95,000,000: hours to days of RPC, and real money at the rates in `docs/bench/750-rpc-cost-2026-08-31.md` |
| **Rebuild the hot store only** - sealed Parquet untouched; drop and re-derive the unsealed tail from `sealed_through` | small: the hot store holds only rows past the sealed watermark, which is finality-bounded rather than history-bounded | little. It is a re-index of the *tail*, not of history |
| **Do nothing** | free | leaves two nests at half their cursor budget in hot storage, and makes multi-nest density worse than it needs to be |

**Recommendation withdrawn.** It was the third option, *conditional on the RSS hypothesis above being
measured first, since the case for spending any part of the promise rests on it*. The prototype
measured it and the hypothesis is false: the encoding buys no RSS once redb's cache size is set. The
condition was not met, so the recommendation does not stand, and the reasoning below is kept only
as the record of what it rested on.

**Recommendation, in its place: the fourth - do nothing to the format - and set redb's cache size
instead**, which is not in this table because it spends none of the promise and therefore is not an
option *about* the promise at all. The rest of this section is the withdrawn argument.
The no-resync commitment is about *history* - the part that costs hours and money - and the hot store
is by construction the finality-bounded tail. Re-deriving it is a bounded, minutes-scale operation
over data the nest has already sealed, and `sealed_through` marks exactly where to resume.

That is still a **narrower promise than "a binary swap changes nothing"**, so it is a promise being
changed and belongs in release notes rather than being discovered. Not free - only much cheaper than
a full resync.

**Before implementing, whichever is chosen:** measure point-read decode cost against a prototype
encoder. The size argument is settled; the latency one is asserted, and this project has published
enough asserted numbers this month.
