# Sprint: honest-heron

## Definition of done

Every issue labelled `honest-heron` closed, and no open PR for one of them.

## The theme

**Make RFC-0042 decidable, or find out it already is.**

`gallant-godwit` answered slice 3 and re-ran the engine gate on a realistic layout. Two results from it
shape this sprint:

1. **#966** - of DuckDB's six roles, none blocks removal on its own. The nearest thing to a blocker,
   the missing `arg_max` family, is unreachable in the product today.
2. **#964** - DataFusion is **2.5-2.9x** slower than DuckDB on `net_balances` at a realistic segment
   layout, at every size measured. Slice 2's "DataFusion wins at 2 M" was a one-segment artefact.

Set that second number against §7:

> A close but slower Rust engine remains experimental. **"Only 15% slower" fails if outside measured
> noise on a load-bearing workload.**

Against a 5% noise floor, 2.5x is roughly ten times the margin §7 already calls disqualifying. **The
obvious candidate has failed the gate.** If DataFusion-as-drop-in were the only candidate on the table,
slice 5 could be written this week.

It is not. §5.3 routes a **heavy known fold** to a specialised Rust operator, not to general SQL - and
#964 measured `net_balances` going down the general-SQL arm. The arm §5.3 actually points it at has
never been measured. That single gap is why the RFC is still open, and closing it is this sprint.

The decision rule is unchanged, and §3a now binds the *shape* of the answer as well:

> There is no preferred answer. If evidence says DuckDB remains best, it stays.

## The pieces

### 1. #987 - the specialised-operator spike. First, because it is the one that decides

`rfc performance verification`. One Rust operator for the `net_balances` i128 fold over sealed Parquet,
no SQL engine in the path, measured on #964's exact harness and segment sweep so the numbers compare
without re-derivation.

Bounded on purpose - §11 lists "specialisation becoming a worse database" as a risk and §10 says slice 4
"follows evidence, not an open-ended plan to write a database". One operator, one query, no production
wiring, parity asserted before any timing counts.

**Both outcomes end the open-endedness.** Close the gap and §5.3's composed path is real, so slice 4
proceeds on evidence. Fail to close it and the strongest arrangement the RFC proposes still fails §7 on
the workload that matters, making *"Keep DuckDB, because these measured regressions remain"* writable -
§0's explicitly successful outcome.

### 2. #986 - the four decision rows nobody has ever measured

`rfc performance verification`. §11's table is **4 of 11 measured, 3 partial, 4 untouched**. The empty
ones - concurrent throughput, ingest throughput, restart-to-ready, and a real peak-RSS figure - are all
runtime behaviour under load.

Worth doing regardless of #987, because **none of them has a DuckDB baseline either.** They can be
filled against the system we already ship, no candidate required, and *"keep DuckDB with these
regressions"* still has to state its regressions against something.

Note `noise-floor.md`: the distribution is bimodal under concurrency, so these need **p95 under
concurrent load**, not more single-client medians. `tools/df-gate` is single-client by construction, so
this is a different harness and should be said so rather than bolted on.

### 3. #985 - the pinned-actions gate advises pinning to checkout v4

`documentation tech-debt`. Small, and a member of a class that already cost us: the gate's failure
message offers `checkout@11d5960a... # v4` as its worked example, three majors stale, in a `.rs` file
Dependabot will never touch.

#984 fixed the instance of that class with teeth - `gate-audit.sh` hardcoded the same SHA as a *needle*,
so landing #925 made the case **SKIP** and stopped that gate being audited at all. This one only gives
bad advice. The general rule both are instances of: **a version pin written outside the files Dependabot
maintains goes stale silently.**

## What is deliberately not here

Slices 5 and 6. Slice 5 is the decision and it is what this sprint makes possible, not what it
performs - writing it before #987 reports would be choosing an answer and then measuring.

And a win in #987 is **not** a licence to remove DuckDB. It settles the general-SQL/heavy-fold role.
The admissible function vocabulary, the lowering AST and authored views still need an engine, and
#966 recorded what each of them requires.
