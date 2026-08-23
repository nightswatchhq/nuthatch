# RFC-0040: The freshness dial - let an operator trade staleness for money

- Status: **Proposed - design only.** No implementation. Under the 2026 feature freeze this is a
  design to argue with, not work to start.
- Author: Jenny
- Date: 2026-08-23
- Depends on: RFC-0029 (the timestamp path this is mostly about), RFC-0028 (the window controller),
  RFC-0039 (the rig every number below came off).
- Origin: #750's production audit and the RFC-0039 rig's first measurements. The board's framing, and
  it is the right one: *nuthatch should be the fastest indexer and also a cheap one.*

## §0 - The problem, in one line

**Freshness is a dial and it is currently welded to maximum.** A nest follows tip as hard as it can,
pays for it continuously, and offers the operator no way to say *"an hour late is fine, spend less"*.

## §1 - What it actually costs, measured

From #750, on our own reference deployment, one week, four nests:

| | |
|---|---|
| RPC requests | **~11.8M** |
| HTTP requests served | **~100** |
| ratio | **~118,000 : 1** |
| `graph-staking-nest` alone | 3,954,332 RPC to serve ~39 requests |

Steady state for one Arbitrum nest is **~549,000 requests a day**. Arbitrum produces ~345,600 blocks
a day, and `block_timestamps` costs a header round trip per distinct block it needs, so the order of
magnitude is right and the dominant term is obvious.

### The term nobody had written down

RFC-0039's tape lets us compare what the code *asks for* against what actually goes over the wire.
On a 120-block USDC range, fixed 20-block window:

| | |
|---|---|
| `Source` calls the code made | **12** (6 `logs`, 6 `block_timestamps`) |
| HTTP requests those became | **84** |
| amplification | **7x** |

The 7x is **retry against a rate limit**. Five of six timestamp batches came back `429` from the
bundled public endpoints, each retried up to `TIMESTAMP_ATTEMPTS = 4` across a three-endpoint pool.

So the cost model has a feedback term: **being rate-limited makes you send more requests, which gets
you rate-limited harder.** An operator on a free tier does not pay the nominal bill, they pay roughly
seven times it, and the multiplier grows exactly when they can least afford it.

Any honest cost story has to name this. It is not a tuning parameter, it is a loop.

## §2 - Why this is not just "turn timestamps off"

`block_timestamps` defaults on because most useful questions are time-filtered, and #750 established
that both Lodestar panels genuinely need the column: one filters `WHERE ts > <seven days ago>`, the
other uses it as an entity's `createdAt`. Turning it off is not a cost knob, it is a capability
removal, and it silently breaks a class of query rather than making it slower.

The dial has to be **freshness**, not **completeness**. Same data, later, for less.

## §3 - The knobs, in the order I would argue for them

Each names what it costs the operator in staleness and what it saves. **None of these are proposed
for implementation this year**; the freeze holds.

**1. Poll interval.** The most obvious and the least interesting: ask for the tip less often. Cheap
to build, and the saving is roughly linear. An operator serving a daily dashboard does not need a
two-second cursor.

**2. Finality-only mode.** Do not follow tip at all. Wake on a schedule, index everything up to the
finality boundary, sleep. For a nest whose consumers are a weekly trend and a seven-day feed - which
describes both Lodestar panels exactly - the tip-following half of the bill buys nothing. This is the
big one, and it is a scheduling change rather than an indexing change.

**3. Timestamp granularity.** A header per event-bearing block is exact. Block times on a given chain
are near-constant, so interpolating between two known anchors is cheap and wrong by a bounded amount.
That is a real accuracy trade and it must be **explicit and stamped on the data**, not a silent
default - a row whose timestamp is interpolated should say so, because #784 is a fresh reminder of
what happens when a timestamp's provenance is invisible.

**4. Back off on the loop, not just the request.** The 7x above is a self-inflicted wound. A pool
that is being rate-limited should slow the *cursor*, not just retry the batch harder. This one is
arguably a bug fix rather than a knob, and it is the cheapest real saving on this list.

## §4 - What must not happen

- **No silent staleness.** If a nest is deliberately behind, `/ready` and the API must say so, in the
  same vocabulary `degraded` already uses for damaged cold data - a fact about the nest, not about
  the rows a caller happened to receive.
- **No fabricated values.** #784's lesson holds: a cheaper path may return *less*, never something
  that looks like data and is not.
- **Determinism survives.** Anything sealed must still be re-executable to the same content address.
  An interpolated timestamp that reaches a sealed segment changes the address, so either it is
  marked and excluded from sealing, or it is not interpolated.

## §5 - Why this matters more than it looks

Our pitch is *be your own indexer*, and the bill has never been on the page. #770 has just put the
steady-state number in the docs for the first time. The next honest step is to give an operator
something to do about it.

An indie dev with one contract and a free RPC key is the golden path. Today that path pays a
tip-following bill sized for a production feed, with a 7x rate-limit multiplier on top, whether or
not anybody reads the data more than once a day. **Fastest is a benchmark. Cheapest is whether
somebody can afford to keep it running.**
