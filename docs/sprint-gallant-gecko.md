# Sprint: gallant-gecko

Filed after auditing fastidious-ferret and reconciling the architecture session against the code.
**Eight issues**, two of them board-only.

## Definition of done

Every issue carrying the **`gallant-gecko`** label is closed, and no open PR is for one of them.
Nothing else is in scope: the firm does not allocate from the backlog, and work discovered during the
sprint is filed as an issue for the board rather than picked up.

That condition is the board's, and it is what puts the firm back to sleep. **It is now actually
enforced** - see the note at the bottom.

## The theme, and why this scope

**Everything we claim, demonstrated on a machine that is not ours.**

The last two sprints made nuthatch tell the truth about itself: earnest-echidna corrected the
documents, fastidious-ferret corrected the binary. This one is the third move, and the hardest to
argue with: **prove it to somebody else.**

The trigger is a reconciliation, not a hunch. Every decision from the 2026-08-04 architecture session
with GraphOps - RFCs 0032, 0033, 0034 and 0035 - was checked against the code on 2026-08-14. The
result is better than expected and it is worth writing down plainly:

- **All four RFCs are implemented.** The tenant runtime, the NID and data identity, the query
  allowlist and the 2.0 breaking surface are all shipped.
- **Cross-nest data reuse is shipped**, and it is cross-nest *by construction*: `runtime::adoptable`
  (`src/runtime.rs:614`) scans every dataset under `data/`, excludes only the nid asking, and adopts
  any whose `registry_hash` and `data_identity()` match. It does not care whether the candidate is a
  later version of the same nest or an unrelated one.
- **Exactly one slice is outstanding** - RFC-0034 slice 2, and only half of it (#568).
- Two items from the raw call notes are **decided against with reasons recorded**: SQLite/Turso
  (RFC-0035, removed by measurement) and per-caller rate limiting (RFC-0034 §6, *"not a rate limiter
  or a quota"*).

So there is almost nothing left to build. What there is instead is a **gap between what is true and
what is demonstrable**, and that gap is now the risk. Two examples make the case:

- Cross-nest adoption is the most persuasive capability nuthatch has, and **every test frames it as
  same-lineage**. We have never run two independently authored nests and watched one adopt the
  other's data (#569).
- `docs/verification.md` grades our own evidence, and its own table says level 5 was **verified on
  v0.9.3 and not since** (#570) - two majors, across a release that changed the unit of storage.

A capability nobody has seen is worth what an argument is worth. This sprint converts four of them
into evidence.

## How this is ordered

**#290 first, and it is the spine.**

Fuzz the decode path. It is a `p1, security` issue with **zero coverage today** - no `fuzz/`
directory, no `cargo-fuzz`, and `proptest` appears only in `factory.rs`, `store.rs` and
`e2e_reorg.rs`. Decode is the one place untrusted input meets stored state: the ABI arrives from
Sourcify or Etherscan, the logs arrive from a chain, and the entire pitch is *point this at any
contract*. A hostile contract emitting deliberately malformed log data is not hypothetical for a
general-purpose indexer, and non-negotiable 4 says anything feeding stored state must be
deterministic and re-executable.

It is also the only p1 on the board that is neither board-only nor already scheduled.

Then the evidence, in any order:

| issue | what it converts from claim to proof |
|---|---|
| **#569** | Cross-nest adoption, demonstrated between two *independently authored* nests. Zero RPC on the second, identical row counts, distinct NIDs. The falsifiable part is the RPC count - point the second nest at a provider that could not serve that history and it still answers |
| **#570** | Level 5 re-run across two real machines on the current release. CI cannot substitute: one runner cannot exercise a lease handover between machines, and the cross-machine half is the unproven one |
| **#568** | The website's production page, which is the missing half of RFC-0034 slice 2. `docs/operators.md:500` already says *"a public nest without an allowlist is an open query engine"*; the site has no production page at all. **`nuthatch-frontend` has no Git auto-deploy** - it needs `vercel --prod --yes` by hand, or the page is written and still unpublished |

*(#571, dating the verification table, was scoped here and then shipped in **v2.4.0** instead: the
table goes stale at the tag, so fixing it afterwards would have published a verification page that
was wrong about the release carrying it.)*

Then the two correctness items, both silent-failure faults of the kind the last two sprints kept
finding:

| issue | |
|---|---|
| **#567** | `Store::recent_by_table` drops unparseable rows via `.ok()…unwrap_or(false)`, so every caller gets a quietly short answer. The function returns `Result` and has somewhere to say so. Note the mutation hazard recorded in the issue: a test asserting the row is skipped passes today **and** would pass with the guard deleted |
| **#302** | Operability: structured logs, an at-tip signal, documented alerting. The literal *can an operator run this well* issue, and the one a partner feels first |

And the two board-only verifications, which are the board's to run and are listed here so the sprint
is not read as complete without them:

| issue | |
|---|---|
| **#441** | The Lodestar production box, unmet for **four consecutive releases** now including 2.4.0. Board ruling 2026-08-14: run it after 2.4.0 ships, not before |
| **#428** | The live `/nest` probe against a real provider key, never run |

## The standing rules, unchanged

- **Paste the mutation artifact**: the diff of what was broken, and the panic line of the test that
  died. A sentence saying a mutation was done is not accepted.
- **Run it, do not reason about it.** This sprint is *entirely* that rule - every issue in it exists
  because something is true but unproven.
- **A skip is not a pass**, and a mutation that does not mutate is not a test.
- **Ask who calls this.** #567 exists because #373's fix turned out to be unreachable through the
  redb store; the swallow was one layer further down than the issue said.
- **A claim is a deliverable.** #571 was that rule applied to the one document whose whole purpose is
  being checkable, and it shipped in 2.4.0 rather than waiting for this sprint.
- **File what you find; do not work it.**
- **`CLAUDE.md`'s factual claims are yours to correct with evidence; its decisions are not.**

## Not in scope, deliberately

- **The performance set** (#295, #296, #298, #282, #285) and the OBIB cases (#306, #308). Deferred
  for a **third** sprint running. The board is aware this is now a pattern and is choosing it again:
  a benchmark improvement on a surface whose distributed claims are two majors unverified is not an
  improvement. #285 is the sharpest edge - 2.4.0 ships with no current published backfill number on a
  project whose RFC-0029 is called *the fastest indexer* - and it stays deferred anyway.
- **RFC-0033 slice 4** (#357), whole-derivation reuse. Parked, and the reason holds: authored views
  are not materialised, so its acceptance criterion cannot fail. It becomes real if RFC-0018 §3
  (#270) ever materialises them.
- **Reversing either recorded decision.** SQLite/Turso and per-caller rate limiting are GraphOps'
  calls to re-open, not ours to pre-empt by building.
- **Anything discovered mid-sprint.** File it.

## One thing that changed since the last sprint

The stopping condition is now **enforced rather than hoped for**, and the firm should know why.

On 2026-08-14 at 02:19 UTC fastidious-ferret's condition went true, autosleep began its hold, the
firm opened a PR for work nobody had asked for, the condition went false, and the hold reset. It
never slept. It worked four more hours on unbriefed issues until the **spend cap** stopped it at $281
over baseline - a money backstop doing a scope control's job.

Two things were fixed:

- The sprint is now a **GitHub label the board applies**, and the condition is *no open issue
  carrying it, and no open PR for one*. Out-of-scope PRs no longer hold a finished sprint open, which
  is the exact hole the firm fell through.
- `sleep_firm()` used to `PATCH /api/companies/{id}`, which returns 200 and **changes no agent**. The
  heartbeat is per-agent. So every sleep the notifier reported stopped nothing, and the page saying
  "the firm is asleep" was false while three agents ran on their timers. It now shells to
  `firm sleep` and returns the read-back rather than the exit status.

**Dispatch checklist**, in this order, because it matters: label the issues above `gallant-gecko`,
set `SPRINT_LABEL` in `nightswatch-notify.py`, clear `capped` and delete `cap_baseline` to re-arm at
a fresh ceiling, then `firm wake`. Clearing the cap before the label is set leaves nothing at all
between the firm and whatever it fancies.
