# Sprint: steady-starling

Filed 2026-08-24, after rigorous-raven landed on main as #806. **Three issues.**
Runs **Sunday 2026-09-14 to Sunday 2026-09-21**, or the Monday after quizzical-quail's four close,
whichever is later. Implementation may land earlier; the dates are the done-by.

## Definition of done

Every issue carrying the **`steady-starling`** label is closed, and no open PR is for one of
them. That is three issues: #744, #763, #776. Work discovered in flight is filed **unlabelled**.
Pulling it into scope needs a board reply.

## The theme

**An explanation is not a measurement, and a command that no longer reproduces its artefact is
the 289 ev/s failure mode with a README around it.**

Nightjar built the tape. Raven made the checks that cannot fail stop pretending. What is still
false on the *published number*, and on the *page a reader follows*:

- #744 item 2 is explained (the network was 99.3% of the live wall clock) and still unmeasured.
  The seal-direct arm aborted on a recorded 429. There is no storage-path ratio from a clean tape.
- RFC-0038 §6e still refuses `--seal-direct` with `[[calls]]` in the present tense, and still
  quotes the 5.5x cost of that refusal as if it were advice. The guard was removed in 2.7.0.
- obib-case6's README still runs `--seal-direct` alone. After #758 that is the fixed-window arm,
  and the artefact it points at is adaptive. The command parses. It measures something else.

Freeze-legal throughout: verification, documentation. Not RFC-0040.

## The three

### 1. #744 - the storage-path question, still unanswered

**The number.** Item 1 (11,758 vs 12,933) is two nests, two workloads, both correct. Item 2 is
*explained* as unanswerable through the network, and still unmeasured on the tape. #767's
acceptance criterion 4 asked for the seal-direct-versus-hot-store ratio on the rig, with
whatever number comes out published. "If it is 0.92x, we publish that. If it is 8x, we publish
that. Not knowing is not."

The existing `usdc-120-fixed` tape preserves recorded 429s on purpose. That is how #784 was
found. It is not the tape this measurement can replay: the seal-direct arm aborts on the error,
which is the mechanism working, and also means the comparison did not run.

**Acceptance**

1. A second tape of the same USDC 120-block Transfer-only range, recorded against an endpoint
   whose timestamp batches all succeed, is committed under `docs/bench/tapes/`. The 429-bearing
   tape stays; it is the #784 reproduction.
2. Both arms (hot-store and seal-direct, same window policy) replay from that tape, five-run
   median, work directory on real disk (`--keep`, not tmpfs).
3. The ratio is published in `docs/benchmarks.md` with artefacts. Whatever it is. The page
   stops saying the storage-path comparison is still a question for the rig.
4. A test fails if the clean tape's `entries.jsonl` contains a recorded error. Reintroducing a
   429 into that file fails the build; the 429 tape is out of scope of that test.

### 2. #763 - RFC-0038 §6e still points at the slow path

**The advice.** §6e is present tense: slice 1 refuses `--seal-direct` alongside declared
`[[calls]]`, the guard is right, and the cost is 5.5x slower on the hot path. The guard was
deliberately removed once the seal-direct paths learned to resolve calls (#657, 2.7.0).
`src/indexer.rs` says so. A reader planning a `[[calls]]` backfill is sent down the slow path
for no reason.

Do not delete the section. The refusal was correct when written. The follow-up it named was
built. The silently-absent-table reasoning aged well. Rewrite it as history.

Do not quote a new multiplier in the RFC without an artefact. Point at `docs/benchmarks.md`.

**Acceptance**

1. §6e is past tense: the refusal existed, it was right, it is gone, current advice is the
   opposite of what the section used to say.
2. The 5.5x figure is labelled as the cost of the *refusal*, not of the combination, and is
   not current advice.
3. No new wall-clock ratio is introduced in that section without a committed `docs/bench/*.json`.

### 3. #776 - obib-case6's published command no longer reproduces its artefact

**The command.** `docs/bench/obib-case6.json` records `seal_direct: true`, `concurrency: 1`,
`window_adaptive: true`. After #758, `--seal-direct` on its own is the fixed-window arm.
The README in `nightswatchhq/obib-case6` still runs `--seal-direct` without `--window-adaptive`.
The number this puts at risk is **16 RPC requests**, quoted as the invariant measure of range
control.

`--window-adaptive` shipped in 2.7.1. The ordering trap in the issue (flag missing from 2.7.0)
is closed. The command can name the flag and the version together.

**Acceptance**

1. `nightswatchhq/obib-case6` README uses `--seal-direct --window-adaptive`.
2. It names nuthatch >= 2.7.1 as the binary the command needs.
3. The in-repo `docs/benchmarks.md` reproduction line, if any still says `--seal-direct` alone
   for this artefact, matches.
4. The upstream Sentio PR ([sentioxyz/open-blockchain-indexer-benchmark#3](https://github.com/sentioxyz/open-blockchain-indexer-benchmark/pull/3)) is either updated or explicitly left
   with a comment saying why not. It does not stay silently stale.

## Explicitly not in this sprint

- **RFC-0040**, the freshness dial. Design, freeze.
- **quizzical-quail's four** (#289, #781, #755, #756). They close on their own PR. Do not
  restack this on that branch. #781's disk-backed default is not a prerequisite: `--keep` is
  already the way to put a measured store on real disk.
- **#750**, the Lodestar VPS still on 2.5.0. Ops.
- **#649 / #638 / #305**, Lodestar product.
- **#286**, the 2 GB budget under a hostile ABI. A live run, not three tickets.
- **#751**, `BenchReport` carrying the declared event set. Real, related to #744 item 1, p2.
  File if it falls out.
- **#760**, the `[[calls]]` volume bound recorded as shipped. Capability. Park.
- **Anything labelled `parked`.**

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** A label is not approval to grow the set. Discovered work is filed
   unlabelled.
2. **`Reviewed-by:` names the party who read the diff.** No proxy signatures.
3. **Acceptance is above.** Build against it, do not rediscover it in review.

Also standing: one worktree per run; never `git add -A`; do not `@`-mention Rowan in GitHub
markdown; `CFLAGS=-std=gnu17` on the Linux box; one merge per CI cycle.

## Context at filing

v2.7.1 is what `curl | sh` installs. Raven is on main (`9b3bb6f`). Quail is still in review
on #805. The three above were already open; #744 was named as riding on nightjar's rig and
the rig has been there since #785.
