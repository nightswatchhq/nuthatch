# RFC-0028: Adaptive `eth_getLogs` range control - stop making the operator guess

- Status: **Draft** (2026-07-28)
- Author: Pete (cargopete)
- Date: 2026-07-28
- Depends on: RFC-0004 (backfill throughput - this tunes the window that RFC introduced), RFC-0026
  (the fault taxonomy this extends to RPC errors).
- Blocks: nothing, but it removes the largest piece of user-written glue reported to date.
- Nature: **mini-RFC** - four questions (§3-§6). One of them (§4) turns out to be about segment
  determinism rather than about ranges, and it is the one that matters most.
- Origin: developer feedback from **ETHGlobal Pragma Lisbon 2026**, where three teams ran nuthatch.
  The Turing Swap team, running a load-bearing World Chain nest, reported: *"I had to build an RPC
  proxy that splits large `eth_getLogs` requests into smaller ranges."* That proxy is the highest-value
  thing this project has been told to build, because someone built it for us.

## Abstract

nuthatch asks the operator to pick an `eth_getLogs` block window up front and then holds them to it
forever. Providers cap `eth_getLogs` by block range, by result count, and by response size, and those
caps differ per provider, per chain, and per contract. A window that is right for a sparse contract on
one endpoint is rejected by another, and nuthatch's response to rejection is to retry the same width
against the next endpoint - which fails identically.

This RFC makes the window **adaptive**: start at the configured ceiling, narrow on evidence that the
request was too large, widen again when the constraint lifts, and never silently drop a log. It also
settles the error taxonomy (which failures mean "too big" versus "try elsewhere" versus "stop"), and -
the part that turned out to matter most - it decouples **segment boundaries** from **fetch boundaries**,
because today they are the same thing and that quietly makes content-addressing depend on the operator's
RPC provider.

## 1. What actually happens today

**One call, one width, no adaptation.** `RpcClient::logs` (`rpc.rs:420-445`) issues a single
`eth_getLogs` for the window it is handed. There is no splitting, no shrink-and-retry, and no
inspection of *why* a call failed. On failure the caller round-robins to another endpoint and retries
**at the same width**. If the width is the problem, every endpoint refuses it, forever. The CLI
reference states this outright: *"the concurrent backfill fails the range rather than auto-shrinking
it."*

**We already know the pattern.** `block_timestamps` splits its work into bounded sub-batches
(`MAX_TIMESTAMP_BATCH`, `rpc.rs:320`) with the comment *"splitting into bounded sub-batches keeps each
request within common limits."* We applied it to the call whose size we control and not to the call
whose size the chain controls.

**It has already bitten us twice more.** COR-5 in the backlog is the same root cause in the tip loop: a
factory nest's topic0-only fetch cannot clear a provider `getLogs` cap on a busy chain, and the ingest
task dies. And on our own production box the `livepeer` nest spent forty minutes logging
`403 Forbidden` and `429 Too Many Requests` in a retry loop before being stopped by hand - a failure
mode that is *not* a range problem and must not be treated as one.

**The window is also mis-specified as a promise.** `--window` today means "use exactly this". Its help
text has to warn the operator to *"keep it under your provider's max block-range"* - which is asking
them to know a number that varies by provider, chain, contract and time of day.

## 2. The principle

**How we fetch must never change what we store.**

Fetching is a transport concern. The row set for a block range is a property of the chain, not of the
batching used to retrieve it. Any adaptive strategy must therefore satisfy: for a given nest and block
range, the stored rows - and the sealed segments over them - are identical regardless of how the range
was split.

The first half of that is already true (rows are keyed by `(block, log_index)`, and sub-ranges that
tile a window produce the same set). **The second half is not true today**, which is §4.

## 3. Question 1 - what counts as "the request was too big"?

Providers disagree on how to say it. The taxonomy has three classes, and the classification is the
whole design - get it wrong and we shrink on network blips or spin forever on an auth failure.

| Class | Signals | Response |
|---|---|---|
| **Narrowable** | JSON-RPC `-32005` / `-32602` with range or result-count text; HTTP `413`; messages matching *"more than N results"*, *"block range too large/wide"*, *"response size exceeded"*, *"query timeout exceeded"* | halve the window and retry the **same** range (§4) |
| **Transient** | HTTP `5xx`, connection/timeout errors, and **plain `429` with no size evidence** | current behaviour: fail over to the next endpoint, retry at the same width |
| **Terminal for that endpoint** | HTTP `401`/`403`, "method not supported", "invalid API key" | mark the endpoint unhealthy and stop sending to it; **never** retry it in a tight loop |

**Measured, not recalled (2026-07-28, Alchemy eth-mainnet).** Two of these classes were probed directly
rather than assumed:

```
# oversized range: USDC, 65,536 blocks, no topic filter
{"error":{"code":-32602,"message":"Log response size exceeded. You can make eth_getLogs requests with
 up to a 10,000 block range and no limit on the response size, or you can request any block range with
 a cap of 10K logs in the response. Based on your parameters and the response size limit, this block
 range should work: [0x1000000, 0x1007fff]"}}

# bad api key
HTTP 401  {"error":{"code":-32600,"message":"Must be authenticated!"}}
```

Two things fall out. First, **`-32602` is generic "invalid params"** - the code alone cannot classify,
so message matching is required, which is why the table above lists text patterns rather than codes.
Second, the auth failure carries **HTTP 401**, so the terminal class is detectable from the HTTP status
before any JSON parsing. Both classes are cheaply and reliably distinguishable; this is not guesswork.

Two rulings that are not obvious:

**`429` is transient first, narrowable second.** A rate limit usually means "you asked too often", but
providers also 429 an expensive query. Shrinking on the first 429 would degrade throughput for what is
really a pacing problem. So: 429 fails over; if **every** endpoint in the pool 429s on the *same*
window, that is evidence about the window, and it escalates to narrowable. Cheap to implement (a
per-window failure counter) and it distinguishes the two causes without guessing.

**Auth failures are terminal, not transient.** This is the livepeer incident: a 403 endpoint was
retried indefinitely because every RPC failure is currently treated as retryable. An endpoint that
rejects our credentials will not start accepting them because we asked 400 more times. It should be
cooled down loudly, and if it is the only endpoint, the cursor should quarantine (RFC-0026) rather than
spin. This is arguably a bug fix that happens to live in this RFC.

## 4. Question 2 - segment boundaries must stop following fetch boundaries

This is the question I did not expect to be asking, and it is the reason this RFC is worth writing
rather than just patching `rpc.rs`.

**Today, a segment's block range is decided by where a fetch window happened to end.** In
`seal_direct` (`indexer.rs:1281-1288`), rows accumulate into a buffer and flush when the buffer reaches
`SEAL_DIRECT_BATCH` (20,000 rows) or the range ends - and the flush is recorded as
`seal_range(dir, &buf, batch_from, chunk_to)`, where `chunk_to` is **the end of the fetch window that
happened to tip the buffer over**.

The consequence is already true before this RFC changes anything: **two operators indexing the same
contract over the same range with different `--window` or `--concurrency` settings produce segments
with different block boundaries, hence different bytes, hence different content hashes.** They do not
dedupe. Neither is wrong, but the "same inputs, same content-addressed output" property is quietly
conditional on matching RPC tuning.

Adaptive windows would take that from *occasional* (someone changed a flag) to *routine* (two operators
on different providers adapt differently within the same run). So the fix belongs here.

**Ruling: seal boundaries become deterministic and fetch-independent.** A segment flushes on a boundary
derived only from the data and a constant - block-aligned, e.g. "flush at the last block boundary at or
before the point the buffer reached `SEAL_DIRECT_BATCH`", with the alignment a compile-time constant
rather than a runtime knob. Two runs over the same range then produce identical segments no matter how
either fetched them.

This makes an existing claim true rather than aspirational, and it strengthens RFC-0019 (bundle
dedup), RFC-0020 slice 4 (segment reuse across versions), and the verifiability argument in
`CLAUDE.md`. It is also the riskiest change in this RFC, which is why it gets its own slice and its own
mutation-checked test.

## 5. Question 3 - the control law

**Per endpoint, not per cursor.** Pools are commonly heterogeneous (the Lisbon team ran three
providers). A window learned against a strict endpoint should not penalise a permissive one. Each
endpoint carries its own effective window, initialised to the configured ceiling.

**Take the provider's answer when it gives one.** The measured Alchemy refusal in §3 does not merely say
"too big" - it says *"this block range should work: [0x1000000, 0x1007fff]"*. That is authoritative and
precise, and blind halving toward it would waste `log2(n)` round trips rediscovering what we were just
told. So: **if a narrowable error carries a suggested range, jump straight to it**; fall back to halving
only when no hint is offered. The hint is parsed defensively (a suggestion wider than what we asked for,
or malformed, is ignored in favour of halving) because it is provider text, not a contract.

This turns the common case from a shrinking search into a single corrective retry, and it means a
first-run backfill against a strict provider converges almost immediately rather than after a visible
stall. It also generalises: any provider that names a workable range gets the same treatment, and any
that does not still works via halving.

**Narrow multiplicatively, widen additively.** Halve on a narrowable error with no usable hint; after `K`
consecutive successes at the current width, widen by a fixed increment rather than doubling. Multiplicative
decrease reacts fast to a wall; additive increase stops the width oscillating between "rejected" and
"accepted" forever. This is AIMD, for the same reason TCP uses it.

**A floor, and a loud failure at it.** Halving stops at 1 block. A single block that still returns
"too many results" is not a range problem - it is a block whose logs exceed what the provider will
serve at all. That is a real error and must surface as one (fail the window, quarantine per RFC-0026),
not become an infinite shrink loop.

**`--window` changes meaning from a fixed size to a ceiling.** The operator says "never ask for more
than this"; nuthatch finds what actually works underneath it. This is strictly kinder than today - a
user who set 50000 now gets *up to* 50000 with automatic backoff instead of hard failures - and it
removes the "know your provider's cap" burden the help text currently imposes. The flag name and its
existing values keep working, so it is a semantic softening, not a breaking change.

**Interaction with `--concurrency`.** With `K` windows in flight, a single narrowable error should
cause **one** shrink, not `K`. Shrinks are applied per generation: errors from windows planned at the
old width are absorbed without re-shrinking. Sustained narrowing should also reduce effective
concurrency, since a provider refusing width is rarely delighted by parallelism.

## 6. Question 4 - what an operator can see

Adaptive behaviour that is invisible is indistinguishable from a mystery slowdown.

- **Metrics:** `nuthatch_rpc_window_blocks{endpoint}` (gauge - the current effective width),
  `nuthatch_rpc_range_shrinks_total{endpoint}`, `nuthatch_rpc_range_grows_total{endpoint}`, and
  `nuthatch_rpc_endpoint_healthy{endpoint}` for the terminal-class cooldowns.
- **Logs:** one `info` on each shrink and grow, rate-limited, naming the endpoint, the old and new
  width, and the classified reason.
- **The runbook question this answers:** "my backfill is slow" becomes checkable - if the effective
  window has collapsed to 20 blocks against a provider capping result counts, the operator can see it
  and go get a better endpoint, rather than filing a bug about throughput.

## 7. Non-goals

- **Provider-specific pagination protocols.** Some providers offer cursor-based log pagination. Parsing
  a suggested block range out of an error message (§5) is in scope because it is cheap, defensive and
  vendor-neutral in shape; adopting a vendor's bespoke pagination protocol is not.
- **Changing the tip loop in the first slices.** COR-5 is real, but the tip loop is sensitive and the
  backfill path carries the reported pain. The tip loop gets the same machinery in a later, deliberate
  slice.
- **Persisting learned widths across restarts.** A restart re-learns in a handful of requests. Worth
  revisiting only if that proves annoying.
- **Making `eth_getLogs` concurrent within a single window.** Splitting is sequential; parallelism
  stays at the `--concurrency` level where it already lives.

## 8. Slices

1. **Error taxonomy** (§3). Classify RPC failures into narrowable / transient / terminal, with the
   loopback JSON-RPC mock (added during sprint amiable-axolotl for the failover tests) as the fixture.
   No adaptation yet - but the **auth-terminal** ruling lands here, which alone fixes the 403 spin seen
   in production.
2. **Deterministic seal boundaries** (§4). Decouple segment flush points from fetch windows. Ships
   before adaptation, so adaptation cannot be blamed for a segment-identity change. Mutation-checked:
   break the alignment, prove the test goes red.
3. **Narrowing** (§5). Halve-on-narrowable with a floor, per endpoint, applied per generation under
   concurrency. `--window` becomes a ceiling.
4. **Widening + observability** (§5, §6). Additive increase, the metrics, the logs.
5. **The tip loop** (COR-5). The same control applied to the tip fetch, with the address-filtered
   fallback the backfill path already has.

## 9. Acceptance

- **Determinism across fetch strategies.** The same nest and range, indexed with `--window 5000` and
  again with `--window 50`, produce identical rows **and byte-identical segments**. This is the
  headline test and it fails today.
- **Convergence.** Against a mock that rejects any range wider than N blocks, a backfill completes,
  with the same rows a permissive mock produces.
- **No shrink on noise.** A mock returning 500s and timeouts triggers failover and retries, and the
  effective window is unchanged at the end.
- **Auth is terminal.** A mock returning 403 marks the endpoint unhealthy within one attempt; the run
  does not loop, and with no healthy endpoints left the cursor quarantines with a clear reason.
- **Recovery.** After the mock stops constraining, the effective window returns to the ceiling.
- **Floor honesty.** A mock that refuses even a single block produces a loud terminal error, not an
  infinite shrink.
- **Throughput is not paid for adaptation.** The RFC-0004 benchmark on a permissive endpoint shows no
  regression against the pre-RFC baseline.

## 10. Open questions (implementation, not scope)

- The widening increment and `K` (consecutive successes before widening). Wants a measurement against a
  real capped provider rather than a guess.
- Whether the per-generation shrink should be per endpoint or per cursor when `--concurrency` is high -
  per endpoint is more correct, but the planner currently chooses an endpoint after choosing a window.
- The seal alignment constant in §4: block-aligned to what? A power of two is tidy; aligning to the
  chain's window default is self-referential and should be avoided precisely because it would reintroduce
  the coupling this RFC removes.
- Whether `nuthatch bench backfill` should report the *effective* window distribution, so a benchmark
  against a capped provider is legible rather than mysteriously slow.
