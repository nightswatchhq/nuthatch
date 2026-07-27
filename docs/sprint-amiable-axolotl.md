# Sprint: amiable-axolotl (2026-07-27 - 2026-08-03)

Working order for the open issue tail, front-loaded by "does this breach a non-negotiable or block
other work." Companion to [backlog.md](backlog.md) (RFC leftovers) and the
[roadmap](high-level-roadmap-jul-aug-2026.md) (strategy, now historical record).

Scope is the two open GitHub issues:

- **[#147](https://github.com/nuthatch-indexer/nuthatch/issues/147)** - Roost per-cursor failure isolation
  (blast-radius conformance gap).
- **[#150](https://github.com/nuthatch-indexer/nuthatch/issues/150)** - Audit tail: remaining LOW/DiD items
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
- **Next:** tier 3 remaining (4 more tests, `utoipa`/`arrow` bumps), then tier 4.

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
