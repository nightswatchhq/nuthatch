# RFC-0041 slice-zero measurements

Status: **the decision was taken. GO, 2026-08-24, on #818**, and CLAUDE.md carries the resulting
carve-out from the 2026 feature freeze. This document is the evidence that decision rests on, not a
record of a decision still pending - it said the latter for a day after the former was true (#839),
which is why the header now says so first.

Read the caveats. Two of the measurements below have open defects filed against them, #835 and #837,
and neither is resolved by the GO.

## Captured Horizon corpus

The first real slice-zero input is a read-only snapshot of the sealed Horizon nest on the Lodestar
VPS, copied on 2026-08-24 to a local measurement directory outside Git. The copied manifest is:

```text
sha256 cabadf2f5e6d061e702afabba910dc1b7bcae7d45192792347327da1eb022303
```

All 9,476 manifest-listed Parquet segments verified against their declared SHA-256 hashes. The
snapshot is 371 MB locally. Its relation is the current delegated position per `(indexer,
delegator)`, restricted to indexers present in Horizon allocation history. It is built from the
actual sealed `staking__tokens_delegated`, `staking__delegated_tokens_withdrawn` and
`service__allocation_*` inputs, with exact decimal/integer arithmetic.

There is schema drift worth stating plainly: this VPS corpus predates the current repository views,
which name `extension__stake_delegated`. The measurement lowerer must normalise the captured
`staking__…` events explicitly. It must not silently query the newer view definitions against an
older corpus and mistake an absent table for an empty relation.

The manifest-bound normalisation and parity run is an ignored test because the 371 MB fixture is not
committed. Run it with:

```text
NUTHATCH_HORIZON_FIXTURE=/path/to/segments \
  cargo test --lib authored_entity_spike::tests::captured_horizon_relation_matches_embedded_dbsp \
  --locked -- --ignored
```

The 2026-08-24 capture produced 876 eligible delegation positions across 48 indexers. DuckDB and
the embedded DBSP circuit matched byte-for-byte after canonical key ordering.

The release-measurement command writes a JSON artefact and requires no RPC, compiler, network fetch
or running Nuthatch service:

```text
nuthatch bench authored-entity --segments /path/to/segments --max-rows 1000 \
  --out docs/bench/rfc-0041-horizon-linux.json
```

Run this using the CI-built Linux release artefact. The Lodestar VPS intentionally has no Rust
toolchain, so it executes the artefact but does not build it.

## Linux cursor budget

Measured on 2026-08-24 on the ThinkPad staging host: Linux x86_64, Rust 1.95.0, release build
from `f7e056065c71dd78645ed8c112a19d105e8e2baf` plus the uncommitted slice-zero spike. The source
and manifest-verified fixture were staged under `/tmp`; neither the working checkout nor the
Lodestar VPS was modified. The host compiler defaults to C23, so the build used
`CFLAGS=-std=gnu17` for the pinned `mimalloc-rust-sys 1.7.2` dependency, which still uses the
removed `ATOMIC_VAR_INIT` macro.

**Re-measured 2026-08-26 with the corrected instrument (#837, #881).** The figures first published
here measured a window that included work they claimed not to include, and are kept below the new
ones because the size of the correction is the point.

**Not a like-for-like rerun of the same bytes.** The capture was re-copied from the live Horizon nest
on the day, so it carries **889 input rows where the original run saw 876** - the nest has gone on
indexing. Both the instrument and the corpus therefore differ between the two tables. Thirteen extra
rows do not explain a 59x change in bytes-per-row, and the corrected figure's arithmetic is shown
below so it can be checked rather than trusted, but the two rows of the comparison table are not the
same experiment run twice and should not be read as one.

| measure | value |
| --- | ---: |
| declared maximum rows | 1,000,000 |
| accepted input rows | 889 |
| result rows | 889 |
| setup (compile + circuit build) | 9 ms |
| apply | 2 ms |
| empty-circuit RSS | 77,452 KB |
| circuit peak RSS | 80,164 KB |
| **normalise scan peak RSS** | **247,556 KB** |
| **RSS per input row** | **3,123 bytes** |
| input rows/sec (apply window) | 444,500 |

`(80,164 - 77,452) x 1024 / 889 = 3,123`. The scan's peak is now its own field, and it is **3.1x the
circuit's own peak** - which is what the original figure was charging to the entity.

### What was published before, and why it was wrong

| measure | published | corrected |
| --- | ---: | ---: |
| RSS per input row | 183,885 bytes | **3,123 bytes** |
| input rows/sec | 15,927 | **444,500** |

The old per-row figure was `peak - fixed` where one sampler spanned the DuckDB normalise scan and the
baseline was taken after that scan released, so it was to a first approximation the scan's transient
divided by the entity's row count. The old rate divided by setup-plus-apply, and setup - a fresh
in-memory DuckDB opened to parse one constant string - was most of it.

**The difference is not academic.** At 183,885 bytes/row the 2 GB per-cursor budget holds about
11,700 entity rows and RFC-0041 should have parked. At 3,123 it holds about 688,000.
`runtime::ENTITY_RSS_BYTES_PER_ROW` is set from this measurement, rounded up to 3,200.

Both runs are single cold-process measurements on one machine, not a release gate. §7 criterion 12's
artifact is a *cursor* measurement and is separate from this.

## Recorded entity-input replay

The sealed Parquet capture is normalised once into a content-addressed sequence of weighted entity
input batches. This is deliberately not an RFC-0039 RPC tape: inventing `eth_getLogs` responses for
a sealed Horizon corpus would make the benchmark look end-to-end while exercising no real source.
The tape starts at the actual slice-zero ingestion boundary, `Spike::apply`, and a replay performs
no DuckDB scan, fixture access, RPC call or network operation.

Record and immediately measure a tape:

```text
nuthatch bench authored-entity --segments /path/to/segments --record /path/to/tape \
  --batch-rows 256 --max-rows 1000 --out replay.json
```

Replay it later without the fixture:

```text
nuthatch bench authored-entity --replay /path/to/tape --max-rows 1000 --out replay.json
```

On the ThinkPad Linux x86_64 release build, the manifest-verified Horizon capture produced a tape
with SHA-256 `00edced52ed7b676eff86c65cf043169c9285e9cc158ae20090599893925aa09`: one indexer-dimension
batch plus four 256-row delegation batches. Five standalone tape-only processes all reproduced the
876 expected entity rows. Their median was 55 ms, 41,888 KB fixed RSS and 44,028 KB peak RSS.

The 15,927 rows/sec this paragraph used to quote came from dividing by a window that included process
start, `compile`, and circuit construction (#837). On the corrected instrument the same boundary
measures 444,500 rows/sec over the apply window, with setup reported separately - and neither number
should be compared against the >=10K events/sec ingest floor in either direction, for the reason the
paragraph below already gives.

Those are circuit-ingestion figures, not a claim about Nuthatch's existing RPC/decode/store
throughput. The product lifecycle does not yet feed authored entities from `indexer.rs`; that is
slice #821. The tape is evidence that the candidate maintained-state boundary is fast and
deterministic, not evidence that the lifecycle work is already done.

## Release binary delta

The supported-architecture comparison was measured on 2026-08-24 on the ThinkPad staging host:
Linux x86_64, Rust 1.95.0 (`59807616e`), `f7e056065c71dd78645ed8c112a19d105e8e2baf` against the
current uncommitted slice-zero spike. Both builds used separate clean target directories and:

```text
CFLAGS=-std=gnu17 cargo build --release --locked
```

The C17 setting is required only because the host compiler defaults to C23 and the pinned
`mimalloc-rust-sys 1.7.2` source uses `ATOMIC_VAR_INIT`. It is not an application build option.

| build | bytes | SHA-256 |
| --- | ---: | --- |
| base | 97,369,248 | `f794e12587473b4dc1ad2d6c7fdce92b57a898860a8fb2c785779d15d7c4c817` |
| slice-zero spike | 99,228,128 | `db05d15be2d54bc7aabff193fc79c262235ad0f95e2b9a950ed2b07daaeec442` |
| delta | 1,858,880 | |

This Linux result is the one relevant to release. It is materially larger than the earlier local
macOS arm64 delta below, so the Linux figure supersedes that figure for the 3.0 decision. The spike
still adds no dependency, external executable, generated code, JVM, compiler service, network
fetch, Cargo invocation at circuit load, or installed toolchain requirement.

Measured on 2026-08-24 from `f7e056065c71dd78645ed8c112a19d105e8e2baf` plus the uncommitted
slice-zero spike, on macOS arm64 with Rust 1.95.0 (`59807616e`). Both builds used:

```text
cargo build --release --locked
```

Each build had a separate clean target directory. The base was a detached worktree at the commit
above. The spike adds no dependency, external executable, generated code, JVM, compiler service,
network fetch, Cargo invocation at circuit load, or installed toolchain requirement.

| build | bytes |
| --- | ---: |
| base | 81,160,320 |
| slice-zero spike | 81,178,000 |
| delta | 17,680 |

This is a local development binary, not the published Linux artefact. It establishes that adding
the AST-gated circuit does not accidentally pull in a second runtime. The release gate must repeat
the comparison on the supported Linux build before publication.

## What the gate asked for, and where each part now lives

The original list had five items. Three were the entry gate and were run; two were never entry
criteria in practice and have been reassigned to the slices that own them, which is the ruling #839
asked for rather than a quiet edit.

**The entry gate (run, with the caveats named):**

1. Select the Lodestar delegation relation by captured raw-history scan cost and save its finalized
   corpus plus its tape content address. Done - the corpus and manifest are recorded above.
2. Run DuckDB parity over that corpus and the embedded circuit, with canonical key ordering. Run,
   **and #835 is open against it**: the captured corpus pre-aggregates, so the circuit's join, filter
   and aggregate are inert on it and the parity cannot fail. It gets sharper now that the circuit is
   derived from the plan (#870) rather than hand-built, and it needs re-pointing at raw weighted
   deltas before it proves what its name claims.
3. At the declared `max_rows`, record empty-circuit RSS, peak cursor RSS and the approximate per-row
   cost. Run, **and #837 is open against it**: the published RSS-per-row and rows/sec figures measure
   the wrong window. #838 is open against the bound itself, which counted one of two input relations.

**Reassigned:**

4. Sustained throughput through the **normal ingest path** belongs to #821, not to the entry gate.
   It cannot be measured until an authored entity is actually fed from `indexer.rs`, which is what
   #864 is building; measuring a circuit fed from a fixture would answer a different question. The
   GO comment says as much in its own words: the figure it published is "deliberately a
   circuit-ingestion measurement, not a claim that `indexer.rs` already maintains authored entities".
5. Repeating the binary measurement on the **Linux release artefact** belongs to the release gate.
   The figure above is a local development binary and says so; the release comparison is a
   publication precondition rather than an implementation one.

Neither reassignment makes the measurements optional. They are owed by the slices named, and #821 is
not complete without 4.

## Per-cursor footprint with authored entities (§7 criterion 12)

Measured 2026-08-26 on a 32-core Debian box, release build, commit `b627eb3`. The scenario is the one
the per-cursor budget is stated in terms of and the one CI's `per-cursor RAM budget` job enforces:
**20 nests on ONE cursor**, the 10-event ABI, 200 blocks of live tip-following after a 1,000-block
backfill, 240,200 rows.

Run twice. A single figure would say the cursor fitted; the pair says what the entities cost.

| | peak RSS | at tip | margin under 2,048 MB |
| --- | ---: | ---: | ---: |
| control, no entities | 138 MB | 137 MB | 1,910 MB |
| **one entity per nest** | **209 MB** | 205 MB | **1,839 MB** |

**71 MB for twenty entity circuits - 3.5 MB each.** Criterion 12 is met with 90% of the budget unused.

The entities were confirmed live mid-run rather than assumed: `/n1/ready` reported `"rows": 790`,
`"current": true`, `"seconds_since_progress": 1`. Twenty declarations that were never fed would have
produced an RSS delta meaning nothing.

### What the measurement changed

`runtime::ENTITY_CIRCUIT_RSS_MB` was `NEST_VIEW_RSS_MB` (40) - the built-in views' allowance, reused
because an entity is a DBSP circuit on a thread exactly as they are. It is **11x the measured cost**.
At 40 MB, twenty entities consume 43% of a cursor's 2 GB before a single row and thirty-two consume
70%; the first run of this measurement was **refused at admission** for that reason, at a declaration
the hardware went on to handle with 1.8 GB to spare.

It is now 8 MB - the measured 3.5 with a deliberate margin, because this is one run on one machine and
an admission figure set too low admits a mount that then breaches the budget at runtime.
