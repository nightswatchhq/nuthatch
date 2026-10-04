# Sprint: undaunted-uakari

**Close the blocker and every high from the 2026-10-02 cold audit of 4.1.0, in nuthatch and in
burrmill, and ship the result as 4.1.x.**

The audit (nuthatch #1629 to #1691, burrmill #9 to #24) was two readers on a copy of the tree the
day after the engine changed. Nothing in it came from a live nest, which is the point: these are
the faults we would otherwise meet in production, in the order production would choose.

## Definition of done

Every issue carrying the **`undaunted-uakari`** label, in `nightswatchhq/nuthatch` and in
`nightswatchhq/burrmill`, is closed, and no open PR is for one of them. The label, not this
document, is the record of scope.

## Order

1. **The process dies from one statement: #1655, burrmill #11, then #1654 and burrmill #9.** A 2 KB
   chain of binary operators overflows the planner's stack and aborts the host, which takes every
   mounted nest with it. Bound expression depth on the parsed AST in nuthatch and plan on a known
   stack in burrmill; both, because either alone is a workaround. The cancel seam is the same
   territory: an interrupt that lands before the token is armed is lost (#1654), and `sql_for_each`
   resets the token on entry (burrmill #9).
2. **Rows stored under the wrong hash: #1629, #1634, #1633, #1630, #1635.** A window's logs and its
   checkpoint come from two calls, so a fork's rows commit under the canonical hash and reorg
   detection never fires. The runtime cursor has no tail refetch and never drops its timestamp
   cache on a reorg, a resumed seal-direct factory backfill runs with no children, and views are fed
   before the window commits. Property tests exist for the reorg store; none of them reorgs between
   the two calls, so the first job is the fixture that does.
3. **The sealed layer counts rows twice: #1631, #1632, #1644.** A crash between a seal and its
   watermark re-seals rows a provisional segment already folded, and the manifest cannot tell. The
   watermark can be durable before the bytes are. A live reclaim can delete a segment a co-tenant's
   seal is about to reference. Sealed segments are immutable by rule, so every one of these is a
   wrong answer for ever; fix the ordering, then write the crash test that would have caught it.
4. **Work with no budget: #1650, #1657, #1652, burrmill #10, #1658.** A wide-cell SELECT is a
   1.6 GB transient before the 64 MiB cap sees a row; `/entity` and the unknown-table error path run
   cold scans with no permit; burrmill's hidden `__raw`, `__hot` and `__union` registrations are
   addressable from a public statement and bypass the historical window. #1658 is the same defect
   on the graph surface and lands last, since it only exists in a `graph` build.
5. **Config and the admin API: #1656, #1638, #1639, #1640, #1641.** A misspelt `sql = "deny"` opens
   `/sql` silently; the fix from #1582 goes on every file, not one. An unmount can be overtaken by
   the job it says is absent, lifecycle routes report success on a failed write, a mount named
   `health` boots refused, and the 32-entity ceiling is not checked on a live mount.
6. **Advice that double-counts: #1664 and burrmill #12.** `SUM(value_dec)` includes the overflow
   rows the column reports as NULL. The engine half fixes the aggregate; the nuthatch half corrects
   the README, the MCP tool text and the skill, checks the fleet's views for a bare sum, and adds a
   39-digit parity case, because the corpus tops out at 21.
7. **Release.** A 4.1.x through `release.yml`, then deployed to the Lodestar box and the QoS nest and
   left running long enough to read RSS and tip lag against 4.1.0.

## Rules

- **Every fix lands with the test that was missing.** The audit found each of these because a test
  stopped one case short. The PR carries the case that fails on main, and a mutation check on the
  enforcing line, not a prose assertion.
- **Reproduce before fixing.** Each issue names a file and a line and a trigger. The first commit on
  any of them is a red test or an executable probe; #1650 in particular wants the real RSS measured
  before the cap is moved.
- **The 2 GB per-cursor budget is the yardstick for items 1 and 4.** A fix that bounds the statement
  by refusing it is fine. A fix that bounds it by spending more memory is not.
- **Burrmill fixes land in burrmill, then the rev bump lands here.** No nuthatch-side workaround for
  an engine defect that has a burrmill issue, and no copy of engine code into this tree.
- A release is cut through `release.yml` with all its gates.

## Not in this sprint

- **The 27 `Unconfirmed` issues (#1665 to #1691, burrmill #22 to #24).** They are smells one reader
  saw and the other did not. Each needs a confirm-or-close pass before it is work; that pass is its
  own short job, after this sprint, and anything confirmed high joins the next one.
- **The confirmed mediums and lows not named above**: #1636, #1637, #1642, #1643, #1645 to #1648,
  #1651, #1653, #1659 to #1663, and #1624. Real, filed, waiting. They fall to the next sprint
  unless a fix above touches their line, in which case take them in the same PR and label them.
- **Burrmill dialect parity (#13 to #20) and build time (#6, #7).** Each is a refusal or a
  difference where DuckDB answered; none loses data or memory. They are a parity sprint of their
  own once the corpus has the 39-digit case from item 6.
- The parked programmes: RFC-0059/0060 (#1441, #1442), cross-nest SQL (#1324), the GTM plan, and
  `docs/frozen-for-2027.md` with its reopening rule.
