# Sprint: observant-osprey

**Five issues.** A sprint is a labelled set, not a calendar. This is the small sprint after a release:
nothing here is a feature, and nothing here is a bug in the ordinary sense. Every item is a thing the
project currently *believes* without having looked, or an instrument that cannot tell us what we need.

## Definition of done

Every issue carrying the **`observant-osprey`** label is closed, and no open PR is for one of them.
That is #1056, #1057, #1058, #1059 and #1060. Work discovered in flight is filed **unlabelled**;
pulling it into scope needs a board reply.

## The theme

**The things nothing tells us.**

`mighty-moorhen` ended with a release and a deploy, and both went well. What they exposed is that
several of our instruments do not answer the question they appear to answer:

- A service ran **2.7.1** for weeks beside others on 3.0.1, and nothing could have said so, because
  its unit names a path rather than a version (#1060).
- redb's cache has been a **1 GiB per-process heap ceiling nobody chose**. It is settable now and
  still set to the default, so the saving is real, measured on a laptop, and unclaimed (#1057).
- Seal boundaries changed in 3.1.0. Production crossed that line on 2026-09-01 and **nobody has
  looked** at what it is writing on the far side (#1059).
- RFC-0017's authoring eval has a proven board, a runner and two isolation modes, and **cannot
  produce a single number** because the container image it requires does not exist (#1058).
- The reviewer that found 26 of 28 real defects last sprint is handed a diff and never a commit
  range, so it reported a security fix missing from a release that contains it (#1056).

None of these fails loudly. That is the point, and it is the same fault the last sprint kept finding
in tests: a mechanism that reports success because it never looked.

## The five

1. **#1056 - the review harness.** Give Jules the commit range, add a per-finding confidence distinct
   from merge-safety, and stop a cancelled run leaving a red check that reads as a verdict. Not a
   model swap: `terra` may be better, but a reviewer with a measured record is not replaced by one
   without, and the evidence for that is a parallel run rather than a switch.
2. **#1057 - pick the cache size.** The RSS/point-read/tip-lag curve at 1 GiB / 512 / 256 MiB **on the
   box that enforces the budget**, then a default and a documented per-cursor figure. The harness
   numbers must not be used: both boxes had the store in page cache, so a miss cost a memcpy rather
   than a read. Board-run; needs the VPS.
3. **#1058 - the eval image.** `claude` on `PATH`, one HTTP client, no repository, no egress. Until it
   exists RFC-0017's acceptance criterion is unmet and "an agent can build a nest end-to-end,
   measured" has no number behind it. Building it needs no credentials; the keyed run that follows
   does.
4. **#1059 - look at the seals.** Segment count and median size either side of the 3.1.0 restart,
   against `docs/bench/segment-layout.md`'s 10,923 files at a 6 KB median. A watch, not a defect -
   and it degrades gradually rather than failing, so nothing will alert on it. Close it once someone
   has actually looked.
5. **#1060 - version the units, verify the deploy.** Every `ExecStart` names a version; the deploy
   asserts the binary it installed before restarting. A `cp` over a running binary fails with `Text
   file busy`, and the script that ignored it nearly reported a fix that had not been deployed.

## Explicitly not in this sprint

- Every `frozen` issue. The 2026 freeze remains intact, and **both carve-outs are spent**.
- New engine, chain, extraction, or AI capability.
- Switching Jules to `terra`. Measure both first; see #1056.
- New findings discovered while doing these five, unless the board adds them explicitly.

## How this sprint runs

Standing rule, and it earned its place last sprint: **a test that passes proves nothing until it has
been made to fail.** Mutate the gate, watch it go red for the right reason, and quote the failure in
the PR. Two of last sprint's checks were green while guarding nothing and only mutation found them.

Second rule, new (#1057-#1060 exist because of it): **anything worth remembering has an open issue.**
A closing sentence is not a queue. If a report says "worth watching" or "someone should", file it in
the same breath.
