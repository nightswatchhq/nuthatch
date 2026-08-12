# Sprint: earnest-echidna

Filed by the board on 2026-08-12, immediately after auditing diligent-dormouse and releasing v2.2.0.
One week.

## Definition of done

Every issue named below is closed, its PR merged, and no PR left open. Nothing else is in scope: the
firm does not allocate from the backlog, and work discovered during the sprint is filed as an issue
for the board rather than picked up.

The sprint is complete when the named set is closed and zero PRs are open. That condition is the
board's, not the firm's, and it is what puts the firm back to sleep.

## The theme, and why this scope

**Everything nuthatch says has to be true.**

The previous sprint's theme was tests that could not fail. This one is its consequence one layer out:
a claim nobody checks. A response field, a doc comment, an RFC, a go-live checklist and a source
comment are all *statements to a user*, and we shipped several this week that are false.

The sprint exists because of one issue in particular. **v2.2.0's headline feature is that a nest
tells you when its cold data is incomplete.** We put that on the website this morning. #472 says
there is a path where it lies: a hot-scan failure falls through to `Default::default()` at
`serve.rs:1080-1084` and again at `1207-1210`, the query is answered from sealed data alone with the
entire live tip missing, and #435 then stamps the response `"degraded": false`. Nothing is logged.
On a tip-following nest the missing stretch is the most recent and most interesting one, so
`SELECT SUM(...)` comes back quietly understated.

That is worse than the bug 2.2.0 fixed, because we have now published a promise that covers it.

The rest of the sprint is the same fault in its other costumes. Note how ordinary each one looks on
its own, and that is the point: no single item here would justify a sprint, and together they are the
reason a caller cannot take our word for anything.

## How this is ordered

**Do #472 and #477 as one piece of work, one agent.** #472 is the defect; #477 is why nobody could
see it. Every cold-data fixture in the tree has exactly one populated table, so a degraded *nest* and
a degraded *result* are the same set in every assertion, and a wrong sentence reads as correct
everywhere. Fixing #472 without #477 repeats the render-versus-flag trap that #453 already fell into
once. The fixture that tells them apart needs **more than one populated table, one of them degraded,
queried on the healthy one.**

Then, independently and in any order:

| issue | what is false |
|---|---|
| **#378** | The `503` hot-scan refusal on `/sql` and `/explain` has no test. `tests/` contains zero occurrences of `SERVICE_UNAVAILABLE` against four live sites in `serve.rs`; both arms are deletable with the suite green. |
| **#500** | `the_sweep_is_bound_by_the_query_s_own_deadline_not_a_fresh_one` cannot see a fresh deadline, only an unbounded one. The name claims more than the assertion pins. Board audit finding; the suggested repair is on the issue. |
| **#417** | `store_holds_rows` is documented "read-only". redb 2.6.3 opens the file `.read(true).write(true)` (`db.rs:1224`), so it is not. **We shipped that sentence this morning** in #484/#486, which strengthened the surrounding claim while leaving the false half. |
| **#377** | RFC-0034 still says `/explain` is unbounded. #367 made that false. |
| **#403** | The go-live checklist demands an admin token that neither deploy recipe shows. A checklist nobody can follow is not a checklist. |
| **#495** | `e2e_reorg.rs:288` says the #461 case is `#[ignore]`d and "fails today". #485 fixed it. |
| **#409** | The `--fail-fast` carve-out comments cite `runtime.rs:1128`; PR #394 moved it to 1135. |

Two cheap ones, to keep throughput up and because they are genuinely worth doing:

| issue | |
|---|---|
| **#376** | `parse_retry_hint` discards Go composite durations, so a long rate-limit hint is silently ignored. |
| **#383** | Superseded CI runs are never cancelled, so every push multiplies runner contention. |

## The new rule: the mutation artifact goes in the PR

Agreed by the board this sprint, on the CEO's recommendation, and it replaces the prose convention.

A PR that claims a mutation check must **paste the artifact**: the diff of what was broken, and the
panic line of the test that died.

```
## Mutation
```diff
-        let corrupt = segments_failing_verification(dir, &tables, deadline);
+        let corrupt = segments_failing_verification(dir, &tables, None);
```
```
test analytics::tests::the_sweep_is_bound_by_the_query_s_own_deadline_not_a_fresh_one ... FAILED
thread panicked at src/analytics.rs:3441
```
```

A sentence saying a mutation was done is no longer accepted. The reason is measured rather than
theoretical: six mutation checks in the last audit, and the one finding was again a test pinning less
than its name claimed. A prose claim cannot be checked by anything; an artifact can be read in ten
seconds by the next person.

Three things this catches that the honour system does not, all of which have happened here:

- a mutation that **never applied** - the patch failed, the script had no `set -e`, and the unmutated
  code passed
- a mutation that went **red for the wrong reason** - two mutations dying on the same `expect` prove
  one thing twice
- a mutation that **did not reproduce the defect** - a bounded-but-fresh deadline is not an unbounded
  one, and the difference is exactly #500

**Read the panic line, not the pass/fail count.**

## Not in scope, deliberately

- **The performance set** (#295, #296, #298, #282, #285) and the remaining OBIB cases (#306, #308).
  Worth doing and deliberately not now: they compete for the same attention as the correctness work,
  and a benchmark improvement resting on a surface that lies is not an improvement.
- **#441 and #428.** Board-only, and #441 is now unmet for a second consecutive release. Not the
  firm's to take.
- **Anything discovered mid-sprint.** File it, do not work it.

## What the board owes the firm, stated plainly

Two things came back from the diligent-dormouse close-out and both are fair.

**The CEO's answer on no-peer-review was the useful one.** The cost of dropping review was not the
missing reader; it was that *the reader was the only thing checking whether CI had run at all*. Three
mechanics were named - a `pull_request: branches:` filter that matches on the base so a stacked PR
never builds, `gh pr edit --base` not re-running anything, and an armed auto-merge firing the moment
its base lands. All three produce a PR that reads green or pending while nothing was verified.

The recommendation was to add the sprint branch to the trigger list. **That is already fixed and more
thoroughly than proposed**: #491 and #492 removed the `branches:` filter from `pull_request`
entirely, so every PR builds regardless of its base. Worth saying so explicitly, because the
recommendation was reasoned from mid-sprint state and the fix landed underneath it.

**Sprint-branch merges close nothing**, because auto-close only fires on the default branch. For a
sprint working on a branch, the tracker stops being the record of what is done. Live with it this
sprint, but nobody should read open-and-unclaimed as undone work.

## The standing practice this sprint inherits

Unchanged from diligent-dormouse, plus the artifact rule above:

- **Run it, do not reason about it.**
- **Ask who calls this.**
- **A skip is not a pass**, and a mutation that does not mutate is not a test.
- **Re-test gates before planning around them.** A gate recorded once and never re-checked becomes
  folklore, and folklore decides what you build next.
- **Benchmark against a real provider, not a mock**, and test the confound.
- **Keep verification.md's "what we have verified" table honest.**
- **A claim is a deliverable.** If a PR changes what the software says about itself - a field, a doc
  comment, an error string, an RFC - the truth of that sentence is part of the review, not a
  footnote.
