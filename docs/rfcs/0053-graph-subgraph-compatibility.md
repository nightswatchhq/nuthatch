# RFC-0053: Graph-subgraph compatibility mode

**Status:** **Accepted 2026-09-09 by Chief**, as the decision RFC-0044 §8 requires for new binary
capability. It **overrides [RFC-0044](0044-the-subgraph-port-skill.md) §11's** "not a subgraph
compatibility layer" non-goal, annotated there. Scope of the authorisation: S0 (#1264, the migration
validator) is in the `resolute-robin` sprint; S1 to S4 (#1265 to #1268) are filed but wait on what S0
measures, because S0 is the only slice that can falsify the rest.

**Date:** 2026-09-09

**Depends on:** RFC-0044, RFC-0041, RFC-0034, RFC-0047

**What it blocks:** Any claim that Nuthatch is a drop-in Graph Protocol subgraph endpoint without an explicit decision about its value contract.

## Abstract

Nuthatch can reproduce the structural and relational surface of a Graph Protocol subgraph with bounded work: generated GraphQL schema, singular and collection queries, filters, pagination, `_meta`, time-travel and `subgraphError` compatibility. The compiler is a Graph-dialect-to-SQL layer over Nuthatch's relational state, not a second indexer.

It cannot, by that route, reproduce every value a reference subgraph returns. Uniswap V4 pricing fields such as `derivedETH`, `Bundle.ethPriceUSD`, `volumeUSD` and `totalValueLockedUSD` are the output of ordered, stateful AssemblyScript handlers, not a declarative function of sealed chain facts. Exact parity requires replaying the original mappings with graph-node host semantics. Thus the endpoint can be a drop-in for shape and reproducible data, but not an automatic promise of byte-identical pricing analytics. Consumers adopt it by changing a GraphQL URL: The Graph gateway has no off-network endpoint registration mechanism.

## Motivation

RFC-0044 makes an imported subgraph useful as a Nuthatch nest, but deliberately does not make Nuthatch impersonate its GraphQL endpoint. A separate compatibility mode is worth considering where a client has a substantial body of existing GraphQL queries and availability of raw and structural data matters more than mapping-derived analytics.

The Uniswap V4 failure shape is the example. A mapping that returns early when token metadata cannot determine decimals may fail to create a Pool; a later handler loading that Pool then aborts deterministically and graph-node can halt the deployment. An event-first Nuthatch port does not run that abortable handler in its data path, and can still serve the decoded facts. This is an availability claim, not an assertion that its USD totals equal the reference mapping.

## Goals

- Generate a Graph-node-shaped schema from imported `schema.graphql`, including generated roots, filters, order enums, `_meta`, scalars and `@derivedFrom` traversal.
- Compile singular and collection queries to parameterised SQL in embedded DuckDB and scaled Postgres modes.
- Support `first`, `skip`, `orderBy`, `orderDirection`, Graph `where` operators, `and`, `or`, nested entity filters, one-level nested sorting and the default ID-ascending order.
- Support `block: { number | hash }` historical queries with a block-ranged entity history.
- Make divergences visible with a side-by-side migration validator.

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

## Risks and evidence limits

False compatibility is the principal risk: a slightly wrong filter, default ordering or decimal formatting still breaks clients. Time travel and N+1 avoidance are the largest engineering risks. The quoted provider landscape and Uniswap V4 incident support the design direction, but exact current mapping lines, incident identifiers, provider claims and runtime line counts need a source pass before appearing as verified release claims. This RFC records an engineering assessment, not a completed compatibility measurement.
