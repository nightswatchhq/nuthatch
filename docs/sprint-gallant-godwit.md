# Sprint: gallant-godwit

## Definition of done

Every issue labelled `gallant-godwit` closed, and no open PR for one of them.

## The theme

**Stop measuring the engine and start measuring the product.**

`fastidious-fulmar` answered RFC-0042's engine question: DataFusion 55 is 2.4-2.6x slower than DuckDB
at 8 M and 20 M rows and **0.84x - faster - at 2 M**, both engines having got ~2.2x quicker since
2026-08-02 without either pulling ahead. That is a real answer and it settles less than it appears to,
for a reason slice 0 wrote down before the measurement was taken:

> Of the six DuckDB roles, a DataFusion port addresses **one**.

So a slice-2 result that had come back decisively either way would have resolved a sixth of the
deletion checklist. This sprint is the other five-sixths, and two of them are product-visible rather
than performance questions at all.

The decision rule is unchanged and governs this sprint as it did the last:

> There is no preferred answer. If evidence says DuckDB remains best, it stays.

**New this sprint, and binding: §3a.** The outcome is binary. Either DuckDB stays in every role slice 0
inventoried, or it goes entirely and a §5.3 Rust-native composition takes all six. A candidate that
wins in one size band only has *not met the gate*; that is a finding to write up with the band named,
never a routing design. Slice 2's crossover is exactly the evidence shape that invites a hybrid, which
is why the rule was promoted from §11's risk list before this sprint started.

## The pieces

### 1. #966 - RFC-0042 slice 3: the four roles a DataFusion port does not address

`rfc verification`. The one that decides the RFC. §10 permitted it to run in parallel with slice 2
after slice 1, so it has been startable since #936 and simply had no issue.

**The admissible function vocabulary (`entities.rs`) is the hardest, because it is a public contract.**
The set of functions a nest may declare *is* `duckdb_functions()` - "the same catalogue the binder
uses", by that code's own comment. Swapping engines swaps what a user may legally write in
`entities.toml`, and a nest declaring a DuckDB-only function stops loading. This can produce "DuckDB
stays" on its own, with no timing involved.

It is also the one guard in this codebase that is an allowlist **derived from the engine's own parser**
rather than a hand-maintained denylist. Replacing the engine without replacing the derivation turns it
back into the thing `docs/` already records as recurring.

**The lowering AST (`entity_lower.rs`)** is the RFC-0041 parser role. The question is not "can a
candidate parse" but whether its tree carries what lowering needs and is stable across versions - a
parser that reshapes its AST on a minor bump makes every authored entity a compatibility surface.

**The graft engine string (`graft.rs`)** is a migration decision, not a measurement. #944 took the
first bite; grafts already on disk record `engine: "duckdb-v1.4.0"`, and what a post-DuckDB runtime
does with them - refuse, migrate, treat as advisory - should be written down before anything is built
against it.

**Turso** gets an honest answer about whether it contributes to these roles at all. §11 lists "Turso
causing an unrelated rewrite" as a risk, so "not relevant here" is a valid and useful outcome.

Ends with a **measured role-by-role result**, per §10. Not a recommendation.

### 2. #964 - re-run the gate over a realistic multi-segment layout

`rfc performance verification`. Independent of #966; runs alongside.

Slice 2's fixture is **one segment. A real nest has 10,923**, at a 6.3 KB median with 80% under 20 KB.
The write-up says so itself and names this as the next measurement. It gated it on #947, which is now
closed and proposed no change - so it is unblocked, and today's layout is the **pessimistic bound**,
because any fix that lands makes segments fewer and larger rather than more and smaller.

If the ratio moves with segment count, the one-segment number is not the number and the comparison has
been measuring the wrong axis. Worth knowing before slice 3 assigns any remaining role.

### 3. #965 - the five parked Dependabot majors

`tech-debt verification`. None has a genuine failure; #929's apparent one is a cancelled run.

The reason to think first: `softprops/action-gh-release` and `docker/login-action` appear **only** in
`release.yml`, which runs on `tags: ['v*']`. **No pull request runs it.** Every green check on those
PRs is evidence about a workflow that does not use them. `upload-artifact` and `download-artifact`
should land as one change, since each PR proves its new version against the other's *old* one, leaving
the post-merge combination untested at the release pipeline's critical path.

Order and reasoning are in the issue.

### 4. #961 - `determinism_gate` has no production caller

`verification tech-debt`. Small, and the smallness is the point. Its own doc comment calls it "the
backstop, and it is the stronger one: it catches float..." - and the only caller is its own test.
Either wire it in or delete it. A backstop nobody invokes is worse than no backstop, because the
comment claims otherwise and the next person reads the comment.

## What is deliberately not here

Slices 4, 5 and 6 stay unfiled. §10: slice 4 "follows evidence, not an open-ended plan to write a
database", and slice 3 is what produces that evidence. Filing them now would be planning a migration
this sprint exists to decide whether to have.
