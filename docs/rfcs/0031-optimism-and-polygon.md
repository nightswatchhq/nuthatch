# RFC-0031: Optimism and Polygon - the two chains that fail in opposite ways

- Status: **Draft** (2026-08-03)
- Author: Pete (cargopete)
- Date: 2026-08-03
- Depends on: RFC-0030 (the chain registry and the endpoint bar this applies), RFC-0004 §2 (the
  adaptive chunker a `log_window` seeds), RFC-0028 (cap-error taxonomy), RFC-0026 (fault taxonomy,
  for the finality-regression case in §3.3).
- Blocks: nothing. Unblocks the two most-requested EVM chains after the three we ship.
- Nature: full RFC. The code is small; the **finality analysis in §3 is the reason this is not a
  mini-RFC**, because getting it wrong writes bad data to immutable storage.

## Abstract

RFC-0030 established *how* a chain enters nuthatch: a registry entry plus a measured endpoint bar.
This RFC applies it to Optimism (chain 10) and Polygon PoS (chain 137), and finds that **neither is
blocked for the reason you would guess**.

Optimism is architecturally free - byte-for-byte the same shape as the Base entry we already ship -
but of ten free endpoints probed, **exactly one** satisfies the bar. It fails on availability.

Polygon has two qualifying endpoints and no availability problem, but reports a `finalized` tag only
**3 blocks** behind the tip, which is a claim about reorg safety we must not accept on trust, because
acting on it seals immutable Parquet. It fails on semantics.

The recommendation is therefore asymmetric: **take Polygon first, with a deliberately conservative
finality policy; hold Optimism until a second endpoint exists** or we accept a single-endpoint entry
as an explicit, documented exception.

## Motivation

Two pulls, one push.

**The pull.** Every "can nuthatch index X?" conversation that ends in "no" ends there because of a
three-element array (`chains::all()`), not because of difficulty. Optimism and Polygon are the two
chains that come up most after mainnet/Arbitrum/Base. The catalogue already over-promises them: the
`uniswap-v3` entry in `nightswatchhq/nests` advertises `["Arbitrum","Ethereum","Optimism","Base",
"Polygon"]`, two of which nuthatch cannot index at all. We are shipping a claim we cannot honour.

**The push.** RFC-0030 asserted a bar without exercising it on a hard case. Gnosis passed cleanly and
therefore proved little. Optimism and Polygon each fail a *different* criterion, which is the useful
test of whether the bar is well-designed - and, as it turns out, of whether it is complete. §3.4
argues it is not.

## Goals

1. Register Polygon PoS with a finality policy justified by measurement, not by its `finalized` tag.
2. Establish whether Optimism can be registered at all under RFC-0030 §4, and say plainly what
   blocks it.
3. Extend the RFC-0030 bar with a **finality-trust criterion** - the gap it did not anticipate.
4. Correct the catalogue so it stops advertising unsupported chains.

## Non-goals

- Non-EVM chains (CLAUDE.md, unchanged).
- Optimism's or Polygon's L2-specific extras - deposit transactions, the L1 attributes predeploy,
  Polygon's state-sync / bridge events. Those are *nest* concerns, indexable as ordinary contracts
  once the chain is registered. Nothing here needs a `#[cfg]` in the core.
- Guaranteeing endpoint availability. §4 is a bar for entry, not an SLA.
- Building an Optimism or Polygon nest. Registering a chain is a precondition for one, not a promise
  of one (RFC-0030 §8).

## Design

### §1 Optimism: architecturally free, blocked on endpoints

Optimism is an OP-stack L2, the same stack as Base, which we already ship. Same block cadence (~2 s),
same L1-derived finality, same `finalized` tag support. The registry entry would differ from `BASE`
in `name`, `chain_id`, and `rpc_urls` - nothing else. There is no design work.

Measured 2026-08-03, ten free endpoints against the RFC-0030 §4 bar:

| Endpoint | chainId | Archive | Batch > 3 | `finalized` lag |
|---|---|---|---|---|
| `mainnet.optimism.io` | 10 | ✅ | ✅ | 571 |
| `optimism.drpc.org` | 10 | ✅ | ❌ | 571 |
| `op-pokt.nodies.app` | 10 | ❌ | ✅ | 572 |
| `optimism-rpc.publicnode.com` | 10 | ❌ | ✅ | 573 |
| `optimism.gateway.tenderly.co` | 10 | ❌ | ✅ | - |
| `1rpc.io/op`, `optimism.llamarpc.com`, `op.rpc.blxrbdn.com`, `optimism.api.onfinality.io/public`, `rpc.ankr.com/optimism` | - | ❌ | ❌ | - |

**Exactly one endpoint clears the bar.** The failure mode is nearly always archive depth: several
endpoints serve the tip happily and refuse block `0x1000000`, which is precisely the shape that makes
a nest look fine in a demo and die on a real backfill.

RFC-0030 §4 requires **two** qualifying endpoints, because round-robin failover across `rpc_urls` is
the only mitigation for a flaky host and a single-endpoint chain has none. Optimism does not clear
that, and the honest conclusion is that **Optimism is blocked - on operations, not engineering**.

Three ways forward, in preference order:

1. **Wait / re-probe.** Endpoint availability moves. `doctor` (RFC-0030 §4.2) makes re-checking a
   one-liner, and a scheduled run would tell us when a second appears.
2. **Register with one endpoint plus a loud entry comment**, accepting that a `mainnet.optimism.io`
   outage stalls every Optimism nest. Defensible only if paired with docs that say "bring your own
   node", and it weakens the bar the day after we wrote it.
3. **Require a user-supplied endpoint** - register the chain but ship `rpc_urls: &[]`, so `--rpc`
   becomes mandatory. This is the most honest option and is discussed as an alternative in §7.

This RFC recommends (1), and treats (3) as the fallback if demand arrives before a second endpoint
does.

### §2 Polygon: endpoints fine, semantics suspicious

| Endpoint | chainId | Archive | Batch > 3 | `finalized` lag |
|---|---|---|---|---|
| `polygon.drpc.org` | 137 | ✅ | ✅ | 3 |
| `polygon-bor-rpc.publicnode.com` | 137 | ✅ | ✅ | 2 |
| `polygon-rpc.com`, `poly-pokt.nodies.app`, `1rpc.io/matic` | - | ❌ | ❌ | - |

Two qualifying endpoints. The bar is cleared. Note `polygon.drpc.org` passes the batch check that its
mainnet and Arbitrum siblings fail - drpc's batch cap is evidently plan- and chain-dependent, which is
a good argument for probing per chain rather than reasoning about a provider's "policy".

The problem is the last column.

### §3 The finality question

#### 3.1 What was measured

Two independent endpoints, queried in the same second, agree exactly:

```
polygon.drpc.org                 latest 91360314   safe (none)   finalized 91360311
polygon-bor-rpc.publicnode.com   latest 91360314   safe (none)   finalized 91360311
```

A **3-block** finality lag. At ~2 s blocks that is roughly six seconds. For comparison, the same
probe against Optimism returns ~571 blocks (~19 minutes), which is what an L1-derived finality
signal should look like.

Also note `safe` returns nothing on either endpoint - so the `latest` / `safe` / `finalized` triad we
might otherwise lean on is only partly populated here.

#### 3.2 Why a 3-block lag deserves suspicion rather than celebration

Polygon PoS historically finalised via Heimdall checkpoints to Ethereum, on the order of tens of
minutes, and was notorious for **deep reorgs** - reorgs of over 100 blocks have been observed on that
chain. A 3-block finality claim is a very large change from that, and there are two possible readings:

1. **It is real.** Polygon's fast-finality work genuinely shortened it, and the tag means what it says.
2. **The tag means something weaker than we assume** - a Bor-local notion of head stability rather
   than checkpointed, L1-anchored irreversibility.

**This RFC does not resolve which.** That is the point: from outside, both produce identical JSON, and
the cost of guessing wrong is asymmetric to the point of being disqualifying.

#### 3.3 Why guessing wrong is unrecoverable

nuthatch's architecture (CLAUDE.md, non-negotiable 4 and the reorg strategy) is explicit: reorgs touch
**only** the mutable hot store; segments are sealed to Parquet strictly past finality and the columnar
layer is append-only and immutable. That design is sound *precisely because* the finality boundary is
assumed correct.

If we seal at `finalized` and Polygon reorgs deeper, we have written wrong data into immutable
storage. There is no rollback path - by design, and correctly so. "If a change requires mutating
sealed segments, the design is wrong" cuts both ways: it also means we must never put ourselves in a
position where mutating them is the only fix.

The failure is also **silent**. A too-shallow finality boundary produces no error, no fault for
RFC-0026 to quarantine, and no metric that moves. It produces a Parquet segment containing a
transaction that is no longer on the canonical chain, discovered - if ever - by someone reconciling
against another source months later. Determinism (non-negotiable 4) is exactly what would be lost.

#### 3.4 The bar RFC-0030 was missing

RFC-0030 §4 asks whether an endpoint *serves* a finality signal. That is necessary and not
sufficient - Polygon serves one on every qualifying endpoint and it is the *meaning* that is in
question.

**Proposed addition to the RFC-0030 §4 bar:**

> **5. Finality trust.** A chain may only use `Finality::FinalizedTag` when its finality mechanism is
> understood and the observed lag is consistent with it. Where the tag's semantics are unverified, or
> the observed lag is implausibly short for the chain's documented mechanism, the entry must use
> `Finality::Depth` with a conservative constant and record the reasoning. A chain's *observed* lag is
> evidence about the tag, never a justification for the seal depth.

Mainnet already sets this precedent for a different reason: it uses `Finality::Depth(64)` and the
comment says the `finalized` tag exists post-merge but `Depth` keeps a single conservative policy
until ExEx lands. Polygon should inherit that conservatism on stronger grounds.

#### 3.5 Proposed Polygon policy

Use `Finality::Depth`, not the tag, with a constant chosen against Polygon's *historical* worst case
rather than its current advertised one. A depth of **512 blocks** (~17 minutes at 2 s) sits
comfortably beyond observed deep reorgs while costing only a larger hot-store window - and on a
sparse-to-moderate chain that is cheap, which is the same trade the Arbitrum entry already makes
("Horizon is sparse, so the extra hot window is cheap").

If the fast-finality reading is later confirmed - ideally by observing actual reorg depths over a
sustained run rather than by reading documentation - the constant can be lowered, or the entry moved
to `FinalizedTag`. **Loosening later is safe; tightening later does not un-seal anything.**

### §4 Proposed entries

```rust
const POLYGON: Chain = Chain {
    name: "polygon",
    chain_id: 137,
    rpc_urls: &[
        // Measured 2026-08-03 (RFC-0031 §2): archive OK, batch > 3 OK on both.
        // NOT listed: polygon-rpc.com, poly-pokt.nodies.app, 1rpc.io/matic - no usable response.
        "https://polygon.drpc.org",
        "https://polygon-bor-rpc.publicnode.com",
    ],
    // DELIBERATELY NOT FinalizedTag. Both endpoints report `finalized` only 2-3 blocks behind
    // `latest`, which is implausibly short against Polygon PoS's historical checkpoint finality and
    // its record of >100-block reorgs. Sealing on that claim writes immutable Parquet we could never
    // correct (RFC-0031 §3.3). 512 blocks (~17 min at 2 s) until reorg depth is measured over a
    // sustained run. Lowering this later is safe; it cannot be tightened retroactively.
    finality: Finality::Depth(512),
    // ~2 s blocks and busy - same profile as Base, whose 1000 has held.
    log_window: 1000,
};
```

with `"polygon" | "matic" | "polygon-pos"` as `lookup()` aliases, appended to `all()` after `BASE`.

Optimism's entry is **not proposed here** - §1 blocks it. When a second endpoint qualifies it is a
copy of `BASE` with `name: "optimism"`, `chain_id: 10`, `finality: FinalizedTag { fallback_depth: 900 }`
(the measured ~571-block lag sits inside that), `log_window: 1000`, aliases `"optimism" | "op" |
"op-mainnet"`.

## Implementation

1. **Slice 1 - the §3.4 bar amendment.** Land the finality-trust criterion in RFC-0030 §4. Documentation
   only, but it gates slice 2 and it is the reusable output of this RFC.
2. **Slice 2 - Polygon.** The constant, aliases, `all()`, and a `lookup()` test mirroring the existing
   per-chain ones. Ships behind the conservative `Depth(512)`.
3. **Slice 3 - catalogue correction.** Fix the `uniswap-v3` entry in `nightswatchhq/nests`, which
   advertises Optimism and Polygon today. After slice 2, Polygon becomes true and Optimism must be
   removed until §1 resolves. **This is user-visible and should not wait for slices 1-2.**
4. **Slice 4 - reorg observation.** Run a Polygon cursor against the tip for a sustained period and
   record observed reorg depths. This is what would justify revisiting `Depth(512)` - and it is the
   only thing that should.
5. **Slice 5 (blocked) - Optimism.** On a second qualifying endpoint, or an explicit decision to
   take §1 option 2 or 3.

Slices 1-3 are independent of 4-5 and can land together.

## Testing

- **Unit:** `lookup()` resolves every Polygon alias to chain 137; `all()` includes it; the existing
  per-chain assertions extended.
- **Endpoint conformance:** `doctor` (RFC-0030 §4.2) over both Polygon endpoints, re-runnable. Not a
  CI gate - it is network-dependent - but a scheduled job that opens an issue on regression, per
  RFC-0030 §8.
- **Reorg property tests:** the existing property tests (random reorg depths must converge to
  canonical state) already cover the hot store. What they do **not** cover is a reorg *deeper than the
  finality boundary*, because that is currently assumed impossible. Slice 4 should add a test that
  asserts the seal boundary is never crossed by a reorg of depth < `Depth(n)`, and that a deeper one
  fails **loudly** rather than silently sealing - turning §3.3's silent corruption into an RFC-0026
  terminal fault.
- **Live:** one real Polygon nest backfilled and tip-followed before the entry is called Implemented.
  RFC-0030 §8 leaves open whether a chain needs a nest to justify entry; for Polygon specifically,
  §3's uncertainty makes a live run the only way to close slice 4.

## Risks

- **The finality reading is wrong in the safe direction.** `Depth(512)` may be needlessly conservative
  if fast finality is real, costing hot-store window and delaying sealing. Cheap, and reversible.
- **The finality reading is wrong in the unsafe direction** - i.e. Polygon can still reorg deeper than
  512. Mitigated by the slice-4 loud-failure test; not fully eliminated without observation. This is
  the residual risk of the whole RFC and should be stated plainly to anyone running a Polygon nest.
- **Single-endpoint Optimism by drift.** If demand pushes us to §1 option 2, we will have breached the
  RFC-0030 bar within a week of setting it. Better to breach it *explicitly*, with the entry comment
  saying so, than to quietly relax the rule.
- **Endpoint rot** (inherited from RFC-0030 §8). Two Polygon endpoints is the minimum, not a margin.
- **Auto-detect cost.** Each entry adds a probe to `init` without `--chain`. Five chains is still fine;
  RFC-0030 §8 flags ~6 as where ordering or concurrency starts to matter.

## Alternatives

- **Trust Polygon's `finalized` tag.** Simplest, matches Arbitrum and Base, and gives much faster
  sealing. Rejected: §3.3's failure is silent, unrecoverable and strikes at determinism. The upside is
  latency; the downside is wrong data in immutable storage. Not a close call.
- **Register Optimism with one endpoint** (§1 option 2). Rejected as the default because it converts a
  measurable bar into a soft preference immediately after adopting it. Available as an explicit,
  documented exception if demand justifies it.
- **Register chains with no default endpoints** (§1 option 3), making `--rpc` mandatory. Genuinely
  attractive - it is honest, it never rots, and anyone indexing Optimism seriously has a node. It also
  breaks the "it just works" property of the existing three and makes auto-detect impossible for that
  chain, since probing needs an endpoint. Worth considering as a distinct *class* of registry entry -
  "supported, bring your own endpoint" - which would need its own RFC.
- **Arbitrary chain via `nuthatch.toml`** (RFC-0030 §9). Would cover both chains and every future
  request, at the cost of pushing the endpoint bar onto users. Still the right long-term escape hatch;
  still not a substitute for curated entries on chains people actually ask for.
- **Do nothing.** The counter is slice 3: we are *already* advertising both chains in the public nest
  catalogue. Doing nothing leaves that false, which is worse than not supporting them.

## Open questions

1. **What does Polygon's `finalized` tag actually mean?** The one question that most changes this RFC.
   Answerable from Polygon's own specs and from observed reorg behaviour - and the second is worth
   more than the first.
2. **Does a chain entry need a live nest before it is "Implemented"?** RFC-0030 §8 left this open.
   Polygon argues yes; Gnosis argued no. A per-chain judgement keyed on how much of the entry is
   *assumed* versus *measured* may be the answer.
3. **Should `Finality::Depth` constants carry a provenance note in the type**, rather than only a code
   comment? Four chains now encode a safety-critical constant justified entirely by a comment that
   nothing checks.
4. **Is there a cheap continuous reorg-depth monitor** worth running across all registered chains? It
   would turn every finality constant from an assumption into an observation, and would have answered
   question 1 without this RFC needing to speculate.
