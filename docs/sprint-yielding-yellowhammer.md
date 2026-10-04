# Sprint: yielding-yellowhammer

**Scoped 2026-10-03, not started. Starts when every `xanthic-xenops` issue is closed and its release
is cut. Two questions for Chief are at the foot.**

**A release that answers production's queries must also answer them correctly, and inside its
memory budget under production's real load.**

`xanthic-xenops` built the gate that 4.1.1 slipped through, and on its first day it caught two
burrmill regressions before either shipped. But it checks only that a statement answers. A
statement that answers wrongly passes: burrmill #44 returns NULL where DuckDB returns the value, and
the gate would wave it through. It also runs statements one at a time, while 8107 allows two at
once. The same binary has peaked anywhere from 1,697 MiB to 3,376 MiB on the same copy, and nobody
can yet say why. And the Lodestar parity script, the only check against the subgraph, last ran on
2026-09-02.

## Definition of done

Every issue carrying the **`yielding-yellowhammer`** label, in `nightswatchhq/nuthatch` and in
`nightswatchhq/burrmill`, is closed, and no open PR is for one of them. A candidate that changes one
answer Lodestar or kittiwake reads is reported red. Two releases running production's set at
production's concurrency peak inside 2 GiB on repeated runs, or the budget question has gone to
Chief with the numbers.

## Order

1. **The gate compares answers, not just statuses: #1772.** Each statement's result is hashed, order-
   insensitively where the statement has no ORDER BY, and the candidate must match production. A
   difference is red, with the first differing row shown. Proved on a deliberate wrong answer:
   burrmill #44's TRY_CAST, run before its fix.
2. **The gate runs at production's concurrency, and the spike is explained: #1773.** A pass at
   `NUTHATCH_SQL_MAX_CONCURRENCY=2` with statements paired as kittiwake's warmer sends them. Each
   statement gets an attribution of where its peak sits, so the 3,376 MiB run is traced to a cause.
   That cause is fixed in burrmill or nuthatch. `#1758` closes here if it is still open, and #1778 exports the gauges that let a page say where the memory sits: the QoS nest climbed past 2 GiB on 2026-10-03 with 64 MiB of it accounted for.
3. **Parity against the subgraph on a timer: #1713, #1718.** Daily, fail-closed, sealed pin and
   head pin. Disagreements post to Discord. Chief said go on 2026-10-03.
4. **The gate's copy stays current: #1774.** The `GATE_REFRESH` hook rsyncs the segments and a consistent
   redb copy from Helsinki. Today's copy is frozen at 2026-10-03 and goes stale a little every day.
5. **The two unconfirmed ingestion smells: #1670, #1671.** Each is confirmed with a reproduction,
   or closed with the evidence that it cannot happen. A log silently missing from the middle of a
   range is the one defect class nothing else here would catch.
6. **Burrmill's open answers: #44, #52.** The TRY_CAST boundary is fixed. Top-k filters reach the
   scan, or the reason they cannot is recorded.
7. **What went red and stayed red.** The scheduled mutants run (#1757), plus three loose ends from
   this sprint: #1775, #1776 and #1777.
8. **Release**, through the gate as it now stands, rolled with `--smoke`.

## Rules

As `xanthic-xenops`:

- A gate is done when it has failed.
- Operations work is committed.
- Every fix lands with its missing test, red first and mutation-checked.

Added:

- **A memory figure is a distribution, not a run.** Any claim about peak RSS cites at least four
  runs on Linux. The 1,157 MiB result that did not reproduce is the reason.

## Not in this sprint

- **The rest of the test-strategy list:** #1716, #1722, #1723, #1724, #1741.
- **Burrmill build time and footprint:** burrmill #6 and #7.
- The parked programmes and `docs/frozen-for-2027.md`.

## For Chief

- **The parity timer.** You held #1713 back on 2026-10-02. Item 3 assumes it can go now. If not, it
  drops out and item 1 carries answer correctness alone, against production rather than the
  subgraph.
- **Helsinki access.** Items 3 and 4, and the rest of #1714, all need commands run on Helsinki. The
  permission check refuses those from here, reads included. Either you run them, or you allow
  `ssh root@89.167.109.4` for this work.
