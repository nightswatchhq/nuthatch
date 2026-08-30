# Sprint: prudent-petrel

## Definition of done

Every issue labelled `prudent-petrel` closed, and no open PR for one of them.

## The theme

**A gate that cannot see what it guards.**

RFC-0042 is parked (§14, KEEP DuckDB), and the way it ended is the reason this sprint exists rather
than a coincidence. The decision's own evidence had to be corrected five times, and all five
corrections were the same shape: **a property asserted in prose that the code did not deliver.**

The fifth is the one to look at squarely. `DuckCache`'s doc comment said *"queries take the mutex"*.
That sentence was read as "queries serialise", published into a benchmark document, then into issue
#991, then into the slice 5 decision input, and it reached a board decision as a *measured engine
property*. Nobody read the body of `attempt()`. The mutex is taken twice for a map operation and
released before the query runs; the real figures are 14.7 qps held against 81.5 qps unheld.

Four sentences of prose, three published documents, one nearly-load-bearing decision input, zero
measurements. The tracker is currently carrying **eight** issues that are instances of the same class
in the test suite rather than in the docs, and that is the sprint.

## The pieces

### 1. The gates that pass while testing nothing - #976 and #974 first

`verification tech-debt p1`. These two are first because they can currently go green having executed
none of what they claim.

**#976** - `tests/duckdb_containment.rs` scans only single-line public functions and structs, and only
three type names. A public type alias, a multiline signature, a fully-qualified DuckDB value type or an
AST type leaks straight through. This is the gate that enforces the engine boundary, and RFC-0042 §14
has just made that boundary a standing commitment rather than a transitional one.

**#974** - `scripts/gate-audit.sh` treats *any* nonzero `cargo test` exit as a caught mutation. A
missing target, a compilation failure or an already-red baseline all read as success. Its drift test
accepts any six live cases while the script declares eight, so coverage can shrink in silence. Require
a clean unmutated baseline, validate each target exists, and recognise the expected assertion failure
rather than any nonzero exit.

### 2. #980 - assert the behaviour, not the comment

`verification p1`. `tests/seal_batching_asymmetry.rs` reads the prose around `take_sealable` and asserts
the comment contains the claimed boundary rule. It executes no sealing. An implementation can make
segment cuts depend on fetch batches, keep the comment, and stay green.

This is the theme in its purest form: a test whose subject is a sentence. Feed identical ordered rows
through distinct batch partitions and assert identical seal boundaries and segment identities.

### 3. #977 - a benchmark that records a timing when the request failed

`performance verification p1`. `scripts/noise-floor.sh` and `scripts/concurrent-floor.sh` both record a
sample when curl fails or `/sql` returns non-2xx, and `noise-floor.sh` accepts N below its own
documented minimum of 15. A dead or saturated server therefore produces *attractive* figures.

Given how this sprint's theme was discovered, a harness that reports its best numbers when the server
is refusing requests deserves to go early rather than late.

### 4. #973 - the security gate's unverified exclusion

`security verification p1`. `tests/actions_are_pinned.rs` excludes every `docker://` reference, and the
comment justifies it with *"a `docker://` reference is pinned by digest elsewhere."* There is no
elsewhere: no workflow uses one today. So the exclusion rests on a claim nothing checks, and the first
`uses: docker://image:latest` anyone adds runs mutable third-party code with workflow credentials and
passes the gate.

Not a live exposure. A latent one, and again justified in prose.

### 5. #975 and #978 - the p2 tail

`#975` - the BOM helpers use GNU-only `find -printf` and `stat -c`, read one developer-local path, and
select an arbitrary lexicographically-last timing report; the macOS script neither requires nor selects
`aarch64-apple-darwin`, so it can publish Rosetta evidence as Apple Silicon evidence. `#978` - the
README states a glibc 2.35 runtime requirement next to a measured 2.34 symbol floor. Building on 2.35
does not establish a 2.35 ABI requirement; name the two things separately.

### 6. #997 - restart-to-ready, stated at the width it was measured

`documentation performance p2`. §14 now records the figure with its limits, which discharges the
reporting half. What remains is the measurement half: 500 blocks against `horizon-nest`'s 10,923
segments, on a harness whose tape source cannot build a large sealed corpus. Either close it with a
realistic-segment fixture, or close it as *"stated honestly, measurement deferred"*. Do not leave it
open with the bare number.

## What is deliberately not here, and why

**`SQL_MAX_CONCURRENCY = 2`, filed this sprint and worked the next.** §14 names it as work the park does
not block, and it is real: the knee is nearer 8 and worth roughly 4.8x throughput. It is held back for
one reason. The knee was measured on a 32-core ThinkPad, and the constraint that actually binds is the
**per-cursor RAM budget** - unbounded at 32 clients reached 1,313 MB, 64% of one cursor's entire 2 GB.
A throughput ceiling measured off the surface that enforces it is how a ceiling gets set below the
healthy figure, which this project has already done once. It wants the Hetzner box, and that is a
sprint of its own.

**Any RFC-0042 work.** The carve-out is spent and the freeze applies in full. §14 carries a reopen date
of 2027-09-01 and four triggers; absent one of those, a proposal to resume is a proposal for a third
carve-out. The one asymmetry worth remembering: **if RFC-0033 slice 4 (#357) is ever scheduled, reopen
RFC-0042 before it, not after.**

**The parked and frozen backlog**, all sixteen issues of it. It is correctly parked and was reviewed
this sprint only to confirm that.
