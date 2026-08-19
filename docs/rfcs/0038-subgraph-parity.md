# RFC-0038: Subgraph parity - any subgraph reproducible as a nest

**Status:** **Accepted, all five slices built** (2026-08-19), and **measured against the live network**
rather than only against fixtures - see §6b (343 Uniswap V3 swaps row-for-row identical to the gateway)
and §6c (219 pools discovered, 219 parameterised `eth_call`s each, no misses).

**What the title claims is narrower than it reads, and §6a says why.** Every *input* a subgraph indexes,
a nest can index. Every entity that is a pure function of those inputs is reproducible exactly.
Uniswap's `derivedETH` reads back its own prior output, so it is reproducible only as a fixed point -
a defensible number, and a different one. **By how much is still unmeasured.**

**Known gaps, none of them closed by this RFC:** internal calls (node-gated, RFC-0003), time-travel
queries, and `@fulltext`. Amends **0023 §3** (tier 3's shape). Depends on 0023
(tiers 1-2 shipped, tier 3 unwired), 0037 (IPFS resolution), 0009 (factory discovery), 0001 (decode).
Borrows the scoping argument from 0036. **Release-sized:** this is a programme with its own release
and its own test plan (§7), not a patch.

## 1. The claim

> **Anything a subgraph can index, a nest can index.**

Nuthatch being visibly *less capable* than the thing it replaces is a worse problem than being slower
than it. A team evaluating a port should never find a mapping we structurally cannot express.

The current launch copy says "events only, no `eth_call`, no IPFS". That sentence is wrong twice and
it has cost us: it understates what ships, and it misnames what is missing.

**What actually ships** (RFC-0023, Accepted): tier 1, the derive-first recipe library
(`src/recipes.rs`), four recipes proven e2e with **no `eth_call` at all** - `total_supply` as Σmints −
Σburns, per-address balances, holder count, Uniswap-V2 reserves as the latest `Sync`. Tier 2, the
immutable-metadata cache (`src/metadata.rs`). Factory and dynamic-contract discovery (RFC-0009).

**What is actually missing**, in order of what it buys:

| # | Gap | State |
|---|---|---|
| 1 | The tier-3 executor | Every part exists; nothing calls it (§2) |
| 2 | **Calls parameterised by indexed data** | **Not designed until this RFC.** The real gap (§3) |
| 3 | IPFS content resolution | Designed in RFC-0037, unbuilt |
| 4 | Top-level call handlers | A scoping error, probably cheap (§5) |

## 2. Gap 1: tier 3 is one wire

`src/calls.rs` has `CallKey` (content-addressed over `(chain_id, block, contract, calldata)` with a
`\x1f` separator so `("0xab","cd")` and `("0xabcd","")` cannot collide), `CallDecl::validate`,
`blocks_in` (anchored on absolute block numbers so a resumed backfill samples the *same* blocks), and
`resolve_at`. `src/rpc.rs` has `eth_call_at` and a self-recursive `eth_call_batch_at` that splits a
batch on failure. `src/config.rs` parses `[[calls]]` and validates it.

Nothing calls `resolve_at`. Issue #262 chose the honest interim: `Config::refuse_unwired_calls` rejects
a `[[calls]]` block at load rather than accepting it and silently producing nothing. Its doc comment
says *"Delete this the moment an executor exists - the test named after it will fail loudly and tell
you to."*

That is the whole of gap 1.

## 3. Gap 2: the declaration cannot say what a subgraph says

This is the finding this RFC exists for. From `CallDecl::calldata`:

> Hex calldata including the 4-byte selector. **Fixed arguments only**: an argument that varied per
> block would make the declaration non-reproducible from the config alone, and reproducibility from
> the declaration is the whole point.

So a declared call is **one fixed calldata, sampled every N blocks**. That expresses an oracle read or
an ungoverned global parameter. It cannot express what a subgraph mapping overwhelmingly does:

```ts
let c = ERC20.bind(event.address)
let bal = c.balanceOf(event.params.to)      // the argument comes from the event
let t0  = Pool.bind(pool).token0()          // the contract comes from the event
```

**Wiring the executor without this would still leave a nest unable to reproduce a typical subgraph.**
That is the "less than a subgraph" gap, and it is a design gap rather than a missing feature.

### The fix keeps every property the design cares about

A call parameterised by an indexed row is **still deterministic and still content-addressable**:

- The **source rows are deterministic** - they are decoded events, which is the founding thesis.
- The **block is the source row's own block**, so the read is pinned exactly as tier 3 requires.
  Nothing here ever reads `latest`.
- **`CallKey` is unchanged.** It is `(chain, block, contract, calldata)` and does not care where the
  calldata came from. Two operators running the same declaration over the same range still produce
  byte-identical results and shareable segments (tier 4), which was the reason for the fixed-argument
  rule and is not weakened by this.
- **Reproducibility from the config survives.** The config declares the *rule*; the indexed data
  supplies the arguments; the indexed data is reproducible from the config. Composition of
  deterministic things.
- **Purity survives.** The host issues the call and hands components data (RFC-0023 §3), so components
  stay zero-capability and may still feed entity derivation.

RFC-0024 half-anticipated this, listing among its dependencies that declared calls "must resolve
`event.params`/`event.address` for dynamically discovered children".

### Proposed shape

```toml
# Unchanged: the fixed, sampled call. Still valid, still the right tool for an oracle.
[[calls]]
name    = "eth_usd"
contract = "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"
calldata = "0xfeaf968c"
every    = 1000

# New: parameterised by an indexed table. One call per source row.
[[calls]]
name      = "pool_token0"
on        = "factory__pool_created"   # the source table
contract  = "{pool}"                  # a column of that table, or a literal address
signature = "token0()"                # ABI signature; calldata is encoded from it
args      = []

[[calls]]
name      = "recipient_balance"
on        = "usdc__transfer"
contract  = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
signature = "balanceOf(address)"
args      = ["{to}"]                  # resolved per row
```

`on` and `every` are mutually exclusive: a declaration is either sampled or row-driven, and accepting
both would leave the sampling ambiguous. Refuse at load, per this repo's habit.

### Volume is the danger, and it needs a declared bound

One call per `Transfer` on a busy ERC-20 is millions of `eth_call`s. That is not a footgun to document,
it is a refusal to build: an unbounded parameterised declaration must fail at load the way
`Extract::scope_check` refuses an unscoped extraction nest, with `--i-mean-it`-style opt-in and a
`max_calls` bound. **Deduplication is the other half:** `CallKey` is a content address, so N rows in a
block asking the same question are one call and one stored row. Dedupe before the RPC, not after.

## 4. What this exposes: row identity for rows that are not logs

`Store::entity_key` is `format!("{block:012}-{log_index:06}")`. It assumes every row descends from a
log. Two kinds already do not:

- **Block rows** (RFC-0036) are one per block and are written with `log_index: 0`.
- **Call results** have no log index at all, and a parameterised call may produce several in one block.

**Suspected defect, not yet proven.** Nothing in `config.rs` refuses a nest that sets `[extract]
blocks` *and* indexes a contract, `to_store` is a plain `Vec<(String, String)>` committed as-is, and
block rows and log rows are both pushed with `Store::entity_key`. A block whose contract emitted a log
at index 0 would therefore produce two entries under one key, and the later write wins. **Slice 0 is a
test that proves or disproves this before anything else is built** - if it is real it is a live data
loss bug independent of this RFC, and if it is not, the reason it is not is the constraint the call-row
key scheme must respect.

Three row kinds sharing a two-field key needs a decision either way. Options, unranked: widen the key
with a row-kind discriminator; give call results their own store table and seal path; reserve a
high-`log_index` band (rejected on sight unless the collision proof shows it is safe - "unreachable at
real gas limits" is the sort of reasoning that ages badly).

## 5. Gaps 3 and 4

**IPFS** is RFC-0037, already written, with a named first customer: Lodestar's `subgraph-names` and
`subgraph-search` routes cannot leave the gateway without it, because subgraph display names live in
IPFS-pinned JSON behind the GNS.

**Top-level call handlers.** `src/calldata.rs` already decodes calls by 4-byte selector, is pure, and
is tested. `[extract] traces` bundles **top-level and internal calls together** behind the node gate,
and the `Extract::traces` doc says so: "Emit a row per call (top-level and internal)".

That is the same bundling-by-shape error RFC-0036 corrected for blocks and transactions. Internal calls
genuinely need `debug_*` and a colocated node. **Top-level calls do not** - they are transactions to
the contract, available from `eth_getBlockByNumber(block, true)`, which is ordinary RPC and which
RFC-0036 already established as an acceptable source. A subgraph's `callHandlers` fire on top-level
calls. Splitting the gate is likely a slice, not a project, and the decoder is already built.

## 6. Slices

Each ends runnable. Nothing merges while an earlier slice has a failing test.

**Slice 0 - prove the key collision, or disprove it. DONE (#642).** It was real, and worse than
suspected: block rows are written *second*, so the block row always won and **every log at index 0 was
silently destroyed** - the first event of every block. Fixed with a reserved index band
(`BLOCK_ROW_LOG_INDEX`, `CALL_ROW_LOG_INDEX_BASE`) rather than the load-time refusal tried first, which
broke three existing tests and so turned out to be a regression rather than a stopgap.

The first attempt at the test **passed**, because it drove `backfill_direct` - the seal-direct path
buffers `(block, json)` into append-only Parquet and cannot collide. A green test against the wrong
path would have closed this as "no bug".

**Slice 1 - the tier-3 executor, fixed calls only. DONE (#262).** The watcher test fired on its own
the moment `resolve_at` gained a caller and said to delete the refusal, exactly as written. The
archive endpoint rides on `Config` as `#[serde(skip)] state_rpc_urls` from a new `--state-rpc`, which
keeps a credential out of the nest's content address by construction. `--seal-direct` with declared
calls is refused, because tier 3 is wired into `process_window` only and a seal-direct run would have
sealed the range with the table silently absent - #262's own shape, guarded before shipping it.

**Slice 2 - parameterised calls. DONE.** `on`/`signature`/`args`/`contract_column`, literals beside
column references, dedupe by `CallKey` **before** the RPC, and the volume bound as a loud refusal.

Two things the build changed about the design. The reserved band had to widen from 1,000 slots to
**500,000**, because a row-driven declaration fires once per source row and one dense block can want
thousands - a narrow band would have recreated the very bug slice 0 fixed. And an **indexed dynamic
parameter is refused as an argument**: the log holds `keccak(value)`, so the original is unrecoverable,
and encoding the hash would produce a well-formed call asking a question nobody meant.

**Slice 3 - IPFS. DONE**, and it was larger than RFC-0037 assumed. A v0 CID hashes the **dag-pb
node**, not the bytes a gateway returns, so `sha256(what came back)` never matches. Fetching raw
blocks would have meant three code paths for three gateway shapes; instead `crate::cid` **re-encodes**
the returned bytes into UnixFS/dag-pb framing and compares, which needs no gateway cooperation. base58,
base32, varint and two protobuf messages are hand-rolled, because no such crate is in the tree and
`deny.toml` makes every dependency a decision.

`[[ipfs]]` declares `on` + `cid_column`; documents are deduped by CID before any fetch, verified
before storage, and stored in their own slice of the reserved band. **Failure is absence, not error:**
an unresolved document has no row, which is exactly what the `LEFT JOIN` design expects, so a gateway
that will not answer leaves a hole rather than failing the nest.

**Stated limit.** Resolution runs inline in the window under a 64-fetch budget, not out of band behind
the cursor as §3 of RFC-0037 asks. That wants a queue and a worker and is the follow-up; the budget is
what keeps tip-following from waiting on a gateway indefinitely meanwhile. Documents over 256 KiB are
multi-block and cannot be verified by re-encoding: they are accepted with `verified = false` and a
loud warning, never silently and never as if proven.

**The bug found while building it.** Fetching one real manifest CID, ipfs.io answered `Unable to
retrieve content within timeout period…` and Pinata answered `The request timed out searching for a
file on the non-pinata IPFS network`, both HTTP 200; only The Graph's gateway served the document.
Two of three would have been vendored as a nest's ABI, because the only check was non-empty. Both
strings are now test fixtures.

**Slice 4 - top-level calls from ordinary RPC. DONE.** `[extract] top_level_calls`, sourced from
`eth_getBlockByNumber(b, true)` via a new `Source::block_bodies` that shares `block_headers`' pacing
and partial-response handling rather than copying it. `Extract::decodes_calls()` splits the
call-decode gate from `enabled()`, which stays the node-gated set - so a top-level-calls nest builds
and runs with no node at all.

This also **closed a gap `CallContext::call_index` had recorded** as "deliberately left for the
extraction slice": call ordinals and log indexes shared one key namespace. #642 proved that defect was
live for block rows; the reserved band now carries all three kinds, split
`500_000..=749_999` for pinned reads, `750_000..=999_998` for calls, and `999_999` for the block row.

**A mutation that did not bite changed the test.** The first fixture set `[extract] contracts`, so
`CallRegistry::in_scope` did the filtering and the test passed with the indexer's own address filter
deleted - it was asserting somebody else's guard. Unscoped, the nest's own addresses are the only
bound, and that is the one that matters: `scope_check` guards `traces`/`state` and returns early for a
top-level-calls nest, so without the filter an unscoped nest would decode every transaction on the
chain.

## 6a. The finding that bounds the claim: Uniswap's entity model is path-dependent

Every capability in §6 is built, but capability is not parity. The top 25 subgraphs by query fee are
**eleven Uniswap deployments**, so whether their *entities* are reproducible decides whether "port the
top 25" is grind or is impossible. Read against `Uniswap/v3-subgraph`'s `src/common/pricing.ts` rather
than from memory:

**`getEthPriceInUSD()` is expressible.** It loads one pool and returns `token1Price`, which derives
from `sqrtPriceX96` - a field carried in the `Initialize` and `Swap` events. A view over the latest
swap on the reference pool reproduces it exactly.

**`findEthPerToken()` is not, and the reason is structural.** It iterates `token.whitelistPools` and,
for each, reads **`token1.derivedETH` - the previously *stored* value of another token's price**,
alongside `pool.totalValueLockedToken1` and `bundle.ethPriceUSD`, all stored entity state:

```ts
const ethLocked = pool.totalValueLockedToken1.times(token1.derivedETH)
if (ethLocked.gt(largestLiquidityETH) && ethLocked.gt(MINIMUM_NATIVE_LOCKED)) {
  priceSoFar = pool.token1Price.times(token1.derivedETH as BigDecimal)
}
```

So a token's price is a function of *when the other token was last written*, not of the event log
alone. Two indexers replaying the same events in a different handler order can legitimately produce
different numbers. **It is order-dependent by construction.**

### What that costs, precisely

| Surface | Reproducible? |
|---|---|
| Raw event tables | **Exactly** |
| Pool prices from `sqrtPriceX96` | **Exactly** - a pure function of the latest swap |
| Liquidity, TVL-per-token, volume | **Exactly** - sums over events |
| `derivedETH`, `ethPriceUSD` | **A fixed point, not the same number** |
| Anything downstream (`totalValueLockedUSD`, `volumeUSD`) | Inherits that difference |

A declarative view can solve the mutual recursion to convergence at each block. That is arguably a
*better* answer - it has no dependence on write order - but it is **not the subgraph's answer**, and a
parity diff would show it.

### The consequence for the claim

Byte-identical entity parity with Uniswap is **not achievable declaratively**, and not because the
view layer is weak: the source is order-dependent, and reproducing it exactly would mean replaying the
same stateful writes in the same sequence. That is imperative mapping execution, which §8 rules out on
purpose.

So the claim this RFC can support is narrower than its title, and should be stated that way:

> Every **input** a subgraph indexes, a nest can index. Every entity that is a *pure function of those
> inputs* is reproducible exactly. Entities whose definition reads back their own prior output are
> reproducible as a fixed point, which is a defensible number and a different one.

**How different is unmeasured.** The gap is bounded by how stale the stored values are - one block in
a busy pool, potentially much longer for a rarely-touched token. Measuring it needs the gateway diff
§7 calls for, which is still outstanding.

## 6b. The gateway diff, run (2026-08-19)

§7 sets the acceptance criterion: *a real port, diffed against the gateway. Anything less is us marking
our own homework.* Run against **Uniswap V3 on Arbitrum**, deliberately - eleven of the network's top
25 subgraphs by query fee are Uniswap, so it is the family that decides the question.

- **Subject:** the WETH/USDC 0.05% pool, `0xc6962004f452be9203591991d15f6b388e09e8d0`, the
  highest-volume V3 pool on Arbitrum by the subgraph's own ranking.
- **Range:** blocks 496,000,000 to 496,010,000, long final.
- **Both sides queried independently:** the subgraph through the decentralised gateway, the nest
  through `nuthatch sql` over its own store.

```
subgraph: 343 swaps    nuthatch: 343 swaps
ROW-FOR-ROW IDENTICAL across all 343 swaps
(block_number, sqrtPriceX96, tick) matched exactly, with multiplicity
```

Compared as a multiset rather than a sorted list, so a duplicated or dropped row cannot hide behind a
matching count. `sqrtPriceX96` and `tick` are raw integers on both sides; `amount0`/`amount1` are
deliberately excluded because the subgraph decimal-adjusts them, which is a formatting difference and
not a data one.

**What this settles.** The *input* layer is exactly reproducible against the most-queried subgraph on
the network, which is §6a's "Exactly" column measured rather than asserted - including
`sqrtPriceX96`, the field every Uniswap price derives from. The nest was scaffolded by
`nuthatch init <address> --chain arbitrum-one` and caught up in **8 seconds, 4,781 events, 600 ev/s**.

**What it does not settle.** §6a's other column. `derivedETH` and `ethPriceUSD` are order-dependent in
the subgraph's own implementation, so a fixed-point view will differ by an amount nobody has measured
yet. That diff needs the entity layer built first and remains the outstanding work.

## 6c. Uniswap V3, ported and run (2026-08-19)

The nest §6b diffed was one pool, scaffolded by hand. This is the **whole subgraph**, ported by
`nuthatch init --from-subgraph QmZ5uwhnws…` from the live Arbitrum deployment - the top-25 entry, and
the family that decides the question.

**The importer needed no help.** It read the manifest, vendored the factory and pool ABIs, and
**inferred the factory rule by itself**: `factory.PoolCreated → Pool via 'pool' (param 'pool' names the
template exactly)`. Five tables, matching exactly the four events the subgraph's manifest handles plus
`PoolCreated`. It also reported honestly that the pool template handles 4 of the 9 events its ABI
defines, which is the subgraph's choice, not a gap.

**Parameterised `eth_call`, proved on a real mapping's real need.** The subgraph's `fetchTokenSymbol`
and `fetchTokenDecimals` call `symbol()` and `decimals()` on each new pool's tokens. Declared as
RFC-0038 §3 intends - `contract_column` naming the address the row itself carries:

```toml
[[calls]]
name = "token0_symbol"
on = "factory__pool_created"
contract_column = "{token0}"
signature = "symbol()"
```

Run over blocks 495,500,000 to tip: **195,515 events in 3m37s (901 ev/s)**.

| Table | Rows |
|---|---:|
| `factory__pool_created` | **219** |
| `token0_symbol` | **219** |
| `token0_decimals` | **219** |
| `token1_symbol` | **219** |
| `pool__swap` | 6,210 |

**Exactly one call per pool, per declaration. No misses, no duplicates.** Factory discovery fed the
call scheduler, each call resolved at its own row's block against the address that row carried, and
the counts line up to the unit. That is the capability §3 was written for, and until now it had only
been proved against stubs I wrote myself.

**Still unproved:** the entity layer. 219 pools is discovery working, not discovery *at scale* - the
full factory history is 495 million blocks, not 674 thousand. And nothing here computes `derivedETH`,
so §6a's divergence remains unmeasured.

## 6d. §6a measured, part one: the root of the pricing tree is exact

§6a said Uniswap's entity model is order-dependent and a declarative fixed point would differ *by an
unmeasured amount*. Measuring it splits cleanly in two, because the pricing tree has a base case and a
recursion, and only one of them is order-dependent.

**The base case.** `getEthPriceInUSD()` loads exactly one pool - Arbitrum's
`STABLE_TOKEN_POOL = 0x17c14d2c…` (WETH/USDC) - and returns `token1Price`, since `token0` is the
reference token. `token1Price` comes from `sqrtPriceX96ToTokenPrices`, a pure function of the pool's
last `sqrtPriceX96`. Nothing stored, nothing recursive, no write order.

**Verified in three steps, 2026-08-19:**

1. The formula was checked *against the subgraph's own stored value* before being trusted:
   `(sqrtPrice² / 2¹⁹²) · 10^dec0 / 10^dec1` reproduced their `token0Price` and `token1Price` to a
   relative difference of **1.3 × 10⁻³⁵** - their `BigDecimal` precision, not an error.
2. The pool was indexed by a plain `nuthatch init 0x17c14d2c… --chain arbitrum-one` (164 events, 3s).
3. Both sides read **at the same block**, using the subgraph's own time-travel query to pin it:

| | value |
|---|---|
| nuthatch, latest `Swap` at or before block 496,213,467 | `sqrtPriceX96 = 3611630104467636802226442` |
| subgraph, `pool(block:{number:496213467}).sqrtPrice` | `3611630104467636802226442` |
| nuthatch `ethPriceUSD` | `2078.008699136272184458689456222861027…` |
| subgraph `bundle.ethPriceUSD` | `2078.008699136272184458689456222861` |

**Identical.** The difference is 10⁻³² absolute, which is where their decimal type stops and ours
does not.

### What this narrows

`ethPriceUSD` is the **root** every Uniswap USD figure hangs from, and it is exactly reproducible. So
§6a's divergence is not "somewhere in the pricing" - it is confined entirely to the **recursive**
`findEthPerToken` layer, which reads back other tokens' stored `derivedETH`.

That is a much better position than §6a assumed. The remaining measurement - how far a fixed-point
`derivedETH` drifts from an order-dependent one - needs the full factory history rather than a
674k-block window, and is the outstanding work.

## 6e. The seal-direct refusal, met in practice (2026-08-19)

Slice 1 refuses `--seal-direct` alongside declared `[[calls]]`, because tier 3 is wired into
`process_window` only and a seal-direct run would sail past every sampled block and seal the range with
the table silently absent. That guard is right and it fired on the first real nest to need it.

The cost is now measured rather than assumed. `graph-allocations-nest` grew a one-line `[[calls]]`
entry for GRT `totalSupply` - a single pinned read, sampled every 100,000 blocks - and the same
454M-block backfill went from **~12 minutes on seal-direct to ~66 minutes on the hot path**. Five and a
half times slower, for one number.

That is a bad trade and the guard is not the thing to change: silently missing rows would be far worse.
What it argues for is **teaching the seal-direct paths to resolve calls**, which is the follow-up the
slice already names. Until then the honest advice is that a nest wanting both a from-deployment
backfill and a pinned read pays for it in wall-clock, and should know that before starting rather than
after.

Worth noting the alternative was worse: deriving `totalSupply` from events means indexing every
`L2GraphToken` Transfer to sum mints minus burns - millions of rows for one scalar. The pinned read is
the right shape; only the backfill path is wrong.

## 7. Testing, because this warrants its own release

The correctness rules in [CLAUDE.md](../../CLAUDE.md) apply in full, and this repo has specific scars
worth naming in the plan rather than rediscovering:

- **Golden tests per declaration**: fixed block fixtures in, exact rows out.
- **The determinism property**: the same declaration over the same range, run twice, produces
  byte-identical `CallKey::address()` values. `calls.rs` already has the casing half of this; extend it
  to the parameterised case, where the risk is argument encoding rather than address casing.
- **A criterion must fail before we build to it.** Check each acceptance test goes red against today's
  tree first. Absence tests that pass with the mechanism removed have shipped here before.
- **Mutation-check every new gate**, and read the panic *line*, not the pass/fail count - two mutations
  dying on the same `expect` prove one thing twice.
- **The acceptance test is a real port.** Pick a subgraph that uses `eth_call` handlers, port it, and
  diff the entities against the gateway. Parity against the thing we claim parity with is the only test
  that settles the claim in the title. Anything less is us marking our own homework.
- **Volume, measured not assumed.** A parameterised declaration over a dense table is the performance
  risk; it gets a benchmark and a CI budget like every other write path.

## 8. Non-goals

- **Not arbitrary imperative mappings.** Components remain pure `batch → batch` functions with all
  state host-side. Parity means the *data* is reproducible, not that we run AssemblyScript.
- **Not `latest`.** Every read here is pinned to a block. `calls.rs` says it plainly and this RFC does
  not relax it.
- **Not retroactive re-decoding.** A better ABI or a newly resolvable CID produces new rows, never a
  rewrite of a sealed segment.
- **Not archive-free.** Tier 3 needs an operator-supplied archive RPC. Running it without one is
  RFC-0024, which stays deferred.

## 9. Open questions

1. **The row-identity decision** (§4), which slice 0 informs but does not settle.
2. **Does `args` need expressions, or only column references?** Column references cover the mapping
   patterns above. Anything more starts building a language, and this project deleted Starlark once
   already.
3. **What is the acceptance subgraph?** It should be one somebody actually runs, not a fixture we
   picked because it passes.
4. **Ordering within a block.** A parameterised call's result is logically "after" the row that caused
   it. If two declarations read the same contract at one block, `CallKey` dedupes them - but the
   *stored* ordering still needs to be deterministic, or two operators' segments diverge on row order
   while agreeing on content.
