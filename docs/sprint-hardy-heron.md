# Sprint: hardy-heron

Filed by the board on 2026-08-16, during the post-2.5.0 freeze. **Four issues.** Runs **Wednesday
2026-08-19 to Friday 2026-08-21**, resuming after the GraphOps sync on Tuesday the 18th.

## Definition of done

Every issue carrying the **`hardy-heron`** label is closed, and no open PR is for one of them.
Nothing else is in scope: work discovered during the sprint is filed as an issue for the board
rather than picked up, and pulling anything into scope needs board approval.

## The theme

**The gates that don't gate.**

This project's signature failure is not a missing mechanism. It is a mechanism that exists, is
believed to do its job, and does not - and it has now happened often enough to be a pattern rather
than a run of bad luck. `verification.md` graded level 5 as verified for two majors after the last
run. `sleep_firm()` PATCHed an endpoint that returned 200 and changed nothing. Four tests and three
RFC acceptance criteria passed with the mechanism they tested removed. In each case the thing that
made it dangerous was not the fault but the **green tick over the top of it**.

2.5.0 shipped three more, and its own release notes admit one of them in as many words: *"the fuzz
job is advisory, not a gate."* This sprint is about not shipping that sentence again.

## The four

### 1. #581 - the dbsp ICE, and the extraction that ends it

**The headline, and everything else waits on it.** `cargo fuzz build` intermittently dies with a
rustc ICE computing a vtable slot for `dbsp`'s `StarJoinFuncTrait` under sanitizer instrumentation.
Reproduced on two separate nightlies, and racy with it: the identical command failed about half the
time with no source change. ASan and debug-assertions had to be dropped to build at all, and the job
carries `continue-on-error: true` as a direct consequence.

The durable fix has been named since #290 and never costed. It has now been costed, and it is
smaller than it sounds:

- **`src/rpc.rs` (2,640 lines) has no `use crate::` lines whatsoever.** It is already standalone.
- **`src/registry.rs` (1,687 lines) touches the rest of the crate in exactly two places**:
  `crate::rpc::Log`, and `Config` in the single constructor `DecodeRegistry::from_nest`.
- The only occurrence of the string `dbsp` in either file is **inside a doc comment**, on the test
  that stands in for the fuzzer today.
- The fuzz targets import only `nuthatch::registry::{ContractSpec, DecodeRegistry}` and
  `nuthatch::rpc::Log`.

So a `nuthatch-decode` crate is `rpc.rs` + `registry.rs`, and the one awkward edge is `from_nest`.
Leave that behind in the main crate as a free function taking `&Config` and the new crate needs no
config dependency at all. Moving `config.rs` as well is the alternative and it is also viable
(1,085 lines, zero `dbsp` references), but it is the larger cut and should not be the first attempt.

**Done means:** the fuzz targets build against a crate that does not link `dbsp`, on a nightly, with
the sanitizer back on and debug-assertions restored - and the ICE cannot recur because the offending
crate is no longer in the graph.

### 2. #593 part 1 - a required context that can actually be red

Currently eight required contexts on `main`, strict, admin-enforced, and `fuzz smoke (decode path)`
is not among them. Adding it **today** would be worse than leaving it out: `continue-on-error: true`
means the job reports **success to the checks API regardless of outcome**, so the context would be a
gate that can never fail. That is the exact thing this issue was filed to complain about, installed
deliberately.

Sequence, and it does not compress: land #581, drop `continue-on-error`, let the job run
**red-capable** on `main` once, then add the context.

**Part 2 is out of scope by board ruling (2026-08-15).** `reviewed-by signature` stays advisory for
now. It works - it proves its matcher in both directions before judging anything, and passes the PR
body through the environment rather than interpolating attacker-controlled text into a shell - but
whether it becomes required is a separate decision and not this sprint's to take.

### 3. #603 - one of three fuzz targets explores nothing

`abi_arbitrary` reports **`exec/s: 2`**. Measured on two independent CI runs, different PRs,
different cache states:

| Target | PR #592 | PR #602 |
|---|---|---|
| `abi_json` | 300,000 runs in **2 s** | 300,000 runs in **2 s** |
| `abi_arbitrary` | **452 runs** in 214 s, corp 10/72b | **473 runs** in 206 s, corp 16/117b |
| `decode_log` | 300,000 runs in **8 s** | 300,000 runs in **8 s** |

It never reaches its 300,000-run budget; it hits the 180-second wall having executed roughly 0.15%
of it, and `lim: 8` says libFuzzer never grew an input past eight bytes. It consumes **206 of the
~216 seconds** of actual fuzzing in the job and finds the least of the three.

This matters beyond the wasted time: "three fuzz targets over the decode path" is a true sentence
that reads far stronger than one-of-three-explored-sixteen-inputs deserves. Either the `Arbitrary`
impl generates inputs the target rejects immediately, or each iteration is enormously expensive.
Those have opposite fixes, so **measure before changing anything** - `-print_final_stats=1` and
compare `stat::number_of_executed_units` against wall time.

### 4. #600 - every release publishes with an empty body

Both `action-gh-release` steps pass `files:` and nothing else, so the GitHub release is created bare
and stays bare until a person remembers `gh release edit --notes-file`. 2.3.0, 2.4.0 and 2.5.0 all
went out empty and were filled in afterwards. They read well now, which is exactly what makes it
invisible: the end state looks fine and the gap only exists in the window where anyone watching the
repo sees the release.

Same family as the other three, and a step performed only from memory is a step that will eventually
not be performed. This project has the identical failure recorded three times over for the website's
version strings.

**Watch for one thing:** the second `action-gh-release` step attaches the scaled tarball to a release
that already exists. Give both a `body_path` and one may overwrite the other. Check whether the two
matrix legs can race before implementing; if they can, the body belongs on a separate step that runs
before the matrix.

## Explicitly not in this sprint

- **All benchmark and OBIB work is parked** (board, 2026-08-16): #282, #285, #298, #306, #308. There
  are five OBIB reports across four commits and two providers and not one tied to a released version,
  which is a real gap - it is simply not this week's.
- **RFC-0023 tier 3** (#268), the `[[calls]]` executor. A genuine feature gap, honestly documented as
  deferred on the site and in the RFC index. Not a false claim, so not urgent.
- **Any optimisation** (#295, #296). You cannot measure and optimise in the same window and know
  which did what, and the measurements are parked.

## Why four and not eight

Three days, and item 1 is a crate extraction that items 2 and 3 both sit downstream of. If #581
lands on Wednesday the rest follows; if it does not, the sprint's honest outcome is #581 plus #600,
and saying that now is better than discovering it on Friday.
