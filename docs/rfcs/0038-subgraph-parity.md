# RFC-0038: Subgraph parity - any subgraph reproducible as a nest

**Status:** Draft (2026-08-19). Nothing built. Amends **0023 §3** (tier 3's shape). Depends on 0023
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

**Slice 0 - prove the key collision, or disprove it.** A test, before any feature. §4.

**Slice 1 - the tier-3 executor, fixed calls only.** Schedule `blocks_in` over each window, batch via
`resolve_at`, store results. Delete `refuse_unwired_calls`; the test named after it fails loudly and
says so, which is the design working as intended.

**Slice 2 - parameterised calls.** The `on`/`signature`/`args` surface, per-block dedupe by `CallKey`,
the volume bound and its refusal.

**Slice 3 - IPFS.** RFC-0037's own slices, verification first.

**Slice 4 - top-level calls from ordinary RPC.** Split `[extract] traces` into the node-gated and
node-independent halves.

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
