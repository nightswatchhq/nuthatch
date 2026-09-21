# Discord announcement: public Arbitrum Network Subgraph endpoint

Copy and paste this into the indexer channel:

> The Night's Watch is now running a public, rate-limited Network Subgraph endpoint for Arbitrum One:
>
> `https://network.thenightswatch.dev/graphql`
>
> It is available without an API key. It serves the fixed GraphQL queries used by indexer-agent, indexer-service-rs and the TAP monitor, including allocations, deployments, graph-network parameters, epochs, `paymentsEscrowAccounts` and `paymentsEscrowTransactions`. The endpoint publishes `_meta.block` and is kept close to the Arbitrum head.
>
> For indexer-agent:
>
> `INDEXER_AGENT_NETWORK_SUBGRAPH_ENDPOINT=https://network.thenightswatch.dev/graphql`
>
> For indexer-service-rs, set `[subgraphs.network].query_url` to the same URL and leave `deployment_id` unset. The endpoint is rate-limited rather than authenticated, so please do not share credentials because there are none to share.
>
> This is the operational public route while the permanent Nuthatch-native full-history facade completes its backfill and parity checks. If you see a stale `_meta` block or a query error, please report the exact query shape, timestamp and `_meta` response in this channel.

If someone asks whether this replaces their local deployment, answer:

> It can be used as the remote Network Subgraph source now. Keep your local deployment available as a fallback until the native Nuthatch facade has completed full-history validation.

Do not describe the endpoint as unauthenticated and unrestricted. It is unauthenticated, but it has per-IP and global rate limits so one flood does not become everybody's outage.
