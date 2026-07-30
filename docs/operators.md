# Running nuthatch as an operator

Everything needed to run nuthatch on your own infrastructure, or on someone else's behalf: how to
deploy it, what it demands of you, how it fails, what to scrape, what to back up, and where it is
still honestly unfinished.

**Two audiences, one document.** If you are putting nuthatch on a box, start at
[Deploy recipes](#deploy-recipes). If you are a platform team deciding whether to adopt it, read
[The division of labour](#the-division-of-labour), [Known gaps](#known-gaps), and the
[Go-live checklist](#go-live-checklist) first.

**Companions:** [`prod-readiness.md`](prod-readiness.md) is the *release* gate - what must be true
before a build ships. This document is the *run* guide - what must be true in your environment.
[`backlog.md`](backlog.md) and the [RFC index](rfcs/README.md) say what is deferred and why.

Written against **0.8.2** (2026-07-30). Read [Known gaps](#known-gaps) before exposing `/sql`.

---

## The division of labour

nuthatch is built to be **fronted**, not exposed raw. The dividing line, stated once and kept:

> **The node owns resource safety. The gateway owns access policy.**

**nuthatch provides:** deterministic indexing, isolated per-nest storage, a bounded query surface,
per-nest health and metrics, and signals rich enough to alert, capacity-plan, and bill against.

**Your platform provides:** identity, authentication, authorisation, per-caller rate limits, quotas,
metering, billing, and TLS. There are no accounts and no tenancy inside the binary, and there will not
be: `CLAUDE.md` puts hosted-SaaS multi-tenancy out of scope permanently.

**What it never does:** phone home. No telemetry, no mandatory API tokens, no gated data service.
Every outbound connection is one you configured - your RPC endpoints, your webhook sinks, and (only
when you run them by hand) the ABI resolvers and sanctions-list fetchers.

---

## Deploy recipes

`nuthatch dev` **is** the serve command - it backfills, follows the tip, and serves the API in one
process. "I tried it locally" to "it's running on my box" is just running that under a supervisor.

### systemd

```ini
# /etc/systemd/system/nuthatch.service
[Unit]
Description=nuthatch indexer
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=nuthatch
WorkingDirectory=/var/lib/nuthatch/mynest        # the nest directory (holds nuthatch.toml)
ExecStart=/usr/local/bin/nuthatch dev --listen 127.0.0.1:8288 --seal-direct --concurrency 8
Restart=on-failure
RestartSec=5
# Off-localhost the admin UI requires this; unset it and bind 127.0.0.1 to disable remote admin.
Environment=NUTHATCH_ADMIN_TOKEN=change-me
# Keep it inside the footprint budget; the box needs headroom for DuckDB queries.
MemoryMax=2G

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload && sudo systemctl enable --now nuthatch
journalctl -u nuthatch -f          # a clean progress line during backfill, then quiet tip-following
```

### Docker

A container image is published per release:

```sh
docker run -d --name nuthatch --restart unless-stopped \
  -v "$PWD/mynest:/nest" -p 127.0.0.1:8288:8288 \
  -e NUTHATCH_ADMIN_TOKEN=change-me \
  ghcr.io/nightswatchhq/nuthatch:0.8.2
```

The image **ships the same binary attached to the GitHub Release** rather than a separate from-source
build, so the two cannot drift. It runs as an unprivileged user (uid 10001), carries only
`ca-certificates` beyond the binary, and mounts the nest directory at `/nest` - the only writable state.

The default command binds `0.0.0.0:8288` *inside* the container; publish it to `127.0.0.1` on the host
as above and put a reverse proxy (TLS + auth) in front, the same posture as bare metal. `docker stop`
sends SIGTERM, which drains and checkpoints cleanly.

`linux/amd64` only for now - a multi-arch image needs an aarch64-linux build we do not yet produce.
Pin the version tag rather than `:latest` for anything you care about.

**Scaled mode needs the `-scaled` image**, not this one. The default image is the embedded build and
carries no database driver. `nuthatch worker` and `nuthatch control` are still *listed* in its `--help`
- the CLI surface is shared - but running either gives you a refusal naming the feature flag rather
than a mysterious failure:

```
Error: the writer-worker role needs a build with `--features postgres-store`. The default
binary is the embedded one and carries no database driver (CLAUDE.md non-negotiable 1).
```

That is deliberate: a subcommand that vanishes from `--help` depending on how the binary was built is
harder to diagnose than one that explains itself. Use the scaled artifact and it works:

```sh
docker run --rm ghcr.io/nightswatchhq/nuthatch:0.8.2-scaled worker --help
```

Two images rather than one because non-negotiable 1 says the primary artifact runs with zero external
services - a binary carrying a Postgres driver is a different promise even when it behaves identically
unused. `-scaled` is deliberately **not** tagged `latest`: the default image stays the one you get by
not thinking about it. The release also attaches `nuthatch-scaled-x86_64-unknown-linux-gnu.tar.gz` for
anyone deploying without containers.

> **Do not use `:0.7.0`.** It is published but cannot start (`GLIBC_2.38 not found`) - the release job
> pushed before it smoke-tested, so the failing test failed the job without unpublishing the image.
> `0.7.1` is the first working tag, and the job now tests before it pushes. `:latest` was broken for the
> same reason and is fixed by `0.7.1`.

**glibc floor.** The Linux binary is dynamically linked and built against **glibc 2.35**, so it runs on
Ubuntu 22.04+, Debian 12+, RHEL 9+ and anything newer. It is built on a pinned runner rather than
`ubuntu-latest` precisely so that floor does not drift upward unnoticed - which it had, to 2.39, until
the container image's smoke test caught it.

To build it yourself instead:

```sh
cargo build --release
cp target/release/nuthatch . && docker build -t nuthatch .
```

---

## Deployment model

One static binary. No Postgres, no Docker daemon, no IPFS, no sidecar. State is a directory.

| Topology | Command | Shape |
|---|---|---|
| **Nest** (one indexer) | `nuthatch dev --dir <nest>` | one chain, one cursor, one API at `/` |
| **Roost** (many nests) | `nuthatch roost dev --dir <roost>` | N nests across one **or more** chains, one isolated cursor per chain, each nest's full API under `/<name>/` |

A **cursor** is the unit that matters. It is always single-chain, single-writer, and one observable
failure boundary. A roost hosting nests on Ethereum and Arbitrum runs two cursors in one process, each
with its own tip, finality view, reorg handling, and RSS budget. Two chains are never multiplexed
behind one cursor; the runtime refuses to.

Multichain is a **capability, not a mandate** - one chain per roost stays valid and is the simplest
default.

### Scaled mode: a fleet across machines (RFC-0022)

Everything above is **embedded mode**, and it is still the primary deliverable: one binary, no
external services, the thing `curl | sh` gets you. Scaled mode is a different deployment for a
different problem - one operator running many nests across many machines - and it is **opt-in at
build time** (`--features postgres-store`). The published binary does not carry a database driver.

Reach for it when a single box can no longer hold your cursors inside its RAM budget, or when serving
load and ingestion load want to scale independently. Not before: a roost on one machine is simpler,
and simplicity is the point of the embedded path.

| Role | Command | Owns |
|---|---|---|
| **Control plane** | `nuthatch control --db <postgres>` | *desired state* - what should run |
| **Writer** | `nuthatch worker --control-db … --hot-store … --chains …` | cursors it holds a **lease** on; ingests, decodes, seals |
| **Query-FE** | `nuthatch serve --dir <nest> --hot-store <postgres>` | nothing - serves from shared state |

```sh
docker compose -f docker-compose.scaled.yml --profile fleet up \
  --scale writer=2 --scale fe=3
```

**The three ideas worth understanding before you run it:**

**1. The control plane states intent; it commands nothing.** `POST /nests` records that a nest should
run and returns 200. That does **not** mean it is running - it means the fleet has been told to. A
writer picks it up on its next tick. There is deliberately no "start this nest on worker w3" endpoint,
because that would be the one call able to put a cursor somewhere the scheduler did not choose and the
lease did not arbitrate.

**`worker` is not `dev`.** `dev` indexes a nest directory it owns outright and knows nothing about a
control plane; running two of those against one store is the double-writer bug rather than a pool.
`worker` reconciles - heartbeat, take a lease per assigned cursor, index only what it holds. A worker
declares the chains it *can* host (`--chains`); the scheduler decides which it *should*, and the lease
decides which it *does*. Its id defaults to the hostname, and it **refuses to start** if it cannot
determine one, because two workers sharing an id look like a single worker to the registry.

**2. Ownership is a lease, and the store enforces it.** A cursor is held by exactly one writer at a
time. If a writer stalls - long GC, paused container, a host that goes away - its lease expires and
another writer takes over. When the original wakes up, its writes are **refused by the store**, not
merely discouraged: every write carries a fence, and a stale fence is rejected inside the same
transaction as the write. This is why `--scale writer=N` is safe.

**3. The control plane and the lease are independent, on purpose.** A control-plane outage stops
*rescheduling*, not *ingestion* - writers keep their leases and keep working. It follows that the two
can legitimately disagree: a writer whose heartbeat has lapsed but whose lease is live keeps its
cursor, and the scheduler's wish to rehome it is refused. That is correct, not a bug. A plan is not
permission.

**Answering "why is my nest not running?"** - `GET /plan` runs the same placement logic the writers
run and reports what could not be placed *and why*:

```json
{"assign":[{"chain":"mainnet","worker":"writer-1"}],
 "unplaceable":[{"chain":"base","rss_mb":2400,"reason":"toolargeforanyworker",
   "detail":"this cursor alone exceeds the largest worker's budget - adding workers will not help"}]}
```

The two unplaceable reasons demand different actions: `noroomrightnow` is fixed by adding a worker,
`toolargeforanyworker` never is. Adding capacity for the second would be money spent on nothing.

**Versions are pinned fleet-wide.** Every FE node resolves an endpoint through the control plane
(`PUT /nests/<name>/pin`), not through the registry's movable `latest`. If each node read `latest`
itself, then during an upgrade one node would serve the new schema while another served the old, and
the same endpoint would answer differently depending on where the load balancer sent the request. A
declared-but-unpinned endpoint is explicitly **not servable** - an FE refuses rather than guesses.

**Secrets never enter a bundle.** Private RPC URLs and API keys live in the control plane keyed by
nest (`PUT /nests/<name>/secrets`), and a writer receives only the secrets of the nests it is
assigned. The interface is **write-only**: you can list which keys exist, never read a value back.
Rotating a secret changes no bundle hash, so it does not invalidate segment reuse or force a
re-index.

**What scaled mode does not do**, and will not: per-tenant billing, metering, quotas, or authz between
mutually-untrusting paying customers. That is a gateway's job in front of nuthatch, and deliberately
out of scope - see RFC-0022 §6.

```
roost/
  roost.toml              # desired state: chains + mounted nest names + budget
  nests/
    lodestar/             # a nest dir exactly as `nuthatch init` produces
      nuthatch.toml       # contracts, events, factories, webhooks, alerts
      semantic.toml       # what the data means (drives MCP + SQL hints)
      schema.json         # generated: registry hash + table list
      abis/               # vendored ABIs (no runtime resolution)
      views/              # authored SQL views
      checks/             # invariant/parity checks for `nuthatch check`
      nuthatch.redb       # hot store (mutable, reorg-affected)
      segments/           # sealed Parquet, content-addressed, immutable
      manifest.json       # segment catalogue + sealed watermark
```

The nest directory is the *entire* state. Move it, copy it, snapshot it.

**Serving in a roost.** `GET /nests` is the roster. Every nest's full API lives under its prefix:
`/<name>/tables`, `/<name>/sql`, `/<name>/_admin/`. `/sql` stays per-nest scoped - a query sees one
nest's data. Stores are per-nest; only the cursor is shared. Static and factory nests co-exist, may
mount at different heights, and each backfills its own history; the cursor only couples them at tip.

Nests live in their own repositories rather than in-tree; see the
[nest catalogue](nest-catalogue.md) for what ships and what is planned.

> **Scaled mode does not exist yet.** The docker-compose topology with Postgres and DataFusion
> federation (RFC-0013, RFC-0022) is designed and not built. Today, horizontal scale means more
> processes on more boxes, each owning its own nests.

---

## Capacity and sizing

**The budget is per active-chain cursor: 2 GB RAM.** A roost's total is the sum of its cursors. This
is a CI-enforced ceiling, not an aspiration, and `roost dev` **refuses to start** a cursor whose
projected footprint exceeds `max_rss_mb` (default 2048).

The projection model (deliberately rough):

| Component | MB |
|---|---|
| runtime base, paid once per process | 120 |
| each nest: hot store + decode registry + balance view | 90 |
| each additional IVM view (exposure, velocity) or factory child registry | 40 |

**Measured reality is far below the projection.** A single-chain ERC-20 nest at tip measures ~37 MB
resident; a two-nest roost measures ~110 MB against a ~300 MB projection. Provision against the
measurement (`nuthatch_rss_bytes`, also reported as `rss_bytes` on `GET /nests`) and treat the
projection as a guard rail rather than a sizing tool.

**What actually drives memory:**

- **Deep-finality chains.** The hot store holds everything between the sealed watermark and the tip.
- **`/sql` on a hot table.** The current query path materialises the whole tip per query. The single
  largest RAM risk on a busy nest (see [Known gaps](#known-gaps)).
- **Factory nests.** A template with many discovered children carries a larger child registry.

**Measure before committing.** `nuthatch bench backfill` reports events/sec, wall clock and peak RSS
over a pinned range; `nuthatch bench query` reports entity point-read p50/p99 plus the `/sql` hot-scan
cost and RSS. Run both against a representative nest on your hardware and your RPC before sizing a
fleet.

**Disk.** Sealed Parquet is Snappy-compressed and content-addressed. Growth is proportional to decoded
events, not chain history: a nest tracking a few events on a few contracts stays small.

---

## Configuration surface

**Files:** `roost.toml` (chains, mounted nests, budget), `nuthatch.toml` per nest (contracts, events,
factories, screening, flags, webhooks, alerts), `semantic.toml` per nest (descriptions for the AI and
SQL surfaces). Full key reference:
[`config-reference.md`](../skills/nuthatch-builder/config-reference.md).

**Environment:**

| Variable | Purpose |
|---|---|
| `NUTHATCH_ADMIN_TOKEN` | required for the admin UI when bound off-localhost; presented as `?token=` (and, from the next release, `Authorization: Bearer`) |

**Runtime flags that matter operationally** (`dev` and `roost dev`):

| Flag | Use |
|---|---|
| `--listen` | bind address. Defaults to `127.0.0.1:8288` |
| `--rpc` | override configured endpoints without editing config. Repeatable. Single-chain roosts only (ambiguous once a roost spans chains) |
| `--seal-direct` | backfill finalised history straight to Parquet, bypassing the hot store. Much faster from deployment |
| `--concurrency` | concurrent window fetches during seal-direct backfill. 8-16 against your own node; low on rate-limited public RPC |
| `--window` | override the `eth_getLogs` block window. A *sparse* contract wants a large window (50k) to turn thousands of near-empty requests into a few. Keep under your provider's range cap |
| `--backfill N` | index only the last N blocks (recent-history mode) |
| `--no-admin` | remove the admin UI routes entirely. Use it when you front your own dashboard |
| `--fail-fast` | exit on first fault instead of quarantining. For CI and operators who prefer fail-stop |

**`block_timestamps` - decide it at `init`, because you cannot change it later.**

Every table carries an implicit `block_timestamp`, and fetching it is the single most expensive thing
a backfill does: timestamps live in the block header, which `eth_getLogs` does not return, so they
cost a separate `eth_getBlockByNumber` per distinct block. On the workloads we measured that is
roughly **85% of backfill wall clock** (RFC-0029 §4). `nuthatch init --no-timestamps` drops the column
and stops paying for it.

Read the next paragraph before you reach for it.

This is **not a tuning flag**. Turning it off removes a column from every table, which is a breaking
change for anyone querying it, and it changes the bytes of every sealed segment - segments are
content-addressed, so a nest that switches **cannot reuse its own history and must re-index from
scratch**. For an existing nest, "faster backfill" and "re-index everything once" largely cancel out.
The win is real, but it is a **new-nest** win.

Because of that, the runtime enforces it rather than trusting the file. A nest that has already
indexed refuses to start if the declaration disagrees with its stored data, and a timestamp-free nest
is stamped `schema_version = 2` so an older nuthatch refuses it outright instead of quietly indexing
timestamps into a store built without them. Changing your mind means a *new* nest, served alongside
the old one until its consumers move - the ordinary breaking-update path (RFC-0020 slice 3).

A nest that indexes timestamps stays `schema_version = 1` and is readable by 0.8.x, so nothing about
this affects you unless you opt in.

**When it is clearly right:** you know every consumer, none of them ask a time-series question, and
you are standing up a new nest over a long history. **When it is clearly wrong:** anyone might want
"per day" or "per hour" later. Blocks give you ordering; only timestamps give you time. If you are
unsure, keep them - the default is on for a reason.

**Secrets.** Private RPC URLs and webhook HMAC secrets live in the nest's `nuthatch.toml`. The rule
(RFC-0019 §4) is that secrets never go into a published bundle. Per-nest secret injection at mount
time is designed (RFC-0022 §5) and not built. Until then: keep private endpoints in the on-disk
config, keep the directory `0700` and owned by the service user, and never publish a bundle built from
a config carrying a credential.

---

## Running an unlisted EVM chain

Ethereum mainnet, Arbitrum One and Base are **built in**: keyless public endpoints, a tuned
`eth_getLogs` window, chain-appropriate finality, and bytecode probing so `init` can detect which of
them a contract lives on.

**Any other EVM chain also works** - it just has to be configured by hand. Teams have run nuthatch on
World Chain (`480`) and Base Sepolia (`84532`) this way. The split is worth knowing exactly:

| Command | Unlisted chain? |
|---|---|
| `dev`, `sql`, `bench`, `roost dev` | **yes** - chain-agnostic, falls back to defaults |
| `init`, `add` | **no** - they refuse an unrecognised `--chain`/config chain with "unknown chain … cannot resolve ABIs" |

So the working recipe is: scaffold nothing, write `nuthatch.toml` yourself, vendor the ABI, and run.

```toml
[nest]
name = "my-nest"
chain = "world-chain"        # any label you like - it is not looked up for an unlisted chain
chain_id = 480               # MUST match what your endpoints report; verified at startup
rpc_urls = ["https://your-endpoint.example"]
schema_version = 1

[[contracts]]
alias = "router"
address = "0x…"
start_block = 1234567        # no bytecode probing here, so supply it yourself
abi = "abis/router.json"     # vendor the ABI by hand; Sourcify/Etherscan lookup is chain-gated
events = ["Swapped"]         # optional allowlist
```

Then `nuthatch dev --dir .` as usual. Everything downstream - decode, sealing, `/sql`, views, MCP,
roosts, bundles - is chain-agnostic and behaves exactly as it does on a built-in chain.

**Two caveats that are easy to miss:**

1. **You inherit default finality and window.** An unlisted chain gets `Depth(64)` finality and a
   **20-block** `eth_getLogs` window, because nuthatch has no per-chain policy for it. Depth-64 is the
   Ethereum-L1-shaped assumption; if your chain finalises differently - a fast L2, or one with deeper
   reorgs - that default is a guess, and the conservative direction is *deeper*. The 20-block window is
   deliberately small and will make a long backfill crawl: raise it with `--window` (a sparse contract
   can often take 50000) up to whatever your provider's range cap allows.
2. **`chain_id` is enforced.** Every endpoint in `rpc_urls` is checked against it at startup and a
   mismatch is refused, because indexing against the wrong chain corrupts state silently. Get the id
   right and the pool stays honest; get it wrong and nuthatch tells you immediately rather than three
   days into a backfill.

If you are running an unlisted chain in anger, say so - a chain with real usage is a candidate for the
built-in registry, which is where the tuned window and finality policy come from.

---

## The service surface

Per-nest routes. In a roost they are prefixed: `/<name>/sql`, `/<name>/tables`, and so on.

| Route | Purpose |
|---|---|
| `GET /` | summary: nest identity, heights, table count |
| `GET /health` | liveness. `200 "ok"` while the process serves |
| `GET /ready` | readiness. Per-nest: `503` if *that* nest is quarantined |
| `GET /metrics` | Prometheus text exposition |
| `GET /tables`, `GET /table/{name}` | schema and recent rows, merged hot and cold |
| `GET /schema` | the full data model |
| `GET /sql?q=…` | read-only analytical SQL over hot and sealed data |
| `GET /explain` | query plan and cost hints |
| `GET /entities`, `GET /entity/{id}` | entity point-reads, transparently across the hot/cold seam |
| `GET /balances`, `GET /balance/{address}` | the derived IVM balance view |
| `GET /exposure/{address}`, `GET /flags` | compliance surfaces (RFC-0008) |
| `GET /nest` | nest identity and registry hash |
| `GET /shape` | capability probe: what this nest can answer (drives adaptive MCP) |
| `GET /_admin/`, `/_admin/events` | admin UI. Token-gated off-localhost; removable with `--no-admin` |

**Roost root routes:** `GET /nests` (roster with live per-nest health), `GET /ready` (roost-wide),
`GET /health`.

---

### Control-plane endpoints (scaled mode only)

| Route | Purpose |
|---|---|
| `GET /nests` · `POST /nests` · `DELETE /nests/{name}` | declare/inspect/remove desired state |
| `GET /nests/{name}/resolve` · `PUT /nests/{name}/pin` | what an endpoint serves, and pinning it |
| `GET /nests/{name}/secrets` · `PUT` · `DELETE .../{key}` | key **names** only on read; write-only values |
| `GET /workers` | live workers and their budgets |
| `GET /plan` | placement, including what cannot be placed and why |
| `GET /health` | unauthenticated, for load balancers |

Bound off-localhost the control plane **refuses to start** without `NUTHATCH_CONTROL_TOKEN`. This is a
refusal rather than a warning because the endpoint decides what an entire fleet runs.

## Security posture

**Bind localhost and front it.** The API defaults to `127.0.0.1:8288`. `/sql` is guarded but **not
authenticated** - the guards below bound *how much*, never *who*. Off-localhost binds log a loud
warning at startup. Put TLS and authentication in front, always.

**Query guards** - node self-protection against a single runaway query or a burst, not per-caller
quotas (that needs identity a single-tenant node does not have):

| Guard | Default | What it bounds |
|---|---|---|
| statement timeout | 30 s | a runaway (e.g. cartesian) query is interrupted mid-flight |
| max result rows | 50,000 | the Rust-side result buffer, outside DuckDB's own memory limit |
| max concurrent queries | 2 | the real DoS multiplier: a semaphore; excess returns `503` |
| max query length | 16 KiB | rejects absurd query strings before the planner |
| max unsealed rows scanned | 2,000,000 | the tip is materialised per query; past this the query is refused with `503` rather than served partially |

`/sql` is **read-only and single-statement**: a query must open with `SELECT` or `WITH`, filesystem
and network table functions are refused, and `;`-stacking a second statement is rejected outright.
Rejections surface as `400`/`503` and count in `nuthatch_sql_rejections_total`.

**Admin surface.** Off-localhost the admin UI requires `NUTHATCH_ADMIN_TOKEN` on every request; token
comparison is constant-time. `--no-admin` removes the routes entirely rather than merely gating them.

**`/metrics` and `/health` are unauthenticated by design.** Scope them to your internal network at the
gateway if you do not want them public.

**Run it unprivileged.** A dedicated service user, `0700` on the nest directory, `MemoryMax` set to
the cursor budget. The binary needs outbound network to your RPC endpoints and webhook sinks, nothing
else.

**Supply chain.** Nest bundles are content-addressed; `nest load` verifies the manifest format, every
file's hash, and that the decode registry regenerated from the inputs matches the manifest. A nest
that does not reproduce its own decode registry is refused. Compliance packs are ed25519-signed
(`nuthatch pack keygen|build|verify`). Licensed `MIT OR Apache-2.0`; `cargo-deny` runs in CI.

---

## Observability

### Metrics

Global series (whole process):

| Series | Meaning |
|---|---|
| `nuthatch_tip_height`, `nuthatch_last_block`, `nuthatch_tip_lag_blocks` | is it keeping up |
| `nuthatch_sealed_through` | cold-layer watermark |
| `nuthatch_rows_decoded_total`, `nuthatch_rows_sealed_total`, `nuthatch_reorgs_total` | ingestion |
| `nuthatch_http_requests_total`, `nuthatch_sql_queries_total`, `nuthatch_sql_rejections_total` | serving |
| `nuthatch_rpc_requests_total` | upstream load |
| `nuthatch_rss_bytes` | process memory: the number to provision against |
| `nuthatch_last_poll_unixtime` | liveness of the ingest loop itself |
| `nuthatch_alert_outbox_depth` | webhook/alert delivery backlog |

Per-nest series, labelled `{nest="…"}` - the ones that make co-tenancy operable:
`nuthatch_nest_last_block`, `nuthatch_nest_sealed_through`, `nuthatch_nest_rows_decoded_total`,
`nuthatch_nest_rows_sealed_total`, `nuthatch_nest_reorgs_total`, `nuthatch_nest_health` (1 indexing /
0 quarantined), `nuthatch_nest_quarantine_total`, and `nuthatch_cursor_live{chain}`.

Transform-runtime counters: `nuthatch_transform_stage`, `nuthatch_transform_screen`,
`nuthatch_transform_effectful`.

### What to alert on

| Alert | Condition | Why |
|---|---|---|
| **Nest quarantined** | `nuthatch_nest_health == 0` | something is broken and a human should look |
| **Cursor dead** | `nuthatch_cursor_live == 0` | a whole chain stopped advancing |
| **Tip lag growing** | `nuthatch_tip_lag_blocks` trending up over 15m | RPC trouble or an overloaded box |
| **Ingest stalled** | `time() - nuthatch_last_poll_unixtime > 300` | the loop is wedged even if the process is alive |
| **Outbox backing up** | `nuthatch_alert_outbox_depth` rising | a webhook sink is down or slow |
| **Memory near budget** | `nuthatch_rss_bytes` over ~75% of the cursor ceiling | usually the `/sql` hot-scan |
| **Query rejections spiking** | `rate(nuthatch_sql_rejections_total)` | a caller hammering the guards; a gateway job |
| **Quarantine flapping** | `increase(nuthatch_nest_quarantine_total[1h]) > 3` | a retryable fault that never settles |

### Health versus readiness

Get this right in your supervisor and your load balancer:

- **`/health`** is liveness. `200` while the process serves. Restart on failure.
- **`/ready`** is readiness. Roost root: `200` only when **every** cursor and nest is indexing; `503`
  with a body naming what is quarantined. Per-nest `/<name>/ready` answers only for that nest.

Readiness is **advice to a supervisor, not a traffic gate**. A roost with one quarantined nest reports
`503` at the root while its healthy nests keep serving correct data on their own prefixes. Wire root
readiness to a load balancer and one sick tenant evicts every healthy tenant from rotation. **Route on
per-nest `/<name>/ready`; page on the root.**

### Logs

Structured `tracing` to stdout. Quarantine and re-admission are `warn!` with nest, chain, fault class
and the full error chain. Off-localhost binds warn at startup. Backfill prints a progress line; tip
following is quiet.

---

## The failure model

**The unit that fails alone:**

| Fault | Blast radius |
|---|---|
| One nest's decode, store, seal, view or webhook error | that nest is quarantined; siblings on the same cursor keep indexing |
| One nest's finality violation (reorg below its sealed watermark) | that nest; terminal |
| A cursor's unrecoverable error | that chain's cursor and its nests; other chains' cursors keep running |
| RPC endpoint failure | none. Round-robin failover; the dead endpoint is cooled down |
| Transient RPC errors, tip fetch failures | none. Retried with escalating stall warnings |

**Retry policy.** Retryable faults back off exponentially from 5s, doubling, capped at 5 minutes,
unbounded attempts - so restarting a wedged RPC provider recovers within minutes without anyone
typing anything. Terminal faults do not retry and stay quarantined until restart, with the reason on
`/nests`.

**Re-admission is safe by construction.** A re-admitted nest rejoins behind its siblings, pulls the
cursor's window position back, and siblings skip windows they already committed. No nest re-processes
or skips a window. The cost is a re-fetch of the intervening range, which is why the backoff cap is
minutes rather than seconds.

**The process exits in exactly three cases:** the server stops (bind failure or shutdown signal);
every cursor has been quarantined (nothing will advance again, so dying honestly under a supervisor
beats serving frozen data); or `--fail-fast` is set and anything faults. Otherwise it stays up and
degrades.

**Reorgs** only ever touch the hot store, and only that of the affected chain's cursor. Sealed
segments are written strictly past finality and are immutable, so the columnar layer never rewinds. A
reorg deeper than the sealed watermark is a terminal fault by design: finality was violated, and
silently rewriting sealed history would be worse than stopping.

**Restart safety.** SIGTERM and SIGINT drain in-flight requests and exit **0**. Progress is
checkpointed and rows are keyed by `(block, log_index)`, so a restart resumes without gaps or
duplicates.

---

## Runbook

| Symptom | First checks | Action |
|---|---|---|
| Root `/ready` is 503 | `GET /nests` - which nest, what `quarantine.reason`, what `class` | retryable: watch for auto re-admission. terminal: fix the cause, restart |
| Tip lag climbing | `nuthatch_rpc_requests_total` rate, provider status | add or replace endpoints in `rpc_urls`; lower `--concurrency` if rate limited |
| A nest stuck at a block | its `quarantine.reason` on `/nests`; logs | a `getLogs` provider cap on a busy template usually wants a smaller `--window` |
| RSS approaching the ceiling | which nest, via `nuthatch_nest_*`; recent `/sql` traffic | the hot-scan is the usual cause. Restrict `/sql` at the gateway, or move the nest to its own cursor |
| Backfill crawling | `--window` versus contract sparsity; `--concurrency` | a sparse contract wants a *large* window; a dense one wants concurrency against your own node |
| Webhook backlog | `nuthatch_alert_outbox_depth`, sink availability | delivery is sequential today; one slow sink throttles others |
| Chain-id mismatch at startup | the startup error names the endpoint | an endpoint in the pool is on the wrong network. Every endpoint is verified at boot, on purpose |
| Suspect data | `nuthatch check --dir <nest>` | runs the nest's committed invariant and parity checks against recorded fixtures |
| Need to prove a compliance result | `nuthatch audit replay --from --to` | re-runs screening over sealed segments and confirms stored hits reproduce exactly |

---

## Data lifecycle

**Backup.** The nest directory is the whole state. Sealed segments are content-addressed and
immutable, so they are safe to copy while the process runs. The hot store (`nuthatch.redb`) is a live
redb file: snapshot it at the filesystem level, or stop the process for a consistent copy. Losing the
hot store costs a re-index of the unsealed window, not history.

**Restore.** Put the directory back and start. Progress resumes from the checkpoint.

**Segment identity across versions.** A segment's hash covers the Parquet file bytes, which include
the `created_by` string stamped by the arrow-rs build. Same binary means identical bytes and identical
hashes, so re-running a backfill or running the same release on two boxes produces byte-identical,
de-duplicating segments. **Across nuthatch versions built on different arrow-rs releases, segment
identity may differ** even when every decoded row is identical. Do not use a segment hash as a
cross-version equality proof; compare decoded rows for that.

**Consistency: entity reads versus derived views.** The entity store and the derived (IVM) views
advance independently, so **a read taken during a reorg window can see transient skew between them**:
`/balances` (a view) and `/sql` (over stored rows) may briefly disagree about the same block. Both
converge within a tick; neither is wrong in isolation. This matters if you **join across the two
surfaces**, because the halves may have been taken either side of a rollback. If that skew would be
visible to your users:

- prefer a **single surface** per answer (each is individually consistent), or
- pin the read at or below `sealed_through` (reported on `/ready` and in `/sql` responses' `provenance`
  block) - sealed data is past finality and never moves.

Tip-following data is inherently provisional; this is the same caveat any indexer carries at the tip,
stated explicitly rather than left to be discovered.

**Binary upgrades.** Proven in production: a nest upgrade from 0.3.0 to 0.6.0 was a binary swap and a
restart, with no data migration and no flag changes. Within 0.x the target is in-place-safe upgrades,
and each release's notes state "in-place safe" or "reseal required" explicitly.

---

## Nest lifecycle operations

The deploy unit is a **content-addressed bundle**: config, ABIs, views, labels and skills, plus a
manifest pinning the expected decode-registry hash.

```sh
nuthatch nest bundle <dir>                        # produce a .bundle, prints its content address
nuthatch nest publish <bundle> --registry <ref>   # publish as name@version, advance latest
nuthatch nest load <ref> --registry <ref>         # pull and install, hash-verified
nuthatch nest load <bundle|url|dir> --expect <h>  # or install directly, asserting the hash
```

The registry is **decoupled** from the binary: a filesystem path or S3-compatible object storage, with
private-nest authentication. nuthatch pulls; it never becomes the registry, and resolution stays
local-first. (Live S3 verification against a real bucket is still pending an operator run.)

**Upgrading a nest without the resync tax** (RFC-0020):

```sh
nuthatch nest diff <old> <new>     # classify: compatible (additive only) or breaking
nuthatch nest upgrade --dir <old> --to <new> --listen …
```

- **Compatible** (internal changes, or purely additive schema): the new version indexes alongside the
  old, then the endpoint atomically flips. The served address never changes and consumers notice
  nothing. When the decode registry is unchanged, the old version's sealed segments are **reused**
  rather than re-indexed, so a view-only or semantic-only change costs no backfill at all.
- **Breaking** (anything a consumer observes as removed, renamed, retyped or semantically changed):
  both versions run. The old stays at the root carrying a `Deprecation: true` header and a `Link` to
  its successor; the new is served under `--new-endpoint` (default `/next`). Consumers migrate on
  their own clock.

**Adding or removing a nest in a running roost currently requires a restart**, which stops every
co-tenant nest in that runtime. This is the largest operational gap for a team onboarding nests on
behalf of others, and it is designed in [RFC-0027](rfcs/0027-the-live-roost.md). Until it lands, plan
nest onboarding as a maintenance window, or give each tenant its own process.

**Compliance operations** (RFC-0008), if you serve regulated customers:

```sh
nuthatch lists fetch ofac-sdn --dir <nest>      # content-addressed list snapshot
nuthatch screen --list <hash> --from --to       # replayable screening over sealed segments
nuthatch audit replay --from --to               # re-prove the stored hits reproduce exactly
nuthatch audit report --from --to --json        # summarise hits and flags
nuthatch pack build --key <keypair>             # signed compliance manifest
```

Screening is deterministic: the same list hash, range and component always produce identical hits.

---

## AI: point a coding agent at your data

The MCP server is compiled in. Wire a client to a running nest in one step - `nuthatch mcp
--print-config` prints exactly what to paste:

```sh
nuthatch dev &                       # the index the agent will query
nuthatch mcp --print-config          # copy-paste config for Claude Code / any MCP client
claude mcp add nuthatch -- nuthatch mcp --url http://127.0.0.1:8288
```

`nuthatch mcp` is a thin, fully-offline stdio bridge to the local `dev` HTTP API - it never contends
with the single writer and nothing phones home. The client launches it; you ask for your contract's
data in plain English and it writes the SQL. Tool advertisement is adaptive (RFC-0025): a nest only
advertises the tools it can actually answer.

---

## Stability contract (0.x)

- **Config**: `nuthatch.toml` keys and the nest `schema_version` follow a deprecation policy - a key
  removed in 0.(n+1) warns in 0.n first.
- **Data layout**: redb tables, segment layout, `manifest.json` and `schema.json` are versioned. The
  target within 0.x is **in-place-safe upgrades**; each release states which it is.

---

## Known gaps

Stated plainly, because finding them yourself in production would be worse.

**Before exposing `/sql`:**

- **0.6.1 and earlier accept `;`-stacked SQL statements**, which combined with `COPY … TO` / `ATTACH`
  is an arbitrary file-write primitive, bounded by the service user's permissions. **Fixed in 0.6.2**
  (released 2026-07-28). If you are on 0.6.1 or earlier, upgrade before exposing `/sql` to anything -
  authentication in front does not help, because the caller is already past it.

**Operational gaps:**

- **`/sql` materialises the whole tip per query**, now **bounded**: past 2,000,000 unsealed rows the
  query is refused with `503` and the response names `sealed_through` so a caller can narrow to sealed
  data. It refuses rather than truncating, because a partial tip would silently change the answer to an
  aggregate. Generous enough to be invisible on a normal chain; it exists so a deep-finality tip turns
  into a clear error instead of an OOM that takes co-tenants with it.
- **No published container image.** Build from the recipe above. For a platform team this is a real
  gap, not a nicety.
- **Secrets live in on-disk config.** Private RPC URLs and webhook HMAC secrets sit in the nest's
  `nuthatch.toml`. Per-nest, per-worker injection is specified (RFC-0022 §5) and not built, so today
  the mitigation is filesystem permissions: `0700`, owned by the service user.
- **No scaled mode.** No Postgres hot store, no writer pool, no query-FE tier, no DataFusion
  federation. All designed (RFC-0013, RFC-0022), none built.
- **No ExEx or trace/state extraction.** Colocated-reth ingestion (RFC-0003) and firehose-class
  extraction (RFC-0014) are gated on a synced node - an infrastructure decision, not a coding one.

**Correctness boundaries, documented and accepted:**

- Balances exceeding `i128` base units are omitted from derived views rather than truncated. No real
  token approaches this.
- One malformed log fails its whole `getLogs` window and retries against another endpoint rather than
  being skipped. Deliberate: silently dropping an on-chain event would be a correctness bug.
- Segment identity may differ across nuthatch versions built on different arrow-rs releases.
- Entity reads and derived views are eventually consistent within the reorg window.

---

## Proving it works

This guide tells you how to *run* nuthatch. [`verification.md`](verification.md) tells you how to
**prove it works** - an acceptance runbook where every step has a command, an expected result, what it
proves, and what a failure means.

Worth walking before go-live, and worth handing to a second operator: a claim someone else confirmed is
worth more than one we assert. It also states which levels **we** have verified and which we have not -
scaled mode's compose stack and any multi-machine run are the honest gaps, and the ones where outside
verification is most valuable.

## Go-live checklist

Before pointing real traffic at a nuthatch deployment:

- [ ] Running a release that includes the `/sql` statement-stacking fix (post-0.6.1).
- [ ] Bound to localhost or an internal interface, with TLS and authentication in front.
- [ ] `NUTHATCH_ADMIN_TOKEN` set, or `--no-admin` in use.
- [ ] Running as an unprivileged user; nest directory `0700`; `MemoryMax` set to the cursor budget.
- [ ] Prometheus scraping `/metrics`; alerts wired for quarantine, cursor death, tip lag, ingest
      stall and memory.
- [ ] Load balancer routes on **per-nest** `/<name>/ready`; paging on roost-root `/ready`.
- [ ] `nuthatch bench backfill` and `nuthatch bench query` run on your hardware and RPC; sizing
      derived from the measurements, not the projection.
- [ ] Every `rpc_urls` pool has at least two endpoints on the correct chain (verified at startup).
- [ ] Backup covering the nest directory, tested by restoring into a clean box.
- [ ] `nuthatch check` passing for each nest, with parity fixtures committed.
- [ ] A restart drill performed: SIGTERM, restart, confirm no gaps or duplicates.
- [ ] A nest upgrade rehearsed with `nest diff` and `nest upgrade` on a non-production copy.
- [ ] Nest onboarding process agreed. *(Adding a nest no longer restarts the roost - `POST
      /_admin/nests` mounts one live, since 0.7.0. This line said otherwise for two releases.)*
- [ ] **The acceptance runbook walked**, at the levels matching your deployment - see
      [`verification.md`](verification.md). It is falsifiable step by step, which this checklist is
      deliberately not: a checklist records a decision, a runbook produces evidence.
