# Sprint: languid-lapwing

Filed after kindly-kestrel closed its issues and v2.6.3 shipped.
**Four issues.**

## Definition of done

Every issue carrying the **`languid-lapwing`** label is closed, and no open PR is for one of them.
Work discovered during the sprint is filed as an issue for the board rather than picked up, and
pulling anything into scope needs board approval.

## The theme

Every sprint so far has been about something being *wrong*: a mechanism that lied, a gate that could
not fail, a front door nobody had walked through. This one is about something being **right and
punished anyway**.

All four issues are a nest that is configured correctly and pays for it - in wall-clock, in noise, or
in a feature that silently does not work. None is a bug in the sense of a wrong answer. Each is the
product charging its own users for doing exactly what the documentation told them to.

That makes this the first properly *performance* sprint under the freeze, and `docs/roadmap-2027.md`
names performance as one of the five workstreams. Benchmarks are CI artefacts here and regressions
fail the build - a rule that exists and wants exercising rather than restating.

**The discipline: measure before and after, and report a median rather than a run.** Four identical
90-second runs of the same demo once measured 2, 15, 28 and 198 events. A number quoted from one
sample is not a measurement, and every item here is a temptation to take one.

## The four

### 1. #657 - `[[calls]]` costs 5.5x, and the guard is right

A nest declaring one `[[calls]]` entry cannot use `--seal-direct`, because a seal-direct run would
sail past every sampled block and seal the range with the table silently absent. The refusal is
correct and stays.

The cost is measured, not estimated: the same 454-million-block backfill went from **~12 minutes to
~66** for one pinned read. Five and a half times slower for one number, and the feature that incurs
it is the one 2.6.0 was announced for.

The work is teaching the seal-direct path to resolve calls, which RFC-0038 §6e already names as the
follow-up. The headline, and the only one with real design in it.

### 2. #656 - six retry storms on one backfill

`block_timestamps = true` over 454M blocks produced **six** retry storms against an archive RPC, each
announced by `every item in a 1-block eth_getBlockByNumber batch returned an error`. Every level of
the halving is exhausted before it gives up, and it is the default setting.

### 3. #663 - a declared event that never fired takes a whole view down

Declare an event the contract really does emit, on a chain where it has not yet emitted, and no table
is created - so any view referencing it fails to load **in its entirety**. Measured: a view supplying
fourteen fields lost all fourteen because one referenced table did not exist.

The configuration is correct. The chain simply has not done the thing yet, and the nest is punished
for the chain's history. Worse, it is order-dependent: the day that event first fires, the view starts
working, and nothing in the logs explains either state.

### 4. #670 - `doctor` probes one contract where a backfill filters on all

#669 fixed the large half of #644 - `doctor --dir` derives an address instead of a range-only probe.
It derives **one**, from the first declared contract, while a real backfill filters on all of them. So
the advice a multi-contract nest gets is measured against a narrower question than the one it will
ask, and the recommendation comes out optimistic.

## Explicitly not in this sprint

- **#649, #638** - Lodestar. Board work, and #649's remaining four gaps now have their rules written
  on the issue.
- **#639** (CI disk) and **#621** (fuzz budget) - real, and their own theme. Reach for them in that
  order if this finishes early.
- **The parked capability issues.** Frozen for 2026, not cancelled.

## Why four

#657 has genuine design in it and could take the whole sprint on its own. The other three are bounded.
If #657 runs long, the disposition is the same as `curatorCount` in industrious-ibis: write down what
was established, name the next place to look, and stop. A half-answer with measurements is worth more
than a whole one without.

## Notes per issue

**#657** is the headline and the only one with real design in it. The refusal is *correct* and
stays: a seal-direct run would sail past every sampled block and seal the range with the table
silently absent. The work is teaching the seal-direct path to resolve calls, which RFC-0038 §6e
already names as the follow-up.

**#663** is order-dependent. The day that event first fires, the view starts working, and nothing in
the logs explains either state. Whatever the fix is, the logs must explain it.

**#670** - #669 already fixed the large half of #644. What remains is that `doctor` derives *one*
address from the first declared contract while a real backfill filters on all of them, so the advice
comes out optimistic.

## Standing rules

These are not new, and they are here because a sprint brief is where they get read.

- Work discovered during the sprint is **filed as an issue for the board, not picked up**. Pulling
  anything into scope needs board approval.
- **One worktree per run**, not per agent. Two agents in one tree has destroyed work twice.
- **Never `git add -A`.** Stage explicit paths, and diff `main...HEAD` before opening a PR.
- Every PR needs a `Reviewed-by:` line **in the PR body**, from a name on `.github/reviewers.txt`. A
  prose verdict in a comment does not count - the audit greps the body.
- **Do not `@`-mention Rowan in GitHub markdown.** It auto-links to an unrelated real user who has
  asked us to stop. Name the agent without the `@`.
- `CFLAGS=-std=gnu17` for every cargo build on the Linux box: GCC 15 against the vendored mimalloc
  that dbsp pulls in. CI cannot catch this.
- `main` is protected - strict up-to-date plus ten required contexts - so it is **one merge per CI
  cycle**, and each merge invalidates the rest. Plan the landing order.
- **A green mutation is a finding.** If a test survives having the thing it tests removed, that is
  the bug, and it is worth an issue.

## Context at filing

v2.6.3 shipped on 2026-08-21: the first run on mainnet stopped stalling. kindly-kestrel closed four
of four. The website, `llms.txt` and `llms-full.txt` were brought current the same morning -
`llms.txt` had been telling agents to run `nuthatch roost`, a command removed in 2.0, for five
releases.
