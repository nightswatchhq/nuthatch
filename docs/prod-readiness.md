# Production-readiness checklist

The bar a nuthatch release must clear before it's pointed at someone's real workload, unattended.
Reconciled against [CLAUDE.md](../CLAUDE.md) (non-negotiables + build order), the
[RFC series](rfcs/README.md), the [issue queue](https://github.com/nightswatchhq/nuthatch/issues), and
[CI](../.github/workflows/ci.yml) on **2026-08-20** (repo at `2.6.0`).

This is a *standing* checklist - the target, not a claim it's all done. Status reflects what's
verifiable today. When you cut a release, walk it top to bottom and update the flags with evidence.

**Every 🟡 and ⛔ names the issue that tracks it.** This file answers *"is this safe to ship?"*; the
[issue queue](https://github.com/nightswatchhq/nuthatch/issues) answers *"what is being done about
it?"* - and the issue is the one that moves. Do not record work here that has no issue, and do not
close an issue by editing this file.

The reason for that rule is on this page's own history: two entries here outlived their fix. MSRV was
listed amber for claiming `rust-version = 1.85` long after it was corrected to `1.95`, and the
"write a production guide" item stayed amber while `operators.md` grew to 975 lines of exactly that
guide. Both are green as of 2026-08-06. A checklist nobody re-reads decides what gets built next.

## Legend & scope

| Flag | Meaning |
|------|---------|
| ✅ | Done and verified (test, bench artifact, or live run backs it) |
| 🟡 | Partial - exists but incomplete, unverified, or narrow |
| ⛔ | Not started, deferred, or blocked (see "Blocked on") |

**The flag is the status; the `[ ]` box beside it means nothing.** 66 rows here read `- [ ] ✅` -
done and verified, in an unchecked box - because the boxes were never maintained. GitHub renders
them as a page of empty checkboxes regardless, so a reader skimming the ticks rather than the flags
reads this document as far worse than it is. Two rows carry `[x]`, which makes the boxes look
meaningful; they are not. Read the emoji.

**Two production targets, graded separately** - don't conflate them:

- **Embedded / single-chain runtime** (the primary deliverable): one binary, one chain, tip-follow +
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
- [ ] ✅ **Footprint ≤ 2 GB RAM** for a single-chain runtime, CI-enforced. - *`footprint.sh` gate, 256 MB
  ceiling, measured ~37 MB, for the single-nest backfill tripwire. The dense-multi-nest-at-tip case is
  now its own **required** CI gate too - `per-cursor RAM budget (dense multi-nest)`, 20 nests on one
  cursor against the 2048 MB budget, mutation-checked (§3/§5, #284/#391). Two ceilings, two scenarios,
  neither subsumes the other.*
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
- [ ] 🟡 Decodings are **versioned**; no retroactive re-decode of stored history when ABIs improve. -
  *The no-retroactive-re-decode half holds. The versioning half has one narrow gap open and tracked:  **[#653]**
  a nest whose config **gains** events keeps running on data indexed under the old config and stamps
  the new `registry_hash` on it anyway - the version tag lies about what actually decoded those rows.
  A fix exists on an unmerged branch; not credited here.*
- [ ] ✅ Golden/deterministic tests per handler and view (fixed fixtures in → exact state out).
- [ ] ✅ Property tests: random reorg depths converge to canonical state (`e2e_reorg.rs`).
- [ ] ✅ Nest invariant/parity checks (`nuthatch check`) run hermetically in CI against committed
  fixtures. *(RFC-0002 §5)*
- [ ] ✅ **Sustained** byte-identical multi-nest-vs-solo table parity. - *Run live 2026-07-28 on
  Arbitrum: two nests indexed solo and again behind one shared cursor over the same 2,400-block range,
  compared table by table - **20 tables, 17,108 rows, byte-identical**, including empty tables and the
  topic0-disambiguated `weth__transfer_ddf2`/`_e192` pair.*
- [ ] 🟡 Factory / dynamic-contract discovery correctness at scale. - *Implemented (0009). The getLogs-  **[#271]**
  cap recovery this row used to name as the open risk shipped and closed real: a factory nest that  **[#272]**
  crosses the cap no longer dies permanently, it recovers via an address-filtered refetch (`#297`,
  `6412ee5`, commit-backed close - not a bare issue-close). What is still open is different: the
  discovered-child watch-set is unbounded, with no `end`/expiry condition (**#271**), and wildcard-
  address decode is unimplemented (**#272**). Both OPEN, zero commits against either.*

## 2. Reliability, reorgs & crash safety

- [ ] ✅ Reorgs only ever touch the mutable hot store; sealed Parquet is append-only past finality.
- [ ] ✅ Atomic seal/prune (no torn segment on crash mid-seal). *(0.4.0 hardening)*
- [ ] ✅ Crash-safety e2e (`e2e_crash_safety.rs`): kill mid-index, restart, converge.
- [ ] ✅ Single-writer discipline: only the ingestion thread writes DuckDB/redb; queries attach
  read-only. No concurrent-writer design anywhere.
- [ ] ✅ Single cursor / single process / one observable failure boundary. A second chain = a second
  process (never multiplex chains behind one cursor).
- [ ] ✅ Per-nest blast-radius isolation in a runtime: one nest's bad view / runaway factory can't harm
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
  STALLED"), and `/ready` returns 503 once no poll has succeeded within 90 s (§7). `/ready` now also
  catches a second, distinct failure shape: a **wedged** cursor that keeps polling successfully but
  makes no block progress, not just a dead one (`#578`, `804249f`, 2026-08-14).*

## 3. Performance & footprint budgets

Benchmarks are **CI artifacts**, not vibes - every published number traces to a `bench-report.json`
with date/provider/hardware/commit (the RFC-0004 house rule).

- [ ] ✅ Backfill throughput bench exists and is reproducible (`nuthatch bench backfill`). - *Floor
  ≥10K events/sec, aim 30K.*
- [ ] 🟡 A **published, current** backfill number for the release commit on reference hardware. -  **[#285]**
  *Still open, and the existing artifact is worse than "stale": `docs/bench/obib-case1.json`
  (3,934.59 events/sec, 2026-07-30) cites commit `707e1af`, which is not a valid object in this repo -
  unreproducible, not just old - and its own number sits under this file's own "floor ≥10K events/sec"
  a few lines up. Needs a fresh run pinned to a real 2.6.0 SHA.*
- [ ] ⛔ Tip-lag benchmark (notification → row queryable) as a tracked number. - *Meaningful number  **[#282]**
  needs ExEx. **Blocked on:** reth node (0003).*
- [ ] ✅ Entity point-read p50/p99 bench tracked across releases, `point-read latency` a **required**  **[#283]**
  CI context. - *Landed PR #375 (`ef3b619`). Both gaps its own discussion left open are since closed
  with real evidence: the gate's fixture was a near-empty 256-row store that a 32-core dev-box and a
  4-core runner couldn't tell apart - re-pointed at the same dense-multi-nest fixture §5/#284 uses,
  mutation re-measured at 87x above ceiling (was 1.8x) (**#424**, PR #455, `03d296b`). And the
  committed baseline was dev-box hardware (p50 1.24µs) enforced on 4-core runner numbers (0.59-0.82µs)
  - `docs/bench/point-read.json` is now a runner-produced artifact, not a ported one (**#385**, PR
  #451, `00249d9`).*
- [ ] ✅ Peak-RSS regression gate wired for the **dense multi-nest** scenario, not just single-nest  **[#284]**
  `--backfill 200`. - *`per-cursor RAM budget (dense multi-nest)` is a **required** CI context: 20
  nests on one cursor, the real 10-event Uniswap V4 ABI, 200 blocks live tip-following, two ceilings
  (2048 MB budget, 180 MB regression band from 8 runs). Mutation-checked against six cases including a
  synthetic 2.4x leak caught at 323 MB with 1.7 GB of budget headroom still unused - the case a
  budget-only ceiling cannot see. PR #391, `76fa504`, 2026-08-10.*
- [ ] ✅ Regressions fail the build (benchmarks-as-gates principle established). - *Extend coverage as
  the benches above land.*

## 4. Security

- [ ] ✅ **`/sql` `;`-statement-stacking fixed and released in 0.6.2** (2026-07-28). A stacked
  `COPY … TO` / `ATTACH` was an arbitrary file write, bounded by the service user. Present in **0.6.1
  and earlier**; fixed by `reject_statement_stacking` with regression tests, released as **v0.6.2**
  with binaries, and the Lodestar box upgraded and verified the same day. *Any deployment still
  exposing `/sql` on ≤0.6.1 remains affected and should upgrade.* Advisory GHSA-jvjx-5528-r6mm was
  drafted and held unpublished as of 2026-07-29 - a decision, not an oversight - and **published on
  2026-08-02**, alongside GHSA-393p-f3vr-rf2r for the arbitrary file read, which is what the
  2026-07-31 audit recommended: two advisories together with the fixed versions named, rather than a
  quiet patch. Both are repository advisories; neither appears in GitHub's *global* database, because
  entry there needs a package in a supported registry and nuthatch ships as a binary rather than a
  crates.io package. Worth revisiting if that ever changes.
- [ ] ✅ Blob-mount RCE fixed (0.4.0 critical).
- [ ] ✅ `/sql` arbitrary file-read fixed (0.4.0 critical).
- [ ] ✅ **DuckDB `allowed_directories` is enforced** when `enable_external_access=false` is set at
  connection open (**[#289]**, quizzical-quail). Measured against `libduckdb-sys` 1.10504.0: the
  list is a restriction only with that startup flag, which we now pass. `reject_file_access` remains
  the primary control. The tripwire now asserts the second layer *does* refuse an out-of-allowlist
  `read_text`.
- [ ] ✅ `/sql` surface is structurally read-only (single-writer + read-only attach).
- [ ] ✅ A security review pass on the **serving surface** (`serve.rs`, `mcp.rs`, `webhooks.rs`,
  `analytics.rs`, `abi.rs`, `rpc.rs`) - *done (0.5.x hardening): no criticals; SQL read-only gate holds
  three-deep, no SSRF (ABI/RPC hosts are fixed constants), no file-read via `/sql`. Fixed: `/nest`
  webhook-URL disclosure, `/sql` error path-scrub, `screen_status` quote-escape, constant-time admin
  token, concurrent webhook delivery. Re-run per release on the diff.*
- [ ] ✅ Bind/exposure defaults are safe, admin surface hardened end to end. - *`dev` binds `127.0.0.1`  **[#292]**
  by default; off-localhost the admin surface requires `NUTHATCH_ADMIN_TOKEN` and is **unmounted**, not
  merely refused, without it (#412). A hardcoded `NUTHATCH_ADMIN_TOKEN=change-me` shipped in both deploy
  recipes and was live/exploitable, not theoretical - the Docker image's `CMD` binds `0.0.0.0:8288` -
  removed in #398. Query-FE derives its admin credential like every other role (#389); the control-API
  token guard moved to the route layer so a new route cannot forget it (#420); the admin surface is
  gated where it claims and discloses nothing extra, pinned on the wire (#418); `sanitize_sql_error`'s
  gap is pinned as structural rather than a fixture limit (#427/#431). **One item #292 named and left
  open, now closed on evidence:** the live `/nest` probe against a real provider key, the credential
  shape no synthetic fixture chooses - run 2026-08-15 against Lodestar's `horizon-nest` (a real key in
  a 25-character path segment), `/nest` `/` `/health` `/tables` probed for host, path segment and full
  URL, no match (**#428**).*
- [ ] ✅ Dependency vulnerability scan (`cargo deny`) wired into CI. - *`deny` job runs advisories +
  licences + bans + sources against `deny.toml`; the permissive-only licence gate is now enforced. Four
  transitive advisories ignored with written rationale (quick-xml not-reachable ×2; rkyv shared-pointer
  validation not-reachable; h2 0.3-line HTTP/2-server DoS not-reachable - we never enable axum's http2
  feature or run actix as a server). The wasmtime-wasi `FilePerms` bypass this row used to name as
  "tracked for a runtime bump" is no longer ignored at all - it's fixed, cleared by the wasmtime 44→46
  bump.*
- [ ] ✅ Effectful (capability-granted) components can only produce **annotations**, never canonical
  entities - purity checkable from the composition manifest. *(transform layer)*

## 5. The ≤2 GB budget under realistic load

Called out separately because it's the headline promise and the current gate only exercises the easy
case.

- [ ] ✅ Single nest, backfill, single chain: measured ~37 MB, gated at 256 MB.
- [ ] ✅ Multiple nests co-located in one runtime at tip, sustained, measured against 2 GB.
  **8 nests on one Arbitrum cursor** (2026-07-29): at tip, mean RSS **84 MB**, peak **89 MB** against
  the 2048 MB per-cursor budget - **4%**. Backfill peaked at **154 MB**, the more demanding phase.
  Adding a nest costs far less than the first one does: the cursor's RPC buffers and decode machinery
  are shared, so only the per-nest hot store is additive.

  **A qualifier the 2026-07-29/30 prod soak made necessary:** this bounds **density**, not
  **workload**. Those 8 nests were small and at tip; a *single* nest doing a 125M-block backfill on
  the same budget reached 427 MB by itself. Per-nest RSS is dominated by what a nest is *doing*, not
  by how many share a cursor - so read this as "co-tenancy is cheap", never as "a cursor uses 84 MB".
- [ ] ✅ Large-ABI / high-event-rate contract at tip (memory doesn't grow unbounded with hot-store  **[#286]**
  churn).
  **The high-event-rate half** is the `per-cursor RAM budget (dense multi-nest)` job: twenty nests,
  real 10-event Uniswap V4 `PoolManager` ABI, 200 logs/block, at tip, 2048 MB budget plus a
  regression ceiling. **The ABI-breadth half** is `wideabi-footprint.sh` in the same job: **one
  nest, 31 event types** (the SubgraphService figure named on the issue), 31 logs/block so every
  table is non-empty, at tip after a backfill, same 2048 MB budget. Floor on every table and on
  row count, so "under 2 GB" cannot pass when the workload indexed nothing. Hermetic; a fork can
  satisfy it. A regression ceiling for the breadth scenario waits on a runner noise band of its
  own - do not copy the density job's 180 MB.
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
- [ ] ✅ e2e harness exists (`TapeSource`) and covers solo, reorg, crash-safety, multi-nest parity.
- [ ] ✅ Fuzz smoke on the decode path - `fuzz smoke (decode path)`, a **required** CI context. -
  *libFuzzer/ASan against malformed ABIs and logs on `nuthatch-decode` (`b20ed5f`, #290). A bounded
  regression run rather than a real fuzzing campaign - nightly-only (SanitizerCoverage), deliberately
  off the pinned-1.95.0 toolchain the shipped binary uses.*
- [ ] ✅ MSRV is honest. - *Fixed at 1.0: `Cargo.toml` now declares `rust-version = "1.95"`, matching
  `rust-toolchain.toml`'s pinned `1.95.0`, so the declared floor is the one that is actually tested. The declared MSRV is not cosmetic: it silently selected DataFusion 48 over 54 during the
  RFC-0013 spike, and cargo reports that as a one-line warning nobody reads.*
- [ ] 🟡 Coverage of the AI/MCP surface (schema discovery, SQL exec, entity lookup, subscribe) with  **[#304]**
  the RFC-0016 eval harness. - *S1 eval harness gates the semantic-layer work; wire it in.*
- [ ] 🟡 `--offline` / no-network test path proving AI features degrade gracefully.  **[#304]**

## 7. Operability & observability

- [ ] ✅ Metrics surface exists (`metrics.rs`), **including per-nest series** - `{nest="…"}`-labelled
  `nuthatch_nest_*` plus `nuthatch_cursor_live{chain}` (RFC-0026), so a co-tenant runtime is attributable
  per nest rather than only process-globally. *(This closes the old SEC-9 gap.)*
- [ ] ✅ Health/readiness endpoint suitable for a supervisor. - *0.5.x: `/health` = liveness (plain
  `200 "ok"`); `/ready` = readiness - JSON with tip / last_block / lag / sealed_through / last-poll age,
  `200` when fresh and **`503` when stalled** (no successful source poll within 90 s ⇒ every RPC endpoint
  down). A just-started node gets grace (never-polled ≠ stalled). **0.6.x (RFC-0026):** `/ready` is now
  also mounted at the **runtime root** - `200` only when every cursor and nest is indexing, `503` naming
  what is quarantined - with per-nest `/<name>/ready` answering for that nest alone. Route traffic on
  the per-nest one and page on the root; wiring a load balancer to the root means one sick nest evicts
  every healthy sibling.*
- [ ] ✅ Structured logs at a sane default level; a clear "we are behind / we are at tip" signal.  **[#302]**
  *`--log-format json` emits one JSON object per line (level, target, message, timestamp, fields);
  `--log-format text` (default) keeps the human-readable format unchanged. `TipHeartbeat` restates
  the `block` / `tip` / `blocks_behind` signal every 60 s from both `index_loop` and
  `runtime_index_loop`, so an operator watching logs (rather than Prometheus) gets a machine-readable
  at-tip / behind-tip line on a slow clock rather than having to pattern-match `✓`. Verified
  compilation, 4 unit tests (lag arithmetic, throttle) and fmt+clippy on 1.95 (2026-08-14, #302).*
- [ ] ✅ Documented restart/recovery runbook and a backup/restore story for the redb hot store +
  sealed segments. - *[operators.md](operators.md) carries the failure model, the symptom→action
  runbook, backup/restore, and a go-live checklist (2026-07-28).*
- [ ] ✅ SSE **push** for live status - `/_admin/events`. *(This entry outlived its fix; it was shipped
  and sat here marked ⛔ regardless, which is how a checklist stops being trusted.)*
- [ ] ✅ Alerting hooks (`alerts.rs`, `webhooks.rs`) documented end-to-end with a runnable example.  **[#302]**
  *`examples/webhooks/README.md` (110 lines, landed `dfed0f8` / #328): both `[[webhooks]]` and
  `[[alerts]]` through one outbox, runnable `receiver.py --secret hunter2`, `nuthatch.toml` block,
  and five "surprises" (at-least-once, `since`, finality, depth gauge, alert signing). Two README
  false claims corrected in #302: the table said `[[webhooks]]` is "triggered by rows sealing (or
  hitting the tip)" - tip delivery does not exist yet, and `nuthatch.toml` load now refuses
  `finality = "tip"` with a clear error (#577); and `[[alerts]]` were described as "signed when the
  named webhook carries a secret" - `Alert` has no `secret` field, signing is `[[webhooks]]`-only.
  Two config tests for the tip-finality refusal (`a_tip_finality_webhook_is_refused_rather_than_silently_ignored`,
  `sealed_finality_webhooks_load_clean`). Verified on Linux 1.95 (2026-08-14, #302).*

## 8. Release engineering

- [ ] ✅ Versioning + release workflow in place (`release.yml`), reproducible `--locked` builds.
  *(RFC-0005)*
- [ ] ✅ `curl | sh` install path.
- [ ] ✅ MSRV is honest. `Cargo.toml` declares `rust-version = "1.95"` and every CI job pins
  `dtolnay/rust-toolchain@1.95.0`, so the declared floor is the tested floor. (Raised from an
  untested `1.85` in `b2abc9f`, 2026-07-14.)
- [ ] ✅ Cross-platform release matrix, stated plainly rather than implied:

  | Target | Built by | Tested by |
  |---|---|---|
  | `x86_64-unknown-linux-gnu` | `release.yml` | full `cargo test --locked` on every CI run |
  | `x86_64-unknown-linux-gnu` (scaled) | `release.yml` | the `--features postgres-store` job |
  | `aarch64-apple-darwin` | `release.yml` | **nothing - built and published, never exercised** |

  The macOS arm64 binary is compiled on a macOS runner and attached to the release, and no job in
  `ci.yml` runs on macOS. So the install path is covered but the behaviour is not: a macOS-only
  regression reaches a user before it reaches us. Scope the claim that way or add a macOS test job;
  do not leave it ambiguous.
- [ ] ✅ CHANGELOG / release-note discipline per tag. The standing rule, from the v0.9.1 incident:
  before tagging, run `git log --stat <previous-tag>..HEAD` and read every entry against the draft
  notes. Anything in the range that is not in the notes is either added or explained. The notes
  additionally state **"in-place safe"** or **"reseal required"** for the on-disk format, because
  that is the one line an operator has to read before upgrading.
- [ ] ✅ Documented upgrade path / on-disk format stability guarantee across `0.x` bumps. - *Proven in
  production: a `0.3.0 → 0.6.0` nest upgrade was a binary swap plus a restart - no data migration, no
  flag or unit changes, sealed segments and hot store preserved. Each release states "in-place safe" or
  "reseal required" explicitly; the contract is in [operators.md](operators.md).*

## 9. AI-native surface (MCP)

- [ ] ✅ MCP server compiled into the binary (`mcp.rs`), works offline against the local instance.
- [ ] ✅ `init` scaffolds schema + views + handlers + tests from the ABI.
- [ ] ✅ Ships `llms.txt` / docs-as-MCP / `.claude/skills/` in scaffolded projects.
- [ ] 🟡 The RFC-0016 governed semantic layer (`semantic.toml`, enriched `schema`, errors-as-prompts,  **[#304]**
  `explain`) - *in design, measure-first, not shipped.*
- [ ] 🟡 The RFC-0017 builder skill with CI-checked CLI/config reference drift. - *CLI-flags direction  **[#353]**
  ships (`cli_reference_names_every_real_flag`, PR #514) but that PR does not touch the gap #353 was
  narrowed to and closed against by mistake: `CONFIG_SOURCES` in `tests/skill_refs.rs` scans
  `config.rs`/`semantic.rs`/`runtime.rs` but not `src/allowlist.rs`, so `queries.toml`'s `NamedQuery`/
  `Ceiling` keys can drift from `config-reference.md` with CI green. Reopened; still real.*

## 10. Docs & first-run UX

- [ ] ✅ `<2 minute` first-indexed-query demo path (`init → dev → sql`).
- [ ] ✅ Terminal-native query REPL (`nuthatch sql`). *(RFC-0015 slice 1)*
- [ ] ✅ Operator docs, factory docs, benchmark docs present.
- [ ] ✅ A single "here's how you run this in production, unattended" guide that ties together
  §7 (ops), §4 (safe exposure), and §8 (upgrades). - *[`operators.md`](operators.md) is it: deploy
  recipes, the division of labour, capacity, what to scrape, what to back up, the stability contract,
  upgrade notes, known gaps, and a go-live checklist. Written against 2.0.0; its own container tags
  were refreshed for 2.5.0 after being caught five releases stale, with the rest of the document
  named explicitly as not re-read since - a stated staleness rather than a silent one, but at 2.6.0
  the gap is now six releases and growing. Not this issue's fix; named for whoever's turn it is.*

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
- [ ] ⛔ DataFusion federation across hot + cold behind one SQL surface. - *0013 §2/§4. **The gate  **[#279]**
  said no for 1.0** - DuckDB stays in both modes. Reopen if a DataFusion release closes the aggregate
  gap, or if a scaled-mode query genuinely needs one plan spanning Postgres hot and Parquet cold,
  which is the case DuckDB cannot serve and the real point of §2.*
- [ ] ⛔ Golden SQL-compat suite across both engines. - *Moot while there is one engine; the spike  **[#279]**
  already showed parity on the fold that matters.*
- [x] ✅ A multi-machine run. - *Done **2026-08-15 on published 2.4.0 artifacts** (#281, #597).  **[#281]**
  Control plane + Postgres + FE on a box in Nuremberg, a second writer on a machine in another
  country, over a real network. Both registered; killing the holding writer moved the lease to the
  remote worker and incremented `owner_fence` by a **real handover**; the remote worker indexed
  **2.2 M blocks** into the other machine's store; and with the control plane stopped and Postgres
  deliberately left up it indexed **3,000,000 blocks straight through the outage** and resumed on
  healing. **Clock skew is NOT covered** and remains 0.9.3-only - pushing a worker's clock needs root
  on a machine running other work; `verification.md` says so too.*

## 12. Infra-gated capabilities (the shared blocker)

Almost everything un-buildable-on-a-laptop traces to one missing box.

- [ ] ⛔ **Colocated reth node** (full for tip, archive for deep backfill/traces). - *Provisioning +  **[#276]**
  days of sync; hardware/ops, not code. Gates the two below.*
- [ ] ⛔ ExEx tip mode wired to a real node; `nuthatch-node` binary; honest tip-latency number. *(0003;  **[#276]**
  groundwork in, **blocked on** the node.)*
- [ ] ⛔ Firehose-class extraction (traces + state diffs), own-node/ExEx only. *(0014; **blocked on**  **[#277]**
  0003.)* - *One node-independent slice is buildable now and forward-compatible: the calldata decoder,
  `[extract]` config, `traces`/`state_diffs` schemas, and the unbounded-volume guard.*

---

## Bottom line

**Embedded, single-chain, single-nest:** the core (§0-§2, §6) is genuinely strong, and stronger than
the last stamp on this file recorded. Since 2.0.0: the **dense-multi-nest RSS gate** is real, required
and mutation-checked (§0/§3/§5, #391), not just a one-off measurement; the **point-read bench** moved
from a near-empty fixture on the wrong hardware to the dense fixture on the enforcing runner (§3,
#283/#424/#385); the **factory getLogs-cap recovery** shipped for real (§1, #297); `/ready` now also
catches a wedged cursor, not just a dead one (§2, #578); the **bind/exposure hardening** closed its
whole cluster including the live-credential probe against production (§4, #292/#428); a **fuzz gate on
the decode path** is a required check this file never named (§6, #290); and the **multi-machine run**
this file spent §11 calling unproven for months finally happened, on 2.4.0 (§11, #281/#597).

What is still genuinely open, not time-based: the **published backfill
number** is worse than stale, it cites a commit that no longer exists (§3, #285); the **DuckDB
file-access defence** is still one layer deep by design of the bundled build, not two (§4, #289); and
the **AI/MCP eval harness and offline path** remain in design (§9, #304). Two issues this walk found
closed against no evidence or the wrong PR - #289 and #353 - are reopened as of this pass; treat any
"closed" state on this file's cited issues as a claim to verify, not a fact, which is the whole reason
this rule exists.

**Scaled mode and anything node-gated (§11, §12):** not production-ready, and correctly deferred - the
project's "build only what we can verify live" discipline is why. Don't let a red column here read as
failure; it's scope, clearly fenced.

Ship the column you can defend, and name it.
