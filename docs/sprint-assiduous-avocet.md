# Sprint: assiduous-avocet

Filed after the 2.7.0 → 2.7.2 audit. **Six issues.** A sprint is a labelled set. It has no
calendar. The alphabet restarts here: zealous-zosterops and the one-issue zippy-zebra closed the
previous cycle.

## Definition of done

Every issue carrying the **`assiduous-avocet`** label is closed, and no open PR is for one of them.
That is six issues: #841, #842, #846, #840, #845, #836. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

## The theme

**An instrument that cannot say "I don't know" is not an instrument.**

Six findings, one defect wearing six coats. Not one of them is a false negative. Every one reports
a *positive* result without having measured: a gate that passes when its own output is missing, a
readiness probe that suppresses every stall term it owns, a script nobody runs that would exit 0
anyway, a cache that answers "unchanged" because it cannot see the change, an eligibility check
that admits twelve of thirteen things it exists to refuse.

The repository already knows the rule and has written it down twice - `analytics.rs` on denylists
over a growing vocabulary, and the whole of rigorous-raven on checks that cannot fail. This sprint
applies it to the six places that did not inherit it.

Freeze-legal throughout: correctness and observability of capabilities already shipped. No new
extraction mode, no RFC-0041 circuit work, no new analytics surface.

## The six

### 1. #841 - the nightly mutation gate has never completed, and its checker is green by default

Three runs since 2026-08-23, three cancellations at exactly `timeout-minutes: 300`, zero verdicts.
The 39 scoped mutants all finish; the job dies before the step that reports them. Separately,
`mutants-check.py` prints "No new survivors" and exits 0 when `mutants.out/missed.txt` is absent, so
a failed run is indistinguishable from a clean one.

First, because it is the instrument that would have caught the rest of this list.

### 2. #842 - the survivor that gate found and could not report

Deleting the `!` in `seal_range_with_snapshot`'s shared-store guard kills no test: nothing seals
into a shared store and then asserts the segment file exists. That is the RFC-0033 §11a arm, the one
two mounts of the same NID depend on. Recovered from the discarded artifact of the 2026-08-24 run.

Second, because repairing the gate is worthless if its first real output stays unread.

### 3. #846 - `/ready` reports ready for a seal-direct frozen ten hours

`seal_direct_active` gates every stall term and nothing replaced them, so "this pass is legitimately
slow" and "this pass has died" are the same observation. Measured: HTTP 200, `ready:true`,
`stalled:false`, `wedged:false`, after ten hours with no poll and no progress.

Suppressing the tip-following checks during a bulk seal was right. Leaving nothing in their place
was not. Wants a `last_seal_progress` stamp and a term of its own.

### 4. #840 - the DuckDB cache cannot see a same-length view rewrite

`DuckInputStamp { len, modified_ns }` misses 497 of 500 same-length rewrites on the Linux dev box,
because the mtime clock resolves to ~3.3 ms (2,000 writes produced nine distinct timestamps). The
cached connection then serves the previous view definition with no error anywhere.

The only issue in this sprint that returns a wrong *answer* rather than a wrong *status*. Fix is a
content hash over files `attempt()` already stats on every query.

### 5. #845 - the required-context drift script is invoked by nothing

`check-required-contexts.sh` is the only thing that compares `.github/required-checks.txt` to live
branch protection, and no workflow, test or documented command calls it. Without a token it exits 0
having compared nothing, so wiring it up carelessly would produce a green step that checked nothing.

There is no drift today - file and live API match on all ten contexts. Closing a detection gap while
it is cheap.

### 6. #836 - the incremental-SQL refusal list refuses 1 of 13

`nuthatch check` tells an author their entity is eligible for incremental maintenance. It admits
`quantile_cont`, `arg_max`, `first`, `string_agg`, `list`, subqueries, `GROUPING SETS` and
`USING SAMPLE`, and a double-quoted identifier bypasses the token scan entirely - a behaviour a test
currently pins. p0, and live on `main`.

The fix is the one `analytics.rs` already made: keep the denylist, add a parser-derived allowlist
beside it, and stop discarding double-quoted text.

## Explicitly not in this sprint

- **#835, #837, #838, #839.** Slice-zero evidence questions, accepted 2026-08-25 as slice-2 work and
  carried by #821. Double-booking them into a sprint would make ownership ambiguous.
- **#844.** PR #848 is open and green but for the review signature. It closes itself.
- **#843 and #847.** Real and cosmetic. `#843`'s `launch_copy` half is four lines and is the
  designated stretch item if the six land early.
- **#829 and #830.** Release integrity - mutable action references and binary provenance. A
  coherent pair needing a signing decision, not a runtime patch. Their own sprint.
- **RFC-0042 and all frozen work.** Unfrozen 2026-08-25 but sequenced behind RFC-0041 (#849).

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; one merge per CI cycle. `Closes` is one keyword per issue, not a
comma list - squash only honours the first.

One addition, from the audit that produced this list: **prove the mutation applied before believing
it went green.** Two of my own probes came back clean because the patch never landed - a JSON key
that did not exist, a phrase that differed by an apostrophe. Print the `diff --stat` and read it.
