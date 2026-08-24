# Sprint: fastidious-ferret

Filed after auditing earnest-echidna. **Eleven issues.**

Eleven rather than fifteen, deliberately. Earnest-echidna shifted fifteen,
but eleven of its issues were corrections to prose and this sprint is almost entirely code with
tests behind it. The slack goes into the p0.

## Definition of done

Every issue named below is closed, its PR merged, and no PR left open. Nothing else is in scope: the
firm does not allocate from the backlog, and work discovered during the sprint is filed as an issue
for the board rather than picked up.

That condition is the board's, and it is what puts the firm back to sleep.

## The theme, and why this scope

**Make the binary do what the page says.**

Earnest-echidna's theme was that everything nuthatch says has to be true, and the firm made it true
the cheaper way: where a document and the product disagreed, it corrected the document. That was
correct - most of those claims were simply stale - and it produced eleven merged PRs and a genuinely
better set of docs.

It also produced **six issues that cannot be fixed that way**, every one found by running the
published v2.2.0 binary rather than by reading code. In these the document is *right* and the
product is wrong, so the only honest repair is in the code.

This sprint is that list. It is the same theme one layer down, and it is the harder half.

## How this is ordered

**#512 first, and by itself if need be.**

`nuthatch init` warns that the README's own USDC quickstart will "index zero rows, silently". It
indexes **1,348 rows in 20 blocks**. That is the headline command, on the headline address, telling a
first-time user their nest is broken at the exact moment they have no way to know better - and it
sits directly on the under-two-minutes first-run demo that CLAUDE.md calls the primary deliverable.

Nothing else in this sprint is reached by as many people. Take it first.

Then the rest of the 2.0 wiring, in any order:

| issue | what the product does that the page denies |
|---|---|
| **#517** | `POST /_admin/nests` resolves nests from `nests/<name>/`, the pre-2.0 roost layout, in a runtime where they live at `data/<nid>/`, and ignores the `source` field the RFC specifies. `DELETE` on the same surface is correct, which is the tell: this is half-migrated, not un-migrated. Labelled `bug`. |
| **#509** | `nuthatch sql`'s API fallback asks for `/sql`, which a `mounts.toml` runtime does not serve. The README promises "the same command works either way". |
| **#510** | A fully dead RPC pool at cold start exits after logging **"API live"**, so `/ready` never reports `stalled`. The README says `/ready` reports `stalled` when every endpoint refuses. Two faults in one: the log line asserts a thing that is not true, and the readiness signal a monitoring setup depends on never fires. |
| **#520** | `serve` without `--hot-store` creates the redb, commits a write txn and holds the exclusive `flock`. It is therefore neither read-only nor shareable, and three claims one file away say it is both. |
| **#511** | `prune` refuses a `mounts.toml` with zero mounts - the state with the most to reclaim. Narrow, the happy path is fine, and the issue says so itself. Cheap, so take it late. |

Then the gates, which have now cost us twice in one sprint:

| issue | |
|---|---|
| **#527** | A sprint-branch PR has no gate at all. `sprint/*` carries no branch protection, so `--auto` does not mean "merge when green", it means merge now - which is how #524 landed with `fmt · clippy · test` still running. It passed, so nothing broke; that is luck, not a process. |
| **#522** | The required check expires at 25 minutes and reports as `cancelled`, which is indistinguishable from a human cancel or a supersede. |
| **#523** | `ci.yml` claims push runs on `main`/`sprint` are never cancelled. A queued one is. |

And two from the board's audit of your own last sprint:

| issue | |
|---|---|
| **#528** | `/explain` still degrades to cold-only in silence - the third hot-scan call site, which #472 as filed never named. **This is board scope leakage, not a miss**: the site was in the sprint brief and not in the issue, so "Closes #472" was accurate. Read the scope note at the bottom of the issue before assuming otherwise. |
| **#529** | Three deadline tests fail at load average 34 and pass three times running on a quiet box, so `cargo test --lib` is red on a busy machine. A gate that behaves that way teaches people to re-run until green. Note the specific trap: #524 rewrote one of these *specifically* to stop depending on wall-clock, and the dependency moved rather than went - which of two error kinds occurs still depends on which of two deadlines expires first. **Demonstrate the fix under deliberate load, not on a quiet box.** |

## The standing rules, unchanged

- **Paste the mutation artifact**: the diff of what was broken, and the panic line of the test that
  died. A sentence saying a mutation was done is not accepted. Earnest-echidna complied on eight of
  eleven PRs and declared the other three honestly, which is the standard.
- **Run it, do not reason about it.** Every issue in this sprint was found that way and none of them
  by reading code.
- **A skip is not a pass**, and a mutation that does not mutate is not a test.
- **Ask who calls this.**
- **A claim is a deliverable.** If a PR changes what the software says about itself - a field, a doc
  comment, an error string, an RFC - the truth of that sentence is part of the review.
- **File what you find; do not work it.** Earnest-echidna filed nine issues and worked one, and that
  one was a CI ceiling blocking its own delivery. That was the right call and the right exception.
- **`CLAUDE.md`'s factual claims are yours to correct with evidence; its decisions are not.** New
  ruling, 2026-08-13, after the CEO correctly replaced a status paragraph that was a release out of
  date. See `BOARD-BRIEF.md`.

## Not in scope, deliberately

- **The performance set** (#295, #296, #298, #282, #285) and the OBIB cases (#306, #308). Deferred
  for the second sprint running, and the board is aware that is now a pattern rather than an
  accident. It stays deferred because a benchmark improvement on a surface that misreports its own
  readiness is not an improvement.
- **#441 and #428.** Board-only. #441 is unmet for a third consecutive release.
- **Anything discovered mid-sprint.** File it.
