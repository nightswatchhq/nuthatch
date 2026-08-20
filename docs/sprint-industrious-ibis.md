# Sprint: industrious-ibis

Filed by the board on 2026-08-20, the day after 2.6.0 and a day after hardy-heron closed its fourth
issue. **Six issues.** Runs **Thursday 2026-08-20 to Sunday 2026-08-23**.

## Definition of done

Every issue carrying the **`industrious-ibis`** label is closed, and no open PR is for one of them.
Nothing else is in scope: work discovered during the sprint is filed as an issue for the board rather
than picked up, and pulling anything into scope needs board approval.

## The theme

**What the runs found.**

Every sprint so far has been about a mechanism: one that lied about the binary (fastidious-ferret),
one that graded claims nobody had run (gallant-gecko), one that reported green over a fault it was
built to catch (hardy-heron). This one is about the opposite failure, and it is quieter.

The runs happened. Somebody built `graph-allocations-nest` against an archive RPC and backfilled 454
million blocks. Somebody resolved five IPFS documents against a live gateway and hand-computed three
of the CIDs first. Somebody diffed a `network-snapshot` nest field-by-field against
`graphNetwork(id:"1")`. All three found real defects, and **the defects are written in prose in
`docs/`, where nothing schedules them.** Two of them have been sitting in `port-queue-nest.md` §8
under the heading "Two small defects found by building it" since the 19th, which is an entirely
honest place to write a defect down and the wrong place to leave it.

Meanwhile 2.6.0 went out under the largest claim this project has made - *"the release that closes
subgraph parity"* - and `docs/rfcs/README.md` still records both of its headline features as
**"Draft; nothing built"**.

So the shape of this week is: the evidence exists and the queue has never heard of it. Turn the runs
into work, and settle the one number that stands against the parity claim.

## The six

### 1. #659 - what `curatorCount` actually counts

**The headline, and the only one with a thesis attached.** RFC-0038's acceptance criterion is one
sentence: *acceptance is a real port diffed against the gateway, because parity is the claim.* The
one port we have reconciles exactly on two fields of fourteen, within 0.03% on three, and is
materially wrong on nine. `curatorCount` reads **50** against the gateway's **1,819**.

This is not missing data. `curation__signalled` has full history and the signal *total* is 3.8% out
while the distinct-curator count is 36x out, because most L2 signal routes through GNS and the
`curator` field holds the GNS contract address rather than a person. The obvious next guess is
already dead: GNS `SignalMinted` yields ~6,587 distinct addresses against the gateway's 1,819, so
that is not the rule either.

**The rule lives in the network subgraph's mappings and nowhere else.** Read
`graphprotocol/graph-network-subgraph` and answer, with file-and-line citations: when a `Curator`
entity is created, what makes one active, and which contracts and events a nest would need to
reproduce both numbers.

**"It cannot be reproduced from logs" is a passing answer** and possibly the more valuable one. A
named, cited, evidenced limit is exactly what an acceptance criterion is for, and it becomes an
RFC-0038 amendment rather than an embarrassment. What does not pass is a plausible rule with no
citation - every wrong answer so far has been plausible.

The reconciliation itself stays with the board (#649): it needs the re-indexed nest, and a re-index
is not this sprint's spend.

### 2. #658 - the index says the features do not exist

Three rows of `docs/rfcs/README.md` are false today. RFC-0037: *"Draft; nothing built."* RFC-0038:
*"Draft; nothing built, release-sized."* RFC-0023, the longest-standing and most specific: *"nothing
calls `resolve_at`, so tier 3 does not execute."*

`resolve_at` has a caller, and `src/indexer.rs:7815` records the moment it acquired one. The guard
test that row describes did its job. The row did not.

And `docs/progress-log.md`, which describes itself as one entry per push, has a newest entry of
**2026-07-28** - **nineteen tags** and two majors ago. Follow the existing 2026-07-22-to-28 catch-up
precedent: one honest summary entry, not nineteen back-filled ones. That precedent exists for this
case.

#268 closes as part of this. It tracks work that shipped.

### 3. #636 - the prod-readiness walk, now due

The file's own rule is *"when you cut a release, walk it top to bottom and update the flags with
evidence."* The stamp says 2.0.0. The repo says 2.6.0. That is the walk deferred six times, and
2.6.0 is the release it was deferred to.

Two things to carry into it, both from #598: the rows that have been wrong were wrong in the
direction that **understates** us, and a closed issue is not evidence - #286 was cited as progress
while having closed on no measurement at all.

### 4. #644 - doctor recommends 320 where 81,920 was measured

`nuthatch doctor` without `--address` recommended a 320-block window on the run that built
`graph-allocations-nest`. Probed **with** `--address` against the same endpoint, the answer was
**81,920**. That backfill spans 42,449,403 → 496,121,293: roughly 1.4 million requests at the
recommended figure against about 5,500 at the measured one, and it completed in 12 minutes.

The defect is not only the number. `src/doctor.rs:280` tells the user the error runs the *other* way
- that a range-only probe overstates what a real backfill sustains - and `recommended_window()` caps
it on that reasoning. In the one case measured end to end, the reasoning is backwards.

This is the item a stranger hits first. Everything else on this list costs us; this one costs them.

### 5. #655 - a correct nest that starts with 38 warnings

Renaming two contract aliases produced **38** startup warnings of the form *"semantic.toml describes
table `c1__signalled`, which the registry has no decoder for"*, because `nuthatch schema` regenerates
`schema.json` and never re-keys `semantic.toml`.

Nothing is wrong with the data. The cost is that an operator learns this warning is noise, and the
next one will not be. The repo has the precedent in its own source: `src/indexer.rs:1736` comments on
an *earlier* bug where a correctly-described table reported "no decoder", and the fix then was to
stop it firing rather than to explain it.

Decide between re-keying on rename and collapsing the whole-alias case into one accurate warning. The
issue argues both; nobody has measured which is right.

### 6. #650 - four security tests red by default on a Mac

`tests/e2e_fe_admin_exposure.rs` binds `127.0.0.2`. Linux aliases all of `127.0.0.0/8`; macOS aliases
only `127.0.0.1`, so four tests panic before a line of nuthatch code runs and `cargo test` reports
**725 passed, 4 failed** on a clean tree.

They are the admin-exposure tests - the four you least want somebody to learn to scroll past. Fix the
helper, not the assertion, and check the fix on both platforms rather than reasoning about loopback
aliasing, which is how this got here.

## Explicitly not in this sprint

- **#656 and #657**, both filed today from the same runs. The timestamp retry storms recovered every
  time and cost nothing; the `[[calls]]`-versus-`--seal-direct` cost is a feature-sized follow-up
  with a correct guard already in place. Filed so the numbers exist. Not now.
- **#619**, the `Reviewed-by: pending` hole. It cannot be scoped until the board rules between a
  checked-in roster file and separate forge identities per agent, and that ruling has not been made.
  It is a decision, not a task, and it is the board's.
- **#639** (CI disk) and **#633** (endpoint probe retry). Both real, both p1, both about the
  scaffolding rather than the claim. #639 gets pulled in the moment it reds a PR in this sprint,
  which on a docs-heavy week is not unlikely - it failed on a documentation-only PR twice on the
  19th.
- **All benchmark and OBIB work**, as in hardy-heron and for the same reason.
- **#638**, the Lodestar migration. Board-shaped by its own filing: the gap there is attention, not
  capability, and the firm cannot reach that repo.

## Why six, and what happens if item 1 runs long

Four days, and only item 1 has an unknown inside it. Items 2, 3, 5 and 6 are bounded work with a
visible end - two of them are reading and writing rather than building - so they are not at risk from
anything except being started last.

Item 1 is genuinely open-ended, and the honest failure mode is that it consumes a whole engineer for
four days and produces a citation-free guess. It should not. If two days in there is no cited answer,
the disposition is to write down what *was* established, name the next place to look, and stop: a
half-answer with citations is worth more than a whole one without.

## Outstanding on the board's side

Recorded here rather than in an issue, because they are the board's and the firm should not wait on
them.

- **Hardy-heron's audit has not happened.** The cycle says audit, then file. This sprint was filed
  first, deliberately, because the two sets share no files - hardy-heron touched `fuzz/`, `ci.yml`,
  `release.yml` and the decode extraction; this one touches `docs/`, `doctor.rs`, `semantic.rs` and
  one e2e test. The board still owes the audit.
- **The #619 ruling**, per above.
- **#649's reconciliation** and the `indexerCount` re-check, once the legacy-ABI re-index has landed.
- **#651 and #653.** #651's fix is merged on `main` and its issue is still open; #653's PR is in
  flight. Neither is the firm's.
