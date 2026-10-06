# Sprint: candid-chaffinch

**Scoped 2026-10-05. Starts when brisk-bunting's last two PRs merge.**

**Everything a reader is told about nuthatch is true of the binary they download.**

Four minors shipped in two days, the engine changed under the docs in 4.1, and the mirror, seeding, the
release gate and the measured budget all arrived after most of the prose was written. The product is
ahead of its own description. Measured 2026-10-05:

- The README is 644 lines, with 17 command lines, 30 figures and 13 external links.
- The site has 9 pages, 45 guide pages (5,189 lines), a 13-chapter book (1,036 lines) and 1,002 lines
  of `llms.txt`, stamped "Checked against 4.7.0" or earlier. 27 DuckDB mentions remain across 7 guide
  files; Burrmill has been the engine since 4.1.
- `docs/` is 100 files (17,869 lines) and the builder skill 1,611 lines, read by agents building nests.

## Definition of done

Every issue labelled **`candid-chaffinch`** in `nightswatchhq/nuthatch` and `nightswatchhq/nuthatch-frontend`
is closed, with no open PR for one. A newcomer following only the README reaches a queryable WETH nest
in under two minutes on a clean macOS shell and a clean Linux container, timed. No present-tense claim
anywhere on the site, in the README, in `docs/` guides or in the skill names a mechanism the current
release does not have.

## Order

1. **The README: #1923.** Every command run, every figure sourced, every link answering, the
   architecture as it is. One commit per item.
2. **Site pages: nuthatch-frontend#80.** Nine pages against the live product and production.
3. **The guides: nuthatch-frontend#81.** 45 pages against the binary, one PR per section.
4. **The book: nuthatch-frontend#82.** Thirteen chapters against the code, worked examples re-run.
5. **`docs/` and the skill: #1924.** Guides corrected; records left dated.
6. **`llms.txt`: nuthatch-frontend#83.** Regenerated last, from the checked docs.
7. **Deploy**, with `scripts/version-check.sh` clean in both repos.

## Rules

As before: a check that needs a person to run it is not finished; every number names its source; no em
dashes; never "fastest"; The Graph is complemented, never replaced; no non-EVM promise; Nightswatch
runs no hosted nests. Added for this sprint: **a claim is checked by running it, not by reading
another document that makes the same claim.** A figure that cannot be re-sourced on the current
release is removed. A document that records a moment (a sprint plan, an audit, a release note, an RFC,
a decision, a bench log) is left as history with its date, never rewritten.
