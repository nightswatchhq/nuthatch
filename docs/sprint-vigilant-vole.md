# Sprint: vigilant-vole

**Scoped 2026-10-02, not started. Starts when every `undaunted-uakari` issue is closed and 4.1.1 is
running on the Lodestar box. Chief settled the three open questions the same day; they are recorded
at the foot.**

**Make production able to tell us when it is wrong, and close the confirmed p1s the cold audit left
behind.**

`undaunted-uakari` fixed what two readers found in the tree. It shipped twenty fixes onto four
production nests that nothing watches: no scraper, no alert rule, a mutation run
that has been dead for twelve days, and a consumer-side monitor that has been red since 2026-10-01.
The next defect of the #1629 class will be found the same way those were, by somebody reading the
code, unless the running system can say so first. That is this sprint's spine. The audit's remaining
p1s ride with it because they are small, confirmed, and three of them are reachable from the network.

## Definition of done

Every issue carrying the **`vigilant-vole`** label, in `nightswatchhq/nuthatch` and in
`nightswatchhq/burrmill`, is closed, and no open PR is for one of them. Stopping one production nest
for ten minutes produces an alert that reaches a person.

## Order

1. **The monitor that is already red: #1720.** Lodestar's 15-minute check has failed on `nest_busy`
   for two routes since 2026-10-01 21:55 UTC. Read `nuthatch_sql_rejections_total{reason="busy"}` on
   8107 and say whether it is the nest's permits or kittiwake's. First, because it is live, and
   because 4.1.1 changes the answer: a cold `/entity` now takes the same gate (#1657).
2. **Scrape and alert: #1714.** Prometheus and Grafana, self-hosted, on the ThinkPad, scraping the
   four Helsinki nests over the tailnet and the QoS nest beside it. The nine rules
   `docs/operators.md` already lists, routed to Discord, and one dashboard. On the ThinkPad and not
   on Helsinki for two reasons: Helsinki is 7.7 GB carrying four nests, and a monitor that lives on
   the box it watches goes down with it. The Helsinki nests bind loopback, so their `/metrics` is
   exposed on the tailnet address only. The ThinkPad then needs a dead-man signal of its own, or
   nobody is watching the watcher. Config committed, not hand-placed.
3. **The parity script still runs: #1721.** Its default port is 8105, which no longer answers. Fix
   it and run the script once by hand against 8107 on 4.1.1, recording the result. The timer itself
   is not in this sprint.
4. **The gates that went quiet: #1715.** The nightly mutation run cancels on timeout and its reporter
   only fires on `failure()`. Fix the condition, make the runs finish, and teach
   `scheduled_workflow_failure_is_reported.rs` that a cancellation is a failure to report.
5. **A process-kill crash harness: #1717.** #1631 and #1632 landed with function-level tests. Spawn
   the binary, `SIGKILL` at four named points, restart, compare against a from-scratch index. The
   seal fixes from the last sprint are its first subjects, and it must be red on 4.1.0.
6. **Reachable from the network: #1642, #1660, burrmill #13.** `--cors` wraps the mutating admin
   routes, direct serving has no header-read timeout, and `EXPLAIN ANALYZE INSERT` writes through
   `Engine::sql`. Each lands with the request that demonstrates it.
7. **Wrong answers that look right: #1659, #1643, #1636.** `/table` reports a hot-only partial as a
   merged success. `DELETE ?reclaim=true` on a suspended mount drops the record, answers 500 and
   reclaims nothing. The checkpoint table is never pruned and is read whole on every reorg walk.
8. **Confirm or close the thirty `Unconfirmed`: #1665 to #1691, burrmill #22 to #24.** One pass, two
   readers, the same rule as the audit. Each issue ends closed, or retitled without the prefix and
   given a real priority. Anything confirmed high is fixed in this sprint; the rest is the next
   sprint's input. Run it in parallel with items 2 to 5, since it writes no code.
9. **Release.** A 4.1.x or 4.2.0, by what item 6 changes for an operator, through `release.yml`,
   deployed, and watched by the alerts from item 2 rather than by hand.

## Rules

- **Operations work is committed.** A scrape config, an alert rule or a timer unit that exists only
  on the box is lost with the box. It lives in the nest repo or under `deploy/`, and the issue says
  where.
- **An alert is done when it has fired.** Not when the rule parses. Item 2 closes on a recorded test
  firing and a recorded clear.
- **Every fix lands with the test that was missing**, and a mutation check on the enforcing line.
  Carried over unchanged.
- **Reproduce before fixing.** For item 6 that is a `curl` in the issue. For item 8 it is the whole
  job.
- **The 2 GB per-cursor budget still binds.** A scraper and an exporter on the box are outside the
  cursor and must not be run inside the nuthatch process.

## Not in this sprint

- **The parity timer, #1713.** Chief, 2026-10-02: not activated yet. The script is kept runnable by
  item 3 and run by hand. #1718 (current-head parity) waits with it.
- **The eleven confirmed p2s**: #1624, #1637, #1645, #1646, #1647, #1648, #1651, #1653, #1661,
  #1662, #1663, and burrmill #21. Real, filed, small. Take one in the same PR when a fix above
  touches its line; otherwise they are the sprint after, with whatever item 8 confirms.
- **Burrmill dialect parity, #14 to #20.** A refusal or a difference where DuckDB answered. It is
  its own sprint, and it wants the 39-digit corpus case first (burrmill #23 is the unconfirmed half
  of that).
- **The rest of the test-strategy list**: #1716 (graph-validate on its corpus), #1719 (scheduled live-chain smoke), #1722, #1723, #1724. Worth doing, none of them tells
  us production is wrong today. #1725 has an outside PR open (#1727) and needs no sprint.
- **Build time and footprint**: burrmill #6 and #7.
- The parked programmes: RFC-0059/0060 (#1441, #1442), cross-nest SQL (#1324), the GTM plan, and
  `docs/frozen-for-2027.md` with its reopening rule.

## Settled by Chief, 2026-10-02

- **The scraper is self-hosted Prometheus and Grafana, on Helsinki or the ThinkPad.** The ThinkPad
  is chosen in item 2, for the reasons given there. Not Grafana Cloud.
- **The parity timer is not activated yet.** #1713 is out of the sprint.
- **One sprint.** Items 6 and 7 stay in.
