# Sprint: amiable-axolotl - **closed**

> Closed. Everything in scope shipped, and the sprint then absorbed RFC-0022 in full
> (built, released as 0.8.0/0.8.1, verified 10/10 on a clean box, documented). Successor:
> [boisterous-badger](sprint-boisterous-badger.md).

Working order for the open issue tail, front-loaded by "does this breach a non-negotiable or block
other work." Companion to [backlog.md](backlog.md) (RFC leftovers) and the
[roadmap](high-level-roadmap-jul-aug-2026.md) (strategy, now historical record).

Scope is the two open GitHub issues:

- **[#147](https://github.com/nightswatchhq/nuthatch/issues/147)** - Roost per-cursor failure isolation
  (blast-radius conformance gap).
- **[#150](https://github.com/nightswatchhq/nuthatch/issues/150)** - Audit tail: remaining LOW/DiD items
  + two feature-sized fixes.

**The cut line is after tier 2.** Everything above it is correctness or conformance; everything below
is hygiene. If the window runs short, ship tiers 1-2 and let the rest accumulate - nothing in tiers
3-4 gates RFC-0021's remaining slices or 0023's tier-3 fallback.

---

## Where the roadmap stands (context for the ordering)

Strategy phase closed 2026-07-21; RFCs 0019-0023 Accepted. Current build state:

| RFC | State |
|-----|-------|
| 0019 Registry & distribution | **Implemented** - FsStore, S3 `ObjectStore`, private-nest auth (live S3 verification pending a VPS run) |
| 0020 N-1 upgrade | **Implemented** - all 4 slices; the subgraph resync tax is dead |
| 0021 Multichain roost | Accepted, **slice 1 shipped** (`[[chains]]`, per-chain grouping, per-cursor runtime + budget) |
| 0022 Distributed scaled mode | Accepted, **design only** - dependency-gated on 0013-scaled + 0021 |
| 0023 eth_call derive-first | **Tiers 1-2 shipped** (recipe library + metadata cache); tiers 3-4 pending |
| 0024 eth_call engine (revm) | Draft, build deferred |

Long-standing blockers unchanged, and both are provisioning rather than coding: a colocated reth node
(unblocks 0003 → 0014), and scaled-mode Postgres (where 0013's DataFusion work should start).

---

## Progress

- **Tier 1: done (2026-07-27).** [RFC-0026](rfcs/0026-fault-quarantine-and-partial-health.md) written
  and all 3 slices shipped - **#147 closed**. A nest's error quarantines the nest; a cursor's death
  quarantines the cursor; nothing quarantined reports itself healthy.
- **Tier 2, item 3: done (2026-07-27).** `detect_reorg` wrong-chain guard - `verify_chain_ids` checks
  every endpoint at startup (per-endpoint, because failover hides a mixed-chain pool), and
  `detect_reorg` now refuses to roll back towards genesis when no checkpoint is canonical, terminally,
  instead of returning `Some(0)`. Loopback JSON-RPC mock harness added - **tier 2 item 4's failover
  test can reuse it**.
- **Tier 2, item 4: done (2026-07-27).** Real RPC failover (broken endpoint tried first, call recovers
  via the next, dead one cooled down; plus the all-broken case) and the warm-restart rebuild e2e.
  The warm-restart work came with a lesson worth keeping: the obvious version of that test - seal, drop,
  respawn, compare to a clean replay - **passes even with the sealed-through guard removed**, because
  pruning makes the watermark redundant on the normal path. Verified by mutation. A second test
  reconstructs the seal→prune crash window (segments hold [1,5], hot still holds [1,10], watermark
  stale), which fails 7000-vs-5500 under that mutation. **Tier 2 is complete.**
- **Tier 3, item 5 (F-C3): done (2026-07-27).** The "single writer" doc corrected (two writer *tasks* -
  ingest and the alert-outbox drain - serialised by redb; what is single is the **cursor**), and both
  the window commit and the outbox drain moved to `spawn_blocking` so a contended fsync no longer parks
  a tokio worker that is also serving the API.
- **🔴 SECURITY (2026-07-27): `/sql` accepted `;`-stacked statements = arbitrary file write.** Found
  while writing tier 3's statement-stacking test - the defence that test was meant to confirm did not
  exist. `conn.prepare` is NOT single-statement on the bundled duckdb-rs: it prepares *and executes*
  `SELECT 1; INSERT …`. The leading-keyword gate only inspects the first statement, and `COPY … TO` /
  `ATTACH` write to disk regardless of the in-memory connection. Verified end-to-end through `query()`:
  both wrote real files. Fixed by `reject_statement_stacking` (our own guard, string-literal aware),
  with 3 regression tests. **Shipped in 0.6.1 and earlier - see the prod-exposure note below.**
- **Also measured:** the `allowed_directories` defence-in-depth layer is **not enforced** on the DuckDB
  we bundle. `reject_file_access` is the only thing stopping a file read. Comments corrected from
  "enforcement varies by version" to the measured fact, with a tripwire test that fails if a future
  bump makes the layer real.
- **Tier 3, item 6: done (2026-07-28, #155).** All four remaining tests landed - screen component
  tamper, `fetch_shape` fail-open, cold-seed i128 overflow parity, and fixture-driven `abi.rs` response
  parsing.
- **Tier 3, item 7 + tier 4 item 10: partly done (2026-07-28, #157).** `getrandom` aligned and the
  `deny.toml` cross-version parse quirk fixed. **Still open:** `utoipa` 4.x → 5.x and the `arrow` 56/58
  dedupe.
- **Tier 4, items 8 + 9: done** on `chore/audit-tail-did-and-docs` (L6 outbound-SSRF warn, L7
  `Authorization: Bearer`, F-D3 segment-hash scope, F-C4 consistency, L4). **Not yet merged** - that
  branch also edits `operators.md`, which tier 5 below rewrites; take the tier-5 version of that file
  wholesale rather than reconciling hunks.
- **CI (2026-07-28).** The footprint job takes its RPC from a secret; free public-RPC limits documented
  in the README.
- **Tier 5 (new, see below): in progress (2026-07-28).** Doc reconciliation.
- **Tier 3, item 7 closed (2026-07-28) - one bump done, one not ours.** `arrow`/`parquet` 56 -> 58
  dedupes the tree (one `arrow` instead of two, `Cargo.lock` down ~150 net lines) and is **held for
  0.7.0**, not 0.6.2: it shifts newly-sealed segment hashes (audit F-D3), which is not something to put
  in a security patch. **`utoipa` is not actionable from this repo** - it arrives transitively via
  `dbsp -> feldera-ir -> utoipa 4.2.3` with no direct use in our source, so the RUSTSEC-2024-0370
  advisory (build-time only) stays ignored in `deny.toml` until an upstream dbsp bump. The backlog item
  was mis-scoped; it should read "re-check on the next dbsp bump".
- **Tier 5 done (2026-07-28).** Items 11-14 shipped in #160.
- **0.6.2 cut (2026-07-28)** - security only. Draft advisory **GHSA-jvjx-5528-r6mm** awaits publication
  once the binaries are up.
- **Issue [#162](https://github.com/nightswatchhq/nuthatch/issues/162) filed** - `e2e_solo::
  compatible_hot_upgrade_flips_backing_after_catchup` is flaky under full-suite parallel load. Written
  up as a possible *correctness* question rather than noise: if `await_catchup_and_flip` can return while
  the new version is genuinely behind, that is the RFC-0020 upgrade guarantee wobbling.
- **CI gate still flaky.** The footprint job has no `FOOTPRINT_RPC` secret, so it runs against free
  public endpoints and fails roughly one PR in three with "indexed 0 transfers". Needs a mainnet
  endpoint; the Alchemy app currently to hand is Arbitrum-only, so either ETH_MAINNET gets enabled on it
  or the job moves to an Arbitrum contract (which would change what the number means).

## What comes next - and why it is not RFC-0027

Developer feedback from **ETHGlobal Pragma Lisbon 2026** (three teams ran nuthatch, two load-bearing)
reprioritised the queue. The single piece of glue a team had to write was an **RPC proxy splitting
oversized `eth_getLogs` requests** - so [RFC-0028](rfcs/0028-adaptive-log-range-control.md) goes ahead
of [RFC-0027](rfcs/0027-the-live-roost.md). Order:

1. **0.6.2** - security only. *(done)*
2. **Docs from the feedback** - the any-EVM-chain path (which `dev` supports and `init` refuses) and
   `operators.md` discoverability. *(#163)*
3. **RFC-0028 build + a minimal `sql --receipt`** -> **0.7.0**, carrying the arrow dedupe.
4. **RFC-0027** - the live roost, for the GraphOps operator story.

- **Next:** land #163 and #161, publish the advisory, then start RFC-0028 slice 1 (error taxonomy).

## Tier 5 - doc reconciliation (added 2026-07-28)

Added to sprint scope because a doc that misinforms is worse than no doc, and these are what an
external operator reads first. **Ordered above tier 3's remaining deps bumps**: `utoipa` 4→5 is a
breaking major that may not land inside the window, and docs must not be what slips.

Blocked-on note: **cut 0.6.2 first if possible.** Both `prod-readiness.md` and `operators.md` have to
state a version truth, and "the fix exists but is unreleased" expires the moment the tag lands.

11. **`operators.md` rewritten and merged** ✅ - absorbed the platform-team material (division of
    labour, sizing, observability + alerting, failure model, runbook, data lifecycle, known gaps,
    go-live checklist) alongside the existing deploy recipes, MCP wiring and stability contract. Fixed
    two wrong facts it had carried since RFC-0021: roosts described as same-chain-only, and the budget
    described as per-runtime rather than per-cursor. A separate readiness doc was drafted and then
    folded in - two files would have duplicated the guards table, the metrics list and the roost
    description, which is exactly how the drift below happened.
12. **Progress log** ✅ - one labelled catch-up entry for 2026-07-22 → 28 rather than eight
    retrospectively fabricated per-push entries.
13. **`backlog.md` + `prod-readiness.md`** ✅ - reconciled to 2026-07-28: RFC rows 0019-0027 added,
    SEC-9 marked resolved (per-nest metrics landed with RFC-0026), the unreleased `/sql` fix recorded
    as a release blocker, `/ready` semantics updated for RFC-0026, and the 0.x upgrade-path question
    answered from the production 0.3.0 → 0.6.0 run.
14. **Small items** ✅ - README RFC range, `bench query` documented in `benchmarks.md`, and RFC-0012's
    dead `examples/roost` link (the directory was deliberately retired in `f154351`).
15. **Still open:** the nest catalogue has no livepeer entry - deferred until that nest is committed.

## 🔴 Open: prod exposure of the `/sql` stacking hole

The fix is on this branch and **not** released. Per session notes (2026-07-22, unverified since): the
Lodestar box runs **0.6.1** with `/sql` reachable through Caddy behind basic auth, services bound to
loopback, processes running as the unprivileged `nuthatch` user. So the hole was live but gated by
basic auth and limited to what `nuthatch` can write (nest dirs, its own home) - not root.

Decisions for Chief, none of them mine to make:
1. Patch release (0.6.2) and bump the box, or wait for the sprint branch to land whole?
2. Does this warrant a `SECURITY.md` advisory / GHSA, given a public (if authenticated) endpoint?
3. Rotate the `lodestar` basic-auth credential as a precaution?

## Standing practice adopted mid-sprint

**Mutation-check any test written to pin a specific bug.** Break the guard, confirm the test goes red,
restore. A regression test that passes in both states documents behaviour but protects nothing - and
the difference is invisible from a green run. Corollary, learned the hard way twice: **when a test is
written to confirm a documented defence, confirm the defence exists.** Both of tier 3's security tests
failed on first run, and neither was a test bug.

**Edit source with anchored replacements, never index arithmetic.** An index-based splice on
`analytics.rs` silently deleted 19 existing tests; the compiler was happy and the remaining tests were
green. Only the test *count* dropping gave it away.

## Tier 1 - start here (blocks other work)

1. **#147 mini-RFC.** A day, not a week. Settle four things only:
   - quarantine semantics - does a *nest* fault differ from a *cursor* fault?
   - retry/backoff policy for a quarantined cursor;
   - how `/ready`, `/nests`, and metrics express **partial** health;
   - whether the process ever exits at all.

   Everything else is implementation detail.

2. **#147 implementation.** Restructure `roost_index_loop` to catch-and-quarantine rather than
   `?`-propagate (`indexer.rs:541`, `:584`), and replace the `select_all(ingests)`-aborts-everything
   behaviour at `roost.rs:385` with per-cursor supervision. Ship with a test that kills cursor A and
   asserts cursor B keeps serving.

**Why first:** the only open item breaching a `CLAUDE.md` non-negotiable ("one chain's stall or reorg
must not harm another"). Today chain A's reorg-below-finality `bail!` tears down chain B and exits the
roost. RFC-0021's remaining slices and 0022's scheduler both need these semantics to *exist* before
they can assume them.

## Tier 2 - next, while the reorg/ingest path is already open

3. **`detect_reorg` wrong-chain guard** (#150, MED). Verify `chain_id` per endpoint once, or require
   bounded-depth/quorum confirmation before a deep rollback. Same code neighbourhood as #147, so cheap
   to fold in; the residual risk is a *fresh* nest re-indexing (an established nest is already
   contained by the sub-finality bail).

4. **Warm-restart rebuild e2e** and **real RPC failover** tests. The two tests that prove
   already-landed HIGH fixes hold end-to-end - highest value per line of the eight test items. The
   warm-restart one needs the TapeSource harness; failover needs only a loopback mock (first endpoint
   fails → call succeeds via second, dead one marked unhealthy).

## Tier 3 - steady chipping (small PRs, any order)

5. **F-C3 single-writer / async blocking.** Reword the inaccurate "single writer" doc (it is two redb
   writers - ingest + alert-outbox - serialized by redb, so integrity holds), then `spawn_blocking` the
   ingest commit/seal and outbox writes so a contended fsync stops parking a tokio worker.

6. **Remaining tests:** `;`-statement-stacking refusal (verify DuckDB single-statement `prepare` first);
   lockdown-backstop-alone (out-of-allowlist read denied by `allowed_directories`/`lock_configuration`
   with the denylist off); screen-component tamper (`component_hash` differs / load refused); cold-seed
   i128 overflow parity; `fetch_shape` fail-open (`transfers:true` for an older nest with no `/shape`);
   `abi.rs` fixture-driven Sourcify/Etherscan parse tests.

7. **Deps with actual weight:** bump `utoipa` 4.x → 5.x (drops `proc-macro-error`, RUSTSEC-2024-0370,
   build-time only) and dedupe `arrow` 56/58.

## Tier 4 - tail end (whenever, or never)

8. **Low-value defence-in-depth:** L6 outbound-SSRF warn on a freshly-loaded nest declaring non-loopback
   webhook/RPC/alert URLs; L7 accept `Authorization` alongside `?token=`. Both sit on top of layers that
   already hold.

9. **Docs-only:** F-D3 (Parquet `created_by` couples segment hash to the arrow-rs build - document the
   cross-version scope boundary or pin it); F-C4 (entity store vs IVM views are eventually consistent -
   document the reorg-window skew for consumers joining `/balances` and `/sql`); L4 (already noted in
   `views.rs`, accepted).

10. **Residual hygiene:** align `getrandom` (0.2 direct pin vs 0.3/0.4 in tree); the `deny.toml`
    cargo-deny 0.18.2 local-parse quirk; re-check the ignored `quick-xml` advisories on the next dbsp
    bump. The `starlark` subtree is really a roadmap decision (0018 §2 retired it), not a chore.

---

## Reviewed, won't-fix (carried from #150)

**L3 - one malformed log fails the whole `getLogs` window.** Skipping would silently drop an on-chain
event, which is a correctness bug. Fail-and-retry round-robins endpoints and stalls loudly only if
*every* endpoint returns bad data. That is the correct fail-safe; keeping it.
