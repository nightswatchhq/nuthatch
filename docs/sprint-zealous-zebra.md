# Sprint: zealous-zebra

**Scoped 2026-10-03, not started. Starts when every `yielding-yellowhammer` issue is closed and its
release is cut. One question for Chief is at the foot.**

**Status 2026-10-04 evening.** 4.4.1 is out, gated on all six nests. Closed: #1794, #1797, #1834, and the sealed audit's code (#1846). Open: #1833 (the VmHWM PR), #1786 (production run with the next release, auditing against the units' arb1.arbitrum.io fallback; GraphOps stays primary). Chief removed the DuckDB reference from the gate on 2026-10-04, so the sentence in the definition of done about a wrong answer two releases share no longer applies: the gate compares with production only, and parity covers the allocations nest.

**Every production nest is gated, and a wrong answer is caught even when the last release made the
same mistake.**

Two things on 2026-10-03 showed where the gate still falls short.

- **Only one nest is gated.** The QoS nest's daily views had been refusing all day, on 4.2.1 and on
  4.3.0 alike. The gate never saw it, because it only runs the allocations nest. Four of the six
  production units rolled to 4.3.0 with no smoke file at all.
- **The answer comparison is relative.** The comparison added in `yielding-yellowhammer` caught
  4.3.0's NULL urls only because 4.2.1 had answered them correctly. A wrong answer two releases share
  passes it. Burrmill's own changes that week also turned up two silent wrong answers in DataFusion's
  dynamic filters. Nothing outside burrmill's own tests would have found those.

So this sprint widens the gate to every nest. It also gives the gate one reference that doesn't come
from nuthatch.

## Definition of done

Every issue carrying the **`zealous-zebra`** label, in `nightswatchhq/nuthatch` and in
`nightswatchhq/burrmill`, is closed, and no open PR is for one of them. Every production unit has a
gate query set and a smoke file. ~~A deliberately wrong answer in a view that two releases share is reported red.~~ Dropped with the
reference, 2026-10-04.

## Order

1. **Every nest is gated: #1794.** The gate runs a copy and a query set for each production nest:
   - QoS;
   - GNS;
   - dips;
   - data services;
   - the frozen staking archive.

   Each set is collected from its real consumers, the way the allocations set came from kittiwake. A
   nest with no consumer outside the dashboard gets the dashboard's statements. Each copy is refreshed
   like the allocations nest's. Production's RSS budget applies to each.
2. **A smoke file for every unit, and a roll that knows when the old binary fails too: #1795.** When a smoke
   test fails, the roll runs the same statement on the previous binary. If that fails too, the roll
   reports a pre-existing failure and keeps the new version. It reverts only when the new binary is
   worse. On 2026-10-03 the QoS revert put the unit back on a binary that was broken in exactly the
   same way.
3. **Burrmill answers checked against DuckDB on production's statements: #1796.** Each gate statement runs
   at a sealed pin on the copy's Parquet segments, through burrmill and through DuckDB in burrmill's
   parity harness. A difference is red whatever the previous release answered. Statements whose
   dialect DuckDB cannot run are listed with the reason.
4. **The detectors for silent wrong answers.**
   - #1786: a sampled audit of sealed ranges against a second endpoint, which is the one way to catch
     a provider omitting logs.
   - Burrmill #64: the two DataFusion dynamic-filter wrong answers, each reproduced on plain
     DataFusion and reported upstream.
   - Burrmill #63: the same TRY_CAST fault from DOUBLE.
5. **The two statements the gate can only count: #1797.** kittiwake's `payments.accounts` and
   `delegation_events` page through ties with `ORDER BY ... LIMIT 100`, so their answers vary between
   runs. A tiebreaker in kittiwake's SQL makes them comparable exactly.
6. **What the last sprint left measured but unfixed.**
   - #1824: the allocations nest's epoch boundary table stops at epoch 1370, so fee collections
     near a boundary land one epoch early. Parity now traces these shifts exactly; this removes them.
   - #1833: the gate samples RSS every half second and missed a spike by about 180 MiB. It should read
     the kernel's high-water mark when each server stops.
   - #1834: entity state size, which DBSP only exposes through a full profile walk.
7. **Release**, gated on every nest, rolled with a smoke test per unit.

## Rules

As before:

- A gate is done when it has failed.
- Operations work is committed.
- Every fix lands with the test that was missing, red first and mutation-checked.
- A memory figure is four Linux runs.

Added:

- **A wrong answer outranks a refusal.** When a release trades one for the other, roll back. Decide
  on the answers, not on memory or speed.

## Not in this sprint

- **The rest of the test-strategy list:** #1716, #1722, #1723, #1724, #1741.
- **Window efficiency after a split:** #1787.
- **Burrmill build time and footprint:** burrmill #6 and #7.
- The parked programmes and `docs/frozen-for-2027.md`.

## Settled

- **Box steps** come as one-command scripts run from Chief's Mac, as the parity, export and roll
  installers did in yielding-yellowhammer.
