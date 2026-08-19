# When a subgraph is down, the data does not have to disappear

Most teams treat their subgraph as one endpoint. It works until it is unreachable, has no indexer,
or falls behind just when somebody needs an answer.

Nuthatch is a small, self-hosted alternative for the on-chain part of that data path. A Nuthatch
nest packages the contracts, pinned ABIs, event selections, and any event-derived SQL views needed
for a protocol or deployment. It indexes directly from chain logs and serves read-only SQL over
HTTP. There is no allocation to wait for and no separate hosted-subgraph bill between the chain and
your query.

The useful operational shape is not necessarily "replace every subgraph." It is a fallback:

1. Keep using your GraphQL subgraph normally.
2. Keep a nest for the event data you cannot afford to lose.
3. If the subgraph is unreachable or unhealthy, switch the affected read to the nest until it
   recovers.

If the nest is already indexed, that switch is immediate. You can run it yourself, keep the sealed
data on your own disk, and query it locally or behind your own API. For deployments where a team
does not want to operate the box, we can also host a bounded, rate-limited temporary endpoint.

The important qualification is that a Nuthatch nest is not automatically a drop-in GraphQL endpoint.
It exposes deterministic, on-chain event data as SQL. Many subgraph entities can be reproduced as
event-derived views, but mappings that make `eth_call`s, fetch IPFS content, or maintain bespoke
off-chain state need their query surface reviewed explicitly. That is a boundary worth stating,
rather than discovering it in the middle of an outage.

We have just used this shape for
[DOUDOCHAIN_V2](https://github.com/nightswatchhq/doudouchain-v2-nest), whose deployment was
unreachable on the network. The port vendors the deployment's pinned source ABIs, indexes its 13
fixed Arbitrum contracts, and has a hosted fallback for its raw event surface. Its IPFS-derived
entities are deliberately not claimed as parity.

If your subgraph is down, unserved, or costing more operational attention than it deserves, send us
the deployment ID and the GraphQL queries that matter. We can determine what is event-derived, make
the boundary explicit, and help build a nest that means your on-chain data still has somewhere to
go when the usual endpoint does not.
