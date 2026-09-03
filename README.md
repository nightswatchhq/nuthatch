# nuthatch

> **Turn any contract into a local SQL database.**
> One command. One tiny binary. Your box, your data - no subgraph to author, no Postgres to run, no
> monthly bill, no third-party API.

[![ci](https://github.com/nightswatchhq/nuthatch/actions/workflows/ci.yml/badge.svg)](https://github.com/nightswatchhq/nuthatch/actions/workflows/ci.yml)
· Website: [www.nuthatch-indexer.com](https://www.nuthatch-indexer.com)

```sh
curl -fsSL https://nuthatch-indexer.com/install.sh | sh   # prebuilt binary, no compiler needed

nuthatch init 0xA0b86991c6218b36c1D19D4a2e9Eb0cE3606eB48 --alias usdc   # USDC - chain auto-detected
nuthatch dev            # backfills from deployment, follows the tip, serves an API
nuthatch sql "SELECT count(*), sum(CAST(value AS DECIMAL(38,0))) FROM usdc__transfer"
```

That's the whole thing. You had an address ninety seconds ago; now you're running `SELECT` over its
on-chain activity, on your own machine.

---

## Why nuthatch

Every other way to get your contract's data fails the solo dev *somewhere*:

| | author a subgraph? | infra to run | query | yours? | pay? |
|---|---|---|---|---|---|
| **The Graph** | yes - schema + manifest + AS mappings | - (decentralised) | GraphQL | no | query fees |
| **Goldsky / hosted** | no | - (their servers) | SQL/GraphQL | **no** | **monthly** |
| **Ponder** | yes - TS handlers | Node + Postgres | SQL | yes | free |
| **Subsquid** | yes | archive + Postgres | GraphQL | yes | free |
| **nuthatch** | **no - init from an address** | **one static binary** | **SQL (DuckDB)** | **yes** | **free** |

Nobody else hits all four of *zero authoring*, *zero infra*, *it's just SQL*, and *it's yours and it's
tiny*. That combination is the point - not any single feature.

- **Zero authoring.** `init 0xAddr` resolves the ABI (Sourcify → Etherscan), generates the schema and
  decoders, and scaffolds the project. You write nothing.
- **Zero infra.** A single static Rust binary. Embedded mode needs no Postgres, no Docker, no IPFS.
- **It's just SQL.** Your contract's events become per-event tables you query with real analytical SQL -
  the live tip *and* sealed history, one surface.
- **It's yours, and it's tiny.** ≤2 GB RAM for single-chain tip-following, CI-enforced. No telemetry, no
  phone-home, no mandatory API token, ever.

---

## Install

```sh
curl -fsSL https://nuthatch-indexer.com/install.sh | sh
```

That downloads the prebuilt binary for your platform from the latest release, verifies its SHA-256,
and installs it to `~/.local/bin` (override with `NUTHATCH_INSTALL_DIR`). **No compiler is
involved**, so whichever rustc you happen to have is irrelevant. Prebuilt binaries cover macOS Apple
Silicon and Linux x86_64 and are attached to every release with their checksums, if you would rather
fetch one by hand.

**The Linux binary is dynamically linked and needs two things**, both measured off the published
artifact with `objdump -T` rather than inferred:

- **glibc 2.34 or newer** - the measured ABI floor, read off the published artifact with `objdump -T`.
  The release is *built* on glibc 2.35, but building on 2.35 does not make 2.35 a runtime
  requirement: the binary references no symbol newer than `GLIBC_2.34`, and that reference set is
  what a loader checks. The two numbers answer different questions - **2.34 is what you need to run
  it, 2.35 is what we compile it on** - and stating the build baseline as the requirement excluded a
  platform we actually support (#978: the list below names RHEL 9, which ships glibc 2.34).
- **libstdc++ from GCC 11 or newer** (`GLIBCXX_3.4.29`, `CXXABI_1.3.13`). This one has a cause worth
  knowing: the binary embeds DuckDB, which is C++, so it links `libstdc++.so.6` alongside `libc`,
  `libm` and `libgcc`.

Debian 12, Ubuntu 22.04, RHEL 9 and Amazon Linux 2023 clear both. A system with new glibc and an old
libstdc++ would not, which is why both are stated rather than only the first.

**Verify who built it.** The SHA-256 sidecar beside each tarball tells you the file did not corrupt in
transit, and nothing more: whoever could replace the tarball could replace the sidecar in the same
breath. Every release binary therefore carries a **build provenance attestation**, signed by GitHub's
identity for the workflow run that produced it and recorded in a public transparency log. One command,
and it needs no key from us:

```sh
gh attestation verify nuthatch-x86_64-unknown-linux-gnu.tar.gz --repo nightswatchhq/nuthatch
```

That answers what a checksum cannot: which repository, which commit and which workflow built this
file. `--repo` is the load-bearing part - without it, an attestation from *any* repository is accepted,
which is most of the property you are checking for. The release workflow runs the same verification
against its own assets before it publishes, so a release whose provenance does not verify never becomes
public in the first place.

**From source**, which is the only route on a platform we do not publish a binary for:

```sh
rustup toolchain install 1.95.0
cargo +1.95.0 install --git https://github.com/nightswatchhq/nuthatch nuthatch
```

The toolchain pin is load-bearing, not decoration. `rust-toolchain.toml` pins 1.95.0 because `dbsp`
hits a next-trait-solver ICE on 1.97, and **that file does not apply to `cargo install --git`**,
which builds in a temporary directory of its own. Without `+1.95.0`, a 1.97 default toolchain fails
after a full dependency build with `error: could not compile dbsp` and installs nothing
([#534](https://github.com/nightswatchhq/nuthatch/issues/534)).

**Container images** are published per release to `ghcr.io/nightswatchhq/nuthatch` - `:<version>` for
embedded, `:<version>-scaled` for the scaled build. The image ships the *same binary attached to the
release*, so the two cannot drift.

**Chains.** Ethereum, Arbitrum One, Base, BSC, Polygon, Gnosis, Optimism and Monad are *built in*, with
measured public endpoints and tuned finality settings - **omit `--chain` and nuthatch probes each for
your contract's bytecode and picks the one it lives on.** Point at your own node with `--rpc`.

**Any other EVM chain works too** - World Chain, Base Sepolia, your own devnet. Name the chain and
say where it lives:

```sh
nuthatch init 0xADDR --chain world-chain --rpc https://your-endpoint.example
```

The chain id is read **from the endpoint itself**, so there is no id to look up and nothing to type
wrong. A built-in chain never dials: `--rpc` is ignored for the eight names above. Omit `--rpc` on an
unregistered name and the refusal tells you the remedy rather than just listing the built-ins.

Public endpoints are a moving target, and the ones shipped here are measured rather than assumed -
but a measurement is a snapshot, not a property. Run `nuthatch doctor --rpc <url>` before trusting a
long backfill to any endpoint, yours or ours: it reports the widest `eth_getLogs` range, the JSON-RPC
batch limit and whether the node has archive depth, and prints the largest safe `--window`.

Everything downstream was always chain-agnostic - `dev`, `sql` and `bench`, and the indexer's
unregistered-chain finality and window defaults. `init`'s allow-list was the only thing narrower than
what nuthatch actually scaffolds, and it went in 2.4.0. See
[running an unlisted EVM chain](docs/operators.md#running-an-unlisted-evm-chain) for the finality
caveat, which is the part worth reading: a chain whose `finalized` tag runs close to the tip needs a
depth-based policy instead, or you seal immutable Parquet that could never be corrected.

### Bring your own RPC endpoint

**nuthatch assumes a paid RPC endpoint, or your own node, for anything you intend to keep running.**
That is the golden path.

Worth knowing, since we are being precise about it: most figures currently in
[`docs/benchmarks.md`](docs/benchmarks.md) were measured against *public* endpoints, and that is a
known weakness of those numbers rather than a recommendation - a benchmark taken through a
rate-limited endpoint measures the endpoint. We measured the network at **99.3% of backfill wall
clock**, which is why the replay rig (RFC-0039) exists and why those figures carry that caveat on the
page itself.

The free public endpoints bundled per chain exist for one job: so `init` → `dev` works with **zero
setup**, which is the two-minute demo, and it is deliberate. Treat them as **testing and initial
validation** - trying it out, checking a contract resolves, following the tip of something quiet.
They are the on-ramp, not the road.

Why they are not fine for real work, said here rather than discovered at 3am:

- **They are rate-limited and shared.** You are queueing behind everyone else using the same free tier
  from the same IP range. Throughput varies by the hour.
- **They fail intermittently, and not always loudly.** A rate-limited endpoint may return an empty
  result rather than an error. nuthatch fails over across the pool and retries, but a window that every
  endpoint refuses will stall until one recovers - `/ready` reports `stalled` when that happens.
- **Deep backfills will crawl or stop.** Full history over a busy contract means millions of
  `eth_getLogs` calls. Expect a free endpoint to throttle you long before that finishes.
- **No archive guarantees.** Many free endpoints prune old state, so a backfill from a 2020 deploy block
  can simply fail partway.

**Check an endpoint before you trust a backfill to it.** `nuthatch doctor` probes one and reports the
largest `getLogs` window it will actually serve, its batch limit, and whether it has archive history -
measured, not taken from the provider's documentation:

```sh
nuthatch doctor --rpc https://your-endpoint.example --address 0xADDR
```

**Use your own endpoint for anything you care about** - your own node, or a paid provider:

```sh
nuthatch init 0xADDR --chain arbitrum-one --rpc https://your-endpoint.example/arbitrum
nuthatch dev --rpc https://your-endpoint.example/arbitrum   # or set rpc_urls in nuthatch.toml
```

`--rpc` is repeatable, and nuthatch round-robins across the pool with per-endpoint health tracking, so
listing two or three endpoints gets you failover as well as throughput. Every endpoint in a pool must be
on the **same chain** - nuthatch verifies this at startup and refuses to run against a mixed pool, since
indexing against the wrong chain corrupts state silently.

---

## Querying your data - the whole point

Every declared event becomes a table named `{alias}__{event}` (e.g. `usdc__transfer`), carrying the
event's fields plus `block_number`, `block_hash`, `block_timestamp`, `tx_hash`, `log_index`,
`address` and a `_seq` ordinal.

> `block_timestamp` costs a block-header round trip per block - about 85% of backfill wall clock. A
> nest that will never ask a time-series question can drop the column with `init --no-timestamps` and
> skip that entirely. It is an **init-time** choice: changing it later is a breaking schema change and
> a full re-index, so it is worth a moment's thought and is deliberately not a flag you can flip.
> [Details](docs/operators.md#configuration-surface).

```sh
# one-shot from the terminal (prints an aligned table; --json to pipe to jq)
nuthatch sql 'SELECT "from" AS sender, count(*) AS n FROM usdc__transfer GROUP BY 1 ORDER BY n DESC LIMIT 5'

# or over HTTP, against a running `nuthatch dev`
curl 'localhost:8288/sql?q=SELECT%20count(*)%20FROM%20usdc__transfer'
```

- **`nuthatch sql`** queries the local store when `dev` is stopped, and transparently falls back to the
  running instance's API when `dev` holds it - the same command works either way.
- **A degraded nest says so.** If a sealed segment is unreadable, nuthatch serves the rest of the table
  rather than failing your query - but it will not let that pass silently. `/sql` returns `degraded`
  and `degraded_tables` naming the affected tables, `nuthatch sql` prints a warning line, and the MCP
  server carries the same notice. The caveat is a fact about the *nest*, not about the row count you
  happened to get, so it appears whether or not this particular query touched the gap.
- **A failed query tells you how to fix it.** A DuckDB error is classified against the nest's own
  schema and an actionable line is appended - the engine's raw message is always kept, the hint is
  added after it. An unknown table names the closest real one; a view that failed to *build* says so
  rather than reporting "does not exist" and sending you hunting for a missing view; and a Solidity
  `bool` column explains itself, because it is stored as exact text `'true'`/`'false'` and therefore
  blows up inside `COALESCE`, `CASE`, `UNION` and `bool_and`/`bool_or` while comparing fine on its
  own. Same treatment on `/sql`, the MCP `sql` tool and the `nuthatch sql` REPL.
- **Hot + cold in one surface.** Queries span the live unsealed tip (redb) *and* sealed history
  (Parquet), transparently - you never think about the boundary.
- **Big-int friendly.** `uint256` values are exact text; each also gets a `{col}_dec` DECIMAL view, so
  `SUM(value_dec)` just works.
- **AI-native.** A Model Context Protocol server is compiled in (`nuthatch mcp`) - point Claude (or any
  MCP client) at your indexer and ask your contract's data in plain English, fully offline.

---

## How fast is it

We ran **someone else's** benchmark rather than writing our own: Sentio's
[OBIB](https://github.com/sentioxyz/open-blockchain-indexer-benchmark).

**Case 1** indexes `Transfer` from LBTC across 22.2M Ethereum blocks.

| | |
|---|---|
| wall clock | **74.8 s** |
| events | **294,278** (matches Sentio's own README exactly) |
| RPC requests | **321** |
| peak RSS | **320 MB** |

**Case 2** is case 1's contract with per-account balances, and OBIB's implementations get them with one
`balanceOf()` per account. **We make none.** For a plain ERC-20 the balance *is* the transfer history,
so we index the token's whole life instead and derive it - trading 2.5M extra blocks of cheap `getLogs`
for zero `eth_call` round trips.

| | |
|---|---|
| wall clock | **49.2 s** (median of 3) |
| accounts | **7,634** - OBIB's published figure, exactly |
| `eth_call` round trips | **0** |
| RPC requests | **136** |
| peak RSS | **325 MB** |

Reference times for the same case: Sentio 7.78 min, Envio 8.54 min, Subsquid 46.85 min.

**Two caveats, stated rather than buried.** First, this is deliberately not like-for-like on *range*:
OBIB windows to 100,001 blocks, we index 2,611,334. On OBIB's own range we take **9.3 s** - but that
run cannot produce the case's output at all, because absolute balances need history from before the
window, which is precisely why the benchmark makes the RPC calls. Second, "derived" is proven rather
than asserted: at the pinned end block, 39 sampled accounts - the ten largest, ten smallest non-zero,
ten zero-balance and ten by address order - **all matched `balanceOf()`**, including every zero-balance
account, which is the case an off-by-one in the ledger would betray.

The count is 7,634 and not 7,635 because `0x0` is the mint/burn counterparty rather than a holder. That
off-by-one was the tell that the interpretation was right.

**Case 6** is the factory-template case: the Uniswap V2 factory over blocks 19,000,000-19,010,000,
discovering pairs from `PairCreated` and indexing `Swap` on every child it finds. No per-child config,
no redeploy, one rule.

| | |
|---|---|
| wall clock | **49.5 s** (median of 5) |
| events | **35,271** = **35,039** swaps, matching OBIB's expected count exactly, plus the 232 `PairCreated` rows |
| children discovered | **232** |
| RPC requests | **16** |
| peak RSS | **247 MB** |

For scale, OBIB's own published figures for case 6 differ between its two tables: the January 2026
results table gives Envio HyperIndex **1.92 min**, Subsquid 5.34 min and Sentio 14.36 min, while the
case-6 page reports Envio at **30 s** from an earlier round. We are quoting both rather than the
flattering one; on the second, Envio is faster than us. Note too that Envio and Subsquid serve this
from their own pre-indexed networks, where nuthatch runs against plain JSON-RPC.

Both against a real provider (Alchemy), on an 11-core laptop. The artifacts are
[`docs/bench/obib-case1.json`](docs/bench/obib-case1.json),
[`docs/bench/obib-case2.json`](docs/bench/obib-case2.json) and
[`docs/bench/obib-case6.json`](docs/bench/obib-case6.json); `nuthatch bench backfill` re-runs any of them.
The case-2 nest is committed at [`obib-case2/`](obib-case2/) - keyless, so the endpoint arrives via
`--rpc`, and verified to rebuild from a clean checkout.
The case-6 nest is published at [`nightswatchhq/obib-case6`](https://github.com/nightswatchhq/obib-case6)
so the run can be reproduced rather than believed, and is submitted upstream as
[sentioxyz/open-blockchain-indexer-benchmark#3](https://github.com/sentioxyz/open-blockchain-indexer-benchmark/pull/3).

**Wall clock on a shared endpoint is the provider's number as much as ours.** The same case-6 range on
the same commit measured anywhere from 17 s to 57 s depending on when it ran. We checked whether the
fast runs were provider caching by re-running against an adjacent, never-fetched range
([`obib-case6-cold-control.json`](docs/bench/obib-case6-cold-control.json)): it landed in the same
band, so caching is not the explanation. The event count and the **16 RPC requests** are invariant
across every run, and they are the honest measure of range control.

Two things that number is worth knowing about:

- **It did not finish at all before v0.9.0.** Alchemy returns its oversized-range refusal as HTTP
  **400**, which our status classifier did not enumerate - so a window that needed splitting was
  retried unchanged, forever. Running an outside benchmark found a defect that our own testing had not.
- **~85% of the original wall clock was buying `block_timestamp`** - one serial round trip per block,
  for a column that workload never stores. Timestamps are now demand-driven and the log window adapts
  to what an endpoint will actually serve. See [RFC-0029](docs/rfcs/0029-the-fastest-indexer.md).

Case 6 found a defect too, in the harness rather than the indexer: `bench backfill` fetched a fixed
address list, so a **factory nest was measured without its children** - 232 events in 2.6 s against an
expected 35,039, reported as a success. Running an outside benchmark has now found two things our own
testing did not.

**Analytical queries** run on DuckDB over sealed Parquet. We benchmark-gated the alternative rather
than arguing about it: DataFusion measured **1.6–2.7× slower** on the fold that matters, with the gap
widening as segments grow, at exact result parity -
[RFC-0013 §5](docs/rfcs/0013-storage-and-query-engine-direction.md).

---

## How it works (the 30-second version)

```
RPC ingestion  →  deterministic decode  →  redb hot store (tip)
                                                            │
                                        past finality  →  content-addressed Parquet segments
                                                            │
                                        DuckDB attaches segments read-only  →  SQL (hot ∪ cold)
```

- **Deterministic core.** Decode, reorg handling, and entity derivation are deterministic and
  re-executable - same inputs, same content-addressed output. No LLM ever sits in the data path.
- **Reorg-safe by construction.** Reorgs only ever touch the mutable hot store; sealed segments are
  strictly past finality and immutable.
- **Single writer.** One ingestion thread writes; queries only ever attach read-only.

---

## Point an AI at it

nuthatch has a Model Context Protocol server compiled in, so a coding agent can query your contract's
data in plain English - offline, no phone-home. Wiring it is one step:

```sh
nuthatch dev &                  # the index the agent will query
nuthatch mcp --print-config     # prints a copy-paste config for Claude Code / any MCP client
```

Or add it to Claude Code directly:

```sh
claude mcp add nuthatch -- nuthatch mcp --url http://127.0.0.1:8288
```

Then just ask: *"what are the top USDC holders?"* - the agent writes the SQL and runs it against your
nest. (Making that correct on the first try is the [semantic-layer work](docs/rfcs/0016-governed-semantic-layer-and-agent-grade-mcp.md).)

**Teach your agent to *build* nests too.** Install the builder skill and an agent can drive nuthatch
itself - `init`, config, factories, compliance, multi-nest runtimes, troubleshooting - before you even have a nest:

```sh
cp -r skills/nuthatch-builder ~/.claude/skills/   # or your repo's .claude/skills/
```

Its CLI/config references are generated from the binary and CI-checked for drift, so the skill never
lies about a flag ([RFC-0017](docs/rfcs/0017-builder-skill.md)).

---

## Everything else it can do

The core is "your contract → SQL." Beyond that, nuthatch has a full feature set for teams and operators
who need more - none of it in the way of the happy path:

- **Many contracts, one nest.** Declare several contracts in `nuthatch.toml`; index them together.
- **Contract state, pinned** (RFC-0023). A `[[calls]]` block reads a contract at a fixed block and
  stores the result as a table, optionally with calldata built from the row that triggered it - the
  `contract.balanceOf(event.params.user)` a subgraph would write. Pinning the block is what keeps it
  deterministic: the answer is fixed, so two operators re-running the same nest get the same bytes,
  and the result is content-addressed on `(chain_id, block, contract, calldata)`. Needs `--state-rpc`
  pointed at an archive node.
- **IPFS documents, verified** (RFC-0037). An `[[ipfs]]` block turns a column of content addresses
  into a table of resolved documents. Every body is re-hashed and checked against the CID it claims
  to be, so a gateway serving the wrong bytes yields no row rather than a plausible one. The CID is
  taken from whatever shape the contract stored - a bare CID, an `ipfs://` URI, a gateway URL, or a
  raw 32-byte digest - and **the host is discarded**, because that string came from a log and
  honouring it would let whoever emitted the event choose what your indexer connects to.
- **Factory / dynamic contracts** (RFC-0009). Watch a factory (e.g. a pool factory); children are
  discovered at runtime and indexed into shared `{template}__*` tables - no redeploy per child.
- **Derivation, three speeds.** Decoded events are tables, sealed as they index. Three built-in
  relations - balances, exposure, velocity - are incremental DBSP circuits; a reorg is a
  retraction. Nest-authored `views/*.sql` are named SQL evaluated at query time over hot ∪ sealed,
  not IVM. And a nest can declare its own **authored incremental entities** in `entities.toml`
  ([RFC-0041](docs/rfcs/0041-authored-incremental-entities.md)): a `SELECT` that DBSP maintains as
  blocks arrive, served from `/derived` and queryable by name from `/sql`, with reorgs handled as
  retractions like the built-ins. On a real nest that took a panel from 2.15 s to 88 ms. A WASM
  transform layer remains the imperative escape hatch.
- **Compliance pack** (RFC-0008). Address labels, sanctions/watch-list screening, threshold & velocity
  flags, counterparty-exposure views, and a signed, replayable audit manifest.
- **Alerts & webhooks** (RFC-0010). HMAC-signed egress with a durable at-least-once outbox; a slow
  endpoint never blocks indexing.
- **Built-in admin UI.** A self-contained page at `/_admin/` - status, tables, view/nest inspector.
  Localhost-open; off-localhost it requires a token per request.
- **Many nests, one runtime, one or more chains** (RFC-0012, RFC-0021). Host many nests in one
  process; nests on the same chain share a single cursor and one `getLogs` per window (N nests for
  roughly one nest's RPC cost), and a runtime can span **multiple chains** with **one isolated cursor per
  chain** - a Base nest and an Arbitrum nest in one runtime. Per-nest isolation, and a footprint budget
  **per active-chain cursor** (≤2 GB). A capability, not a mandate: one chain per runtime stays the simple
  default.
- **Mount and unmount nests without a restart** (RFC-0027). Changing a runtime's nest set
  used to mean editing config and restarting, which stops every *co-tenant* nest too - so the blast
  radius of a config change was larger than that of a fault. Now `POST /_admin/nests` mounts one and
  `DELETE /_admin/nests/<name>` unmounts one, live. A mount is admitted only if it fits the cursor's RAM
  budget (refused with `507`, never a warning - a budget that can be quietly exceeded is not a budget),
  catches up *before* it joins so it never drags co-tenants back through history, and only then gets
  routes. An unmount is a **drain**, not a route removal: the cursor finishes its window and releases
  the store before anything is torn down. The set is persisted to `mounts.toml`, so a restart converges
  on what you last asked for.
- **Scaled mode - a fleet across machines** (RFC-0022). When one box can no longer hold your cursors,
  or when serving and ingestion want to scale independently, the *same crates* run as three roles:
  a **control plane** holding what should run, a **writer pool** (`nuthatch worker`) whose members take
  cursor **leases**, and a **query-FE tier** (`nuthatch serve`) that serves from shared state and owns
  nothing. A role flag, never a fork -
  and opt-in at build time (`--features postgres-store`), so the published binary carries no database
  driver and embedded mode stays a single file with zero services. The writer pool is safely scalable
  because ownership is enforced *by the store*: every write carries a fence, and a stalled worker that
  wakes up finds its writes **refused** rather than merely discouraged. Nests are added and removed
  over HTTP with no restarts, versions are pinned fleet-wide so two FE nodes can never serve the same
  endpoint from different schemas, and runtime secrets are injected at mount - scoped to the nests a
  worker actually holds, write-only, and never baked into a content-addressed bundle. A worker **pulls
  the nests it is assigned** from a registry, because the machine the scheduler picks may have nothing
  on disk; with a `bundle_hash` pinned the fetch is by **content address**, so re-tagging a version in
  a registry cannot change what a fleet runs. This is the
  **self-hosted distributed** path for one operator's cooperating nests; per-tenant billing and authz
  between untrusting paying customers stay firmly out of scope.
- **Nest bundles + registry - bundle one, publish it, load it anywhere.** `nuthatch nest bundle` packs
  a nest's authored inputs into one portable, content-addressed `.bundle`; `nest load <bundle-or-url>`
  verifies and installs it - regenerating the decode registry and asserting it matches - so anyone runs
  your *exact* nest, hash-verified. Share at scale with a **registry** (RFC-0019): `nest publish <bundle>
  --registry <path|s3://…> --as name@version`, then `nest load name@version --registry …` - a filesystem
  path or any S3-compatible bucket (MinIO/S3/R2, via `AWS_*` env), with **private nests** behind your
  bucket's auth. Self-hosted-first: the registry is decoupled and never mandatory - a self-built bundle
  and `load <file|dir>` need no registry at all. S3/MinIO/R2 is built in - configure it with the usual
  `AWS_*` env (`AWS_ENDPOINT` for non-AWS), verified live against Hetzner Object Storage.
- **Safe upgrades - no resync tax** (RFC-0020, RFC-0033). Updating a nest is not a subgraph-style
  genesis resync, and in 2.0 it needs no command to remember. The **runtime** classifies the update
  when a nest's identity changes: *compatible* (additive only) is applied, *breaking* (a
  consumer-observable change - a dropped column, a removed table) is **named and refused** until you
  say `--allow-breaking`. Grafting does the rest: a **cosmetic** edit - a comment, a renamed view, a
  doc change - moves the nest's identity and **adopts the existing dataset**, so nothing re-indexes.
  Segments are content-addressed and shared across the runtime, so **two nests that decode the same
  contract hold one copy**, not two. What a subgraph pays a full resync for, nuthatch answers with a
  hash comparison.
- **Derive-first - the `eth_call` you don't need** (RFC-0023). >70% of subgraphs call `eth_call` for
  reads that are *derivable* from the events they already index - they fetch only because they have no
  way to derive. Nuthatch does: `nuthatch recipe add total_supply` drops in a SQL view
  that computes an ERC-20's `totalSupply()` as Σ minted − Σ burned from Transfer events - deterministic,
  free, no archive node. That view runs at query time; it is not a DBSP circuit. It derives what a
  subgraph pays an archive node to fetch. For the handful of
  reads that *aren't* derivable but never change - `decimals`/`symbol`/`name` - `nuthatch metadata fetch`
  calls once and caches forever.
- **Ingestion that survives real providers** (RFC-0028). An oversized `eth_getLogs` is split and
  retried, taking the provider's own suggested range when it offers one; a failure we *cannot* classify
  is split once anyway, so an endpoint whose phrasing we have never seen still works rather than
  stalling. Rate limits, transport blips and credential rejections are told apart - a rejected API key
  is cooled down loudly instead of retried forever. And sealed segments now flush on a boundary derived
  from the **data**, not from wherever a fetch window happened to stop, so two operators indexing the
  same range produce byte-identical segments regardless of their RPC tuning.
- **Metrics.** Prometheus `/metrics` - tip lag, rows decoded/sealed, reorgs, query counts, RSS.

---

## Running it in production

nuthatch is built to be **fronted**, not exposed raw - gateways, auth, and metering are the operator's
layer; nuthatch ships the *guards* (query timeout, row cap, result-byte cap, concurrency limit, a
filesystem-access denylist on `/sql`) and *signals* (`/metrics`) that make fronting it safe. It binds `127.0.0.1` by
default; `--listen` elsewhere and put a gateway in front. See [`docs/operators.md`](docs/operators.md).

- **Footprint:** ≤2 GB RAM per active-chain cursor, single static binary, graceful SIGTERM shutdown with
  checkpointed resume.
- **Durability:** content-addressed segments are safe to copy while running; back up the nest directory.
- **`dev` is the serve command** - it backfills, follows the tip, and serves in one process.
  Copy-paste **systemd** and **Docker** recipes are in [`docs/operators.md`](docs/operators.md#deploy-recipes).
- **Outgrown one machine?** [Scaled mode](docs/operators.md#scaled-mode-a-fleet-across-machines-rfc-0022)
  spreads cursors across a writer pool with an independently-scaled serving tier. Reach for it when a
  single box cannot hold your cursors inside its RAM budget - not before, because several nests on one
  machine is simpler and that simplicity is the point of the embedded path.

**[`docs/operators.md`](docs/operators.md) is the full operating guide**, and worth reading before you
run this for real rather than after. **[`docs/verification.md`](docs/verification.md)** is its
counterpart: an acceptance runbook that *proves* a deployment works, step by falsifiable step, and says
plainly which levels we have verified ourselves and which we have not.

**Still deciding whether to trust it at all?** [`docs/kicking-the-tyres.md`](docs/kicking-the-tyres.md)
is written for that: a guide to *falsifying* nuthatch rather than confirming it, with the cold walk,
correctness against a public subgraph, what it costs to keep running, a red-team pass on `/sql`, and a
section listing where we have already been wrong - including two of our own security patches and an
open finding. We would rather you found the next one than a user did.

The guide covers the questions people actually hit:

| If you're wondering | Go to |
|---|---|
| how do I tune backfill against my RPC's limits? | [configuration surface](docs/operators.md#configuration-surface) - `--window`, `--concurrency`, `--seal-direct` |
| what do I scrape, and what should page me? | [observability](docs/operators.md#observability) - metrics, alerts, health vs readiness |
| what happens when something breaks? | [the failure model](docs/operators.md#the-failure-model) and the [runbook](docs/operators.md#runbook) |
| how do I back this up? | [data lifecycle](docs/operators.md#data-lifecycle) |
| how do I run an unlisted chain? | [running an unlisted EVM chain](docs/operators.md#running-an-unlisted-evm-chain) |
| what isn't finished yet? | [known gaps](docs/operators.md#known-gaps) - stated plainly |

---

## What a stable release means here

A major version is a promise about **stability**, not a claim of completeness.

- **Semantic versioning.** Within a major version we do not rename or remove a CLI flag, an HTTP route, a config
  key, or a generated column without a major bump. The one thing that has never needed a promise is
  on-disk state: a newer binary has always read an older release's hot store and sealed segments as
  they are, and that stays true.
- **Upgrades are a binary swap.** No data migration, no re-backfill, no conversion step. Proven on a
  production box across 0.3.0 → 0.6.0 → 0.7.2, and in CI on every release since.
- **MSRV 1.95**, measured rather than asserted - it is what CI, `rust-toolchain.toml` and the release
  build all use. (Before 1.0 this file claimed 1.85, which `cargo +1.85.0 check` refutes in one
  command. A version nobody tests is not a promise.)
- **Embedded mode is the production path.** `dev` runs in production today, whether it is hosting one nest or many. **Scaled mode
  is built and verified across real machines, but younger** - and until 0.9.3 its writer pool did not
  index at all. If one process per box is enough, that is still the shape to reach for.

**What is deliberately not here:** a hosted service, a token, telemetry, non-EVM chains, or any
deployment story beyond binary + compose. Those are not backlog items; they are out of scope.

## Security

nuthatch binds `127.0.0.1` by default and is built to be **fronted**. Before you expose `/sql` to
anyone you do not trust, read [`SECURITY.md`](SECURITY.md) - and be on a current release:

- **v0.9.3** fixes an **arbitrary file read** on `/sql`. DuckDB accepts a *quoted* function name and
  the guard only matched an unquoted one, so `SELECT * FROM "read_csv"('/etc/passwd')` executed. Every
  earlier release is affected.
- **v0.6.2** fixes an **arbitrary file write** on `/sql` via `;`-stacked `COPY … TO`.

Both have published advisories on the repo's Security tab. The full pre-1.0 adversary pass, including
the findings we closed as *not ours to fix* and why, is in
[`docs/security-audit-2026-07-31.md`](docs/security-audit-2026-07-31.md).

---

## Project

- **Design** lives in [RFCs](docs/rfcs/) (0001-0036); the north star and the CLI/UX direction are
  [RFC-0015](docs/rfcs/0015-the-delightful-core.md). Deferred/leftover work is in
  [`docs/backlog.md`](docs/backlog.md); the running log is [`docs/progress-log.md`](docs/progress-log.md).
- **Governance:** a grant-funded public good (NLnet / EF-ESP). No hosted service, no token, no
  phone-home. See [`GOVERNANCE.md`](GOVERNANCE.md) and the standing design brief [`CLAUDE.md`](CLAUDE.md).
- **Out of scope:** a hosted/metered service, non-EVM chains before EVM is airtight, or any deployment
  story beyond binary + compose.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
work by you shall be dual licensed as above, without any additional terms or conditions.

---

<p align="center"><i>be your own indexer.</i></p>
