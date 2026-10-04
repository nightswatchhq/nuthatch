# How nuthatch is tested, and what is not

A cold read of the tree, the CI, the Lodestar box and the consumer side, taken on 2026-10-02 against
4.1.0 (main at 79d3180). Three questions: does the binary still work, do the Lodestar nests agree with
the subgraphs they replaced, and would anyone find out if either stopped being true. Facts first,
gaps at the end. Figures are what was read today, not what a document says.

## Summary

The tree is tested hard and honestly: about 1,660 unit tests, about 700 integration tests over a
scripted chain double, six property tests, three fuzz targets, nightly mutation testing, and a set of
guard tests whose whole job is to stop the documentation, the CI and the gates lying. Ten required
contexts gate `main`, including three measured budgets, and the gates themselves were mutation-audited.

Parity with subgraphs is proven at pinned blocks, by hand, exactly and fail-closed. The most recent
record is 2026-09-30: 14,415 active allocations, identical id sets, SHA-256 matched. Nothing runs it
on a timer, and the script that would has never run from CI. Current-head parity has never been
measured.

Production monitoring is where it thins out. The binary exports a complete Prometheus surface and the
docs say exactly what to alert on. Nothing scrapes it. The Lodestar box has no Prometheus, no Grafana,
no alert rule and no watchdog beyond `Restart=always`. The only thing that pages anyone is Lodestar's
own 15-minute live monitor, which alerts to Discord when the dashboard's routes misbehave. It has
been red since 2026-10-01 21:55 UTC on two routes answering `nest_busy`.

One silent failure was found while reading. The nightly mutation run has been cancelled every day
since at least 2026-09-21 and reported nothing, because a cancelled run is not a failed one.

## 1. Does the binary work: the tree tests itself

### Unit and integration

| Layer | Where | Count | Notes |
|---|---|---:|---|
| Unit tests | `src/` (94 files), `decode/src/` | 1,664 | `indexer.rs` 193, `serve.rs` 106, `analytics.rs` 100 |
| Integration | `tests/` (87 files, 78 in the `it` umbrella binary) | ~700 | hermetic, driven by the `TapeSource` chain double |
| Property tests | `store.rs`, `factory.rs`, `entity_view.rs`, `entity_circuit.rs`, `e2e_reorg.rs`, `e2e_entity_reorg.rs` | 6 | one regression seed checked in |
| Fuzz | `fuzz/` (`abi_json`, `abi_arbitrary`, `decode_log`) | 3 | panics and unbounded allocation only, not wrong answers |
| WASM components | `components/*` | 0 | nothing inside their own `src` |

No test in `tests/` touches a live RPC. Fixtures are the tape double or `scripts/fixture_rpc.py` over
loopback. That is deliberate (`docs/ci-network.md`: a silent skip is not on the list) and it means
the suite is deterministic, and also that nothing in CI ever sees a real provider's behaviour.

### What the e2e suites actually exercise

- **Lifecycle:** mount, unmount, prune, migrate, shared dataset, warm restart, early cutoff, nid
  resolution. `e2e_runtime_lifecycle.rs` alone is 38 tests.
- **Reorg:** detection and rollback, a reorg below the sealed watermark halts loudly, an authored
  entity survives a reorg. Two `proptest!` blocks over random depths.
- **Determinism:** `e2e_seal_determinism.rs` pins that segment bytes do not depend on `--window`.
  `e2e_runtime_parity.rs` proves the shared cursor's union-fetch-then-demux is byte-identical to solo
  nests. `e2e_migrate_parity.rs` proves a layout migration changes no served byte.
- **Isolation:** cursor death and stall isolation (RFC-0021), fencing, writer and query-FE plane split,
  the #1165 concurrent `/sql` SEGV.
- **Scaled mode:** `pg_parity.rs` drives redb and Postgres through identical operations; control
  plane, reconcile, secrets. CI sets `NUTHATCH_REQUIRE_PG=1` so a missing database fails rather than
  skips.
- **Upgrade:** `upgrade_golden.rs` opens directories written by 3.13.2 and checks exact values. The
  fixture README says never regenerate, only add.
- **Crash safety:** `e2e_crash_safety.rs` asserts `seal_range` is idempotent. It is a unit-level test
  of the function; no test kills the process.

### Guards: tests that keep the repo honest

This is the distinctive part of the strategy and worth naming. A family of tests asserts the
documentation, the CI and the gates against the code, not the other way round:

- `doc_command_check.rs` walks every backticked command in the docs against the real clap tree.
- `actions_are_pinned.rs`: every third-party Action pinned to a SHA, docker refs by digest.
- `core_stays_pristine.rs`: the graph lane touches production code at exactly two sites.
- `payment_absent.rs`, `folds_absent.rs`: RFC-0046 §1's deletion test, executable.
- `required_checks.rs`, `required_contexts_script.rs`: the committed required-context list.
- `scheduled_workflow_failure_is_reported.rs`: every scheduled workflow has the reporter job.
- `bench_citations.rs`, `bench_commits.rs`, `bench_helpers_reject_failures.rs`: a benchmark cannot
  cite a file that does not exist, a commit that does not resolve, or a timing from a failed request.
- `launch_copy.rs`, `verification_non_claims.rs`, `ivm_claims.rs`, `abi_floors_documented.rs`.

`scripts/gate-audit.sh` goes one step further: it mutates the artefact each gate guards and asserts
the gate goes red. Eight cases across seven gates, all caught, and `tests/gate_audit_cases.rs` checks
every case still has a target.

### CI gates on `main`

Ten required contexts (`.github/required-checks.txt`):

| Context | What it enforces |
|---|---|
| fmt · clippy · test | default and `exex` legs, both crates, `-D warnings` |
| footprint (RAM budget) | single nest peak RSS ≤ 256 MB, and ≥ 8,004 rows actually indexed |
| per-cursor RAM budget (dense multi-nest) | 20 nests on one cursor ≤ 2,048 MB budget, ≤ 602 MB regression tripwire, every nest at tip with ≥ 90% expected rows |
| point-read latency | entity point-read p50 ≤ 8 µs against a baseline recorded on the same runner class; p99 tracked, not gated |
| scaled mode (parity · control plane · reconcile) | the Postgres suite, `--test-threads=1` |
| the compose fleet comes up | `docker compose --profile fleet`, 2 writers, 2 FEs, `verify.sh 5`, exactly one writer acquires cursors |
| cargo-deny | advisories, 16-licence allow-list, Materialize and HyperSync banned, one git source allowed |
| build release | the artifact the three measurement jobs consume |
| fuzz smoke (decode path) | 300,000 runs or 180 s per target, every push |
| Jules approval | the review harness |

Not required, by design: the `graph` facade leg, S3 publish via versitygw, the Trino contract, the
`counter` leg, and the nightly mutation run.

### Mutation testing

`cargo-mutants` nightly at 03:00 UTC over `chunker.rs`, `seal.rs`, `registry.rs` (about 300 mutants
of 4,503 crate-wide). A survivor not in `.github/mutants-baseline.toml` fails the run, as does a
truncated run or one that enumerated nothing. Advisory, not required.

**It has not completed in at least twelve days.** Every nightly from 2026-09-21 to 2026-10-02 shows
`cancelled`. Today's run: `registry.rs` succeeded in 29 minutes, `seal.rs` was cancelled at its
150-minute timeout, `chunker.rs` at its 240-minute timeout. The reporter job is conditioned on
`failure()`, and a timeout cancellation is not a failure, so "surface a failed scheduled run" was
`skipped` and nothing was filed. The `mut/` directory is a separate thing: 49 hand-written patch
scripts for the GraphQL facade, run by hand, with a hardcoded worktree path.

### Benchmarks and budgets

`docs/benchmarks.md` and about 45 artefacts under `docs/bench/`. Three numbers fail the build (the
two RSS ceilings and the point-read p50). Everything else is measured by hand and recorded:
`dbsp_step_cost`, `bench_restart_to_ready`, `bench_compact_rows`, `lodestar_panel`,
`seal_latency_with_folds`, `publish-throughput-gate.sh`. No `cargo bench` or criterion target exists.
The backfill events/sec floor from CLAUDE.md (≥ 10K) is not a CI gate.

### Release

`release.yml` builds on tag, attests provenance with Sigstore, and verifies the attestation before
publishing. It runs no tests of its own; the gates ran on the merge. `tests/release_provenance.rs`
guards the workflow shape.

### Reviews and audits

Jules reviews every PR and is a required context. The 2026-09-01 independent audit
(`docs/audits/`) filed two issues. The 2026-10-02 two-reader cold audit filed 48 confirmed issues
and 27 unconfirmed smells, the day after the engine changed; the blocker (#1655) is a planner stack
overflow that takes every mount with it. That sprint (`undaunted-uakari`) is the current one, and its
rule is that every fix lands with the test that was missing.

## 2. Does it agree with the subgraph: parity

### The Lodestar nests

Four nests serve Lodestar from the Helsinki box, all on 4.1.0 today, all at tip with `lag_blocks 0`:
`nuthatch-dips` (8104), `graph-allocations-nest-next` (8107), `graph-gns-nest-next` (8113),
`data-services-nest` (8114). Parity evidence for them, in order of strength:

1. **Committed checks in the nest repo.** `graph-allocations-nest/checks/*.sql` with
   `checks/expected/*.json`, run by `nuthatch check`. Four invariant and parity checks: indexer daily
   sums, pool agreement, populations, port queue. They assert internal consistency and recorded
   expected results, not the subgraph.
2. **The 2026-09-30 verification record** (`graph-allocations-nest-verify/verification/2026-09-30/`).
   At Arbitrum block 510,395,917, 14,415 active allocations on both sides, sorted id lists identical,
   SHA-256 `3d2baf43…` each. All-time count 870,521 matched the subgraph's `allocationCount`, count
   only. Reproducible: both sides take a block. This is the strongest single piece of parity evidence
   in the project.
3. **`scripts/lodestar-parity.sh`** (836 lines). Compares allocations (count), disputes (id set),
   epochs (six fields, each from a measured comparability boundary: rewards from 1195, fees from
   1302, `signalled_tokens` drift-classed from 1105), and escrow rows joined by decoded
   `(tx_hash, log_index)`. No numeric tolerance. Exit 0 clean, 2 known differences only
   (#1114 self-collections, #1116 drift pair), 1 for anything else including an absent comparison.
   Last recorded run: 2026-09-02 on 3.1.0, then the epoch work through #1132 on 2026-09-03. On the
   box, `/root/lodestar-parity.sh` and six `parity-*-{before,after}.txt` files are dated
   2026-08-29 and are a different, older script (per-table counts around a release roll).
4. **`tests/network_contract.rs`** (graph feature, 22 tests). Replays 22 recorded gateway references
   from `tests/fixtures/network-nest/validation/`: 35 network and 13 epoch fields at 42.46M, 43.44M,
   84.36M and 123.6M across 261 epochs, 18 fields for all 39 indexers at 123.6M, deployments at
   129M, allocations at 200M and 506M, signers at 502M. Its own header: "source-anchored fixture
   replay, not a claim of live parity". It stopped at 123.6M because a pinned gateway read cost
   36.1 s. Current-head parity has never been measured.
5. **Epoch mismatch, understood.** #1113 found three of six `lodestar_epochs` fields disagreeing on
   27 to 30 of 266 closed epochs, all below epoch 1195, and the comparability boundaries in the script
   are the measured answer. The script asserts a boundary is the lowest at which the property holds,
   so it cannot be raised until green.

What #1076 asked for and did not get: a timer, a pinned-block comparison on a schedule, and
disagreement made visible. It closed on 2026-09-02 with the first recorded DIFF (which turned out to
be predicate mismatch, since fixed), and the timer half never happened. The Helsinki box has no
timer, no cron entry and no unit for it. `crontab` holds one line: the nightly Postgres backup pull.

### Subgraph ports in general

- **`nuthatch graph-validate`** (RFC-0053 S0, #1264). Posts a corpus of queries to a reference and a
  nest, compares the selection parsed from the query text so that dropping a field cannot produce a
  clean result. Exits non-zero on any divergence or any failure to compare. No committed corpus, no
  committed run, no CI use. The 21 network-client documents in `tests/fixtures/network-clients/`
  are its intended input and have not been run through it.
- **`scripts/port-diff-uniswap-v4.py`** and `docs/port-uniswap-v4-mainnet.md`. Pinned at
  25,945,634 against a keyless reference. Swaps 6,243 of 6,243, pools 132,769 aligned with 47 the
  subgraph silently drops, seven fields byte-identical across 151,811 rows. And the honest
  denominator: the emitted views answer 27 of 231 fields; nine of 19 views are placeholders.
- **`tests/graph_schema_golden.rs`** (13 tests) diffs the generated introspection against a recorded
  real graph-node introspection of Uniswap V4, field for field, operator for operator.
- **`tests/graph_over_indexed_data.rs`** serves GraphQL over the tape and traces every asserted value
  to an emitted log.
- **RFC-0038 §6b** recorded 343 of 343 swaps row-for-row identical on a V3 pool; §6d matched
  `ethPriceUSD` to 10⁻³². Both manual, neither committed as a script.

The documented limit stands and is not a testing gap: `derivedETH`, `volumeUSD`,
`totalValueLockedUSD` are order-dependent by construction and a nest produces a fixed point, not the
subgraph's number. RFC-0053 was rescoped on 2026-09-12 to "exact for the event-shaped part, a named
refusal for the rest", and `docs/graph-compatibility-what-it-is.md` says in so many words that it is
not a drop-in replacement.

### Standing lesson

Every defect the S1/S2 review found was a silent substitution, not a crash. The parity tooling is
built around that: refuse rather than approximate, an absent comparison is a failure, test the exact
value. That principle is sound. It is also only as good as how often the comparison runs, and today
that is "when someone remembers".

## 3. Would anyone know: production

### What the binary gives an operator

- `GET /metrics`, hand-rolled Prometheus text, unauthenticated, on by default. About 50 process-global
  series, a labelled per-nest set (`nuthatch_nest_tip_lag_blocks`, `nuthatch_nest_health`,
  `nuthatch_nest_quarantine_total`, `nuthatch_cursor_live{chain}`, hot and sealed bytes, IPFS
  counters), per-entity gauges, and publish-mirror gauges when mirroring.
- `GET /ready` per nest, 503 on quarantine or stall, with `tip`, `last_block`, `lag_blocks`,
  `sealed_through`, `seconds_since_poll`, freshness mode, seal-direct state, entity stall. Root
  `/ready` is 503 if any nest is quarantined or stalled; the docs call it advice, not a gate.
- `GET /health` is liveness only. `GET /nests` is the roster with quarantine class and reason.
- `docs/operators.md` has a nine-row "what to alert on" table with PromQL conditions, a runbook
  table, and a go-live checklist with "Prometheus scraping `/metrics`; alerts wired" as an unticked
  box.
- The `[[alerts]]` webhook outbox is compliance annotations (RFC-0008), not ops alerting.
- No OpenTelemetry, no Grafana JSON, no scrape config, no alert rules file, no Dockerfile
  `HEALTHCHECK`, no compose healthcheck except Postgres. No self-kill on RSS: admission is a
  projection (507 over budget), the ceiling is `MemoryMax` on the unit.

### What is actually deployed

Read from Helsinki today:

- Four units active, `Restart=always`, nothing else watching them. No Prometheus, Grafana,
  Alertmanager, node_exporter or uptime process. No timers of ours; the only cron is the backup pull.
- All four nests report `ready: true`, `lag_blocks 0`, `stalled false`. RSS 97 MB, 805 MB, 491 MB and 83 MB.
  Seal lag 3,786 to 52,634 blocks, which is the hot window, not a fault.
- The QoS nest on the ThinkPad (`100.83.44.63:8124`) did not answer from this MacBook. Tailscale
  showed no status here, so that is this laptop's tailnet, not necessarily the nest. Unverified.
- 8105, the old allocations port, no longer answers; 8107 is the live one. Correct, and the parity
  script's default `NEST_URL` still says 8105.

### What actually alerts

Lodestar's `e2e-monitor.yml`, in the dashboard repo: every 15 minutes, 58 contract checks and a
rendered-pages job against production, Discord webhook on failure. It exists because #114 shipped a
shape change that rendered every score as a dash while every unit test passed. It is the de facto
nest monitor: a dead nest makes Lodestar's routes 503 through `nuthatchSqlReady`, and the monitor
sees that.

It is red now. The last four runs, from 2026-10-01 21:55 UTC, fail two of 58: delegator portfolio
and APR provenance answer `503 nest_busy`. The script's own comment says kittiwake sheds work by
design and a 503 must not hide a failure, so this is the alert working. Whether anyone has read it is
not something the tree can tell me.

CI-side scheduled checks that do report: `live-endpoints.yml` weekly (shipped public RPCs can still
backfill; last four green), `required-contexts.yml` daily (green; the token it needs arrived with #1095 on 2026-09-03, and
`docs/ci-network.md` still says it cannot run, which is the doc being stale), `sprint-landed.yml` daily. Each files a
"Scheduled check is failing" issue on `failure()`. Four such issues have been filed and closed.

### Grafana

`lodestar/grafana/` holds four dashboards for Lodestar's own Postgres: ingestion freshness, cron
performance, protocol metrics, indexer analytics. None reads nuthatch. There is no Grafana for the
nests anywhere.

## 4. Simulations

- **The tape double** (`tests/common/tape.rs`) is the simulation layer: a scripted chain with
  reorgs, finality, factories, and a moving tip. Every e2e and the two reorg property tests run on
  it. CI's footprint jobs use mock RPCs (`footprint-rpc.py`, `multinest-rpc.py`) with 200 logs a
  block and a moving tip.
- **`scripts/fleet-lab.sh`** provisions real Hetzner boxes from release artefacts, runs
  `docs/verification.md`, then partitions the network and skews the clock. By hand, costs money.
- **`scripts/verify.sh`**, six levels, is the executable acceptance runbook; CI runs level 5.
- **Soaks** are recorded, not automated: 23 h RSS soak in `docs/prod-readiness.md`, the Monad
  `Depth(8)` soak in RFC-0051, the QoS 7 GB glibc finding that motivated jemalloc in 4.1.0.
- Not simulated anywhere: a `kill -9` mid-seal at process level (the cold audit's #1631 and #1632
  are exactly that class), a provider that lies (empty instead of error), a reorg between the logs
  call and the checkpoint call (#1629, filed, the fixture is item 2 of the sprint), and a long-running
  tip follow against a real chain.

## 5. What is missing, ranked

Ordered by how much it would have saved already.

1. **A parity timer** (#1713). `lodestar-parity.sh` on a systemd timer on Helsinki, daily, at
   `sealed_through`, exit code to a file and a Discord line on 1. The script already refuses to pass
   on an absent comparison. The blockers were a gateway key on the box and 8105 → 8107. #1076 closed
   without this half; it wants reopening or a new issue. Until then the 2026-09-30 verification is
   the last word, and it is a count and an id set, not the epoch fields.
2. **Scrape the metrics that already exist** (#1714). One Prometheus on Helsinki (or Grafana Cloud's agent,
   which Lodestar already uses for Postgres) scraping four `/metrics` endpoints, and the nine alert
   rules from `docs/operators.md` typed in as PromQL. The binary's half of this has been done for
   months; the operator's half has not been started. Add the ThinkPad QoS nest.
3. **The mutation run has been dead for twelve days and said nothing** (#1715). Two fixes: condition the
   reporter on `!success()` rather than `failure()`, and either raise the matrix timeouts or split
   `chunker.rs` and `seal.rs` further. This is a sharper instance of the lesson already in the notes:
   a gate must exist, be able to go red, and be seen when it does.
4. **`graph-validate` has never run on its corpus** (#1716). The 21 client documents are committed, the tool
   is merged, and no result exists. One by-hand run at a pinned block against
   `network.thenightswatch.dev` gives RFC-0060 S4 its first number, parked or not.
5. **Process-kill crash tests** (#1717). The seal idempotency test is a function call. #1631 and #1632 need
   the real thing: spawn the binary, `SIGKILL` between segment write and watermark, restart, assert
   no double rows. The sprint rule already says the test lands with the fix; make it a harness,
   not a one-off.
6. **Current-head parity** (#1718). Every parity number is at a sealed pin. The hot window is a day of
   Arbitrum and nothing has ever compared it. The 36 s pinned-read cost does not apply at head.
7. **A live-chain smoke, scheduled** (#1719). `live-endpoints.yml` proves the shipped RPCs answer `doctor`.
   Nothing scheduled runs `init` → `dev` → first query against a real chain and times it against the
   two-minute promise. The `fleet-lab` script is close; a weekly single-box run with a kill at the
   end is the cheap version.
8. **Lodestar's monitor is red and the nest side cannot see why** (#1720). `nest_busy` on two routes for
   four runs is either kittiwake's permits or a real queue. `nuthatch_sql_rejections_total{reason="busy"}`
   on 8107 would say which, which is item 2 again.
9. **Smaller things** (#1721 stale port, #1722 components, #1723 throughput floor, #1724 yatr and verify.sh, #1725 stale ci-network.md). `yatr ci` runs a subset of CI (no feature legs, no decode crate). `verify.sh`
   is not wired as a pre-push hook despite the memory note calling it one. The parity script's
   default port is stale. `components/*` have no tests. The ≥ 10K events/s backfill floor is in
   CLAUDE.md and in no gate.

## 6. What was checked and what was not

Checked today: the tree at 79d3180, all nine workflows and their last runs, the Helsinki box over
ssh (timers, units, crontab, processes, `/ready` and `/metrics` on four ports, the parity files),
the Lodestar repo's monitor and its last six runs, the nest repos' `checks/` and verification
directories, and the GitHub record for #1076, #1113, #1118 and the scheduled-failure issues.

Not checked: the ThinkPad (unreachable from here), whether anyone receives the Lodestar Discord
alerts, the content of `verify.sh` beyond its header, and the drift check's step output, which the run log does not surface.
