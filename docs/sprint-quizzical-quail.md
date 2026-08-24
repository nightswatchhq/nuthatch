# Sprint: quizzical-quail

Filed while pragmatic-peregrine's CI was still running. **Four issues.**

## Definition of done

Every issue carrying the **`quizzical-quail`** label is closed, and no open PR is for one of
them. That is four issues: #289, #781, #755, #756. #757 is the same class as #755 and is closed
from that PR if the diff stays small; it is not a fifth labelled slot. Work discovered in flight
is filed **unlabelled**. Pulling it into scope needs a board reply.

## The theme

**A control that does not work is worse than one that is absent, and a number whose commit you
cannot check out is the 289 ev/s failure mode with the digits filed off.**

Peregrine makes CU and `$1,192` honest, and fails the build when `benchmarks.md` cites a missing
file. It does not notice that the file's commit is a squash head nobody can check out, that
`bench backfill` writes the store onto a ramdisk, that `operators.md` still calls `--seal-direct`
"much faster", or that DuckDB `allowed_directories` is set and then ignored. Those are nightjar's
generators, still running.

#289 was promised as next-but-one in peregrine. That was a promise.

Freeze-legal throughout: bug, security, documentation, a gate. Not RFC-0040.

## The four

### 1. #289 - DuckDB `allowed_directories` is documented, set, and inert

**The control.** We `SET allowed_directories` and `lock_configuration`. The bundled DuckDB does
not treat that list as a restriction unless `enable_external_access` is false, which is the other
half of DuckDB's own docs and a startup-only setting we never passed. The denylist is the only
layer that actually stops a file read. A tripwire test currently *asserts* the lockdown does not
work, so a bump that fixed it would look like a regression.

**Acceptance**

1. Confirmed against `duckdb`/`libduckdb-sys` 1.10504.0 (the bundled build), not upstream prose.
2. An out-of-allowlist `read_text` fails with the denylist *removed* from the path the test takes.
   Deleting `enable_external_access = false` fails that test.
3. A `/sql` query over sealed Parquet in the nest's segments dir still works.
4. Security docs and prod-readiness no longer describe the layer as inert. The dated audit record
   notes the fix rather than being rewritten as if 2026-07-31 knew it.

### 2. #781 - `bench backfill` measures storage on a ramdisk

**The rig.** Unless `--keep` is passed, `one_run` writes redb and Parquet under
`std::env::temp_dir()`. On the box that publishes numbers, `/tmp` is a 32 GB tmpfs. The
seal-direct versus hot-store comparison is RAM versus RAM, `peak_rss_mb` understates the
commitment, and CLAUDE.md already forbids build output in `/tmp`.

**Acceptance**

1. The default work directory is on real disk (`$XDG_CACHE_HOME` or `$HOME/.cache/nuthatch-bench-…`),
   never `temp_dir()`. `--keep` is unchanged.
2. `BenchReport` records whether the work dir was tmpfs, so a published number says what it sat on.
3. A test fails if the default path is under `temp_dir()`.

### 3. #755 - `--seal-direct` is still "much faster" in operator-facing copy

**The adjective.** `benchmarks.md` measures 0.92× the hot store on a 120-block range, and the
comparison was confounded besides. `operators.md` and the shipped builder skill (clap help, so
`cli-reference.md`) still say "much faster from deployment". That is a multiplier with the digits
filed off.

Do not restate a speed claim for `--seal-direct` in isolation. The honest line: it bypasses the
hot store's write-then-seal round trip and is the prerequisite for `--concurrency`, which is where
the measured speedup is; see `docs/benchmarks.md`. Both files point there rather than carrying
their own adjective.

#757 (grant docs claiming a measured, CI-enforced ~40 MB that is neither 37 nor 58) is the same
shape. Until a RAM figure has a committed artefact, the grant docs do not quote one. Do not
hand-correct 40 → 37.

**Acceptance**

1. `operators.md` and the clap help for `dev --seal-direct` do not call the flag faster. They
   point at `docs/benchmarks.md`.
2. `cli-reference.md` is regenerated from clap; the drift gate stays green.
3. Grant docs no longer say ~40 MB, measured, or CI-enforced of that figure.
4. The `dev` and `dev` duplication in `operators.md` is fixed while someone is in the file.

### 4. #756 - bench artefacts cite squash heads nobody can check out

**The provenance.** 11 of 13 `docs/bench/*.json` files name a commit that `git cat-file` cannot
see on a clone of `main`, because we recorded squash-merged PR heads. README's "How fast is it"
traces to three of those. #741 (peregrine) checks the field is non-empty. A ghost hash is
non-empty.

Recovered via the GitHub API: each ghost SHA maps to the merge commit of the PR that landed it.
Rewrite the field to that merge commit. A CI check fails if a `commit` in `docs/bench/*.json`
does not resolve with `git cat-file -t` on a clone that has `main`'s history.

**Acceptance**

1. Every `docs/bench/*.json` `commit` resolves with `git cat-file -t` against `origin/main`
   history (CI unshallows for this).
2. Reintroducing `707e1af` / `12ba1ad` / `ffb49a8` (the three README-facing ghosts) fails that
   check without needing git.
3. Existing artefacts are rewritten in the same PR as the gate; the gate does not ship green
   over a file it cannot defend.

## Explicitly not in this sprint

- **RFC-0040**, the freshness dial. Design, freeze.
- **#760**, the `[[calls]]` volume bound recorded as shipped. Capability. Park.
- **#750**, the Lodestar VPS. Ops. Swap 2.7.1 on the box.
- **#649 / #638 / #305**, Lodestar product. Not this theme.
- **#716 / #710 / #715**, gates that do not gate. Real, next-but-one, do not grow this set.
- **#286**, the 2 GB budget under a hostile ABI. A live run, not four tickets.
- **#790 / #789**, predictions lockfile and a flake.
- **#763 / #776**, stale RFC prose and an obib reproduction command. File if they fall out.
- **Anything labelled `parked`.**
- **pragmatic-peregrine's four.** They close on their own PR. Do not restack this on that branch.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** A label is not approval to grow the set. Discovered work is filed
   unlabelled.
2. **`Reviewed-by:` names the party who read the diff.** No proxy signatures.
3. **Acceptance is above.** Build against it, do not rediscover it in review.

Also standing: one worktree per run; never `git add -A`; do not `@`-mention Rowan in GitHub
markdown; `CFLAGS=-std=gnu17` on the Linux box; one merge per CI cycle.

## Context at filing

v2.7.1 is what `curl | sh` installs. Peregrine is in review on #804. The four above were already
open; #289 was named as next-but-one when peregrine started.
