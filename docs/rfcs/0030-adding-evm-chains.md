# RFC-0030: Adding EVM chains - the registry, the endpoint bar, and Gnosis first

- Status: **Draft** (2026-08-03)
- Author: Pete (cargopete)
- Date: 2026-08-03
- Depends on: RFC-0004 §2 (the adaptive chunker a new chain's `log_window` seeds), RFC-0028 (the
  cap-error taxonomy the endpoint probe must reuse), RFC-0021 (the multichain roost, which multiplies
  the value of every chain added here).
- Blocks: nothing structurally; unblocks a class of migration requests we currently refuse.
- Nature: **mini-RFC**. Mostly a *process* proposal - the code change is ~30 lines and is not the
  interesting part.
- Origin: a Graph Discord thread (2026-08-01/02). A subgraph developer with a stalled Arbitrum
  deployment mentioned running a second subgraph on Gnosis. We shipped them
  [`poa-nest`](https://github.com/nightswatchhq/poa-nest) for Arbitrum in a day; we could not have
  helped at all on Gnosis, because nuthatch does not know the chain exists.

## 1. The gap

`src/chains.rs` registers exactly three chains:

```rust
pub fn all() -> &'static [&'static Chain] {
    &[&MAINNET, &ARBITRUM_ONE, &BASE]
}
```

Everything downstream reads from that list. `lookup()` resolves `--chain`; `project.rs:1236` walks
`all()` to auto-detect which chain a contract lives on when `--chain` is omitted;
`indexer.rs:1490` and `bench.rs:139` take finality policy and `log_window` from the entry. A chain
absent from this array is not merely undocumented - it is unindexable and undiscoverable.

That is a defensible scope decision (CLAUDE.md: *non-EVM chains before EVM is airtight*), but Gnosis,
Optimism, Polygon and friends are not non-EVM. They are the same decode path, the same reorg model,
the same everything - gated behind a three-element array.

The concrete cost today: when someone arrives with a stuck subgraph, our answer is "yes, in a day"
or "no, not that chain", and which one they get is decided by a constant.

## 2. What a chain entry actually costs

Three touchpoints, all mechanical:

| Change | Location | Size |
|---|---|---|
| `const GNOSIS: Chain { name, chain_id, rpc_urls, finality, log_window }` | `chains.rs` | ~20 lines |
| A `lookup()` match arm with aliases | `chains.rs:114` | 1 line |
| Append `&GNOSIS` to `all()` | `chains.rs:127` | 1 line |

Auto-detect, `init`, `add`, `bench`, roost cursors and the multichain path (RFC-0021) all pick it up
for free, because they read the registry rather than hard-coding chains. The architecture is already
right. **This RFC is therefore not about the code.**

## 3. The actual hard part: the endpoints

The `rpc_urls` list is where a chain entry is won or lost, and it is the one field that cannot be
derived from a chain spec. RFC-0028 established that endpoints lie about their limits in ways only
measurement reveals; the module note in `chains.rs` already concedes these lists have an expiry date
and records two endpoints removed on 2026-07-31 after they started refusing archive requests.

Building `poa-nest` re-taught this at some cost. A usable endpoint for that nest had to serve
**three independent capabilities**, and the free Arbitrum endpoints each failed a different one:

| Endpoint | Wide `getLogs` | Batch > 3 | Verdict |
|---|---|---|---|
| `arb1.arbitrum.io` | yes | yes | usable; rate-limits into partial timestamp responses near tip |
| `arbitrum-one.public.blastapi.io` | capped | yes | timestamps only |
| `1rpc.io/arb` | 50-block cap | yes | unusable for backfill |
| `arbitrum.drpc.org` | - | **capped at 3** | timestamps never complete |
| `arb-pokt.nodies.app` | 403 | - | unusable |

Note `arbitrum.drpc.org` and `arb-pokt.nodies.app` are **currently shipped defaults** for
`ARBITRUM_ONE`. The list is already partly stale. That is not an indictment of whoever wrote it - it
is the nature of free endpoints, and precisely why this RFC proposes a repeatable probe rather than a
better-curated constant.

The batch-size failure is worth singling out because it interacts badly with existing adaptation:
the block-timestamp fetcher batches JSON-RPC requests, and drpc's free plan caps a batch at 3. The
RFC-0029 narrowing shrank the block window `781 → 234 → 220 → 218 → 218 …` and stalled, because the
limit was on **batch count**, not payload size. Narrowing a block range cannot fix a request-count
cap. A new chain whose default endpoints have that property would look simply "slow".

## 4. The endpoint bar

A chain may enter `all()` only when **at least two** of its default `rpc_urls` independently satisfy,
by measurement recorded in the RFC or commit message:

1. **Archive depth** - serves `eth_getBlockByNumber` at the chain's early history, not just recent
   blocks. (This is what killed the two endpoints removed on 2026-07-31.)
2. **`eth_getLogs` width** - a documented maximum block span, measured, with `log_window` set at or
   below it.
3. **JSON-RPC batch size > 3** - enough for the timestamp fetcher to make progress.
4. **Finality signal** - serves the `finalized` block tag, or the entry declares `Finality::Depth`
   with a justified constant instead.

Two endpoints, not one, because a single flaky host stalls a run and the round-robin failover in
`Chain::rpc_urls` is the mitigation.

### 4.1 A probe must not conflate cap classes

Measuring §4.2 naively gives wrong answers. When probing `getLogs` width, a failure can mean either
*the block range is too wide* or *the result set is too large* - RFC-0028's `is_result_too_large`
exists precisely because these are different faults with different remedies. A probe that picks a
busy contract measures result-size caps and reports them as range caps.

This bit the Gnosis measurement below: probing with WXDAI (a very busy token) produced failures at
every width on two endpoints, which is *not* evidence of a narrow range cap. The probe must use a
**sparse** address, or an address-free range small enough to be result-bounded, and must classify the
error via the RFC-0028 marker list before recording a width.

### 4.2 Proposal: `nuthatch doctor --rpc <url>`

A small subcommand that runs the four checks and prints a verdict plus the largest safe `--window`:

```
$ nuthatch doctor --rpc https://rpc.gnosischain.com
chain      100 (gnosis)          tip 47,531,954
archive    OK      (block 20,000,000 served)
getLogs    OK      max range 10,000 blocks   [range cap, not result cap]
batch      OK      5/5 responses
finalized  OK      lag 12 blocks
verdict    usable  suggested --window 5000
```

This turns the bisection I did by hand for `poa-nest` into one command, serves operators pointing at
their own node, and is the mechanism by which §4's bar is enforced rather than merely asserted. It is
also the honest place to encode §4.1, so the classification logic lives in one tested spot.

## 5. Gnosis as the first instance

Measured 2026-08-03 (raw JSON-RPC, no nuthatch involved):

| Endpoint | chainId | `finalized` | batch 5 | archive | `getLogs` |
|---|---|---|---|---|---|
| `rpc.gnosischain.com` | 100 | OK | OK | OK | ≥10,000 |
| `rpc.gnosis.gateway.fm` | 100 | OK | OK | OK | ≥10,000 |
| `gnosis-rpc.publicnode.com` | 100 | OK | OK | OK | unmeasured - see §4.1 |
| `gnosis.drpc.org` | 100 | OK | **NO** | OK | unmeasured - see §4.1 |
| `gnosis-pokt.nodies.app` | - | NO | NO | - | no response |
| `rpc.ankr.com/gnosis` | - | NO | NO | - | no response |

The first two clear the §4 bar on all four criteria, which satisfies the two-endpoint rule. The
`unmeasured` entries are deliberate: those probes used WXDAI and failed at every width, which under
§4.1 is not evidence of a range cap. They are not recorded as failures and should be re-probed with
a sparse address before being ranked.

Proposed entry:

```rust
const GNOSIS: Chain = Chain {
    name: "gnosis",
    chain_id: 100,
    rpc_urls: &[
        // Measured 2026-08-03 (RFC-0030 §5): archive OK, batch>3 OK, `finalized` OK,
        // eth_getLogs >= 10k blocks. Re-measure before trusting - see the module note.
        "https://rpc.gnosischain.com",
        "https://rpc.gnosis.gateway.fm",
        // NOT listed: gnosis.drpc.org (batch capped at 3 - the timestamp fetcher cannot
        // progress, and RFC-0029 narrowing cannot fix a request-count cap);
        // gnosis-pokt.nodies.app and rpc.ankr.com/gnosis (no usable response).
    ],
    // Gnosis is PoS with real finality and serves the `finalized` tag on every endpoint above.
    // Fallback ~30 min at ~5 s blocks.
    finality: Finality::FinalizedTag { fallback_depth: 360 },
    // ~5 s blocks, moderate density - between Base (2 s, 1000) and Arbitrum (250 ms, 2000),
    // and comfortably under the measured 10k endpoint cap. The adaptive chunker tunes from here.
    log_window: 500,
};
```

with `"gnosis" | "xdai" | "gnosis-chain"` as `lookup()` aliases.

**Deliberately not claimed:** that this makes any particular Gnosis subgraph portable. We do not have
a Gnosis manifest in hand. The developer who prompted this posted a hash they described as their
Gnosis subgraph; it declares `arbitrum-one` 37 times and is in fact a newer revision of their stuck
Arbitrum one. By their own account the Gnosis deployment *is not stuck*. So Gnosis is the right first
instance because it is measurable and cheap, **not** because there is a specific rescue waiting.

## 6. Goals / Non-goals

**Goals.** A stated bar for chain entry; a repeatable way to measure it (`doctor`); Gnosis registered
as the first chain to clear it; a template making the next chain a mechanical exercise.

**Non-goals.** Non-EVM chains (CLAUDE.md, unchanged). Per-chain forks of business logic - a chain
entry is data, and if a chain ever needs a `#[cfg]` in the decode or reorg path, that is a design
failure to escalate, not to implement. Endpoint SLAs: these are free endpoints, the lists rot, and
§4 is a bar for *entry*, not a promise of continued availability.

## 7. Slices

1. **`nuthatch doctor --rpc`** - the four probes, RFC-0028-aware error classification (§4.1), and the
   suggested-window output. Independently useful for operators on their own node, and useful today
   for the three existing chains.
2. **Gnosis entry** - the constant, the alias arm, `all()`, plus a `lookup()` test mirroring the
   existing per-chain tests.
3. **Re-measure the incumbents** - run `doctor` over all shipped `rpc_urls` and prune what no longer
   clears the bar. `arbitrum.drpc.org` and `arb-pokt.nodies.app` are already known-bad from §3 and
   are still shipped defaults.
4. *(Optional, follow-on)* Optimism and Polygon by the same template, if demand appears.

Slice 3 is the one with immediate user-visible value: the defaults are stale **now**, on chains we
already support, and every new nest pays for it during its first backfill.

## 8. Risks and open questions

- **Endpoint rot is structural.** This RFC does not fix it, it makes it measurable. Whether `doctor`
  should run in CI against shipped defaults - a network-dependent test, normally something we avoid -
  is open. A weekly scheduled job that opens an issue on regression is the likeliest compromise.
- **`log_window: 500` for Gnosis is an estimate**, interpolated between Base and Arbitrum and bounded
  by the measured 10k cap. It is a seed for the adaptive chunker, not a tuned value; the first real
  Gnosis nest should report what it converged to.
- **Auto-detect cost grows linearly.** `init` without `--chain` probes every entry in `all()`. Three
  chains is free; twelve is a noticeable wait on a cold cache. Ordering matters (the doc comment
  already says "L1 first, then the busiest L2s"), and beyond ~6 chains this likely wants concurrency
  or a short-circuit on first bytecode hit.
- **Does a chain need a nest to justify entry?** This RFC says no - a registered chain is a
  precondition for anyone building a nest, so requiring a nest first is circular. But it does mean
  chains can accumulate unexercised, and an entry nobody has run a real backfill against is weaker
  evidence than one that has. Marking `log_window` as unvalidated until a real nest runs is a
  possible middle ground.

## 9. Alternatives considered

- **Arbitrary chain via config** - let `nuthatch.toml` supply `chain_id` + `rpc_urls` + finality
  directly, no registry entry. Maximally flexible and genuinely tempting; rejected as the *primary*
  path because it pushes the §4 bar onto every user, who will discover their endpoint's batch cap
  the way I did. Worth revisiting as an escape hatch for private/dev chains once `doctor` exists to make
  the bar self-service.
- **Vendor a chain list** (chainlist.org and similar) - hundreds of chains, endpoint quality unknown
  and unmeasured, directly contrary to §4. It would let us claim breadth we cannot stand behind.
- **Do nothing** - defensible while the beachhead is Graph-infra on Arbitrum. The counter is that the
  marginal cost is ~30 lines plus one afternoon of measurement, and the current answer to a Gnosis
  user is "no" for reasons that have nothing to do with the difficulty of their chain.
