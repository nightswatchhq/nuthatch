# Sprint: meticulous-magpie

Filed by the board on 2026-08-22, after languid-lapwing closed four of four and was audited.
**Four issues.** Runs **Saturday 2026-08-22 to Monday 2026-08-24**.

## Definition of done

Every issue carrying the **`meticulous-magpie`** label is closed, and no open PR is for one of them.
Work discovered during the sprint is filed as an issue for the board rather than picked up, and
pulling anything into scope needs board approval.

## The theme

**The numbers we quote about ourselves.**

languid-lapwing established a discipline: measure before and after, and report a median rather than
a run. It was enforced properly - #724 refused to inherit the board's own 5.5x because it was one
sample, and measured ~7% itself instead.

This sprint turns that discipline on the numbers **we have already published**. Every issue here is
a figure nuthatch states about its own performance that is wrong, unmeasurable, or describes a
harness that no longer exists. Two of them are in outward-facing launch and grant documents.

That makes this the second performance sprint under the freeze, and it is squarely in scope:
`docs/roadmap-2027.md` names performance, maintenance and marketing among the five workstreams.
A wrong benchmark is all three at once.

The uncomfortable part, stated plainly because it should be: **the strawman baseline was found in
July.** #224 established on 2026-07-30 that the hot-store bench had been writing one redb
transaction per row. The multipliers computed against that baseline were never recomputed, and they
have been quoted in a grant document ever since. `docs/launch/strategy-review-2026-08-19.md:189`
reached the same conclusion independently three days ago. Nobody has been careless; the number
simply had no owner.

## The four

### 1. #722 - the 8.7x and 20x were measured against a harness that no longer exists

**The headline, and #726 cannot start until this produces numbers.**

`docs/benchmarks.md` carries `hot store 289 events/sec` as the denominator of both multipliers -
seal-direct ~8.7x, +8-way pipeline ~20x. That 289 was measured on 2026-07-16, two weeks before #224
found the hot-store path was calling `put_entity` per row: one write transaction, one fsync, per
row. The denominator was a strawman. Both multipliers inherit it.

Re-measure on the current harness and report what the ratio actually is. It may be smaller. That is
a fine outcome and it is the point.

### 2. #726 - the strawman multipliers are still quoted in the launch and grant docs

Same numbers, outward-facing. **Blocked on #722 by its own instruction:** do not restate a
multiplier anywhere until #722 has one. Until then the honest move is to remove or caveat, not to
guess a replacement.

Sequenced second deliberately. If #722 runs long, this becomes "caveat what is published and say
the measurement is in flight", which is still a complete piece of work.

### 3. #725 - the bench cannot measure the fix it was meant to measure

All three seal-direct bench call sites pass an empty `[[calls]]` slice, so `nuthatch bench` cannot
exercise the #657 work at all. Filed by Iris from her own review of #724 - the reviewer finding the
hole in the thing she was signing.

The board audit reached the same place from the other side: disabling call resolution in
`backfill_direct` leaves the **entire suite green**, `e2e_seal_determinism` included. Mitigating,
and worth knowing before anyone panics: `bench.rs` records that path has no non-test caller, and the
pipelined path is what production takes. It is an unmeasured and untested path, not a live data
hole.

### 4. #720 - tier-3 pays two round trips per sampled block, one of them redundant

Found by Mabel while establishing where #657's 54 minutes actually went. The tier-3 loop makes two
sequential, unbatched RPC calls per sampled block, and the second duplicates work the batched
timestamp fetch already does when `block_timestamps = true` - which is the default.

A real cost on the existing hot path, with the diagnosis already written on the issue. It belongs
here because it is the kind of thing a benchmark should have shown and did not.

## Ordering

**#722 first, #726 after it.** The other two are independent and can run in parallel with either.
If #722 has not produced a number by Sunday, say so and take #726's caveat-only path rather than
holding both.

## Explicitly not in this sprint

- **#719, #621** - the fuzz job's budget and the comment that misstates its own bounding. Same
  family, genuinely, but the fuzz job is its own theme and mixing it in would make this sprint about
  two things.
- **#649, #638** - Lodestar. Board work. #649's four gaps now all have rules; gap 3 was closed out on
  2026-08-22 (indexers slashed to zero that we still count, plus a nine-count drift in the subgraph's
  own counter).
- **#727, #729** - the two #663 follow-ups. Real, bounded, and next in line if this finishes early.
  Reach for them in that order.
- **Anything labelled `parked`.** The feature freeze runs to the end of 2026: no new capability.

## The discipline, carried forward

Unchanged from languid-lapwing, and more load-bearing here than it was there, because this sprint's
entire output *is* numbers:

**Report a median and the run count, never a single sample.** Four identical 90-second runs of the
same demo once measured 2, 15, 28 and 198 events.

And one addition, from the audit that closed the last sprint:

**Say which machine, and which harness commit.** A figure without its harness is how 289 events/sec
outlived the code that produced it. Every number this sprint publishes carries where it was measured
and against what.

## Standing rules

- Work discovered during the sprint is **filed as an issue for the board, not picked up**.
- **One worktree per run**, not per agent.
- **Never `git add -A`.** Stage explicit paths, and diff `main...HEAD` before opening a PR.
- Every PR needs a `Reviewed-by:` line **in the PR body** from a name on `.github/reviewers.txt`.
  The gate's self-review refusal compares against the GitHub *login*, and every agent pushes as
  `cargopete` - so it **cannot** catch one agent signing another's work as if it were review. That
  boundary is honour-system. Keep it.
- **Do not `@`-mention Rowan in GitHub markdown.**
- `CFLAGS=-std=gnu17` for every cargo build on the Linux box.
- `main` is protected - strict, ten required contexts - so it is **one merge per CI cycle**.
- **A green mutation is a finding**, and so is a green *suite*: #725 exists because disabling a
  mechanism changed nothing anywhere.

## Context at filing

v2.6.3 shipped 2026-08-21. languid-lapwing closed four of four and was audited by the board on the
22nd; three passed on mutation, one produced #725. The audit also found `main` red at random -
an exact request-count assertion across a cancelling `try_join!`, nine failures in ten locally while
CI showed green - filed as #735 and fixed in #736. `main` is green as of `f0e2ca3`.
