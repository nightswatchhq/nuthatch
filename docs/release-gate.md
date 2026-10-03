# The release gate

A release candidate is run against a copy of the Lodestar allocations nest, with the queries Lodestar
and kittiwake actually send it, under the budget that nest runs on in production. A refusal, an
error, an out-of-memory or a time regression past a stated bound turns the candidate red. Red stops
the roll to production; it does not block the tag or the release (Chief, 2026-10-03). Issue #1749.

It exists because of 4.1.1. On 2026-10-03 it reached the Lodestar box and its allocations nest
refused the dashboard's join-heavy views within minutes (`HashJoinInput` out of memory under a 2 GB
budget). Every CI gate had passed: the footprint jobs index synthetic data and run no authored view.

## The pieces

| File | What it is |
|---|---|
| `scripts/gate/alloc-queries.tsv` | The query set: 74 statements, one per line, each with its consumer and call site. |
| `scripts/gate/collect-queries.sh`, `scripts/gate/collect/main.rs` | How the set is generated from kittiwake's source. |
| `scripts/release-gate.sh` | Serves a nest copy with one binary, runs the set, gives the verdict. |
| `scripts/gate/baseline-4.2.0.tsv` | The proof run's baseline: 4.2.0 on the 2026-10-03 copy, on a MacBook. |
| `scripts/release-gate-run.sh` | The ThinkPad's job: fetch the candidate, gate it, post the status. |

## The query set

Lodestar sends nothing to the nest itself: its dashboard reads kittiwake's API, and kittiwake is the
only thing that opens a socket to a nest (`crates/nest`). So the set is every statement kittiwake
sends to the allocations nest (`NestId::Alloc`), labelled `lodestar` where it serves a dashboard
route and `kittiwake` where it is one of kittiwake's own jobs (the directory refresh, the ingest
crons, the live feed, RAV collection, QoS scoring). 57 are Lodestar's, 15 kittiwake's, and 2 are
asked by both.

Parameters are representative literals pinned to 2026-10-03, so the set is the same file however
often it runs. Addresses and deployments are the heaviest on the nest (the largest indexer on every
axis, which kittiwake's own tests use as the worst case); windows and page sizes are the edge
handlers' defaults and the warmer's. Each statement appears once: the nest memoises answers by
statement text, so a duplicate would time the memo.

It is refreshed on purpose, not by drift:

    scripts/gate/collect-queries.sh ~/Projects/kittiwake > scripts/gate/alloc-queries.tsv
    git diff scripts/gate/alloc-queries.tsv

`collect-queries.sh` compiles `collect/main.rs` against a copy of kittiwake's
`crates/read/src/sql.rs` with plain `rustc`, so every statement built there regenerates itself. The
handful written inline at a call site are copied in `main.rs` and marked `inline`; on a refresh,
re-read those call sites and any new ones:

    grep -rn 'NestId::Alloc' ~/Projects/kittiwake/crates --include='*.rs' | grep -v /tests/

Other consumers may join later; the set is Lodestar's and kittiwake's first.

## Running the gate

    scripts/release-gate.sh [--baseline F] [--write-baseline F] [--passes N] <binary> <nest-copy> <set>

The copy needs its sealed segments **and** a copy of its `nuthatch.redb`: without the redb it serves
no sealed history. The script starts `nuthatch serve` on the copy under production's environment,
which is factored into the script's `PROD_ENV` and copied from the allocations nest's unit on the
Lodestar box (port 8107):

    NUTHATCH_SQL_MAX_CONCURRENCY=2  NUTHATCH_ANALYTICS_MEMORY_LIMIT=256MB  NUTHATCH_ENGINE=burrmill
    NUTHATCH_BURRMILL_MEMORY_LIMIT=2GB  NUTHATCH_ANALYTICS_THREADS=8  NUTHATCH_MAX_RSS=6GB

When that unit's environment changes, change `PROD_ENV` with it, or the gate tests a budget nobody
runs. Each pass starts a fresh server, because of the memo; a query's status is its worst pass and
its time the median of its passes. For each query it records status, rows and time.

It fails (exit 1) on:

- any error or refusal from `/sql`, an out-of-memory, or a degraded answer;
- the server dying under a query;
- with `--baseline`, a query slower than **2x its baseline and more than 1000 ms slower**, or the
  set's p99 slower than **1.5x the baseline's and more than 1000 ms slower**. Both halves must hold,
  so a 40 ms query taking 90 ms is noise. The bounds are `GATE_QUERY_FACTOR`, `GATE_QUERY_SLACK_MS`,
  `GATE_P99_FACTOR` and `GATE_P99_SLACK_MS`.

Row counts are reported against the baseline but never fail: a refreshed copy holds more rows. Exit
2 is a broken rig (no redb, a port, a binary that will not start), not a verdict on the binary.

`tests/release_gate_script.rs` runs the script against a nest sealed from the fixture chain: a
refused query fails and is named, an answered set passes and writes a baseline, and a regression
fails past the bound and passes inside it.

## Where it runs: the ThinkPad

The copy is about 700 MB and the gate wants a quiet box for its timings, so it does not fit a GitHub
runner. It runs on the ThinkPad, which already runs the QoS nest; the gate is scheduled away from
that nest's busy periods. Every gate run against a copy, the timer's or one by hand with
`release-gate.sh`, takes the same lock beside it (`alloc-nest.gate-lock`; flock on Linux, a mkdir
lock elsewhere) and waits for any other: two `serve`s cannot open one redb.

**The copy** lives at `~/release-gate/alloc-nest` and is refreshed from the Lodestar box in Helsinki
before each run, through the `GATE_REFRESH` hook, `deploy/release-gate/refresh-from-helsinki.sh`
(#1774). It has two halves:

- On Helsinki, `deploy/release-gate/helsinki/gate-export.sh` is the forced command of the
  ThinkPad's key (`~/.ssh/nuthatch-gate`) in root's `authorized_keys`, accepted from the ThinkPad's
  tailnet address only. It answers `snapshot` and a read-only rsync of its stage
  (`/var/lib/nuthatch-gate/stage`), and refuses anything else.
- `snapshot` stages the nest directory: sealed segments hardlinked (they are immutable), the
  config and views copied, and a copy of `nuthatch.redb`. The redb is open in the unit and redb has
  no online backup, but a store nothing is writing is a byte image of its last commit, because every
  commit is fsynced. The nest writes only during an ingest cycle, so the copy is checked rather than
  timed: the redb and `segments/manifest.json` are hashed before and after, the copy is hashed, and
  `/ready`'s `last_block` is read on both sides. Any difference means a commit or a seal landed
  during the copy, and the copy is taken again, up to ten times. It ends with a `PROVENANCE` file
  carrying the version, `last_block`, `sealed_through` and the redb's sha256. Nothing is stopped.
- The ThinkPad pulls the stage with rsync into `alloc-nest.incoming`, hardlinking unchanged
  segments from the current copy. It swaps the new copy in only when the pulled `PROVENANCE` is the
  snapshot it asked for, the redb matches the staged sha256, and `sealed_through` has not gone
  backwards. The previous copy is kept as `alloc-nest.prev`. Any failure leaves the copy as it was
  and fails the refresh, which `release-gate-run.sh` posts as `error`. It takes the copy's gate
  lock, so a refresh run by hand waits for a gate that is serving the copy.

A redb that passes those checks and that `serve` still refuses to open stops the gate at startup:
`release-gate.sh` exits 2 when `serve` exits before answering `/health`, and the run posts `error`,
not a verdict. The provenance line of each run (`as_of`, `sealed_through`) shows how fresh the copy
was.

**The trigger** is `scripts/release-gate-run.sh`:

- `--poll`, from a systemd timer every 15 minutes: gates the newest published release or release
  candidate that is newer than production, by the version in its tag, and whose commit carries no
  `release-gate/alloc-nest` status. A release at or below production is never gated, and with
  nothing newer it exits quietly. A `-rc` tag runs the release
  workflow like any `v*` tag and is published as a prerelease with its binaries, so tagging a
  candidate is enough to have it gated within the quarter hour.
- `<tag>` by hand, to gate or re-gate one release.
- `--binary PATH --sha SHA` for a local build, which is how a burrmill rev bump is gated before
  anything is tagged.

For each run it downloads and checksums the candidate's Linux binary
(`nuthatch-x86_64-unknown-linux-gnu.tar.gz`) and the production release's, measures production
first and writes that as the baseline, then gates the candidate against it. The baseline is
therefore the same box, the same copy and the same day, so neither a refreshed copy nor a
different machine reads as a regression. The committed `baseline-4.2.0.tsv` is the proof run's
record, not what the ThinkPad compares against. Production defaults to the latest full release
that is not the candidate; `--production <tag>` names it when that is wrong.

Production failing its own gate is still the baseline, since it is what production does: over
the RSS budget, or refusing statements of its own. Its peak and failures are named in the
candidate's status and output. The candidate's verdict stays its own: it fails on its own
refusals, its regressions against production's times and its own peak, and a statement production
fails cannot regress. Only a production run that could not be measured (exit 2) posts `error`.

**The result** reaches the release as a commit status on the candidate's commit, context
`release-gate/alloc-nest`, posted with `gh api repos/nightswatchhq/nuthatch/statuses/<sha>`:
`pending` when it starts, then `success`, `failure` (with the failed queries named) or `error`
(the gate could not run). The full output stays in `~/release-gate/runs/` on the ThinkPad.

It never blocks the tag: the tag exists before the gate runs, a status on a tagged commit gates
nothing on GitHub, and `release-gate/alloc-nest` must not be added to the required contexts. What
it stops is the roll: read the status before `deploy-nest.sh roll`, and a red one means the
candidate does not go to production.

A run that has posted any final status is not repeated by `--poll`; re-gate by hand with the tag.

The timer is `deploy/release-gate/release-gate.{service,timer}`, a systemd user unit running `--poll` every 15 minutes from `~/nuthatch-ops`, a worktree of the repo on `main`; the unit file carries the install lines. The unit sets `GATE_REFRESH` to the refresh script, so the Helsinki half must be installed before the unit file is copied over; the install lines are in `gate-export.sh`.
