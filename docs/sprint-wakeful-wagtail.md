# Sprint: wakeful-wagtail

**Scoped 2026-10-03. Give burrmill the gates nuthatch has, then close every confirmed defect the
audit and the triage left open, in both repositories.**

Two passes over 4.1.0 have left a list of confirmed faults: the cold audit's mediums and lows that
`undaunted-uakari` did not reach, the twelve the `vigilant-vole` triage confirmed, and burrmill's
dialect differences from DuckDB. None is a blocker. Together they are the difference between an
engine and a product that answers like the one it replaced.

The engine half has a prior problem. Burrmill has no CI. Its main fails its own `cargo fmt --check`
(804 diffs) and `clippy -D warnings` (31 errors), so every burrmill fix this month was verified on
one laptop and merged on trust. A dialect sprint on that footing would be the same thing at scale.
So the gates come first.

## Definition of done

Every issue carrying the **`wakeful-wagtail`** label, in `nightswatchhq/nuthatch` and in
`nightswatchhq/burrmill`, is closed, and no open PR is for one of them. Burrmill's main passes a CI
workflow on every push and pull request.

## Order

1. **Burrmill gets a CI, and its main goes green under it.** A workflow running `cargo fmt --check`,
   `clippy --all-targets --features datafusion -D warnings` and the test suite, on every push and PR,
   with actions pinned by SHA as nuthatch's are. The 804 formatting diffs land as one mechanical
   commit with nothing else in it; the 31 clippy errors are fixed, not allowed. The one test that
   fails on macOS under the 64 MiB sort budget is understood, not skipped. Branch protection is
   Chief's to switch on once the workflow is green.
2. **Reorg detection that can go quiet: #1665, #1666, #1668, #1667.** One seam. The pin at the cut
   happens before the watermark moves or the watermark does not move; a window committed with no
   checkpoint is a warning a human sees; a walk where every answer was `None` is not a fork at block
   0; and a finalized tag past what the chain could have finalized is refused. #1736's checkpoint
   prune changed the shape of the first and third; read the triage comments first.
3. **Burrmill answers like DuckDB, or refuses by name: #16, #14, #15, #19, #17, #18, #20, #21, #33,
   #22.** The p1s first: `//` over a `DECIMAL(38,0)`, ORDER BY an expression over an alias, `%` by
   zero. Each lands with a case in the dialect-parity corpus that DuckDB's recorded answer pins, which
   now carries a 39-digit value. #21 and #33 are the statement-boundary pair; #22 is the generator's
   missing in-tree target. The nuthatch pin bump follows each burrmill merge, never a nuthatch-side
   workaround.
4. **Numbers past the column: #1679, #1678, #1653.** A value between 10^38 and `i128::MAX` passes
   `load_relation` and dies executing; offchain snapshots of two integer widths refuse to merge and
   the view is skipped; DuckDB-era authored SQL that Burrmill refuses at define time vanishes at debug.
   Each one is a view or an entity that silently is not there.
5. **The admin API's edges: #1645, #1646, #1647, #1673, #1674, #1648.** A suspended name re-mounted
   with a different NID, `?wait=true` that is not idempotent, a suspend whose record is absent between
   two writes, `_admin` as an alias, reclaim racing a fetched mount, and a health gauge that outlives
   its nest.
6. **Secrets and unbounded reads: #1663, #1690, #1651, #1661.** Webhook URLs in logs, the MCP bridge
   echoing its URL to a hosted model, the `/sql` scrubber missing object_store's path form, and an
   unbounded JSON line.
7. **The rest: #1677, #1683, #1624, #1637, #1662.** Hot rows held twice per session, an OOM hint that
   names the wrong variable, doctor calling a pruned endpoint archive, factory children folded in
   map order, and the scaffolded skill's wrong advice about `/sql`.
8. **Release.** 4.2.0 rather than a patch: 4.1.1 to here carries an operator-visible admin change
   (suspend and resume need a JSON content type, #1734) and engine answers that move. Cut through
   `release.yml`. The deploy waits for Chief, with the 4.1.1 roll.

## Rules

- **Every fix lands with the test that was missing**, red on main first, and a mutation check on the
  enforcing line. Carried over.
- **Burrmill fixes land in burrmill, verified by its CI, then the rev bump lands here.** No copy of
  engine code into nuthatch and no nuthatch-side workaround for an engine defect.
- **Burrmill PRs merge on green CI once item 1 is in.** Until then, nothing else merges there.
- **A dialect fix states DuckDB's answer, not ours.** The corpus is the reference; a refusal by name
  is acceptable where matching DuckDB would mean guessing, and the refusal says so.
- **The 2 GB per-cursor budget binds** any change to how hot rows or sessions are held (#1677).
- One PR per seam is fine; one commit per issue inside it, each closing its own issue.

## Not in this sprint

- **The two operations items from `vigilant-vole`, #1714 and #1720, and finishing the 4.1.1 roll.**
  Held for Chief; they need the box.
- **Build time and footprint**: burrmill #6 and #7.
- **The test-strategy list**: #1713 (parity timer, deliberately not yet), #1716, #1718, #1719, #1722,
  #1723, #1724, and #1741.
- **The undecided pair**, #1670 and #1671, until someone captures the evidence each names.
- The parked programmes: RFC-0059/0060 (#1441, #1442), cross-nest SQL (#1324), the GTM plan, and
  `docs/frozen-for-2027.md` with its reopening rule.
