# Launch copy - Show HN

Draft. Refresh every number against the [README](../../README.md) at post time (Rule: every published
number traces to a reproducible artifact). Post one channel per day; this is the Phase 2 artifact from
[RFC-0007](../rfcs/0007-launch-and-validation.md).

---

## Title (pick one at post time)

> **Show HN: Nuthatch - a self-hosted blockchain indexer in one Rust binary (58 MB RAM)**

Alternate, if the single-contract figure reads better:

> **Show HN: Nuthatch - be your own blockchain indexer (one Rust binary, 37 MB RAM)**

Both numbers are measured and CI-enforced. Lead with the footprint - it's the figure nobody else in
this space publishes, and it's the whole thesis in four characters.

---

## First comment (the "why I built this")

I got tired of every app that reads a blockchain depending on a handful of hosted providers that
meter, gate, and can cut you off. Nuthatch is the opposite bet: one Rust binary, one command, a live
queryable API in under two minutes, with **no mandatory third-party data dependency, ever**. No
Postgres, no Docker, no IPFS, no token, no phone-home.

You point it at a contract address, it fetches the ABI, and it decodes every event into tables you can
query over SQL (or over an MCP server compiled into the binary, so a coding agent gets real schema
instead of hallucinating). Storage is a mutable hot store at the chain tip (redb) and
content-addressed Parquet past finality, with DuckDB attaching the segments read-only for analytics. A
reorg only ever touches the hot store; sealed segments are immutable.

The numbers I actually care about, all measured on the release build and reproducible in-repo:

- **~58 MB peak RAM** for a live 3-contract nest (USDC + WETH + DAI, 23 tables, indexing + sealing +
  DuckDB SQL + incremental balance views all at once); ~37 MB for a single contract. The RAM budget is
  ≤2 GB and CI fails the build above 256 MB - it's a budget, not a hope.
- Backfill throughput is benchmarked in the open, not asserted: byte-identical output is proven
  across the hot-store, seal-direct, and pipelined paths (whichever one writes, the sealed segments
  are the same), and an 8-way concurrent pipeline gets a real, reproducible speedup by overlapping
  RPC round-trip latency. The storage-path multiplier (seal-direct vs. hot store) is under active
  re-measurement after a harness fix exposed the earlier figure as an artifact of a fixed strawman -
  current numbers, machine, and harness commit are in `docs/benchmarks.md` (#722); a discrepancy in
  that re-measurement is still open in #744, so no multiplier is quoted here until it settles.

One genuinely different bit: entity views are **incremental** (Feldera/DBSP). A per-address balance
view treats a reorg as a *retraction*, not a recompute - the same circuit runs a backfill as a batch
and a reorg as a diff. Balances are i128 end-to-end (a transfer above i64::MAX won't silently vanish),
and they survive a restart.

**Honest limits, because you'll ask:** Ethereum + Arbitrum + Base ship with bundled endpoints
(anything else needs `--rpc`); no call/trace decoding, which genuinely needs a colocated node; RPC
polling - the reth ExEx in-process path is designed and stubbed, not shipped; no GraphQL layer yet
(SQL + point-reads + MCP today). **Contract state is derive-first**, which is the part I'd defend
rather than apologise for: The Graph makes you run a ~1.77 TB archive node for any subgraph with
`eth_call` handlers, and most of what those calls ask for is derivable from events you already index -
`total_supply` as mints minus burns, balances per address, holder count, Uniswap-V2 reserves as the
latest `Sync`. Those ship and cost nothing. Immutable metadata (`decimals`/`symbol`/`name`) is fetched
once and cached. What's left is the irreducible residue - an oracle read, an ungoverned parameter - and
for that a pinned-block `eth_call` against an operator-supplied archive RPC is designed, addressed and
tested but **has no executor yet**, so a nest declaring one is refused at load rather than silently
producing nothing. IPFS-derived entities aren't indexed at all yet (RFC-0037).

It's v2.5.0, solo-maintained, and running in production. `MIT OR Apache-2.0`, a grant-funded public
good, not a startup - the sustainability plan and the "what we'll never build" list are both in-repo.

Install, quickstart, the footprint methodology, and the full progress log:
https://github.com/nightswatchhq/nuthatch

Happy to answer anything - architecture, the DuckDB single-writer design, the determinism proofs, or
why the binary is 67 MB (DuckDB + DBSP + wasmtime statically bundled; it's 5.8 MB without them, but a
single file is the non-negotiable).

---

## Anticipated questions (have answers ready, don't pre-post them)

- **"Why not just use The Graph / Ponder / Envio?"** → The Graph and Alchemy/Infura-class RPC are a
  metered third party you can be cut off from. Ponder still needs a capable (often paid) RPC. Envio's
  self-host now wants a token for HyperSync (phones home). Nuthatch's wedge: Rust single-binary ops +
  zero mandatory third-party API + IVM correctness + AI-native surface.
- **"67 MB binary?"** → answered inline above; offer the 5.8 MB no-embed figure.
- **"Is this a GraphOps product?"** → No. An operator is preparing a hosted offering and shares revenue
  to fund core dev; the permissive licence means anyone can host, fork or embed the identical software. Link
  GOVERNANCE.md - don't argue it, link it.
- **"Events only is a dealbreaker for me because X"** → thank them, that's exactly the validation
  signal; log it (docs/validation).
