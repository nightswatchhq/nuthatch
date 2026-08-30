# Sprint: prudent-petrel

## Definition of done

Every issue labelled `prudent-petrel` closed, and no open PR for one of them.

## The theme

**A gate that cannot see what it guards.**

RFC-0042 is parked (§14, KEEP DuckDB), and the way it ended is why this sprint exists. The decision's
evidence needed five corrections, and every one was the same shape: **a property asserted in prose that
the code did not deliver.** The fifth travelled from a doc comment in `analytics.rs`, into a benchmark
document, into an issue, into the slice 5 decision input, and reached a board decision as a *measured
engine property*. Nobody opened `attempt()`.

The eight issues below are that same class living in the test suite instead of the docs.

**Every finding here was re-verified against the code before this sprint was scoped**, because scoping
from issue text would be the same mistake one level up. Two issues turned out to need their own text
corrected first, and that is recorded per-issue rather than discovered by whoever picks them up.

## Ordering

1. **#976, #974** - the two that can go green having executed nothing.
2. **#980, #977** - a test whose subject is a comment, and a harness that rewards a dead server.
3. **#973** - latent, security-labelled, small.
4. **#975, #978** - the p2 tail.
5. **#997** - closes either way, but not left open with a bare number.

#976 leads because RFC-0042 §14 has just turned the DuckDB boundary from a transitional thing into a
standing commitment, and #976 is the gate that enforces it.

## The pieces

### 1. #976 - the containment gate is blind to `pub(crate)`, and there is a live instance

`verification tech-debt p1`. **Verified: the defect is real and currently firing.**

`the_analytical_surface_keeps_duckdb_types_internal` filters lines with
`starts_with("pub fn") || starts_with("pub struct")`, then matches three type names.

`graft.rs` has **five** functions taking `&Connection`:

```
pub(crate) fn canonical_plan(conn: &Connection, sql: &str) -> CanonicalPlan
pub(crate) fn engine_version(conn: &Connection) -> String
pub(crate) fn parser_connection() -> Result<Connection>
pub(crate) fn build(conn: &Connection, files: &[(String, String)]) -> Dag
pub(crate) fn determinism_gate(conn: &Connection, sql: &str) -> Result<()>
```

None starts with `"pub fn"`, so the test sees none of them and asserts `leaks.is_empty()` green. **The
test's own doc comment says `graft.rs` "does not" satisfy §6 and that this "records the gap with a
number".** There is no number, and the assertion states the opposite of the comment above it.

Also invisible: `pub async fn`, `pub type` aliases, any multiline signature, and the DuckDB type family
beyond the three named - `Statement`, `Appender`, `Row`/`Rows`, `duckdb::Error`, `Config`.

**The design question the fix must answer, and it is not obvious:** does `pub(crate)` count as a leak?
It is crate-internal, so under a strict reading of §6 it is not. But the file's prose says it is a
gap. **Decide it explicitly and write the decision into the test**, rather than leaving the current
state where the answer depends on a string prefix nobody chose deliberately.

**Done when:** every public item form is covered (including `pub(crate)` if that is the ruling, `pub
async fn`, `pub type`, multiline signatures), the prohibited type family is complete rather than three
names, `graft.rs`'s five sites are either allowed with a recorded count or fixed, and a regression case
exists for a multiline signature and a type alias. Mutation-check it: introduce a leak of each form and
confirm red.

**Size:** medium. The scan wants restructuring rather than another `contains` arm.

### 2. #974 - any nonzero exit counts as a caught mutation

`verification tech-debt p1`. **Verified, but the issue is half wrong and its number is stale. Read this
before starting.**

**Confirmed.** `scripts/gate-audit.sh:88` is `if [ $rc -ne 0 ]` → "caught". A missing test target, a
compilation failure, or an already-red baseline all report success. There is no baseline run anywhere
in the script.

**Correction to the issue text.** It says *"its drift test also accepts any six live cases although the
script currently declares eight, allowing coverage to shrink silently."*

- The script declares **ten**, not eight.
- **Drift is already caught.** A needle that stops matching produces a SKIP, the script's final
  expression is `[ "$SURVIVED" -eq 0 ] && [ "$SKIPPED" -eq 0 ]`, and `gate_audit_cases.rs` asserts
  `out.status.success()`. So the stated hole does not exist.
- The hole that **does** exist is **deletion**. `targets >= 6` against ten declared means four cases can
  be removed from the `CASES` array with both assertions still passing.

Anyone fixing the issue as written would harden a path that is already hard and leave the real one open.

**Done when:** a clean unmutated baseline runs and must be green before any case is trusted; each
target is validated to exist; the expected assertion failure is distinguished from setup failure
(compile error, missing target); and the drift test pins the **complete expected case name set**, not a
floor. Prove the baseline check works by making the baseline red on purpose and confirming the audit
refuses to report.

**Size:** medium. Mostly shell, but the baseline run makes it slower and that needs saying in the doc.

### 3. #980 - the test reads a comment and never seals anything

`verification p1`. **Verified. The purest instance in the sprint.**

`the_seal_boundary_is_a_function_of_the_data_not_the_fetch` reads `src/indexer.rs` **as a string**,
takes the 1400 characters before `fn take_sealable`, and asserts that window contains the literals
`"from the **data**"` and `"identical"`. It executes no sealing. Its own comment says so plainly:
*"Deliberately not calling the private helper: this asserts the documented rule."*

The window it searches **is the doc comment**, so this is a gate matching its own documentation - the
failure `duckdb_containment.rs` explicitly guards against and this file does not.

An implementation can make segment cuts depend on fetch batches, keep the prose intact, and stay green.

**Done when:** identical ordered rows are fed through at least two distinct batch partitions and the
resulting seal boundaries **and segment identities** are asserted identical. Keep the prose check if you
like, but it is not the test. Mutate: make the cut depend on batch size and confirm red.

**Size:** medium-large - the only one needing a real fixture rather than a scan rewrite. It is the
highest-value item here and should not be squeezed in last.

### 4. #977 - the benchmark records a timing when the request failed

`performance verification p1`. **Verified in both scripts.**

`scripts/noise-floor.sh:18` and `scripts/concurrent-floor.sh:20` both run
`curl -s --max-time N ... > /dev/null` with no `--fail` and no status inspection, then record
`t1 - t0` unconditionally.

The consequence is the wrong way round from an ordinary bug: **a dead or refusing server returns
instantly, so the noise floor gets *tighter* the more broken the server is.** `noise-floor.sh` is the
source of `docs/bench/noise-floor.md`, which is the threshold every RFC-0042 measurement was judged
against.

`N=${N:-15}` defaults correctly but has no floor, so `N=3` is accepted against a documented minimum
of 15.

**Done when:** a sample is recorded only for a 2xx response; the run retries or fails until the
requested count of **successful** samples is met; N below 15 is rejected; and a failure-path regression
test covers a server returning 503 and a server that is not listening.

**Size:** small-medium. Do it early - the sprint's theme was found by distrusting a harness.

### 5. #973 - a security exclusion whose justification has no referent

`security verification p1`. **Verified: latent, not live.**

`tests/actions_are_pinned.rs:57` is `!reference.starts_with("docker://")`, justified on line 54 by
*"a `docker://` reference is pinned by digest elsewhere."* There is no elsewhere: `grep -rn "docker://"
.github/workflows/` returns nothing, so the exclusion protects nothing and is checked by nothing.

Not a live exposure. The first `uses: docker://image:latest` anyone adds runs mutable third-party code
with workflow credentials and passes the gate.

**Done when:** `docker://` references require an immutable digest, or the exclusion is narrowed to
syntax independently verified elsewhere and that elsewhere is named and tested. Regression test with a
mutable Docker reference.

**Size:** small.

### 6. #975 and #978 - the p2 tail

**#975, verified, all three parts.** `scripts/native-bom.sh:55` uses `find -printf` and `:63` uses
`stat -c` (both GNU-only); `scripts/bom-timings.py:2` is
`sorted(pathlib.Path("/home/pepe/bom/target/cargo-timings").glob(...))[-1]` - one developer's absolute
path, and a lexicographic pick rather than a chosen run; `scripts/bom-mac.sh` neither requires nor
selects `aarch64-apple-darwin`, so it can publish host or Rosetta evidence as Apple Silicon evidence.

Make the timing input explicit, record which run and artefact the numbers came from, document or remove
the GNU-only assumptions, and make the macOS target explicit or fail on the wrong architecture. These
produced RFC-0042 slice 0's published figures, which §14 now cites.

**#978, verified and sharper than filed. The README contradicts itself.** It states *"glibc 2.35 or
newer. The measured floor is 2.34; 2.35 is what the release is built against, so it is the number to
trust"*, and eleven lines later lists **RHEL 9** as clearing it. RHEL 9 ships **glibc 2.34**. So a RHEL
9 user is told both that they are below the requirement and that their platform is supported.

Both numbers are already present; the fault is that a build baseline is presented as an ABI
requirement. Name 2.34 as the measured ABI floor and 2.35 as the release build baseline, and make the
supported-platform list agree with whichever one is the requirement.

**Size:** #975 small-medium, #978 small.

### 7. #997 - restart-to-ready, closed one way or the other

`documentation performance p2`. §14 now records the figure with its limits, which discharges the
reporting half. What remains is the measurement half: 500 blocks against `horizon-nest`'s 10,923
segments, on a harness whose tape source cannot build a large sealed corpus.

**Done when** either a fixture seals a realistic segment count and the number is re-taken, or the issue
is closed as *"stated honestly, measurement deferred"* with §14 cited. Do not leave it open carrying the
bare figure - that is how the 500-block number gets quoted again.

**Size:** small if closed as deferred; medium if the fixture is built.

## What is deliberately not here, and why

**`SQL_MAX_CONCURRENCY`, filed as #1006 and worked next sprint.** §14 names it as work the park does not
block, and it is real: the knee is nearer 8, worth roughly 4.8x. Held back for one reason. The knee was
measured on a 32-core ThinkPad, and what binds is the **per-cursor RAM budget** - unbounded at 32
clients reached 1,313 MB, 64% of one cursor's entire 2 GB, shared across every nest on that cursor. A
throughput ceiling measured off the surface that enforces it is how a ceiling lands below the healthy
figure, which this project has done before. It wants the Hetzner box and a realistic nest count.

**Any RFC-0042 work.** The carve-out is spent and the 2026 freeze applies in full. §14 carries a reopen
date of 2027-09-01 and four triggers; absent one, a proposal to resume is a proposal for a third
carve-out. One asymmetry to keep in view: **if RFC-0033 slice 4 (#357) is ever scheduled, reopen
RFC-0042 before it, not after** - swapping the engine before durable grafting wires in costs nothing,
after it costs a full recompute per derivation. #357 sitting quietly in the parked pile is exactly how
that gets missed.

**The parked and frozen backlog**, sixteen issues. Reviewed this sprint only to confirm it is correctly
parked.

## A standing note for whoever works these

Five of the eight are gates. **A fix to a gate is not done when the gate passes - it is done when the
gate has been shown to fail.** Introduce the defect it claims to catch, watch it go red, then remove
the defect. A green gate proves nothing about a gate, which is the whole content of this sprint.
