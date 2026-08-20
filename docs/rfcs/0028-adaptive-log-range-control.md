# RFC-0028: Closing the gaps in `eth_getLogs` range control

- Status: **Implemented** (2026-07-28; **substantially revised the same day - see §0**)
- Author: Pete (cargopete)
- Date: 2026-07-28
- Depends on: RFC-0004 §2 (the adaptive chunker this extends), RFC-0026 (the fault taxonomy).
- Nature: **mini-RFC**, and now a **fix pack** rather than a new capability.
- Origin: developer feedback from ETHGlobal Pragma Lisbon 2026, where a team reported writing an RPC
  proxy to split oversized `eth_getLogs` requests. We have no contact with that team, so everything
  below is grounded in what can be measured from this repository and from live endpoints.

## §0 - Correction: most of this already exists

The first draft of this RFC claimed nuthatch had "no splitting, no shrink-and-retry" and proposed
building adaptive range control from scratch. **That was wrong**, and the error is recorded here
because it shaped a day of planning.

The mistake: I read `RpcClient::logs` (`rpc.rs`), correctly observed that the *transport* layer issues
one call at one width, and concluded the system had no adaptation. The adaptation lives one layer up,
in the callers - which is arguably the right place for it.

What has been implemented since 2026-07-17 (`c2fe50c`; PR #57):

| Capability | Where |
|---|---|
| Recursive binary splitting of an oversized range, reassembling results | `indexer.rs::fetch_logs_splitting` |
| Adaptive window sizing toward a response budget (shrink on overshoot, grow on undershoot) | `chunker.rs::AdaptiveWindow` |
| Cap-error detection driving shrink-and-retry | `chunker.rs::is_result_too_large` |
| Single-block-over-cap floor, failing loudly instead of shrinking forever (H3) | `indexer.rs::single_block_over_cap` |
| Transient all-endpoints retry with capped backoff | `indexer.rs::retry_transient` |

Wired into six call sites: `roost_index_loop`, `index_loop`, `backfill_direct`,
`backfill_direct_pipelined`, `backfill_direct_factory`, `logs_with_retry`.

So nuthatch already does the thing that team built a proxy for. The question is not "why don't we
split?" but **"why didn't splitting fire?"** - and that has a measurable answer.

## 1. The gap, measured

`is_result_too_large` matches on message text, and its marker list was written against Alchemy and
Infura. Probing live endpoints for their oversized-range response (2026-07-28, Arbitrum One native
USDC over a populated 199k-block range):

```
arb1.arbitrum.io          HTTP 200  {"code":-32000,"message":"logs matched by query exceeds limit of 10000"}
eth-mainnet Alchemy       HTTP 200  {"code":-32602,"message":"Log response size exceeded. … this block
                                     range should work: [0x1000000, 0x1007fff]"}
arbitrum-one.publicnode   HTTP 403  {"code":-32602,"message":"Archive requests require a personal token"}
```

Checked against the marker list:

| Message | Matched |
|---|---|
| `logs matched by query exceeds limit of 10000` | **no** |
| `Log response size exceeded …` | yes (`response size`) |
| `query returned more than 10000 results` | yes (`query returned more than`) |

**`arb1.arbitrum.io` is one of the public endpoints nuthatch ships as an Arbitrum default.** Its cap
message matches nothing, so `is_result_too_large` returns false, `fetch_logs_splitting` never recurses,
and the window is retried at the same width until someone intervenes. The zero-setup path we advertise
- `init` an Arbitrum contract, `dev`, watch it index - fails exactly this way on a busy contract.

That explains the field report without needing to reach the team, and it makes this a bug in our
shipped defaults rather than a missing feature.

## 2. The principle

**A marker list is a liability.** Any design that requires having seen a provider's phrasing in advance
will keep failing on providers we have not met. The fix is not merely to add `"exceeds limit of"` - it
is to stop depending solely on the list.

## 3. What to change

**(a) Widen the markers, with tests carrying measured strings.** Add the arb1 phrasing and its obvious
neighbours. Cheap and immediate; the regression test uses *measured* provider text so the list stays
grounded rather than imagined.

**(b) Speculatively split an unclassifiable failure.** The durable fix: when a window fails in a way we
cannot classify, and it spans more than one block, **try splitting once before giving up**. Splitting is
safe - sub-ranges tile exactly, results reassemble, rows are keyed by `(block, log_index)` - so a
speculative split costs one extra round trip and removes the dependency on recognising vendor prose. If
both halves fail too, the failure was not about size and normal handling resumes.

**(c) Honour a provider-suggested range.** Alchemy names the range that would have worked; jumping to it
beats halving toward it. Parsed defensively, ignored unless it is genuinely narrower.

**(d) Escalate a pool-wide 429 on the same window.** A rate limit is about pacing, not size, so it stays
transient - but if *every* endpoint 429s on the *same* window, that is evidence about the window. Our
own production logs show `arb1.arbitrum.io` returning 429 on `getLogs` while the livepeer nest ran, so
this path is real rather than hypothetical.

**(e) Consolidate the two classifiers.** Slice 1 (merged, #165) added `rpc.rs::classify_rpc_error`
alongside the existing `chunker::is_result_too_large`. Two independent error classifiers is worse than
one; `is_result_too_large` should become a thin interface onto the richer classification.

**(f) A capability 403 is not a credentials 403.** The publicnode probe above returns 403 for *archive*
requests while serving recent blocks perfectly well. Slice 1 classifies 403 as terminal and cools the
endpoint for five minutes, which is too blunt for an endpoint that could still serve the tip. Worth
narrowing - a shorter cooldown, or health tracked per request kind - when the body indicates a
capability limit rather than an auth failure.

## 4. Segment boundaries still follow fetch boundaries

This part of the original draft stands, and was verified independently of everything above.

In `backfill_direct` (`indexer.rs:1281-1288`) rows accumulate into a buffer and flush when it reaches
`SEAL_DIRECT_BATCH` (20,000 rows) or the range ends - recorded as
`seal_range(dir, &buf, batch_from, chunk_to)`, where `chunk_to` is **the end of whichever fetch window
tipped the buffer over**.

So **two operators indexing the same contract over the same range with different `--window` or
`--concurrency` produce segments with different block boundaries, different bytes, and different content
hashes.** Nothing is incorrect - the rows are identical - but "same inputs, same content-addressed
output" is quietly conditional on matching RPC tuning, which is not what the docs or `CLAUDE.md` imply,
and it is load-bearing for RFC-0019 bundle dedup and RFC-0020 segment reuse.

The adaptive chunker makes this *worse*, not better: the effective window varies with provider behaviour
during a single run, so even one operator's segments depend on how their provider was feeling.

**Ruling: flush at a boundary derived from the data, not the fetch.** When the buffer reaches the
threshold, seal up to the **block at which the threshold was crossed**, retaining rows from later blocks
for the next segment. "Cumulative rows by block" is a property of the chain, so the flush point is
identical however the range was fetched. Blocks are never split across segments.

## 5. Non-goals

- Rebuilding the chunker. It works; this widens what it recognises and makes its output deterministic.
- Vendor-specific pagination protocols.
- Persisting learned widths across restarts.

## 6. Slices

1. ~~Error taxonomy~~ **merged** (#165) - `FailureClass`, terminal auth handling, suggested-range parser.
2. **Marker coverage + speculative split** (§3a, §3b), with regression tests carrying the measured
   provider strings - including the arb1 phrasing that started this.
3. **Hint honouring + 429 escalation + classifier consolidation** (§3c, §3d, §3e) and the 403 narrowing
   (§3f).
4. **Deterministic seal boundaries** (§4) - the riskiest, mutation-checked.

## 7. Acceptance

- A mock returning `"logs matched by query exceeds limit of 10000"` triggers splitting and the backfill
  completes with the same rows a permissive mock produces. **Fails today.**
- An unclassifiable failure on a multi-block window triggers one speculative split before giving up.
- The same nest and range, indexed with `--window 5000` and again with `--window 50`, produce identical
  rows **and byte-identical segments**. Fails today.
- No shrink on 5xx or timeouts; the effective window is unchanged after a burst of them.
- The RFC-0004 benchmark shows no regression against a permissive endpoint.

## 8. Open questions

- Speculative split once, or recursively? Once is safer and probably enough - a genuine size failure
  re-triggers on the halves.
- Should `is_result_too_large` be retired in favour of the `rpc.rs` classifier, or kept as the chunker's
  narrow interface onto it?
- The right treatment of a capability-limited 403 (§3f): shorter cooldown, or per-request-kind health.
