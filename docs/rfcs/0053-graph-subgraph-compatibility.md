# RFC-0053: Graph-subgraph compatibility mode

*A partial read surface for a subgraph that has stopped: exact for its event-shaped fields, a named
refusal for the rest. Not a drop-in replacement - see the rescope note below and
[Measured outcome](#measured-outcome).*

**Status:** **PARKED 2026-09-12 by Chief.** No further slices are to be started. What shipped stays
shipped and supported - the compatibility surface is on main, tested, and documented in
[what it is and what it is not](../graph-compatibility-what-it-is.md). What is parked is the *programme*:
S3's entity history, S4's value contract decision, the S0 validator that was never built, and the
remaining coverage grind.

**The trigger to unpark**, so this is a decision rather than a drift: someone with a stopped subgraph
whose queries fall inside the event-shaped surface, asking for it. Field coverage is not the trigger -
that was the mistake this programme made for three weeks. A named consumer is.

Originally **accepted 2026-09-09 by Chief**, as the decision RFC-0044 §8 requires for new binary
capability. It **overrides [RFC-0044](0044-the-subgraph-port-skill.md) §11's** "not a subgraph
compatibility layer" non-goal, annotated there. Scope of the authorisation: S0 (#1264, the migration
validator) is in the `resolute-robin` sprint; S1 to S4 (#1265 to #1268) are filed but wait on what S0
measures, because S0 is the only slice that can falsify the rest.

**Date:** 2026-09-09

**Depends on:** RFC-0044, RFC-0041, RFC-0034, RFC-0047

**What it blocks:** Any claim that Nuthatch is a drop-in Graph Protocol subgraph endpoint without an explicit decision about its value contract.

> **Rescoped 2026-09-12, after building S1, S2 and the first slice of S3 and measuring them.** The
> deliverable is **not** a drop-in replacement and this RFC should not be read as promising one. It is a
> **partial read surface**: exact for the event-shaped part of a subgraph, a named refusal for the rest.
> What changed is the evidence, not anyone's mind - see [Measured outcome](#measured-outcome), which is
> the section to read before the design below.

## Abstract

Nuthatch can reproduce the structural and relational surface of a Graph Protocol subgraph with bounded work: generated GraphQL schema, singular and collection queries, filters, pagination, `_meta`, time-travel and `subgraphError` compatibility. The compiler is a Graph-dialect-to-SQL layer over Nuthatch's relational state, not a second indexer.

It cannot, by that route, reproduce every value a reference subgraph returns. Uniswap V4 pricing fields such as `derivedETH`, `Bundle.ethPriceUSD`, `volumeUSD` and `totalValueLockedUSD` are the output of ordered, stateful AssemblyScript handlers, not a declarative function of sealed chain facts. Exact parity requires replaying the original mappings with graph-node host semantics. Thus the endpoint can be a drop-in for shape and reproducible data, but not an automatic promise of byte-identical pricing analytics. Consumers adopt it by changing a GraphQL URL: The Graph gateway has no off-network endpoint registration mechanism.

> **2026-09-12.** "A drop-in for shape and reproducible data" was written before anything was built, and
> measurement has since shown it to be too generous in a way that matters. *Reproducible data* turned out
> to mean identity and structure - ids, timestamps, addresses, log indices - while every economic quantity
> falls outside it. And because GraphQL refuses a whole query for one unanswerable field, "drop-in for
> part of the surface" does not compose into a drop-in for any client whose queries touch the other part.
> [Measured outcome](#measured-outcome) has the figures.

## Measured outcome

*Added 2026-09-12. Everything above this line is the September 9th assessment; this is what building it
showed. Where the two disagree, this section is the record.*

### The numbers

Against the pinned Uniswap V4 deployment, with S1, S2 and `block.number_gte` shipped:

| | |
| --- | --- |
| fields the report classifies `exact` | 184 of 231 |
| of those, **answered** by the overlay | **70 (38%)** |
| call-derived, needing `eth_call` at index time | 24 |
| fixed point, ruled out by RFC-0038 §6a | 23 |

Carbon, the second pinned target, answers 41 of 93 (44%) and has **zero** fixed-point fields. The
subgraph's shape decides the outcome far more than the compiler's completeness does.

### The number that matters more, and why 38% overstates it

**GraphQL has no partial-answer mode.** A query naming one unanswerable field is refused entirely - the
client does not get the other fields, it gets an error. So field coverage is an upper bound on query
coverage and a loose one.

Worse, the answered and unanswered sets are not a random 38/62 split. They divide by *kind*:

- **answered**: ids, timestamps, log indices, addresses, token ids, raw `sqrtPriceX96`, constants that
  are zero. Identity and structure.
- **not answered**: `Token.symbol`, `name`, `decimals`; `Pool.volumeUSD`, `feesUSD`,
  `totalValueLockedUSD`, `token0Price`, `token1Price`, `liquidity`, `txCount`, `tick`, `feeTier`;
  `Swap.amount0`, `amount1`, `amountUSD`. Every economic quantity.
- **no view at all**: `PoolDayData`, `PoolHourData`, `TokenDayData`, `UniswapDayData`, `Bundle`,
  `PoolManager`, `Transaction`.

An ordinary consumer query - `pools(orderBy: totalValueLockedUSD) { totalValueLockedUSD volumeUSD
token0 { symbol } }` - names four fields and a nest answers none of them. For a Uniswap-shaped
analytics consumer the honest summary is that it currently answers **nothing they would ask for**.

### What this is actually good for

A consumer asking *"every Swap on pool X since block N, with sender, transaction hash and log index"*
gets an exact, complete answer today, over genuinely indexed data spanning the hot store and sealed
Parquet. That is bots, reconciliation, audit trails, backfills - anyone who wants the **event record**
rather than a dashboard. Carbon's shape says a good number of subgraphs are like that.

So the deliverable is real, and it is not the one the title implies.

### The ceiling, and which part of it is permanent

- **Architectural, will not move.** The priced family (`derivedETH`, `Bundle.ethPriceUSD`, `*USD`) needs
  the ordered pricing chain, which is a second indexer. RFC-0038 §6a refuses it and CLAUDE.md reaffirms
  it. These will never be *exact*. They could be *converged* - but see below.
- **A decision, not code.** S4 (#1268) asks whether a nest may serve a converged value, clearly marked.
  The definition of done already allows it - *"converged where it is converged"* - and **no converged
  lane has been built or specified**. Until that decision is taken the ceiling stands where it is.
- **Unblocked work.** `Token.symbol`, `name` and `decimals` are call-derived; `port-emit` already emits
  the `[[calls]]` stanzas and nothing has run them end to end. Token metadata appears in nearly every
  real query, which makes this the highest value per unit of effort remaining.
- **Grindable.** 114 exact fields still answer nothing: accumulator keys (#1313), `event.transaction.from`
  with no column to bind to (#1280), and mapping operations the emitter cannot render.

### What the acceptance criteria below are worth

The definition of done - *"change nothing but the URL, and its recorded query set answers"* - **cannot be
met for an analytics subgraph**, and could not have been at any point in this programme, because its
first bullet requires an unmodified client and an analytics client's every query names a refused field.
It remains the right standard for judging an *answer*: exact, converged, or named, and never a wrong
number presented as a right one. That half has held - there is no known path that returns a wrong
number. It is the wrong standard for judging *adoption*, and this RFC was using it for both.

## Motivation

RFC-0044 makes an imported subgraph useful as a Nuthatch nest, but deliberately does not make Nuthatch impersonate its GraphQL endpoint. A separate compatibility mode is worth considering where a client has a substantial body of existing GraphQL queries and availability of raw and structural data matters more than mapping-derived analytics.

The Uniswap V4 failure shape is the example. A mapping that returns early when token metadata cannot determine decimals may fail to create a Pool; a later handler loading that Pool then aborts deterministically and graph-node can halt the deployment. An event-first Nuthatch port does not run that abortable handler in its data path, and can still serve the decoded facts. This is an availability claim, not an assertion that its USD totals equal the reference mapping.

## Goals

*Status added 2026-09-12: **delivered**, **partial**, or **ruled out**.*

- **Delivered** (#1282). Generate a Graph-node-shaped schema from imported `schema.graphql`, including generated roots, filters, order enums, `_meta`, scalars and `@derivedFrom` traversal. Golden-tested against a recorded graph-node reference; pluralisation uses graph-node's own `to_plural`.
- **Partial** (#1266). Compiled for embedded DuckDB. **Postgres has no branch in `graph_query.rs` and nothing tests it.**
- **Delivered.** `first`, `skip`, `orderBy`, `orderDirection`, Graph `where` operators, `and`, `or`, nested entity filters and the default ID-ascending order. Ordering on a big number compares numerically rather than as text (#1325). `orderBy` on a relation is refused by name.
- **Partial** (#1267). `block: { number_gte: N }` is answered as the head precondition it is, needing no history at all (#1330). `{ number: N }` and `{ hash: … }` remain refused: the block-ranged entity history is not built.
- **Not built.** The side-by-side migration validator (S0, #1264) was never written. Every coverage figure in this RFC comes from `port-emit`'s own accounting instead, which measures what the overlay answers rather than how it differs from a live reference.

## Non-goals

- Claiming byte-for-byte parity for stateful, mapping-derived values without replaying mappings.
- Running AssemblyScript mappings in the ordinary Nuthatch indexing path.
- Registering a Nuthatch endpoint in The Graph gateway or Studio.
- Reopening arbitrary SQL or RFC-0034's resource limits merely because the request arrived as GraphQL.

## Compatibility contracts

### Schema

Introspection must expose graph-node's generated schema, not merely Nuthatch tables: singular and collection roots, `*_filter` and `*_orderBy`, `OrderDirection`, `_Block_`, `_Meta_`, `Bytes`, `BigInt`, `BigDecimal` and reverse `@derivedFrom` fields. Generated clients validate this before a useful query reaches the server.

### Query semantics

Collection queries take `first` (default and maximum 1000), `skip`, `orderBy`, `orderDirection` and `where`. Comparison, membership, contains, prefix/suffix, negation, case-insensitive operators, nested entity filters and `and`/`or` lower naturally to SQL predicates. Nested selections should use joins and JSON aggregation, not an N+1 path which only looks reasonable on a fixture.

### Time travel

`block: { number: N }`, `block: { hash: H }` and `number_gte` select versions valid at that point. graph-node records entity versions with a `block_range`; updates close the previous range and add a new version. Nuthatch needs the equivalent SCD-2 or bitemporal store plus a block-hash index. A latest-state table cannot answer this contract honestly.

### Values

Raw event data, addresses, ticks, liquidity, counts and readable metadata are functions of chain facts. Pricing state is not. Helpers such as `findNativePerToken` and `getNativePriceInUSD` read state written by earlier handlers, including other entities. Their values depend on event and write ordering, host semantics and `BigDecimal` behaviour. A SQL view over facts cannot generally reconstruct them.

| Tier | Deliverable | Boundary |
| --- | --- | --- |
| 1 | Generated schema, structural query semantics, `_meta`, raw chain-derived fields | No pricing-parity promise |
| 2 | Time travel, error envelopes, interfaces/unions/enums, deterministic local derivations | Exact full-text and decimal behaviour need parity tests |
| 3 | `derivedETH`, USD volume/TVL and other stateful mapping output | Requires original mapping replay |

## Design

Compatibility is an optional front-end. It reads an RFC-0044 import, generates a GraphQL type system, parses an accepted dialect and lowers it into bounded SQL. Response shape, scalar serialisation and error envelopes are part of the contract. `BigInt`, `Bytes` and `BigDecimal` need golden comparisons against a graph-node reference. Introspection remains available when a partial error response is otherwise returned.

`subgraphError: deny` remains the default. Nuthatch must not invent a reference indexing failure in order to look familiar. If it models a partial-port or source-data error, `allow` returns the normal Graph-shaped extension; otherwise `_meta.deployment.hasIndexingErrors` is false.

### Mapping replay

Faithful tier-3 output requires original WASM mappings plus graph-node-compatible `store.get/set`, entity-cache ordering, `ethereum.call`, AssemblyScript numeric types, templates and reorg semantics. Matchstick demonstrates the practical approach: use graph-node's runtime with substituted store and RPC implementations. There is no small independent host to adopt.

Embedding graph-node's runtime is not a casual shortcut. It imports its operational and deterministic failure modes, and every dependency must satisfy Nuthatch's `MIT OR Apache-2.0` policy and pass `cargo-deny`. If byte parity is hard-required, the default recommendation is a patched graph-node deployment rather than embedding its runtime. If it is not, the compatibility schema must label divergent pricing fields explicitly. It must never silently approximate a value and call the endpoint drop-in.

## Implementation sequence

1. Build a migration validator which runs real queries against a reference and Nuthatch, compares response shapes and fields, and lists permitted divergences.
2. Generate and golden-test schema introspection for one imported schema.
3. Implement the bounded singular/collection compiler, filters, ordering, nested selection and scalar serialisation.
4. Add block-ranged history and hash resolution; test boundary pagination and nested historical relations.
5. Decide tier-3 policy in a separate accepted RFC: absent fields, stated approximation, or a separately operated mapping runtime.

Tier-1 and tier-2 coverage for one schema is estimated at 5,000 to 9,000 Rust LOC and six to twelve weeks for one senior engineer. This is not a commitment and excludes tier-3 parity.

## Acceptance criteria

**The definition of done, and every criterion below is subordinate to it.** Chief settled this on
2026-09-11, after the three research passes that closed off the alternative routes:

> Point a client that was talking to the pinned broken deployment at a nest, change nothing but the URL,
> and its recorded query set answers: exact where the field is exact, converged where it is converged, a
> named refusal where it is neither, and **never a wrong number presented as a right one**.

Four things that sentence decides, each of which had been answered the other way at some point in this
programme.

**The client is unmodified and the only change is the URL.** Not a ported client, not a reduced query
set, not a documented list of selections to avoid. If adoption needs an edit to the consumer, the
compatibility surface has not been built.

**The target is a *pinned broken* deployment, named in advance.** Measuring against a subgraph that was
redeployed within days proves nothing, which is why the programme's evidence kept being about a
deployment nobody needs us for.

**Three legitimate answers, and a field must declare which one it is giving.** Exact, converged, or a
named refusal. Converged is not a consolation: for the ordered mapping-derived family it is *more*
correct than the reference, which carries stale write-order artefacts (§6a of RFC-0038, and the
`derivedETH` measurements on the tracking issue).

**A fourth answer is a defect, not a shortfall.** A plausible substitute - a fallback value, an
unfiltered set where a filter was asked for, an empty list where the schema promises a non-null one,
`null` against a non-null field - is worse than no endpoint, because the caller cannot see it. Every
defect found in S1 and S2 review so far has been this shape and not one has been a crash. A criterion
that cannot fail on a silent substitution is not a criterion.

- Generated introspection matches a recorded graph-node reference except for a reviewed, machine-readable divergence list.
- A corpus of real consumer queries proves shape, ordering, pagination and scalar parity.
- RFC-0034 admission bounds every request before it reaches DuckDB or Postgres.
- Historical tests prove that a query at N returns only versions valid at N, including nested boundary cases.
- The migration validator reports every mismatch and cannot become clean by dropping unsupported selections.
- Operator documentation describes adoption as a URL change and states the tier-3 value policy.

### Rescoped deliverables, 2026-09-12

The list above describes a drop-in replacement. These are what the programme should be judged on
instead. The distinction throughout is between **an answer being right**, which is met and should stay
met, and **a client being able to adopt**, which is not and which the original list conflated with it.

**D1 - No wrong numbers. Met; the one criterion that must never regress.** Every field either answers
exactly, or is refused by name with its reason. There is no known path that returns a plausible
substitute. Held by the mutation discipline and by the end-to-end test over indexed data, and everything
below is subordinate to it.

**D2 - An honest account of coverage, per port.** `port-emit` prints the figure and the generated
`README.md` carries it, with every unanswered field named and reasoned in the view comments. A reader
must be able to tell before adopting whether their queries are answerable. **Met.**

**D3 - The event-shaped surface answers over real data.** Collection and singular roots, filters,
ordering, pagination, traversals, `_meta`, `block.number_gte`, graph-node's wire types - over hot and
sealed storage alike. **Met.**

**D4 - Query-level coverage is measured, not inferred from field counts.** A corpus of real consumer
queries run against a real port, reporting how many *answer* rather than how many fields exist. Not
done, and it is the number that decides whether anyone can adopt. **Outstanding.**

**D5 - Token metadata answers.** `symbol`, `name`, `decimals` via the `[[calls]]` stanzas `port-emit`
already emits. Highest value per unit of effort left, unblocked, and it appears in nearly every real
query. **Outstanding.**

**D6 - The converged lane exists, or is ruled out in writing.** The definition of done permits three
answers and only two are implemented. S4 (#1268) is the decision; until it is taken, "converged where it
is converged" is a sentence with nothing behind it. **Outstanding - a decision, not code.**

Explicitly **not** deliverables any more: byte-identical pricing analytics (ruled out by RFC-0038 §6a,
not by effort); adoption by an unmodified analytics client; and the S0 migration validator, which was
never built and whose job `port-emit`'s own coverage accounting has been doing less directly.

## Risks and evidence limits

False compatibility is the principal risk: a slightly wrong filter, default ordering or decimal formatting still breaks clients. Time travel and N+1 avoidance are the largest engineering risks. The quoted provider landscape and Uniswap V4 incident support the design direction, but exact current mapping lines, incident identifiers, provider claims and runtime line counts need a source pass before appearing as verified release claims. This RFC records an engineering assessment, not a completed compatibility measurement.

**2026-09-12: two of those risks resolved the other way, and a third appeared.** *False compatibility*
was the principal risk and it did not materialise as feared - every defect found in S1 and S2 review was
a silent substitution caught before merge, and there is no known wrong-number path left. *Time travel*
turned out to be a read-side change and cheaper than assumed; a bounded query prunes row groups and is
**faster** than current state, which contradicts the risk stated here.

The risk that was not on this list is the one that governs: **partial field coverage does not compose
into partial usefulness.** One unanswerable field refuses a whole query, so a surface that answers 38%
of fields may answer far less than 38% of queries, and for an analytics client it answers none. Nothing
in this RFC anticipated that, and it is the reason for the rescope above.
