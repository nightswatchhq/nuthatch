# RFC-0029: The fastest indexer - a backfill that finishes, then one that flies

- Status: **Implemented** (v0.8.2-v0.9.0; all 5 slices)
- Author: Pete (cargopete)
- Date: 2026-07-29
- Depends on: RFC-0004 (the bench harness and the adaptive chunker), RFC-0028 (range control - §3 below
  corrects one of its consequences), RFC-0003 (ExEx tip mode - §6 depends on it).
- Blocks: any published throughput claim. The house rule (RFC-0004) says every number traces to a
  `bench-report.json`; §2b shows the harness has been measuring code no user runs.
- Origin: **OBIB**, Sentio's open blockchain indexer benchmark
  ([blog](https://www.sentio.xyz/blog/obib-open-blockchain-indexer-benchmark/index.html),
  [repo](https://github.com/sentioxyz/open-blockchain-indexer-benchmark)), raised in the GraphOps
  Discord on 2026-07-29. Every number below was measured that day, against Ethereum mainnet through an
  Alchemy endpoint, on an M-series MacBook, from commit `bc5f138`.

## §0 - What this RFC is not

It is not "make the loops faster". Nuthatch's decode path is not the bottleneck and no amount of
threading will make it one. Every measurement below says the same thing: **we are round-trip bound, and
most of our round trips buy a column the workload never asked for.**

It is also not benchmark-chasing. OBIB is a fair, reproducible workload, and pointing nuthatch at it
surfaced a defect that stops a backfill dead against the largest RPC provider in the ecosystem. Fixing
that because a leaderboard exists would be a poor reason; fixing it because it is a defect is a good one.
The leaderboard is merely how we found out.

The two halves belong in one document because they are the same discovery: **the first thing that makes
an indexer fast is finishing.**

## 1. The workload, and the part we already win

OBIB case 1: index `Transfer` from LBTC (`0x8236a870…5634494`) over blocks **0 → 22,200,000**. Write-only,
no RPC calls required by the workload, six stored fields - `id`, `from`, `to`, `value`, `blockNumber`,
`transactionHash`. **No timestamp.**

Scaffolding is worth recording because it needs no work at all:

```
nuthatch init 0x8236a87084f8B84306f72007F36F2618A5634494 --chain mainnet
  → proxy → implementation 0x072072317469ebb6c340a47e41561c9c3b782bd9
  → ABI resolved via Sourcify
  → deployed at block 19888667
✓ scaffolded nest (1 contract, 1 table) — 23 seconds, no flags, no hand-written ABI
```

LBTC is a proxy. OBIB's own Ponder fixture vendors a hand-extracted implementation ABI into the repo to
work around exactly this. Nuthatch resolved it unattended, and `nuthatch schema` produced a single table
carrying precisely the case-1 fields. That is RFC-0015's magical `init` and RFC-0028-era proxy handling
doing their jobs.

Then we ran it, and it died.

## 2. The headline: the backfill does not finish

`nuthatch bench backfill --from 0 --to 22200000 --window 50000 --concurrency 8 --seal-direct`:

> Left as it was typed, because this section records a run rather than offering a recipe - but a
> reader will copy it, so: **as of 2.2.0 that line does not run.** `bench backfill` takes no
> `--window` and rejects it with `error: unexpected argument '--window' found`. Its flags are
> `--dir --from --to --runs --rpc --out --label --seal-direct --concurrency --keep`. The getLogs
> width is `nuthatch doctor`'s to measure and the nest's to configure.

```
Error: seal-direct getLogs 20700000..=20749999 failed after 5 attempts
Caused by:
  0: getLogs 20700000..=20749999
  1: HTTP 400 Bad Request: {"jsonrpc":"2.0","id":1,"error":{"code":-32602,
     "message":"Log response size exceeded. You can make eth_getLogs requests with up to a
      10,000 block range and no limit on the response size, or you can request any block range
      with a cap of 10K logs in the response. Based on your parameters and the respo…
```

Not slow. **Aborted.** And `--window 50000` is not an exotic choice - it is what our own `dev --window`
help text recommends for a sparse contract over a long backfill ("many allow 100k+ when the result set is
small"), and what RFC-0004's sparse-range reasoning implies. A user following our documentation, on
Alchemy, on the default path, gets a dead backfill.

The same run logged the *opposite* behaviour on neighbouring windows:

```
WARN seal-direct getLogs 20750000..=20799999 failed (attempt 1/5): HTTP 400 …
     "Log response size exceeded…"; retrying in 250ms
INFO getLogs 20850000..=20899999 succeeded when split - the provider was refusing the range
     without saying so; treating it as a cap
```

Same cap, same provider, one window rescued and another fatal. §3 explains why, and the explanation is
not luck - it is a classification bug with a deterministic consequence.

## 3. Root cause: a status code nobody enumerated

RFC-0028 §3e consolidated two error classifiers into one and ruled that **structured classification wins
over text matching**. Right in principle. But `classify_status` (`rpc.rs:132`) enumerates 401, 403, 413
and 429, and falls through:

```rust
_ => FailureClass::Transient,
```

**HTTP 400 is not in that list.** Alchemy returns its oversized-range refusal as **HTTP 400** carrying a
JSON-RPC body. RFC-0028 was written against a measured Alchemy response of **HTTP 200** with a JSON-RPC
error object; the 400 shape walks straight past everything built for it. The consequences compound in
four steps:

1. `classify_status(400, …)` returns `Transient`.
2. `is_result_too_large` (`chunker.rs:80`) consults `class_of` **first** and returns `false` for
   `Transient` - so the widened marker list, which contains `"response size"` and would match this exact
   text, is **unreachable**.
3. `send_classified` (`rpc.rs:428`) routes a body to `classify_rpc_error` only on a **2xx**. On a non-2xx
   the body becomes a 300-character log string, so `suggested_range` never sees it either. Alchemy names
   the range that would have worked; RFC-0028 §3c shipped honouring exactly that hint, and we discard it
   into a truncated log line.
4. The window is treated as a transient blip and retried **at the same width**, five times, with
   250 ms → 500 ms → 1 s → 2 s backoff, then aborts the backfill.

### 3a. Why the §3b safety net could not save it

RFC-0028 §3b added a speculative split for *unclassifiable* failures, and its doc comment states the
design assumption plainly:

> The speculative split is deliberately **not** recursive: `speculative` is cleared for the halves […]
> A genuine size failure re-triggers the *classified* path on the halves anyway, which recurses properly.

That assumption is exactly what this bug breaks. The halves are misclassified too, so the classified path
never re-triggers on them. A 50,000-block window over a 10,000-log cap needs several levels of halving;
the one-level speculative split reaches 25,000, both halves fail unclassified-but-not-speculative, the
original error surfaces, and `retry_transient` grinds to its limit. The windows that logged `succeeded
when split` were simply the ones where a single halving happened to be enough.

The safety net was designed against the right failure mode and defeated by the wrong classification.

### 3b. The durable rule

RFC-0028's thesis was "a marker list is a liability". This extends it: **a status-code list is a liability
too.** The fix is not "add 400" - that is the same mistake with a different number. The rule is:

> **Structured classification wins only when it is confident.** `Transient` is the fall-through default -
> the *absence* of a classification, not a positive finding - and it must never outrank direct textual
> evidence of a cap.

## 4. Where the time goes, once it finishes

A pinned dense sub-range (100,001 blocks, 21,000,000 → 21,100,000), seal-direct, `--window 10000`, which
stays inside Alchemy's hard block-range allowance and so avoids §3 entirely:

| concurrency | wall clock | RPC requests | events/sec |
|---|---|---|---|
| 1 | 101.1 s | 76 | 221 |
| 8 | 88.1 s | 78 | 253 |
| 16 | **83.7 s** | 78 | 267 |

22,325 events in every case, peak RSS 113-135 MB. **Sixteen-way concurrency bought 1.2×.** If we were
RPC-parallelism bound, it would have bought far more.

Decomposing those 76 requests: ~11 are `eth_getLogs`. **The other ~65 are `eth_getBlockByNumber` batches
fetching block timestamps.** Measured against the same endpoint:

| call | measured |
|---|---|
| `eth_getLogs`, 2,000 blocks, empty range | 0.22 s |
| `eth_getLogs`, 2,000 blocks, dense range | 0.52 s |
| `eth_getBlockByNumber` × 200 (one `MAX_TIMESTAMP_BATCH`) | **~1.5 s** |
| 10 such batches, 10-way concurrent | 7.99 s wall (6.3×, **2 of 10 threw `IncompleteRead`**) |

LBTC averages **2.13 logs per distinct block** (11,805 logs across 5,534 blocks, sampled over four 10k
windows in the dense region). Case 1's ~294k events therefore touch roughly **138,000 distinct blocks** -
about **690 timestamp batches, ~17 minutes of `eth_getBlockByNumber` alone**, for a column case 1 does not
store.

### 4a. Why concurrency cannot rescue it

`RpcClient::block_timestamps` (`rpc.rs:585`) fans out like this:

```rust
for chunk in blocks.chunks(MAX_TIMESTAMP_BATCH) {
    out.extend(self.fetch_timestamp_batch(chunk).await?);
}
```

Serial. A window holding 1,000 distinct blocks pays 5 × 1.5 s back-to-back **inside a single window
future**. `--concurrency` fans out *windows*, overlapping the cheap `getLogs` calls while leaving the
dominant cost untouched. That is the 1.2× above, fully explained.

### 4b. The harness has been measuring a strawman

Two defects in our own instrumentation, both flattering nobody:

1. **`bench backfill` had no `--window`**, using the hardcoded per-chain constant (2,000 on mainnet).
   Over 22.2M blocks that is ~11,100 requests before any consideration of density - measuring a constant,
   not a pipeline. `dev` has taken `--window` for months. *(Added during this investigation; the window is
   recorded in the report so a number is only comparable against the same one.)*
2. **`bench.rs::hot_store_backfill` calls `Store::put_entity` per row** - one redb
   `begin_write`/`commit`/fsync **per event** - while the real tip loop uses `Store::commit_window`
   (PERF-2, added precisely because per-row commits "capped tip-follow throughput far below the decode
   rate"). Every hot-store number this harness has ever produced measures code no user runs.

A harness that libels its own product is worse than none, because it gets believed.

## 4b. Measured: case 1 completes in ~75 seconds (2026-07-30)

Everything above was written from a backfill that **did not finish**. It finishes now. Reports in
[`docs/bench/`](../bench/); method in §11.1.

| | wall clock | events | RPC requests | peak RSS | retries |
|---|---|---|---|---|---|
| median of 3 | **74.8 s** | 294,278 | 321 | 320 MB | 0 |
| cold control (range shifted 3 blocks) | 77.0 s | 294,276 | 321 | 310 MB | 0 |
| first run (2026-07-30 15:19) | 289.9 s | 294,278 | 454 | 306 MB | 15 |

**The cold control exists because the first run was 3.9× slower than the second**, and the obvious
suspect was Alchemy caching the exact `getLogs` queries we had just made - which would have made 74.8 s
a warm-cache number and any published comparison indefensible. Re-running the same workload over a
range shifted by three blocks (never fetched before) took **77.0 s**. Caching is not the explanation;
the first run was degraded by 15 transport errors and their retry backoff, which is also where its
extra 133 requests came from. The control is kept in `docs/bench/` because a number whose confound was
tested is worth more than a faster one whose was not.

Two internal consistency checks, both of which had to hold:

- Every run decoded **exactly 294,278** records - the count does not move with cache state, retries, or
  run order.
- The 3-block-shorter control found exactly **two fewer** transfers. The counts move precisely as the
  range does.

**321 requests for 22,200,001 blocks** is the range-control result, and it is the one caching cannot
flatter. Note what the harness starts from: `bench backfill` has no `--window` override, so it uses the
chain default - **20** on mainnet, deliberately small for a dense L1 tip. Before §6f the pipelined path
would have held that width for the whole range: **1,110,000 requests**. The benchmark was not slow
before this work; it was unable to run at all through the harness, and §2b's "the harness measures code
no user runs" applied here in a second way we had not noticed.

**On comparing this to anyone.** Envio HyperIndex's 6.94 min and Sentio's 11.02 min are served from
purpose-built columnar stores, not JSON-RPC - which is *why* OBIB normalises the endpoint (§5). The
defensible claim is not a placing. It is that a general-purpose-RPC indexer, on a laptop, against a
commercial endpoint anyone can rent, is in the same conversation as infrastructure built specifically
to avoid RPC. Publishing it as a head-to-head win would invite a correction we would deserve.

Still unmeasured: the `--concurrency 16` vs `--concurrency 1` ratio (§10), and any figure for a
*timestamped* nest on this workload - so the cost `--no-timestamps` avoids is quantified here only by
the RFC's earlier ~85% measurement, not by an A/B on this range.

### 4c. The A/B, and a third instance of one defect (2026-07-31)

The same nest, range, endpoint and commit, differing only in `block_timestamps`:

| | wall clock | events | RPC requests | peak RSS |
|---|---|---|---|---|
| `--no-timestamps` | **74.8 s** | 294,278 | 321 | 320 MB |
| with timestamps | **1689.1 s** | 294,278 | 2,099 | 461 MB |

**22.6x**, with byte-identical output modulo the column. §4's ~85%-of-wall-clock estimate is now too
low - 22.6x implies ~95.6% - and the reason is §6f rather than any regression: adaptive windows cut the
log half to 321 requests, so the timestamp half went from dominant to overwhelming. **A
"percentage of wall clock" is only meaningful against the wall clock it was measured on.**
**6.5x of all requests are timestamps** (2,099 vs 321).

Getting this number took three attempts, because the timestamped run **aborted twice** - and both
aborts were the same defect this RFC opened with, in new locations:

| # | Where | Trigger | Found by |
|---|---|---|---|
| 1 | `getLogs` status codes (§3) | HTTP 400 not enumerated | running OBIB at all |
| 2 | `getLogs` body read | request timeout covers body streaming | the A/B |
| 3 | timestamp batch | same, and no narrowing path at all | the A/B again |

Each time the shape was identical: **a failure that halving would fix, reissued at the same size until
the attempts ran out.** So the rule is worth stating above the three fixes, because patching a fourth
instance is not a plan:

> **A batched RPC call needs a narrowing path, not just a retry path.**

There is also an interaction §6c and §6f did not account for: `MAX_TIMESTAMP_BATCH` (200) x
`TIMESTAMP_FANOUT` (4) x `--concurrency` (8) is **up to 32 concurrent large responses**, sharing one
connection pool and one timeout budget, and nothing bounds that product. §6f sharpened it by growing
windows to 100,000 blocks. Halving on failure adapts without predicting it, which is better than
retuning constants whose right values depend on the provider, the contract's density, and the
concurrency the operator chose.

**Only `--no-timestamps` ships a headline number.** The timestamped path completes but is 22.6x
slower; that is the honest statement, and it is what makes §6b a *new-nest* decision rather than a
tuning flag.

## 5. The principle

**You cannot out-run RPC with RPC.**

Envio HyperIndex finishes case 1 in 6.94 minutes and HyperSync reads 100,000 blocks in 3.19 s because
neither speaks JSON-RPC to a general-purpose node - both query a purpose-built columnar store. Sentio's
11.02 min is likewise served from its own infrastructure. OBIB normalises the RPC endpoint precisely
because it is the dominant variable.

For nuthatch this cuts two ways, and honesty about both matters more than a good placing:

- **Where we are RPC-bound, the ceiling is the provider's**, and the only wins are *fewer and better round
  trips*. That is §6.
- **The way to stop being RPC-bound is to stop using RPC** - reth ExEx (RFC-0003), where extraction is a
  local function call and the timestamp arrives in the header we already hold. That is §7, and it is the
  only path on which "fastest" is achievable rather than merely respectable.

A self-hosted-first indexer whose fast path is *colocation with your own node* is not a consolation prize.
It is the thesis: no mandatory third-party data dependency, ever.

## 6. What to change (RPC path)

**(a) Fix the classification regression** (§3). Route non-2xx bodies through `classify_rpc_error`; make
`Transient` defer to positive textual evidence; add 400 to `classify_status` as belt to that braces.
Regression tests carry the **measured** Alchemy 400 body, per RFC-0028's grounding convention. Reconsider
whether the speculative split should be allowed one *recursive* level when the halves fail identically -
the current single level is defeated by any window more than 2× over cap.

**(b) Stop acquiring data nothing consumes.** Block timestamps become demand-driven: a nest whose tables,
views and semantic layer never reference `block_timestamp` should not pay ~85% of its wall clock for it.
This is determinism-visible - `block_timestamp` is a sealed column and the segment's content hash depends
on it - so it needs an explicit per-nest declaration and a schema version bump, **not** a quiet flag.
Deriving or interpolating timestamps is **rejected**: exactness is not negotiable in the core.

**(c) Parallelise the timestamp fan-out.** `block_timestamps` should issue its chunks concurrently under
the same budget the window fan-out uses, bounded well below where the connection starts throwing
`IncompleteRead` - 10-way already produced 2 failures in 10 on the measured endpoint, so the cap should be
adaptive rather than a constant we guess.

**(d) Deduplicate timestamps across windows.** Batching is per window today. A global LRU keyed by block
number costs little and removes re-fetching entirely on retry and on split-and-retry - where we currently
re-fetch every timestamp in the range we just split.

**(e) Make the harness honest** (§4b): `hot_store_backfill` uses `commit_window`; `--window` recorded in
every report (done); the report gains `provider` and `hardware` fields so the house rule is mechanically
enforceable rather than a documentation promise.

**(f) Adaptive windows on the pipelined path.** `backfill_direct_pipelined` builds its window list up
front at a fixed width, so the `AdaptiveWindow` controller that `backfill_direct` uses is bypassed on the
*concurrent* path - our fast path is the one without adaptation. Case 1's empty 0 → 19.89M prefix is the
pathological case: at `--window 10000` it costs ~1,989 requests returning nothing, where a controller
growing 4× on empty responses reaches the 100,000 ceiling in a handful of steps. Note this interacts with
RFC-0028 §4: seal boundaries are now data-determined, so varying the window no longer varies segment
identity - which is what makes adaptation safe here.

### 6b-i. Amendment: turning timestamps off is a **breaking** schema change

Found while mapping slice 4, and it changes the design rather than merely qualifying it.

`block_timestamp` is one of the seven implicit columns every table carries (`registry.rs`). A nest that
declares no use of it drops that column from **every table it produces**. RFC-0020's classifier calls a
dropped column `ColumnRemoved`, which is **breaking** - correctly, since a consumer selecting it gets an
error rather than a null.

So "turn timestamps off to go 6× faster" is not a tuning flag. On an existing nest it is a breaking
version: a new endpoint served alongside the old under RFC-0020 slice 3, with the old one kept alive for
its consumers. That is a much larger ask than the §6b framing implies, and it interacts with the fleet-
wide pinning in RFC-0022 §4 - both endpoints are separately placed and separately resolved.

Three consequences for the build:

1. **The declaration belongs at `init`, and changing it later must route through the ordinary breaking-
   update path** rather than being a config edit that silently invalidates every consumer's query. The
   classifier already refuses to be fooled here, which is a point in favour of the existing machinery.
2. **Segment identity changes too**, which is the sharper half. Sealed segments are content-addressed
   over their bytes, and the bytes contain the column. A timestamp-free nest cannot reuse a timestamped
   nest's segments (RFC-0020 slice 4) even over an identical range - so the switch costs a full
   re-index, on top of being breaking. Worth stating plainly, because "faster backfill" and "re-index
   everything once" cancel out for anyone doing it to an existing nest.
3. **The win is therefore mostly for new nests**, where it is chosen at `init` and no consumer or
   segment exists yet. That is still a large win - §4 measures ~85% of wall clock - but it is a
   different, narrower claim than "backfills get 6× faster", and the RFC should not be read as offering
   the latter to existing deployments.

None of this argues against building it. It argues for building it as an `init`-time declaration with an
explicit breaking-change path, rather than as the flag the §6b wording could be read to permit.

## 7. What to change (the path off RPC)

RFC-0003 (reth ExEx) is currently "Accepted - groundwork landed; ExEx mode deferred". §5 argues it is the
only route to a genuinely leading number, and it dissolves most of §6: in-process extraction has no round
trips to economise, and `block_timestamp` arrives with the logs at zero marginal cost - **(b), (c) and (d)
simply evaporate**.

This RFC does not schedule that work. It records that the §6 slices are *ceiling-raising* and ExEx is
*ceiling-removing*, so the two should not be confused when prioritising, and §6 should not be gold-plated
in place of it.

## 8. Non-goals

- Rewriting the decode path. It is not the bottleneck and no measurement suggests it is.
- A columnar side-store to mimic HyperSync. That is a data service; `CLAUDE.md` puts it out of scope.
- Beating Envio on someone else's infrastructure. We compete on *your* infrastructure - that is the point.
- Vendor-specific fast paths. Provider-shaped behaviour belongs in the classifier, not the pipeline.
- Loosening determinism for speed. §6(b) is a declared, versioned schema change or it does not happen.

## 9. Slices

1. **Classifier fix** (§6a) - smallest, most urgent, and the only one that changes *whether* a backfill
   completes. Affects every nest on Alchemy today.
2. **Honest harness** (§6e) - must precede further optimisation, or we cannot tell what worked.
3. **Timestamp fan-out + cross-window dedup** (§6c, §6d) - pure win, no schema implications.
4. **Demand-driven timestamps** (§6b) - the big one; schema version bump, determinism review.
   **Built (0.9.0).** `[nest] block_timestamps`, declared by `init --no-timestamps` and refused as an
   in-place edit, exactly as §6b-i argued: the column is *omitted* rather than nulled (open question 3,
   answered - a null keeps the schema stable but makes `ORDER BY block_timestamp` silently arbitrary,
   and a wrong answer beats an error only in the short term), and non-use is an **explicit
   declaration** rather than static analysis of views (open question 2, answered - the leaning was
   right, and the analysis would have had to be sound over authored SQL to be worth trusting).
   Two things the RFC did not anticipate: a timestamp-free nest is stamped `schema_version = 2` so an
   *older* binary refuses it instead of indexing timestamps into a store built without them - the
   runtime guard protects this build, the version protects against the previous one; and the factory
   provenance view had a hardcoded `block_timestamp` projection whose failure was swallowed as a
   `debug!`, so a timestamp-free factory nest would have silently lost `{template}__children`.
5. **Adaptive windows on the pipelined path** (§6f). **Built (0.9.0).** The window list is generated
   lazily from an `AdaptiveWindow` rather than materialised up front, so the concurrent path adapts as
   the sequential one always has. The controller is fed **raw log count**, not decoded rows: it is
   sizing a *response*, and a log matching no decoder still costs bytes and still counts against the
   provider's cap - feeding it decoded rows would make a nest with a narrow event allowlist read dense
   history as empty and grow toward the ceiling, which is the one direction that actually hurts.
   Feedback lags by up to `concurrency` windows, which is left alone deliberately: the controller is
   damped to 4× a step, so the lag costs a few steps of convergence, and removing it means waiting for
   feedback before generating the next window - which is the sequential path.

## 10. Acceptance

- A mock returning **HTTP 400** with Alchemy's measured `Log response size exceeded` body triggers a split
  on the **first** attempt, consuming **zero** `retry_transient` attempts. **Fails today.**
- OBIB case 1 completes at `--window 50000` against Alchemy. **Fails today** (§2).
- A provider-suggested range delivered on a non-2xx is honoured. **Fails today.**
- The hot-store bench path issues one redb commit per window, not per row. The resulting events/sec jump
  is recorded as a **harness correction, never as a product gain**.
- On the pinned dense 100k range, `--concurrency 16` beats `--concurrency 1` by **>3×** (today: 1.2×).
  **Not yet measured** - this needs a real endpoint and a pinned range, not a mock. The unit tests
  assert the *mechanism* (a long empty prefix costs a small fraction of the fixed-window request
  count; dense history does not push the window toward the ceiling); they say nothing about the
  speedup ratio, and no ratio is published until it is run.
- A nest declaring no use of `block_timestamp` completes case 1 issuing **zero** `eth_getBlockByNumber`
  calls, with rows otherwise byte-identical to a timestamped run modulo that column. **Met** -
  `a_timestamp_free_nest_backfills_without_a_single_timestamp_call` asserts both halves against the
  sealed Parquet, because either alone is satisfiable by a broken implementation (zero calls by
  failing to backfill; identical rows by fetching timestamps and discarding them).
- No regression on the RFC-0004 W1/W2/W3 workloads.

## 11. Open questions

1. **ANSWERED (2026-07-30): 294,278.** We ran case 1 to completion and decoded **exactly 294,278**
   `Transfer` records - the top-level README's figure, to the record, with no tuning toward it. The
   case-1 README's 296,734/296,278 are the ones that do not reconcile. Method, so the claim is
   checkable: LBTC `0x8236a870…5634494`, blocks 0-22,200,000 inclusive, `events = ["Transfer"]`
   (topic0-filtered at the RPC), Alchemy mainnet with **no failover endpoints configured**, seal-direct,
   `--concurrency 8`, commit `707e1af`. Worth raising with them politely - a 2,456-record spread in
   their own repo is the kind of thing an implementer trips over rather than a rounding difference.

   The original text, kept because the question was the right one to ask:

   ~~**Which count is right?** OBIB's top-level README publishes **294,278** expected records for case 1;~~
   the case-1 README's own table shows **296,734** for five platforms and 296,278 for a sixth. A
   2,456-record spread their repo does not reconcile. We should establish our own figure, state our
   method, and raise it with them politely - rather than quietly matching whichever number flatters us.
2. **ANSWERED: explicit declaration** (slice 4, 0.9.0). The lean was right. Static analysis would have
   had to be *sound* over authored SQL to be worth trusting, and an unsound analysis that silently
   drops a column is worse than a flag someone has to type.
3. **ANSWERED: omit** (slice 4, 0.9.0). The nulls option was tempting for schema stability, and its own
   downside decided it: a silently-wrong `ORDER BY block_timestamp` beats an error only in the short
   term.
4. **What is the right concurrency cap** when the endpoint fails at 10-way? Adaptive and per-endpoint,
   learned at runtime - or configured and documented?
5. **Should the speculative split recurse?** §6(a) proposes one extra level. Unbounded recursion on a dead
   endpoint is the failure RFC-0028 was avoiding; the right bound may be "recurse while both halves fail
   *identically*", which distinguishes a cap from an outage.
