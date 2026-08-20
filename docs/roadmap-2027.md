# The 2027 vision, and what the rest of 2026 is for

**Status: agreed 2026-08-20.** Direction doc, not an RFC slate. Companion to
[high-level-roadmap-jul-aug-2026.md](high-level-roadmap-jul-aug-2026.md), which covered the previous
window and is now historical record.

## The thesis

nuthatch's core deliverables are shipped, and then some. The original vision was a single static
binary that turns a contract address into a local SQL database in under two minutes, with no
mandatory third party. That exists. 2.6.0 closed the last capability gap anyone could point at:
pinned `eth_call` and verified IPFS resolution, so **any subgraph should now be reproducible as a
nuthatch nest**.

So the constraint has moved. It is no longer "can nuthatch do the thing". It is "is nuthatch good
enough, and known enough, that someone picks it over the tool they already use".

**For the rest of 2026 we build no new capability.** The work is bug fixes, security, performance,
maintenance and marketing, with one goal above the others: make the delightful core (RFC-0015) *truly
best in class*.

## Why now, and why this specifically

The week this was decided produced the argument for it. Every defect worth finding came from *running*
the product rather than extending it, and the most serious was not in an exotic corner:

- A single dropped connection killed an eight-hour backfill at 87.6% ([#651](https://github.com/nightswatchhq/nuthatch/issues/651)).
- A nest served old data under a new content address, silently ([#653](https://github.com/nightswatchhq/nuthatch/issues/653)).
- The **flagship first run on Ethereum mainnet indexes 15 events in 90 seconds**, and behind a
  provider that refuses over-wide ranges without saying why, it stalls permanently
  ([#672](https://github.com/nightswatchhq/nuthatch/issues/672)).

That last one had been true for months. Nobody noticed because every nest we operate passes an
explicit `--window` and runs on chains whose providers tolerate wide ranges. **We had never once run
the demo the way a stranger runs it.** A product whose pitch is "one command, on a cheap box" had a
broken front door while its backlog described further construction.

Building more on top of that would have been the wrong year.

## The five workstreams

Each names real, open, already-triaged work. This is a plan, not a wish list.

### 1. The delightful core, made best in class

The acceptance bar of RFC-0015 - *a stranger goes from an address to querying, delighted, in under
two minutes* - was written a month ago and **never measured**. The first time anyone ran it was the
week of this doc, and it failed.

- [#672](https://github.com/nightswatchhq/nuthatch/issues/672) the first run stalls behind a capped provider. The biggest single item here.
- [#676](https://github.com/nightswatchhq/nuthatch/issues/676) instrument the bar so it cannot rot again. Prerequisite for the above: four identical runs measured 2, 15, 28 and 198 events, so nothing could be evaluated at all until the measurement was made deterministic.
- [#674](https://github.com/nightswatchhq/nuthatch/issues/674) 24 subcommands, with scaled-mode ones ranking above `sql`. RFC-0015's own non-goal says the enterprise breadth must not be the front door.
- [#675](https://github.com/nightswatchhq/nuthatch/issues/675) the one blemish on `init`'s otherwise excellent output.

### 2. Correctness and bug fixes

Where most of the value has been. The pattern to keep: find them by running real nests against real
chains, and verify with a mutation rather than a green test.

- [#663](https://github.com/nightswatchhq/nuthatch/issues/663) a declared event that never fired takes a whole view down.
- [#656](https://github.com/nightswatchhq/nuthatch/issues/656), [#657](https://github.com/nightswatchhq/nuthatch/issues/657), [#671](https://github.com/nightswatchhq/nuthatch/issues/671) retry storms, the seal-direct refusal's cost, alias renames.
- [#649](https://github.com/nightswatchhq/nuthatch/issues/649) the Lodestar parity gaps, which are the best correctness harness we have: a real subgraph to disagree with, field by field.

### 3. Security

Small and specific, which is how it should stay.

- [#289](https://github.com/nightswatchhq/nuthatch/issues/289) DuckDB `allowed_directories` is not enforced on the build we bundle.
- The standing rules that already hold and must keep holding: a component with zero capabilities is
  deterministic by construction; an IPFS document's host is discarded so a log cannot choose what the
  indexer connects to; no phone-home.

### 4. Performance

Measured, not asserted. Benchmarks are CI artefacts and regressions fail the build - that rule exists
and wants enforcing rather than restating.

- [#295](https://github.com/nightswatchhq/nuthatch/issues/295) hold a persistent DuckDB connection instead of rebuilding the world per query.
- [#296](https://github.com/nightswatchhq/nuthatch/issues/296) a compact binary row format instead of JSON-string storage.
- [#282](https://github.com/nightswatchhq/nuthatch/issues/282), [#285](https://github.com/nightswatchhq/nuthatch/issues/285), [#286](https://github.com/nightswatchhq/nuthatch/issues/286), [#298](https://github.com/nightswatchhq/nuthatch/issues/298) tip lag, a published backfill number, the RAM budget under a hostile contract, the RFC-0004 perf set.

### 5. Maintenance and marketing

Including the thing this week proved is not optional: **shipped endpoints go stale**. Polygon shipped
one day and failed its own endpoint bar the next ([#679](https://github.com/nightswatchhq/nuthatch/issues/679)), because a recorded measurement is a snapshot presented as a property.

- Recurring endpoint probes rather than one-time gates.
- CI health: [#639](https://github.com/nightswatchhq/nuthatch/issues/639) disk, [#621](https://github.com/nightswatchhq/nuthatch/issues/621) fuzz budget, [#619](https://github.com/nightswatchhq/nuthatch/issues/619) a review gate that accepts the word "pending".
- Distribution: the nests catalogue and the port queue. Porting an unserved subgraph and posting it in
  that protocol's own channel is the cheapest credible marketing we have, because the artefact is the
  argument.

## Parked, not cancelled

A dozen open issues describe new capability: the revm state engine, trace and state-diff extraction,
ExEx against a real reth node, DataFusion convergence, a Turso hot store, tier-4 hosted call cache,
wildcard-address decode, the OBIB cases, whole-derivation reuse.

None of these are bad ideas and none are repudiated. They are **parked for 2026** and want a label
saying so, because an open issue reads as an invitation and these currently point contributors and
agents at construction the project is not doing this year.

The `CLAUDE.md` out-of-scope list is unchanged and still binds: no hosted service, no token, no
non-EVM before EVM is airtight, no TEE or zk, no Kubernetes.

## What 2027 looks like if this works

Nothing here is a 2027 commitment. It is what the year above is *for*:

- **The demo is the pitch.** A stranger runs three commands on a mainnet contract and is querying it
  in under two minutes, measured and defended by a number in CI rather than by an assertion in a
  README.
- **Ports, not promises.** Enough real subgraphs reproduced as nests that "any subgraph should be
  reproducible" is a catalogue rather than a claim, with the divergences published where they exist.
- **Boring in production.** A nest that survives a bad provider minute, a config change, and an
  upgrade without an operator learning anything new.

Then, and only then, the parked capability list becomes interesting again - because the thing it would
be built on top of will be worth building on.
