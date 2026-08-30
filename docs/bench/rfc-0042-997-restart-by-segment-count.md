# #997: restart-to-ready against segment count

Measured 2026-08-31 on the MacBook (arm64, load ~2.5 at start), in-process, medians of 5 restarts
per size. Harness: `tests/bench_restart_to_ready.rs::restart_to_ready_against_segment_count`,
`cargo test --test bench_restart_to_ready -- --ignored --nocapture`.

## Why this exists

#992 measured restart-to-ready at 10, 100 and 500 **blocks** and found roughly 10% growth for 50x
the data - at that scale, a constant. §11's row carried **74.0 ms**, and slice 5's decision input
carried it too.

**Block count was the wrong variable.** `horizon-nest` holds **10,923 sealed segments** (#889), and
both #964 and #987 found segment count dominating everything else at a realistic layout - #964 saw
the same rows go from 37 ms to 856 ms purely by splitting them across 10,000 files. A warm restart
attaches those segments. #992's harness could not reach that: its tape source builds a small chain,
and 500 blocks of one transfer each seal almost nothing.

## Measured

| segments | restart-to-ready | vs 0 segments |
| ---: | ---: | ---: |
| 0 | 49.6 ms | 1.00x |
| 100 | 66.5 ms | 1.34x |
| 1,000 | 196.0 ms | 3.95x |
| 5,000 | 801.1 ms | 16.14x |
| **11,000** | **1.7 s** | **34.19x** |

11,000 brackets `horizon-nest`'s real 10,923.

## What this means for the published figure

**Restart-to-ready at a production segment count is ~1.7 s, not 74 ms.** The 74 ms figure was never
wrong - it is what a 500-block fixture does - but it was quoted for a property it does not have.
Growth is roughly linear in segment count and there is no knee: 100 segments already costs 34% over
empty.

Against RFC-0042, this does not change §14's decision. It sharpens one row: the DuckDB baseline for
restart-to-ready is a **segment-count curve**, not a scalar, and any future candidate must be
compared against the curve. §14 already recorded the figure as not extrapolating; this replaces that
caveat with numbers.

## What this does not measure

- **In-process.** `spawn_nest` to rebuilt view. Process spawn and the HTTP `/ready` transition are
  excluded, because the tape source is test-only. Real restart-to-ready is this plus process start.
- **Three rows per segment**, written directly with `seal::seal_range` - the shape a tip-following
  nest accumulates, since `seal_finalized` has no batch threshold and seals whatever finalised.
  `docs/bench/segment-layout.md` measured the real distribution at a 6.3 KB median with 80% under
  20 KB, so these are at the small end. **Treat 1.7 s as a floor for the attach cost**, not a
  ceiling: segments carrying more rows will not be faster.
- **One machine**, and a laptop rather than the Hetzner box that serves production. The shape is the
  finding; the absolute numbers are this machine's.
- **Not a regression gate.** It is `#[ignore]`d for the same reason #992's is: a timing inside the
  CI-critical suite is flaky under contention and would let a slow box fail a test about
  determinism.
