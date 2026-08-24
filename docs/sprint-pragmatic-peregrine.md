# Sprint: pragmatic-peregrine

Filed after ominous-owl closed its labelled set and **v2.7.1** shipped. **Four issues.**

## Definition of done

Every issue carrying the **`pragmatic-peregrine`** label is closed, and no open PR is for one of
them. That is four issues: #761, #765, #783, #741. Work discovered in flight is filed **unlabelled**.
Pulling it into scope needs a board reply.

## The theme

**The zero-setup path, and the published cost, must both be true.**

ominous-owl made `--rpc` the pool you named and `/ready` refuse a dead first poll. 2.7.1 is that
binary. What is still false on the *zero-setup* path, and on the *quoted bill*:

- the shipped mainnet and BSC defaults cannot backfill
- almost the entire metered bill is headers, many of them for logs we then throw away
- `benchmarks.md` still prices those headers below the rate we measured, and a **$1,192** figure
  rests on it
- the house rule that every published number traces to a `bench-report.json` is unenforced

Owl made the flag honest. This sprint makes the defaults and the dollar figure honest.

Freeze-legal throughout: bug, performance, documentation, a gate. Not RFC-0040.

## The four

### 1. #761 - mainnet and BSC as shipped cannot backfill

**The front door.** Two of three mainnet defaults are dead (`eth-pokt.nodies.app` 403, onfinality
transport-dead; only `eth.drpc.org` answers). BSC's only default refuses archive *and* address-less
`getLogs`, which is the RFC-0009 factory flip. A `uniswap-v2` backfill on the shipped list does not
finish. Same class as Polygon #679.

`nuthatch doctor --rpc` already reports this when pointed at an endpoint. It is the *defaults* that
are stale.

**Acceptance**

1. Re-probe every shipped URL in `chains.rs` for mainnet and BSC the way #679 did.
2. Drop what no longer serves. A chain that would then have no working keyless archive endpoint
   says so in `chains.rs`, rather than shipping a failover list of one live host.
3. The BSC address-less `getLogs` refusal is either gone (a default that allows the flip) or named
   at load for a factory nest, not discovered mid-backfill as `-32701 Please specify an address`.
4. A from-deployment backfill of a quiet-enough mainnet contract against the *shipped* list either
   makes progress, or refuses at start with the reason. It does not cycle retries across corpses.

### 2. #765 - headers are 99.5% of the bill, and most of them stamp logs we discard

**The cost term.** A counting proxy in front of Alchemy, `uniswap-v3` on Arbitrum, 171,509-block
catch-up: 61,709 `eth_getBlockByNumber` (20 CU) vs 110 `eth_getLogs` vs 24 `eth_call`. Headers are
the bill. After the topic0 flip we fetch timestamps for every topic0 match on the chain, then
discard those whose address is not a known child. 1,627 surviving event-bearing blocks, ~200,000
headers.

Stamp only blocks that survive local filtering. Tractable: `decode_window` already knows the
survivors before the stamp.

This is **not** RFC-0040. 0040 is "follow tip less hard." This is "stop paying for rows we throw
away."

**Acceptance**

1. After a topic0-only fetch, `block_timestamp` is requested only for blocks that survive the
   nest's address filter (known contracts plus discovered children).
2. A factory-shaped fixture (topic0 hits on foreign addresses) fetches fewer headers than survivor
   blocks would not justify; deleting the filter-before-stamp fails a test.
3. A live `uniswap-v3` Arbitrum catch-up reports header count vs surviving event-bearing blocks.
   The ratio is the measurement, not a target invented here.

### 3. #783 - the published $1,192 rests on a header CU we did not measure

**The quoted figure.** `operators.md` and #765 agree `eth_getBlockByNumber` is 20 CU. `benchmarks.md`
prices a BSC backfill at ~3.0M CU total while 180k event-bearing blocks at 20 CU are 3.60M CU for
headers *alone*. The **$1,192** on that page is built on the inconsistent rate.

**Acceptance**

1. Every CU figure in `docs/benchmarks.md` uses 20 CU for `eth_getBlockByNumber`, or says why a
   different rate was used and names the source.
2. The $1,192 is recomputed from that rate or withdrawn with a sentence saying it cannot be stood
   behind (the 2.7.0 posture). No silent leftover.
3. `operators.md` and `benchmarks.md` do not disagree about the header CU.

### 4. #741 - the bench-report house rule is unenforced

**The machine.** `docs/benchmarks.md` already says every published performance number traces to a
committed `bench-report.json`. Magpie existed because hand-typed 8.7x / 20x outlived the harness.
Nightjar built the tape. Nothing fails the build when a number has no artefact.

**Acceptance**

1. A CI check or test fails if `docs/benchmarks.md` cites a performance number with no matching
   `docs/bench/*.json` (date, provider, hardware, commit as the house rule already names).
2. Reintroducing a hand-typed multiplier of the #722 shape fails that check.
3. Existing citations that cannot be traced are either given an artefact or removed in the same
   PR as the gate; the gate does not ship green over a page it cannot defend.

## Explicitly not in this sprint

- **RFC-0040**, the freshness dial. Proposed, design only, freeze. Do not start it because #765
  is about headers.
- **#750**, the Lodestar VPS still on 2.5.0. Ops. The board can swap 2.7.1 on that box; it is not
  a labelled issue.
- **#649**, Lodestar curator/indexer counts. Different nest, and not reconstruction.
- **#289**, DuckDB `allowed_directories`. Real, security, freeze-legal. Next-but-one, not this
  theme.
- **#760**, the `[[calls]]` volume bound recorded as shipped and never built. Capability. Park.
- **#790 / #789**, eval lockfile and a flake.
- **Anything labelled `parked`.**

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** A label is not approval to grow the set. Discovered work is filed
   unlabelled.
2. **`Reviewed-by:` names the party who read the diff.** No proxy signatures.
3. **Acceptance is above.** Build against it, do not rediscover it in review.

Also standing: one worktree per run; never `git add -A`; do not `@`-mention Rowan in GitHub
markdown; `CFLAGS=-std=gnu17` on the Linux box; one merge per CI cycle.

## Context at filing

v2.7.1 is what `curl | sh` installs. Owl's labelled set is closed. The three p0s above were already
open when 2.7.1 cut; they were not missed, they were next.
