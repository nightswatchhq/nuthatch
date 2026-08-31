# Sprint: sedulous-shrike

## Definition of done

Every issue labelled `sedulous-shrike` closed, and no open PR for one of them.

## The theme

**What a running nest actually costs.**

`prudent-petrel` was entirely inward - eight gates that could not see what they guarded. Two of those
in a row would be navel-gazing while the reference deployment burns money, so this one points
outward.

The number that sets the theme, from #750's board audit of the Lodestar VPS:

> **~11.8M RPC requests. 97 HTTP requests served.** About **122,000 RPC calls per request served.**

Three of the four nests point at `arb1.arbitrum.io`, the free public endpoint our own documentation
calls *"rate-limited, shared, and unsuitable for deep backfills or production"*, at roughly **10.7M
requests a week**. "Be your own indexer" and a 122,000:1 ratio on our own shop window are in tension,
and that tension is a product fact rather than an ops slip.

## What is already known, so the sprint does not re-derive it

**The obvious lever is closed.** #750 left `block_timestamps = false` as the untested candidate. It is
not available for `graph-allocations-nest`: **three of its eight views use `block_timestamp`** - 12
references across `40-lodestar-allocations.sql`, `60-lodestar-disputes.sql` and
`70-lodestar-escrow.sql`. And flipping the flag is a **breaking schema change** (RFC-0029 §6b, and
`config.rs` refuses to start on a mismatch), so it means a full re-index rather than a config edit.

That makes the ratio structural for any nest whose views need timestamps, which is the finding rather
than an obstacle to one.

## The pieces

### 1. #750 - the ratio, and the four things found beside it

`performance tech-debt p1 board-only`. **Measure and fix**, board decision 2026-08-31: the safe items
are done directly, the two that change what users see come back first.

**Do directly:**

- **Upgrade `nuthatch-ds-upstream` (:8110) off 2.5.0**, three releases behind. Upgrades are a
  drop-in binary swap, proven in prod. It exposes no `/metrics`, so its request volume is not even in
  the table - fix that too, or record why it cannot be.
- **Remove the two dead Caddy routes** (`127.0.0.1:8787`, `:8788`). Nothing listens on either.
- **Confirm where the panels are actually served from.** Ports 8095/8096/8098 are loopback-only with
  nothing in front of them; Caddy routes to 8090/8099/8787/8788 and nginx only to 8100. That may be
  deliberate, but "may be" is not a state to leave a production deployment in.

**Measure, then propose - do not change:**

- **The endpoint.** Moving three nests off `arb1.arbitrum.io` means moving them *onto* something, and
  the only paid endpoint on that box is the Alchemy key `horizon-nest` uses for state RPC. At ~10.7M
  requests a week that is real money, so the sprint produces a costed proposal, not a switch.
- **A fresh post-stop measurement**, so the ratio is a current number rather than an August one.

**Board call, explicitly not mine:**

- **`doudouchain-v2-nest`**, named *temporary*, 2.78M requests in five days to serve 28, and publicly
  reachable through nginx. Stopping it is a product decision.
- **Any `block_timestamps` change**, given the view dependency above and the re-index it implies.

Standing rules for this box: `root@89.167.109.4`, `hetzner_drpc` key, **three** services (one not
named `nuthatch*`), `Restart=always`, and **never `pkill`**.

### 2. #1006 - the concurrency ceiling, measured where it is enforced

`performance verification p2`. Held back from `prudent-petrel` deliberately, for the reason that
still applies: the knee was measured on a 32-core ThinkPad and the constraint that binds is the
**per-cursor RAM budget**, which the ThinkPad does not enforce.

`SQL_MAX_CONCURRENCY = 2` is a memory bound nobody chose as one. The knee is nearer **8**, worth
roughly **4.8x** throughput, and unbounded at 32 clients reached **1,313 MB - 64% of one cursor's
entire 2 GB**, shared across every nest on that cursor.

Same box as #750, same visit. Re-measure the throughput/RSS curve at 1/2/4/8/16 permits, state it
against the **per-cursor** budget at N=1 and at a realistic multi-nest N, and either raise the default
with the curve recorded beside it or keep 2 and document *why* with the number. The current value has
neither. Whatever is chosen, the `footprint` CI gate must be the thing that catches a regression, not
a comment.

### 3. #1011 and #1013 - two published figures rest on a harness that misreports

`rfc performance verification p1`. **In scope because RFC-0042 §14 cites their output**, not as
tooling upkeep. Both verified against the code before this sprint was written.

**#1011** - `view_exec.rs` runs its DataFusion repeats as
`if let Ok(df) = … { if df.collect().await.is_ok() { push(elapsed) } }`. A failed repeat is silently
dropped and the median taken over whatever survived; four failures of five yields a one-sample
"median". `REPEATS` defaults to 5 and `.max(1)` permits one. **This is the same defect as #977**,
which `prudent-petrel` fixed in `noise-floor.sh` - a benchmark discarding failures and publishing the
survivors. It produced #996's `0.81-1.64x`, cited in §14's regressions 2 and 3.

**#1013** - `view_dialect.rs` takes text from the first `SELECT` to the file's final semicolon.
`50-lodestar-epochs.sql` contains **two** `CREATE VIEW` statements, so both parsers receive
`SELECT …; CREATE VIEW … AS SELECT …` as one supposed view body. The published per-view parse count
does not establish what it claims. Extract each `CREATE VIEW` body independently and re-run.

**Done when** both are fixed *and* the affected figures are re-taken or explicitly marked as
unreproduced in §14. A corrected harness that leaves a stale number standing has fixed nothing.

### 4. #1012 and #1014 - retire and narrow, rather than restore and normalise

`rfc performance verification p1` / `verification tech-debt p2`. Both small, and both resolved by
saying less rather than building more.

**#1012** - the `CONCURRENCY`/`PER_CLIENT` branch was deleted from `tools/df-gate/src/main.rs`, so
those variables are now silently ignored and a requested concurrency run returns single-client
evidence. **Retire it loudly**: reject the variables with an error naming §14, which withdrew that row
anyway. Restoring a benchmark for a withdrawn row would be building for nobody.

**#1014** - a comment claims the specialised fold's errors match DuckDB's wording and quotes
`Out of Range Error: Overflow in addition of INT128`. The Rust path produces different negation,
accumulation and merge messages. **Narrow the claim** to matching failure *semantics*. One line, and
it is the same class as the doc comment that put a false serialisation property into three published
documents.

## What is deliberately not here

**#638, the Lodestar migration.** Board decision 2026-08-31: it stays with Chief and chris. The issue
says it plainly - 2 of 39 routes, static for a month, and *"the gap is attention, not capability"*.
That is not solved by writing more nuthatch, and putting it in a sprint would misrepresent it as
engineering.

**RFC-0042 itself.** Parked, carve-out spent, freeze in full. The four issues above are corrections to
published evidence, which §14 explicitly names as work the park does not block. They are not a slice
and they are not a reopening. If #357 is ever scheduled, RFC-0042 reopens **before** it.

**The parked and frozen backlog**, sixteen issues, reviewed only to confirm it is correctly parked.

## A standing note

`prudent-petrel` ended with *a fix to a gate is not done until the gate has been shown to fail*. The
equivalent here: **a measurement of production is not done until it has been taken twice** - once
before a change and once after - because every number in #750 is a rate, and a rate quoted without
its window is the mistake this project keeps making. Never extrapolate one.
