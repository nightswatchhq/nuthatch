# nuthatch

> **Turn any contract into a local SQL database.**
> One command. One tiny binary. Your box, your data - no subgraph to author, no Postgres to run, no
> monthly bill, no third-party API.

[![ci](https://github.com/nuthatch-indexer/nuthatch/actions/workflows/ci.yml/badge.svg)](https://github.com/nuthatch-indexer/nuthatch/actions/workflows/ci.yml)
· Website: [www.nuthatch-indexer.com](https://www.nuthatch-indexer.com)

```sh
cargo install --git https://github.com/nuthatch-indexer/nuthatch nuthatch

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
# from source (any platform with a Rust toolchain)
cargo install --git https://github.com/nuthatch-indexer/nuthatch nuthatch
```

Prebuilt binaries (macOS Apple Silicon, Linux x86_64) ship with each release, or install in one line:
`curl -fsSL https://nuthatch-indexer.com/install.sh | sh`.

**Chains.** Ethereum, Arbitrum One and Base are *built in*, with keyless public endpoints and tuned
finality settings - **omit `--chain` and nuthatch probes each for your contract's bytecode and picks the
one it lives on.** Point at your own node with `--rpc`.

**Any other EVM chain works too** - World Chain, Base Sepolia, your own devnet - by supplying the chain
id and an RPC endpoint yourself. See
[running an unlisted EVM chain](docs/operators.md#running-an-unlisted-evm-chain); the short version is
that `dev`, `sql` and `bench` are chain-agnostic, while `init` currently only scaffolds the three
built-in chains, so an unlisted chain means writing `nuthatch.toml` by hand (a dozen lines).

### A word on the free public RPCs

nuthatch ships with free public endpoints per chain so that `init` → `dev` works with **zero setup** -
that is the two-minute demo, and it is deliberate. They are fine for trying it out, following the tip of
a low-traffic contract, or a modest recent-history backfill.

They are **not** fine for real work, and it is better to hear that here than to discover it at 3am:

- **They are rate-limited and shared.** You are queueing behind everyone else using the same free tier
  from the same IP range. Throughput varies by the hour.
- **They fail intermittently, and not always loudly.** A rate-limited endpoint may return an empty
  result rather than an error. nuthatch fails over across the pool and retries, but a window that every
  endpoint refuses will stall until one recovers - `/ready` reports `stalled` when that happens.
- **Deep backfills will crawl or stop.** Full history over a busy contract means millions of
  `eth_getLogs` calls. Expect a free endpoint to throttle you long before that finishes.
- **No archive guarantees.** Many free endpoints prune old state, so a backfill from a 2020 deploy block
  can simply fail partway.

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
event's fields plus `block_number`, `block_timestamp`, `tx_hash`, `log_index`, `address`.

```sh
# one-shot from the terminal (prints an aligned table; --json to pipe to jq)
nuthatch sql 'SELECT "from" AS sender, count(*) AS n FROM usdc__transfer GROUP BY 1 ORDER BY n DESC LIMIT 5'

# or over HTTP, against a running `nuthatch dev`
curl 'localhost:8288/sql?q=SELECT%20count(*)%20FROM%20usdc__transfer'
```

- **`nuthatch sql`** queries the local store when `dev` is stopped, and transparently falls back to the
  running instance's API when `dev` holds it - the same command works either way.
- **Hot + cold in one surface.** Queries span the live unsealed tip (redb) *and* sealed history
  (Parquet), transparently - you never think about the boundary.
- **Big-int friendly.** `uint256` values are exact text; each also gets a `{col}_dec` DECIMAL view, so
  `SUM(value_dec)` just works.
- **AI-native.** A Model Context Protocol server is compiled in (`nuthatch mcp`) - point Claude (or any
  MCP client) at your indexer and ask your contract's data in plain English, fully offline.

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
itself - `init`, config, factories, compliance, roosts, troubleshooting - before you even have a nest:

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
- **Factory / dynamic contracts** (RFC-0009). Watch a factory (e.g. a pool factory); children are
  discovered at runtime and indexed into shared `{template}__*` tables - no redeploy per child.
- **Declarative + imperative derivation.** Incremental views maintained by DBSP (reorgs become
  retractions), plus a WASM transform layer for custom pure-function pipelines.
- **Compliance pack** (RFC-0008). Address labels, sanctions/watch-list screening, threshold & velocity
  flags, counterparty-exposure views, and a signed, replayable audit manifest.
- **Alerts & webhooks** (RFC-0010). HMAC-signed egress with a durable at-least-once outbox; a slow
  endpoint never blocks indexing.
- **Built-in admin UI.** A self-contained page at `/_admin/` - status, tables, view/nest inspector.
  Localhost-open; off-localhost it requires a token per request.
- **Roost - many nests, one runtime, one or more chains** (RFC-0012, RFC-0021). Host many nests in one
  process; nests on the same chain share a single cursor and one `getLogs` per window (N nests for
  roughly one nest's RPC cost), and a roost can span **multiple chains** with **one isolated cursor per
  chain** - a Base nest and an Arbitrum nest in one runtime. Per-nest isolation, and a footprint budget
  **per active-chain cursor** (≤2 GB). A capability, not a mandate: one-chain-per-roost stays the simple
  default.
- **The live roost - mount and unmount nests without a restart** (RFC-0027). Changing a roost's nest set
  used to mean editing `roost.toml` and restarting, which stops every *co-tenant* nest too - so the blast
  radius of a config change was larger than that of a fault. Now `POST /_admin/nests` mounts one and
  `DELETE /_admin/nests/<name>` unmounts one, live. A mount is admitted only if it fits the cursor's RAM
  budget (refused with `507`, never a warning - a budget that can be quietly exceeded is not a budget),
  catches up *before* it joins so it never drags co-tenants back through history, and only then gets
  routes. An unmount is a **drain**, not a route removal: the cursor finishes its window and releases
  the store before anything is torn down. The set is persisted to `roost.toml`, so a restart converges
  on what you last asked for.
- **Nest bundles + registry - bundle one, publish it, load it anywhere.** `nuthatch nest bundle` packs
  a nest's authored inputs into one portable, content-addressed `.bundle`; `nest load <bundle-or-url>`
  verifies and installs it - regenerating the decode registry and asserting it matches - so anyone runs
  your *exact* nest, hash-verified. Share at scale with a **registry** (RFC-0019): `nest publish <bundle>
  --registry <path|s3://…> --as name@version`, then `nest load name@version --registry …` - a filesystem
  path or any S3-compatible bucket (MinIO/S3/R2, via `AWS_*` env), with **private nests** behind your
  bucket's auth. Self-hosted-first: the registry is decoupled and never mandatory - a self-built bundle
  and `load <file|dir>` need no registry at all. S3/MinIO/R2 is built in - configure it with the usual
  `AWS_*` env (`AWS_ENDPOINT` for non-AWS), verified live against Hetzner Object Storage.
- **Safe upgrades - no resync tax** (RFC-0020). `nuthatch nest diff <old> <new>` classifies a nest
  update as *compatible* (additive only) or *breaking* (a consumer-observable change); `nuthatch nest
  upgrade --to <new>` then handles either kind. A **compatible** update is **hot-swapped with zero
  downtime** - it serves the old version, indexes the new one concurrently, and atomically flips the
  endpoint the moment the new one catches up, so the served address never changes. A **breaking** update
  instead serves the new version on a new endpoint (`/next`) alongside the old - which keeps working, now
  carrying a `Deprecation` header - so downstream migrate on their own clock before the old is sunset.
  Either way, updating a nest stops being a subgraph-style genesis resync - and when a compatible
  update's decode is unchanged, the new version **mounts the old's sealed content-addressed segments**
  instead of re-indexing history at all: a true no-re-index upgrade subgraphs structurally can't do.
- **Derive-first - the `eth_call` you don't need** (RFC-0023). >70% of subgraphs call `eth_call` for
  reads that are *derivable* from the events they already index - they fetch only because they have no
  incremental-view engine. Nuthatch does: `nuthatch recipe add total_supply` drops in a derived view
  that computes an ERC-20's `totalSupply()` as Σ minted − Σ burned from Transfer events - deterministic,
  free, no archive node. It derives what a subgraph pays an archive node to fetch. For the handful of
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

**[`docs/operators.md`](docs/operators.md) is the full operating guide**, and worth reading before you
run this for real rather than after. It covers the questions people actually hit:

| If you're wondering | Go to |
|---|---|
| how do I tune backfill against my RPC's limits? | [configuration surface](docs/operators.md#configuration-surface) - `--window`, `--concurrency`, `--seal-direct` |
| what do I scrape, and what should page me? | [observability](docs/operators.md#observability) - metrics, alerts, health vs readiness |
| what happens when something breaks? | [the failure model](docs/operators.md#the-failure-model) and the [runbook](docs/operators.md#runbook) |
| how do I back this up? | [data lifecycle](docs/operators.md#data-lifecycle) |
| how do I run an unlisted chain? | [running an unlisted EVM chain](docs/operators.md#running-an-unlisted-evm-chain) |
| what isn't finished yet? | [known gaps](docs/operators.md#known-gaps) - stated plainly |

---

## Project

- **Design** lives in [RFCs](docs/rfcs/) (0001-0028); the north star and the CLI/UX direction are
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
