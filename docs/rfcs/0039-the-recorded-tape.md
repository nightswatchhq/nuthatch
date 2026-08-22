# RFC-0039: The recorded tape - record RPC once, replay from disk

- Status: **Proposed - design only.** Implementation is a separate issue, opened once this is
  signed. This document does not build the rig.
- Author: Rowan
- Date: 2026-08-22
- Depends on: RFC-0004 (the bench harness this wires into), RFC-0028 / RFC-0029 §6f (the adaptive
  window controller - §3 leans on it being response-driven, not latency-driven).
- Closes, once implemented: #744 (the seal-direct-vs-hot-store question), and unblocks every
  performance claim after it - #767 is filed as the sprint's headline for exactly that reason.
- Origin: #767. magpie's own conclusion working #744 was that nuthatch cannot currently measure
  itself: a 3.8x spread inside one arm in one session, and seal-direct reading 0.92x - slower than
  the thing it is supposed to beat by an order of magnitude, contradicting a 5.2x measured hours
  earlier at the same commit.

## §0 - What this is not

Not a new ingestion mode, not a cache, not a fixture format anyone outside this rig has to consume
- the issue that opened this work says so explicitly, and it's worth repeating because the trait
this hangs off (`Source`) is central enough that scope creep here would be easy and wrong.

Not a fix for a busy box. It removes the network as a source of variance in a benchmark; it does
not remove another agent's `cargo build` sharing the same disk. §4 says plainly what this design
does instead about that (measurement discipline, not code) rather than implying the recorder makes
the box's own contention go away.

Not `[[calls]]`-capable in v1. Tier-3 `eth_call` resolution (RFC-0023) is not reachable through
`Source` at all - it goes through `RpcClient` directly, everywhere in `indexer.rs` (§1). A tape
covers `Source` traffic only. A nest declaring `[[calls]]` cannot be replayed until `eth_call` gets
a seam of its own; #744's own USDC fixture declares none, so this doesn't block closing it, but it
is named here rather than discovered later.

## §1 - Where this starts from

`Source` (`src/source.rs:72`) is an eight-method trait, five of them defaulted: a new source has to
answer `tip`, `block_hash`, `logs`, and can inherit `finalized`, `block_timestamps`,
`block_headers`, `block_bodies`, `forget_cached_above`. Two production implementations exist,
`RpcClient` and `ExExSource`; 23 of the tree's 26 implementations are test doubles in `indexer.rs`.
The seam is real, the breadth claim isn't - and the cheapness of this rig rests on the trait's small
required surface, not on how many things already implement it.

`indexer.rs`'s real backfill entrypoints already take `source: &dyn Source` -
`backfill_direct` (2563), `backfill_direct_pipelined` (3020), `backfill_direct_factory` (3272), and
everything downstream of them. **Nothing changes there.** They were built swappable from the start.

`tests/common/tape.rs`'s `TapeSource` is the existing proof this works end to end: 504 lines, a
`Source` impl with interior mutability that already drives the real `spawn_nest` loop through
land → seal → reorg with no network involved. It is *scripted* - a test author decides what block N
contains - not *recorded* - nothing captures a real chain's actual bytes. The honest description of
this rig, and the reason #767 was scoped as cheap: **`TapeSource` plus a recorder and an on-disk
format.** `TapeSource` itself is not replaced or reused directly (it stays the right tool for
control-flow tests that don't care about realistic content); what's reused is the proof that a
`Source` impl with no network can drive the real production loop.

What is *not* yet swappable is the bench harness's own wiring, and this is the actual gap #767
closes:

- `src/bench.rs`'s `one_run` hardcodes `let source = RpcClient::new(rpc_urls.to_vec())?`. There is
  no seam here at all today - it constructs a live client directly.
- `hot_store_backfill` takes `source: &RpcClient` specifically, not `&dyn Source` - even though its
  body only ever calls `Source` trait methods (`logs`, `block_headers`, `block_timestamps`). This is
  a type-signature change, not a rewrite.
- The one place a genuinely new capability is needed is `RpcClient::request_count()`, which
  `one_run` reads for `BenchReport.rpc_requests`. A replay source has no HTTP requests to count;
  §5 gives it an equivalent (tape reads served) rather than leaving the field meaningless.

Second, more fundamental gap: the tier-3 `[[calls]]` archive endpoint (`state_rpc`) is typed as
`&crate::rpc::RpcClient` / `Option<&crate::rpc::RpcClient>` throughout `indexer.rs` - never as
`&dyn Source`, because `eth_call` isn't one of `Source`'s eight methods. This is why §0 scopes v1 to
`calls_declared: 0` nests. Extending the trait to cover state reads is a real design question on its
own (RFC-0023/0024 territory) and not one this document answers.

## §2 - The tape format

Answering directly: keyed how, and what happens when the code under test asks for something the
tape does not contain.

**Recording happens at the `Source` trait boundary, not the raw JSON-RPC/HTTP boundary.** Three
reasons: it's the layer `indexer.rs` and `bench.rs` already call through, so nothing new needs
inventing; it collapses `RpcClient`'s own internal failover and retry noise (round-robin endpoint
cursor, health cooldowns, an atomic counter that already counts "including failover retries") into
one deterministic outcome per logical call - the tape doesn't need to know a request bounced off
two unhealthy endpoints before a third answered, only what `Source::logs(...)` ultimately returned;
and it keeps the recorded surface to eight methods instead of an open-ended RPC method set.

- A `RecordingSource` wraps any `Source` (in practice `RpcClient`) and appends one entry per call as
  it happens.
- A `ReplaySource` implements `Source` by answering out of a tape loaded into memory.

**Keying is content-keyed, not order-keyed.** Each entry's key is the method name plus its
canonicalised arguments: `logs` keys on sorted addresses + sorted topic0s + `from` + `to`;
`block_timestamps` / `block_headers` key on the sorted block list; `tip` / `finalized` have no
arguments (a singleton key each); `block_hash` keys on the block number. Order-keying - "replay call
N verbatim" - breaks the instant concurrency is on the table: the `Pipelined` arm fires several
window fetches at once, and a record session and a replay session can resolve them in a different
interleaving without either being wrong. Content-keying is indifferent to interleaving by
construction, which is also what makes one tape answer both the sequential and the pipelined arms
in §7 without a second recording.

Each key maps to a **queue** of recorded outcomes, consumed FIFO, rather than a single value - the
same call can legitimately happen more than once in one run. (A shrink-retry after a provider cap
usually has a different `to`, and therefore a different key, but a queue is the cheap, correct
answer for the cases where it doesn't, rather than asserting up front that it never happens.)

**Both success and error outcomes are recorded, not just successes.** This is the detail that makes
replay trustworthy rather than merely plausible: the adaptive window controller (RFC-0028/0029 §6f)
grows and shrinks in response to what came back, including a provider-cap *error* - indexer.rs
already has tests asserting the pass-two window shrinks on exactly that signal. A recorder that only
captured `Ok` responses would let replay skip the cap event that drove a real shrink during
recording, the window trajectory would silently diverge from the workload that was actually
measured, and the whole premise of the rig - replay the workload that really happened - would be
false without anyone noticing. So each entry's response is `Ok(value) | Err(message)`, and
`ReplaySource` replays either faithfully.

**A tape miss is a loud, specific failure, never a synthesised default.** `ReplaySource` returns an
error naming the exact miss and what the tape does cover, e.g. `tape miss: logs(addr=[0xa0b8...],
topic0=[0xddf2...], 100..200) not recorded; this tape covers blocks 25,809,368..=25,809,487 from
eth-pokt.nodies.app, recorded 2026-08-22`. A miss means the code under test asked for something the
recording didn't - a changed chunker, a changed retry policy, a different range - and that is a real
signal to surface, not something to paper over with an empty result. This is the direct answer to
the issue's own framing: "a rig that quietly invents data is generator two with a file behind it."

**On disk:** a directory, not an opaque blob, so it stays diffable in review -

- `manifest.json` - chain, provider host (via the existing `provider_of()` credential-stripping,
  §"provenance" below - no key ever reaches the tape), from/to block range, recorded-at date,
  recording commit, and the content address.
- `entries.jsonl` - one line per **unique key**, sorted deterministically by key so the file's own
  bytes are stable across two honest recordings of the same past-finality range (network jitter
  affects timing, not content, once a range is finalised) - which is what makes the tape
  content-addressable at all, and diffable when it legitimately changes.

**Content address:** canonical bytes of `entries.jsonl` → `sha256` → hex - exactly `src/lists.rs`'s
own pattern for content-addressed list snapshots (`lists/<sha256>.json`, "idempotent by content
address"), reused rather than inventing a third addressing scheme next to `cid.rs`'s CIDv0 and
`lists.rs`'s own sha256. This is the same field #744's acceptance criterion 3 already asks
`BenchReport` to carry for a live run (so a published number names the exact bytes it came from); a
replayed run populates it from the tape's hash instead of a live commit+provider+date triple - one
field, two ways to produce it, not two report shapes.

## §3 - The determinism boundary

Say what is deliberately left outside it and why.

**Above the line - a pure function of the tape:**

- Every request/response `Source` carries: logs, headers, timestamps, tip, finalized, block hashes.
- The adaptive window controller's trajectory. Because it reacts to response *content* (row counts,
  cap errors) and not to response *timing*, an identical tape drives it to an identical sequence of
  window boundaries on every replay. This is the load-bearing fact that makes replaying the
  concurrent/adaptive arms safe at all, not only the fixed-window ones - if the controller reacted
  to latency instead, content-keyed replay would still serve the right bytes but the *shape* of the
  backfill (how many windows, how wide) could differ run to run, and §7's four/five-arm comparison
  would be comparing different workloads again.
- Decoded event count, decoded row content, `calls_resolved` (0 by construction on a
  `calls_declared: 0` tape), sealed segment bytes and redb content - all byte-reproducible given the
  same tape and the same code, the same way `seal::seal_direct_matches_seal_via_hot_store` already
  proves the two storage paths agree on content today.

**Below the line - not a function of the tape, deliberately:**

- `wall_clock_s` / `events_per_sec` - how fast *this machine, right now* executes decode + store
  against a now-fixed workload. This is the actual measurement target, not a leftover variable.
  Determinism of the inputs is necessary to make this number mean anything; it is not sufficient to
  make it stable across five runs - that's §4's job, not this section's.
- `peak_rss_mb` - same reasoning.

**Explicitly outside v1's boundary, named rather than silently dropped:**

- `[[calls]]` / tier-3 `eth_call` traffic (§1, §0).
- `ExExSource` - push-shaped, no polling loop to record against. Out of scope; this rig targets the
  RPC-polling arms.
- DuckDB attach cost. Checked directly rather than assumed: `duckdb::Connection` does not appear
  anywhere in the backfill critical path (`bench.rs`'s `backfill`/`one_run`/`hot_store_backfill`,
  or `indexer.rs`'s `backfill_direct*`) - it's confined to `bench query` (a different harness) and to
  `seal.rs`'s own tests. Nothing to do for v1; flagged so it isn't rediscovered as a surprise if the
  rig is ever extended to cover `bench query`, which would need the same "load before you time"
  discipline §4 gives the tape.

## §4 - What could still blow ±2%, and what to do about each

Design backwards from the number, in the issue's own named order, plus one it named as the leading
suspect but didn't list as a checklist item.

1. **Page cache state between runs.** The tape file: load and parse the whole tape into memory
   *before* `Instant::now()` starts. This is not a new idiom - `bench query`'s own harness already
   does exactly this for its read-path cache ("warm the cache first, and discard it") and records
   why in a comment; replay reuses that discipline rather than inventing a second one for the write
   path. The output side (redb file, Parquet segments) needs nothing new: `one_run` already clears
   and creates a fresh, uniquely-named work directory per run number
   (`nuthatch-bench-{pid}-{run}`), so no run's output path is ever reused across the five measured
   runs - there is no "a later run benefits from an earlier run's warm page cache on the same file"
   effect to control for, because there is no shared file. Preserve this; `--replay` must not
   introduce a shared output path across runs as a convenience.

2. **redb file reuse.** Covered by the same mechanism as above - nothing new required. Called out on
   its own because it would be an easy thing to "optimise" away later by reusing one path for speed,
   and that optimisation is exactly the regression this item exists to prevent.

3. **DuckDB attach cost.** Not in the backfill critical path today (§3). No action for v1.

4. **Allocator warmup.** mimalloc is a real transitive dependency here (via `dbsp` - this box's own
   build notes already exist because of a GCC/mimalloc interaction). Arena and thread-cache warmup
   mostly costs the *first* allocation-heavy run in a process, not later ones, and all five measured
   runs already happen inside one process (`--runs N` loops `one_run` in-process; it does not shell
   out five times) - so a run-1-vs-run-5 asymmetry from warmup is a real, plausible contributor to
   the 3.8x magpie saw, not a hypothetical one. Mitigation: one **discarded warm-up run** before the
   five measured runs, mirroring `bench query`'s existing warm-and-discard pattern rather than a new
   one. Cheap, and reuses an idiom already reviewed and shipped in this file.

5. **The wall clock itself.** Already `Instant::now()` / `.elapsed()` - a monotonic clock, immune to
   NTP step adjustments. Checked, not assumed; no change needed.

6. **The box, not named as a checklist item in the issue but the one magpie's own report names as
   the leading suspect** (3.8x spread, load 4.8-6.4 on 32 cores during the measurement window). This
   rig cannot fix it in code, and the design should say so rather than imply the recorder solves it
   by itself:
   - `BenchReport` gains a best-effort `box_load: Option<String>` (load average at measurement
     start), on the same "`None` over a guess" discipline `hardware_summary()` already applies to
     cores/RAM - an invented number is worse than an absent one because it looks authoritative.
   - The ±2% demonstration in §6 is preconditioned on an idle box, stated as part of the
     demonstration's own instructions - a run taken on a loaded box is void, not evidence either
     way, and the report should make that checkable rather than trusted.
   - A hard refusal above some load threshold is left as a follow-up knob, not specified here - the
     right threshold is itself an empirical question the first real demonstration run should answer,
     and picking one now would be inventing a number with the same confidence problem this whole
     rig exists to avoid.

## §5 - Wiring

Mechanical and small, on purpose - everything expensive about this rig is the format and the
discipline in §2-§4, not the plumbing:

- `nuthatch bench backfill` gains `--record <path>` and `--replay <path>`, mutually exclusive with
  each other. `--replay` is mutually exclusive with `--rpc` / `--state-rpc` and touches no network
  by construction - a `ReplaySource` never holds an `RpcClient`, so there is nothing to point at a
  live endpoint even by mistake.
- `one_run` stops hardcoding `RpcClient::new`. It selects one of three `Source` constructions: live
  (`RpcClient`), live-and-recording (`RecordingSource` wrapping `RpcClient`, flushing the tape to
  `--record`'s path on completion), or replay (`ReplaySource` reading `--replay`'s path).
  `hot_store_backfill`'s `source: &RpcClient` parameter becomes `source: &dyn Source` - its body
  already calls nothing else.
- `RecordingSource` / `ReplaySource` are production code, living with `Source` (`src/source.rs`) or
  in a new `src/tape.rs` - not test-only, unlike `TapeSource`. `TapeSource` is unchanged and keeps
  its job: a scripted double for control-flow tests that don't need realistic bytes.
- A replay source needs a stand-in for `RpcClient::request_count()` (`BenchReport.rpc_requests`):
  count tape reads served instead of HTTP requests sent. The field keeps meaning "how many times did
  the code under test ask the source for something" on both arms; it stops meaning "how many bytes
  went over a wire" on replay, and the report should say so once rather than leave a reader to infer
  it from a report emitted after a live-run-only field went strangely low.
- `BenchReport` gains `fixture_content_address: Option<String>` (§2, populated on both record and
  replay) and `box_load: Option<String>` (§4).

## §6 - Demonstrating ±2%, and the predicted numbers

Named command, predicted numbers stated before the command runs - not asserted only after.

```sh
# Once, from a real provider. Same nest, same range #744 already used - nothing new to justify here.
nuthatch bench backfill --dir <usdc-nest> --from 25809368 --to 25809487 \
  --record docs/bench/tapes/usdc-120-transfer.jsonl --runs 1

# The demonstration. Precondition: an idle box - check `uptime` before starting and record it
# (§4 item 6); a run taken on a loaded box is void, not evidence.
nuthatch bench backfill --dir <usdc-nest> --from 25809368 --to 25809487 \
  --replay docs/bench/tapes/usdc-120-transfer.jsonl --runs 5 --out replay-hot.json
```

**Predicted, before running, and falsifiable:**

- `events` is exactly **11,758** on all five runs, no variance at all - that is the entire mechanism
  working, not a secondary observation. If it moves even once across the five runs, the tape/replay
  design has already failed at a level where the wall-clock variance question isn't worth asking
  yet.
- `events_per_sec`'s spread should collapse from magpie's measured 3.8x to something close to
  ordinary single-process measurement noise on an idle box. The closest real data point for this
  exact fixture's hot-store arm is `docs/bench/722-hot.json`'s 2,860 ev/s (itself a median of 5
  runs, on this same box, under different and undocumented load conditions) - I'd expect the five
  replay runs to land within a couple of percent of a median somewhere in the 2,500-3,000 ev/s band.
- If the demonstration instead reproduces anything resembling the 3.8x spread with the network fully
  removed, the leading suspect moves from "the network" to "the box" with much higher confidence
  than magpie could establish - which is itself a useful, falsifiable outcome, not a failure of the
  rig.

## §7 - Closing #744

"Which four arms get run, on which tape" - answered concretely, reusing the exact four-way split
`docs/benchmarks.md` already names as the honest decomposition since `--window-adaptive` (#744,
PR #758) separated window policy from storage path: *"hot-fixed, hot-adaptive, seal-fixed and
seal-adaptive are all reachable independently."* This rig gives that decomposition a number worth
trusting, rather than inventing a new one:

1. hot store, fixed window (`BackfillPath::HotStore`, `window_adaptive: false`)
2. hot store, adaptive window (`BackfillPath::HotStore`, `window_adaptive: true`)
3. seal-direct sequential, fixed window (`BackfillPath::Direct`, `window_adaptive: false`)
4. seal-direct sequential, adaptive window (`BackfillPath::Direct`, `window_adaptive: true`)

All four against the one `usdc-120-transfer.jsonl` tape from §6. Content-keying (§2) makes this safe
regardless of window policy or storage path, since none of these four issue concurrent fetches.

`Pipelined` (seal-direct + `--concurrency 8`) runs as a fifth, informational arm on the *same* tape,
for the separate pipeline-multiplier claim `docs/benchmarks.md`'s "Pipeline" section makes.
Concurrency changes request *interleaving*, not request *content*, and content-keyed replay is
indifferent to interleaving by construction (§2) - so one recording answers both questions, no
second tape needed.

Whatever the five numbers say, publish them - closing #744 means the number is trusted, not that it
favours seal-direct. "If seal-direct really is ~0.92x, we publish that. If it is 8x, we publish
that." Either closes it; "we still don't know" is the only outcome that doesn't.

(Note for whoever picks up implementation: GitHub currently shows #744 as **closed**, from the
`--window-adaptive` decoupling PR landing separately from this rig. The sprint's Definition of Done
still names it as riding on this issue's fourth acceptance criterion - read that as "the trustworthy
number is still owed," not as a discrepancy to silently resolve either way.)

## §8 - Would this have caught `289 events/sec`?

The honest answer, not the comfortable one: only if the tape is checked into the repo, not
regenerated ad hoc per measurement session. A tape that lives only on whoever's machine recorded it
decays into folklore exactly like every number before it - "re-run this against USDC blocks X-Y"
already failed once for a reason narrower than network noise: 12,933 and 11,758 were **both
correct**, for two different declared event sets over the same nominal range (`Approval` + other
logs included, or not). "The same range" was never precise enough; a checked-in, content-addressed
tape *is* the range, the provider's actual response, and the declared event set, all three, byte for
byte, forever - which is what "the tape has to be an artifact the repo holds and CI can reach"
concretely requires:

- `docs/bench/tapes/usdc-120-transfer.jsonl` (and siblings for W2/W3 as they're recorded) is
  committed alongside the `bench-report.json` artifacts already living in `docs/bench/`, referenced
  by content address from the report that used it.
- A cheap, always-on CI check replays the tape and asserts the decoded event count against a
  hardcoded expectation (11,758) - a *correctness* check, not a perf gate, needing no network and no
  idle-box precondition. No CI YAML change is needed for this: the same precedent #769/PR #775
  already established (a plain `cargo test` file runs inside the already-required fmt/clippy/test
  job the moment it merges) applies here unchanged.
- The **wall-clock** ±2% gate is a different kind of claim - it needs an idle box, which this shared
  dev box cannot promise on every PR (a documented, standing, unresolved hazard on this machine).
  Recommend it as a scheduled or manually-triggered job against the checked-in tape, not a per-PR
  gate. This is a deliberate scope boundary for the implementation issue to carry forward, not
  something fudged here by pretending a busy CI runner will hit ±2% on demand.
- Re-deriving "was 289 events/sec ever real" in week six becomes:
  `nuthatch bench backfill --replay docs/bench/tapes/<the-tape-that-produced-it>.jsonl --runs 5` -
  no provider, no "which 120 blocks, exactly", no live chain state that has since moved on. That is
  the actual answer to "would the design have caught it": not automatically, on its own, forever -
  but it turns an unanswerable question into a one-command re-derivation, which is the most honest
  thing "structurally impossible" can mean for a number that necessarily depends on this machine's
  clock.

## Open questions for review

1. Directory-per-tape vs. a single file: this document assumes a directory
   (`manifest.json` + `entries.jsonl`) for diffability. A single JSONL file with the manifest as its
   first line would also work and is simpler to reference by one path - worth a second opinion
   before implementation picks one.
2. `docs/bench/tapes/` as the checked-in location, alongside the existing `docs/bench/*.json`
   reports - reasonable, or does tape size argue for somewhere else (e.g. LFS, or a separate
   `tests/fixtures/tapes/` if some tapes are meant for `cargo test` rather than `docs/benchmarks.md`
   provenance)? 120 blocks of USDC transfers is small; a W1-sized tape (100,000 blocks) may not be.
3. §4's load-average refusal threshold is deliberately left unspecified pending the first real
   demonstration run's numbers - confirm that's the right amount of design-now-vs-decide-later, or
   whether the board wants a number pinned before implementation starts.
4. §7's five-arm plan reuses the existing `docs/benchmarks.md` "hot-fixed / hot-adaptive /
   seal-fixed / seal-adaptive" framing verbatim - confirm that's the intended reading of "four arms"
   in the issue, since the `BackfillPath` enum itself has four *different* variants (the fourth being
   `Factory`, which this fixture can't exercise).
