# Production-readiness checklist

The bar a nuthatch release must clear before it's pointed at someone's real workload, unattended.
Reconciled against [CLAUDE.md](../CLAUDE.md) (non-negotiables + build order), the
[RFC series](rfcs/README.md), the [backlog](backlog.md), and [CI](../.github/workflows/ci.yml) on
**2026-07-30** (repo at `0.7.2`).

This is a *standing* checklist - the target, not a claim it's all done. Status reflects what's
verifiable today. When you cut a release, walk it top to bottom and update the flags with evidence.

## Legend & scope

| Flag | Meaning |
|------|---------|
| ✅ | Done and verified (test, bench artifact, or live run backs it) |
| 🟡 | Partial - exists but incomplete, unverified, or narrow |
| ⛔ | Not started, deferred, or blocked (see "Blocked on") |

**Two production targets, graded separately** - don't conflate them:

- **Embedded / single-chain roost** (the primary deliverable): one binary, one chain, tip-follow +
  serve, `≤2 GB` RAM. This is the thing that can be "prod ready" *now*.
- **Scaled mode** (docker-compose, Postgres + DataFusion federation): greenfield. Nowhere near a
  release, and honestly so - most of its checklist is ⛔ by design, not neglect.

A green embedded column with a red scaled column is a **legitimate ship** - just say which one you're
shipping.

---

## 0. The non-negotiables (gate everything else)

If any of these is ❌ the release does not go out, full stop. These are the CLAUDE.md invariants.

- [ ] ✅ **Single static binary, zero external services in embedded mode.** `curl | sh` → `init` →
  `dev` → live API, no Postgres/Docker/IPFS. - *CI builds the release binary; footprint job runs the
  real `init → dev` path.*
- [ ] ✅ **Footprint ≤ 2 GB RAM** for a single-chain roost, CI-enforced. - *`footprint.sh` gate, 256 MB
  ceiling, measured ~37 MB. The CI scenario is only `--backfill 200` on one nest, so the *dense
  multi-nest roost at tip* was measured out-of-band instead: 8 nests on one cursor peaked at 89 MB at
  tip, 4% of budget - see §5. Wiring that density into the gate itself is still open (§3).*
- [ ] ✅ **No phone-home.** No telemetry, no mandatory tokens, AI degrades offline. - *Verify per
  release: grep for outbound calls not gated behind explicit user config / BYO-key.*
- [ ] ✅ **Determinism in the core.** Decode, reorg, entity derivation re-executable; no LLM output in
  the runtime data path. - *Golden tests + the RFC-0016/0017 hard fence. Re-assert on any new
  data-path code.*
- [ ] ✅ **Licence hygiene (`MIT OR Apache-2.0`).** No copyleft-we-don't-own ports (SQD worker-rs), no Materialize
  (BSL), no Envio/HyperSync dep. - *`cargo tree` audit each release; deps stay in the CLAUDE.md
  safe-list.*

---

## 1. Correctness & determinism

- [ ] ✅ Deterministic decode: topic0-keyed, contract-ABI priority with generic fallback. *(RFC-0001)*
- [ ] ✅ ABI acquisition Sourcify → Etherscan-class, cached locally.
- [ ] ✅ Decodings are **versioned**; no retroactive re-decode of stored history when ABIs improve.
- [ ] ✅ Golden/deterministic tests per handler and view (fixed fixtures in → exact state out).
- [ ] ✅ Property tests: random reorg depths converge to canonical state (`e2e_reorg.rs`).
- [ ] ✅ Nest invariant/parity checks (`nuthatch check`) run hermetically in CI against committed
  fixtures. *(RFC-0002 §5)*
- [ ] ✅ **Sustained** byte-identical multi-nest-vs-solo table parity. - *Run live 2026-07-28 on
  Arbitrum: two nests indexed solo and again behind one shared cursor over the same 2,400-block range,
  compared table by table - **20 tables, 17,108 rows, byte-identical**, including empty tables and the
  topic0-disambiguated `weth__transfer_ddf2`/`_e192` pair.*
- [ ] 🟡 Factory / dynamic-contract discovery correctness at scale. - *Implemented (0009); child
  `end`/expiry conditions and wildcard-address decode still open.*

## 2. Reliability, reorgs & crash safety

- [ ] ✅ Reorgs only ever touch the mutable hot store; sealed Parquet is append-only past finality.
- [ ] ✅ Atomic seal/prune (no torn segment on crash mid-seal). *(0.4.0 hardening)*
- [ ] ✅ Crash-safety e2e (`e2e_crash_safety.rs`): kill mid-index, restart, converge.
- [ ] ✅ Single-writer discipline: only the ingestion thread writes DuckDB/redb; queries attach
  read-only. No concurrent-writer design anywhere.
- [ ] ✅ Single cursor / single process / one observable failure boundary. A second chain = a second
  process (never multiplex chains behind one cursor).
- [ ] ✅ Per-nest blast-radius isolation in a roost: one nest's bad view / runaway factory can't harm
  another. *(RFC-0012)*
- [ ] ✅ Graceful recovery from a corrupt/partial segment on startup (detect + quarantine + resume
  rather than crash-loop). - *0.5.x: `seal::verify_and_quarantine` runs at startup - each manifest
  segment is hash-verified against its content address; a corrupt/tampered/unreadable one is moved to a
  sibling `quarantine/` dir with a loud error, and `define_views` skips any missing file, so one bad
  segment reduces a table's cold data instead of failing every `/sql`. Fixtures: `seal.rs`
  quarantine test + `analytics.rs` query-survives-missing-segment test.*
- [ ] ✅ RPC-provider failure handling: dead-provider failover + honest stall reporting under sustained
  provider flakiness. - *Failover is **health-aware** (a failed endpoint gets a 30 s cooldown, so a dead
  provider no longer costs a request-timeout on every round-robin hit), the tip loop retries the same
  window (no silent gaps), and a stall is now **loud**: `nuthatch_last_poll_unixtime` in `/metrics`, an
  escalating tip-loop log (warn on the first miss → error every ~60 s of "all endpoints unreachable →
  STALLED"), and `/ready` returns 503 once no poll has succeeded within 90 s (§7).*

## 3. Performance & footprint budgets

Benchmarks are **CI artifacts**, not vibes - every published number traces to a `bench-report.json`
with date/provider/hardware/commit (the RFC-0004 house rule).

- [ ] ✅ Backfill throughput bench exists and is reproducible (`nuthatch bench backfill`). - *Floor
  ≥10K events/sec, aim 30K.*
- [ ] 🟡 A **published, current** backfill number for the release commit on reference hardware. -
  *Re-run per release; don't ship a stale figure.*
- [ ] ⛔ Tip-lag benchmark (notification → row queryable) as a tracked number. - *Meaningful number
  needs ExEx. **Blocked on:** reth node (0003).*
- [ ] 🟡 Entity point-read p50/p99 bench tracked across releases (regressions fail the build).
- [ ] 🟡 Peak-RSS regression gate wired for the **dense multi-nest** scenario, not just single-nest
  `--backfill 200`. - *The density itself is now measured (§5); what is missing is the **gate** - a
  one-off run does not catch the release that regresses it.*
- [ ] ✅ Regressions fail the build (benchmarks-as-gates principle established). - *Extend coverage as
  the benches above land.*

## 4. Security

- [ ] ✅ **`/sql` `;`-statement-stacking fixed and released in 0.6.2** (2026-07-28). A stacked
  `COPY … TO` / `ATTACH` was an arbitrary file write, bounded by the service user. Present in **0.6.1
  and earlier**; fixed by `reject_statement_stacking` with regression tests, released as **v0.6.2**
  with binaries, and the Lodestar box upgraded and verified the same day. *Any deployment still
  exposing `/sql` on ≤0.6.1 remains affected and should upgrade.* Advisory GHSA-jvjx-5528-r6mm is
  drafted and **deliberately unpublished as of 2026-07-29** - a decision, not an oversight. The fix
  shipped in 0.6.2 and 0.7.x, so the remedy is available to anyone who looks; publication is a
  disclosure call for the maintainer to make, not a task waiting on engineering.
- [ ] ✅ Blob-mount RCE fixed (0.4.0 critical).
- [ ] ✅ `/sql` arbitrary file-read fixed (0.4.0 critical).
- [ ] 🟡 **DuckDB `allowed_directories` is not enforced on the build we bundle** (measured 2026-07-27).
  `reject_file_access` is the only control stopping a file read, so the file-access defence is one layer
  deep, not two. A tripwire test fails if a future bump makes the layer real. *Re-check on every duckdb
  bump.*
- [ ] ✅ `/sql` surface is structurally read-only (single-writer + read-only attach).
- [ ] ✅ A security review pass on the **serving surface** (`serve.rs`, `mcp.rs`, `webhooks.rs`,
  `analytics.rs`, `abi.rs`, `rpc.rs`) - *done (0.5.x hardening): no criticals; SQL read-only gate holds
  three-deep, no SSRF (ABI/RPC hosts are fixed constants), no file-read via `/sql`. Fixed: `/nest`
  webhook-URL disclosure, `/sql` error path-scrub, `screen_status` quote-escape, constant-time admin
  token, concurrent webhook delivery. Re-run per release on the diff.*
- [ ] 🟡 Bind/exposure defaults are safe. - *`dev` binds `127.0.0.1` by default; off-localhost it warns
  loudly that the data surface has NO auth (the gateway's job). Confirmed by the review; the one control
  a fronting gateway must enforce is auth on **every** route, not just `/_admin`.*
- [ ] ✅ Dependency vulnerability scan (`cargo deny`) wired into CI. - *`deny` job runs advisories +
  licences + bans + sources against `deny.toml`; the permissive-only licence gate is now enforced. Three
  transitive advisories ignored with written rationale (quick-xml not-reachable ×2; wasmtime-wasi
  FilePerms tracked for a runtime bump).*
- [ ] ✅ Effectful (capability-granted) components can only produce **annotations**, never canonical
  entities - purity checkable from the composition manifest. *(transform layer)*

## 5. The ≤2 GB budget under realistic load

Called out separately because it's the headline promise and the current gate only exercises the easy
case.

- [ ] ✅ Single nest, backfill, single chain: measured ~37 MB, gated at 256 MB.
- [ ] ✅ Multiple nests co-located in one roost at tip, sustained, measured against 2 GB.
  **8 nests on one Arbitrum cursor** (2026-07-29): at tip, mean RSS **84 MB**, peak **89 MB** against
  the 2048 MB per-cursor budget - **4%**. Backfill peaked at **154 MB**, the more demanding phase.
  Adding a nest costs far less than the first one does: the cursor's RPC buffers and decode machinery
  are shared, so only the per-nest hot store is additive.

  **A qualifier the 2026-07-29/30 prod soak made necessary:** this bounds **density**, not
  **workload**. Those 8 nests were small and at tip; a *single* nest doing a 125M-block backfill on
  the same budget reached 427 MB by itself. Per-nest RSS is dominated by what a nest is *doing*, not
  by how many share a cursor - so read this as "co-tenancy is cheap", never as "a cursor uses 84 MB".
- [ ] ⛔ Large-ABI / high-event-rate contract at tip (memory doesn't grow unbounded with hot-store
  churn).
- [ ] ✅ Long-running soak (23h) with no RSS creep (leak check).
  **Two nests on the Lodestar prod box, 0.7.2, 2026-07-29 → 30.** Final RSS **459 MB** and **427 MB**
  against the 2048 MB per-cursor budget - 22% and 21% - and **flat across repeated samples** at the
  end rather than still climbing. No OOM, no restart, both healthy throughout.

  The honest reading of the shape, because the raw delta looks alarming and is not: `graph-gns-nest`
  went 66 MB → 427 MB, which is *backfill working set*, not creep - it was mid-backfill at the first
  reading (`--backfill 125000000 --window 50000`) and had plateaued by the second.
  `graph-staking-nest` moved 426 → 459 MB over the same 21 hours, which is the flat profile you want.

  **What this does not establish:** a 23h window containing a workload transition cannot cleanly
  separate a slow leak from a working set that grew and settled. A clean leak check wants a soak at
  *steady* tip-following with no backfill in it. Recorded as passing on the evidence available, with
  the caveat attached rather than quietly dropped.

## 6. Testing & CI gates

- [ ] ✅ `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test --locked` on every
  PR + main.
- [ ] ✅ Release binary builds `--locked`; footprint gate runs against the built artifact.
- [ ] ✅ e2e harness exists (`TapeSource`) and covers solo, reorg, crash-safety, roost parity.
- [ ] 🟡 MSRV is honest. - *`Cargo.toml` declares `rust-version = 1.85`, but CI pins the toolchain to
  `1.95.0`. Either test against 1.85 in CI or bump the declared MSRV; right now the claim is
  unverified. The declared MSRV is not cosmetic: it silently selected DataFusion 48 over 54 during the
  RFC-0013 spike, and cargo reports that as a one-line warning nobody reads.*
- [ ] 🟡 Coverage of the AI/MCP surface (schema discovery, SQL exec, entity lookup, subscribe) with
  the RFC-0016 eval harness. - *S1 eval harness gates the semantic-layer work; wire it in.*
- [ ] 🟡 `--offline` / no-network test path proving AI features degrade gracefully.

## 7. Operability & observability

- [ ] ✅ Metrics surface exists (`metrics.rs`), **including per-nest series** - `{nest="…"}`-labelled
  `nuthatch_nest_*` plus `nuthatch_cursor_live{chain}` (RFC-0026), so a co-tenant roost is attributable
  per nest rather than only process-globally. *(This closes the old SEC-9 gap.)*
- [ ] ✅ Health/readiness endpoint suitable for a supervisor. - *0.5.x: `/health` = liveness (plain
  `200 "ok"`); `/ready` = readiness - JSON with tip / last_block / lag / sealed_through / last-poll age,
  `200` when fresh and **`503` when stalled** (no successful source poll within 90 s ⇒ every RPC endpoint
  down). A just-started node gets grace (never-polled ≠ stalled). **0.6.x (RFC-0026):** `/ready` is now
  also mounted at the **roost root** - `200` only when every cursor and nest is indexing, `503` naming
  what is quarantined - with per-nest `/<name>/ready` answering for that nest alone. Route traffic on
  the per-nest one and page on the root; wiring a load balancer to the root means one sick nest evicts
  every healthy sibling.*
- [ ] 🟡 Structured logs at a sane default level; a clear "we are behind / we are at tip" signal.
- [ ] ✅ Documented restart/recovery runbook and a backup/restore story for the redb hot store +
  sealed segments. - *[operators.md](operators.md) carries the failure model, the symptom→action
  runbook, backup/restore, and a go-live checklist (2026-07-28).*
- [ ] ✅ SSE **push** for live status - `/_admin/events`. *(This entry outlived its fix; it was shipped
  and sat here marked ⛔ regardless, which is how a checklist stops being trusted.)*
- [ ] 🟡 Alerting hooks (`alerts.rs`, `webhooks.rs`) documented end-to-end with a runnable example.

## 8. Release engineering

- [ ] ✅ Versioning + release workflow in place (`release.yml`), reproducible `--locked` builds.
  *(RFC-0005)*
- [ ] ✅ `curl | sh` install path.
- [ ] 🟡 Cross-platform release matrix - which targets are built/tested? (Linux x86_64 is the CI host;
  macOS/arm64 install claims should be tested or scoped.)
- [ ] 🟡 CHANGELOG / release notes discipline per tag (the progress-log is close; formalise for
  consumers).
- [ ] ✅ Documented upgrade path / on-disk format stability guarantee across `0.x` bumps. - *Proven in
  production: a `0.3.0 → 0.6.0` nest upgrade was a binary swap plus a restart - no data migration, no
  flag or unit changes, sealed segments and hot store preserved. Each release states "in-place safe" or
  "reseal required" explicitly; the contract is in [operators.md](operators.md).*

## 9. AI-native surface (MCP)

- [ ] ✅ MCP server compiled into the binary (`mcp.rs`), works offline against the local instance.
- [ ] ✅ `init` scaffolds schema + views + handlers + tests from the ABI.
- [ ] ✅ Ships `llms.txt` / docs-as-MCP / `.claude/skills/` in scaffolded projects.
- [ ] 🟡 The RFC-0016 governed semantic layer (`semantic.toml`, enriched `schema`, errors-as-prompts,
  `explain`) - *in design, measure-first, not shipped.*
- [ ] 🟡 The RFC-0017 builder skill with CI-checked CLI/config reference drift. - *in design.*

## 10. Docs & first-run UX

- [ ] ✅ `<2 minute` first-indexed-query demo path (`init → dev → sql`).
- [ ] ✅ Terminal-native query REPL (`nuthatch sql`). *(RFC-0015 slice 1)*
- [ ] ✅ Operator docs, factory docs, benchmark docs present.
- [ ] 🟡 A single "here's how you run this in production, unattended" guide that ties together
  §7 (ops), §4 (safe exposure), and §8 (upgrades). - *This checklist's operational cousin; write it
  when the 🟡s above go green.*

---

## 11. Scaled mode

**No longer "mostly ⛔ by design".** RFC-0022 was built out 2026-07-29/30; all six of its own
acceptance tests pass, with 39 tests running against a live Postgres in CI. Nothing here blocks an
**embedded** release either way.

- [ ] ✅ Postgres hot store behind the `HotStore` trait (no `#[cfg]` forks of business logic). -
  *Trait extracted first as a pure refactor, then `PgStore` behind it, with a parity suite asserting
  the two backends answer identically after every mutation.*
- [ ] ✅ Read/write plane split: a writer pool owning cursors, an independently-scaled query-FE tier
  serving from shared state. *(RFC-0022 §1)*
- [ ] ✅ Single-owner enforced rather than assumed: cursor leases plus a monotonic fence the **store**
  checks, so a stalled worker that wakes up has its writes refused. *(§2)*
- [ ] ✅ Control plane + reconcile loop: desired state, worker registry, placement, drain. *(§2/§3)*
- [ ] ✅ Dynamic lifecycle over HTTP - add/remove a nest with no restart. *(§3)*
- [ ] ✅ Fleet-wide resolution: one pinned answer per endpoint, so two FE nodes can never serve the
  same endpoint from different schemas. *(§4)*
- [ ] ✅ Runtime secret injection - scoped to a worker's assigned nests, write-only, never in a
  bundle. *(§5)*
- [ ] ✅ docker-compose deployment story tested end-to-end (2026-07-30). - *The full stack brought up -
  Postgres, control plane, 2 writers, 2 FE nodes - and walked through `verification.md` level 5:
  workers registering, a nest declared over HTTP and picked up within a tick, **exactly one** owner for
  the cursor, failover to the survivor after killing the owner and waiting out the lease, fleet-wide
  version pinning, and write-only secrets with a canary absent from disk.*

  **It failed four ways first, and all four are fixed.** A migration race (`CREATE … IF NOT EXISTS` is
  not atomic in Postgres, and the control plane plus every worker migrate at startup - invisible to
  every single-process test); the control plane refusing to start because Docker publishes ports by
  binding `0.0.0.0` inside the container; the FE dying on a `:ro` mount because it still opens the
  local redb; and an undocumented `./nest` requirement whose failure is asymmetric - writers survive,
  FE nodes do not. Bringing it up was worth more than any amount of reading it.
- [x] ✅ DuckDB-vs-DataFusion spike (latency + RSS). - *Run 2026-08-02 on `net_balances` over sealed
  segments at 2M/8M/20M rows, each size in both engine orders to defeat the page-cache confound.
  **DataFusion is 1.6-2.7x slower and the gap widens with size**, at exact result parity. RFC-0013 §5;
  artifact in `docs/bench/rfc-0013-datafusion-gate.json`.*
- [ ] ⛔ DataFusion federation across hot + cold behind one SQL surface. - *0013 §2/§4. **The gate
  said no for 1.0** - DuckDB stays in both modes. Reopen if a DataFusion release closes the aggregate
  gap, or if a scaled-mode query genuinely needs one plan spanning Postgres hot and Parquet cold,
  which is the case DuckDB cannot serve and the real point of §2.*
- [ ] ⛔ Golden SQL-compat suite across both engines. - *Moot while there is one engine; the spike
  already showed parity on the fold that matters.*
- [ ] ⛔ A multi-machine run. - *Everything above is verified on one host: several processes and
  connections against one database, which is what two machines are from the data's point of view for
  every invariant tested. It is **not** a substitute for real network partitions or clock skew, and
  the RFC always said scale validation happens on operator infra.*

## 12. Infra-gated capabilities (the shared blocker)

Almost everything un-buildable-on-a-laptop traces to one missing box.

- [ ] ⛔ **Colocated reth node** (full for tip, archive for deep backfill/traces). - *Provisioning +
  days of sync; hardware/ops, not code. Gates the two below.*
- [ ] ⛔ ExEx tip mode wired to a real node; `nuthatch-node` binary; honest tip-latency number. *(0003;
  groundwork in, **blocked on** the node.)*
- [ ] ⛔ Firehose-class extraction (traces + state diffs), own-node/ExEx only. *(0014; **blocked on**
  0003.)* - *One node-independent slice is buildable now and forward-compatible: the calldata decoder,
  `[extract]` config, `traces`/`state_diffs` schemas, and the unbounded-volume guard.*

---

## Bottom line

**Embedded, single-chain, single-nest:** the core (§0-§2, §6) is genuinely strong - this is the
column that can go to `1.0` first. The honest gaps before you'd point a stranger's workload at it
unattended are the operational and load ones. Several have since closed - the **dense-roost RAM proof**
(§5, measured at 4% of budget), **provider-failure resilience** (§2, RFC-0028), **safe-exposure
defaults** (§4) and the **unattended-operation runbook** (§7, §10) are done. What is left is time-based
rather than build-based: a **24h+ soak** for RSS creep and a **sustained parity run** (§1). Neither can
be shortcut by writing more code, which is the honest reason they are still open.

**Scaled mode and anything node-gated (§11, §12):** not production-ready, and correctly deferred - the
project's "build only what we can verify live" discipline is why. Don't let a red column here read as
failure; it's scope, clearly fenced.

Ship the column you can defend, and name it.
