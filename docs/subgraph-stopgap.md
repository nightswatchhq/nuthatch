# Subgraph stopgap: nests for subgraphs the network does not serve

**Opened 2026-10-06. Status: S0 not started.** This file is the central tracker. Issues carry the
detail and are labelled [`subgraph-stopgap`](https://github.com/nightswatchhq/nuthatch/labels/subgraph-stopgap).

## Why

On 2026-10-08 Studio traffic for BNB Smart Chain and Polygon starts moving onto The Graph network.
A deployment that no indexer picks up stops answering, and its developer finds out when their app
breaks.

We offered the Graph Foundation a stopgap in #subgraph-development on 2026-10-04: build a nest from
the subgraph's manifest so its event data keeps answering, and point the developer back at the
network once an indexer serves it. They replied on 2026-10-05 that they would follow up in DMs.
Chief decided on 2026-10-06 to build it now rather than wait for that conversation.

## What the offer is, and is not

A nest reproduces what a subgraph derives **from decoded events**: ids, addresses, block numbers,
timestamps, event parameters, `@derivedFrom` relations. Every field answers exactly or is refused by
name. It does **not** reproduce what mappings accumulate: prices, running totals, day and hour
aggregates. GraphQL fails a whole query for one refused field, so an analytics dashboard will
usually not work unmodified, while bots, audit trails and event readers usually will.

This is **not a drop-in replacement** and must not be described as one anywhere.
[What the compatibility surface is](graph-compatibility-what-it-is.md) is the reference for the
boundary.

Two deliverables, in order of how certain their value is:

1. **The orphan list.** Which BNB and Polygon deployments have demand on chain and no indexer
   serving them. Useful to the Foundation whatever the stopgap turns out to cover.
2. **The stopgap nest.** One command from a deployment CID to a serving nest, with an honest
   per-field coverage report.

## What already exists

| piece | where | since |
| --- | --- | --- |
| manifest importer, `init --from-subgraph <CID>` | `src/subgraph_import.rs`, RFC-0038 | v2.6.0 |
| port skill and per-field report | `skills/nuthatch-subgraph-port`, `port-emit`, RFC-0044 | 2026-09-09 |
| Graph-dialect GraphQL read surface | `src/graph_query.rs`, RFC-0053 (parked, shipped parts supported) | 2026-09-12 |
| BNB and Polygon chain presets | `src/chains.rs` | |
| deployment and allocation state | `graph-allocations-nest` | |
| precedent: an unserved deployment ported | [doudouchain-v2-nest](https://github.com/nightswatchhq/doudouchain-v2-nest) | |

No slice below needs new capability in the default binary. RFC-0053 and RFC-0044 stay parked: this
programme fixes what the BNB and Polygon sample shows broken in what already shipped, and starts no
further slices of either.

## Slices

| slice | issue | what | fails if | status |
| --- | --- | --- | --- | --- |
| S0 | [#1940](https://github.com/nightswatchhq/nuthatch/issues/1940) | `init --from-subgraph` and `port-emit` on 15-20 published `bsc` and `matic` deployments, mixed shapes; record init success, fields answered, backfill cost | fewer than a third of the sample yield a serving nest with any answered field; the offer then narrows to S2 alone | not started |
| S1 | [#1941](https://github.com/nightswatchhq/nuthatch/issues/1941) | fix what S0 broke; CID to serving GraphQL surface end to end on both chains, golden tests from the sample | a sample deployment that should port does not | waits on S0 |
| S2 | [#1942](https://github.com/nightswatchhq/nuthatch/issues/1942) | resolve each manifest's `network:` from IPFS; view of `bsc`/`matic` deployments with signal or an indexing agreement and zero active allocations; alert on new entries | the list disagrees with the network subgraph's allocation counts at a pinned block | not started |
| S3 | [#1943](https://github.com/nightswatchhq/nuthatch/issues/1943) | per-subgraph handback package: coverage report, how to run, how to point back at the network | someone who did not build it cannot stand one up from the package alone | waits on S1 |

S0 and S2 are independent and run in parallel. S0 is the slice that can stop the stopgap half.

## Known limits

- **S2 sees only what is on chain.** A Studio deployment never published on chain is invisible to
  it. Only the Foundation can see those.
- **BNB backfill needs a paid archive endpoint.** The only working public BNB endpoint refuses
  history a million blocks back (`src/chains.rs`), and Polygon's archive coverage is narrow. S0
  measures the cost per subgraph before anything is promised.
- **Mapping logic is not run.** No AssemblyScript, anywhere, per RFC-0038 §8. Pricing and
  accumulated fields are refused permanently, not pending.

## Decided

- **Who runs a stopgap nest when the developer cannot: we do.** Chief lifted the 2026-09-29
  no-hosting rule for this programme on 2026-10-06 (CLAUDE.md, out-of-scope list). Nests are served
  from the ThinkPad or new VPSes, stock binary, no billing, until an indexer allocates. The
  [fallback forum post](nuthatch-subgraph-fallback-forum-post.md)'s hosted-endpoint offer is consistent
  with this again, but scope it to unserved subgraphs before posting.

## Log

- **2026-10-06** Tracker opened, S0 to S3 filed as #1940 to #1943.
- **2026-10-06** Chief lifted the no-hosting rule for the stopgap: we serve the nests ourselves.
