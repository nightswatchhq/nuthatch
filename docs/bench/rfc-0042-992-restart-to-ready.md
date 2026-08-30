# RFC-0042 §11: restart-to-ready (#992)

Measured 2026-08-30 on Apple Silicon, in-process, medians of 3, 1 ms poll interval. Harness:
`tests/bench_restart_to_ready.rs`, `#[ignore]`d and run explicitly:

```
cargo test --test bench_restart_to_ready -- --ignored --nocapture
```

## Result

| blocks stored | cold to analytically current | **restart-to-ready** | ratio |
| ---: | ---: | ---: | ---: |
| 10 | 27.2 ms | **67.7 ms** | 2.49 |
| 100 | 27.1 ms | **70.0 ms** | 2.58 |
| 500 | 54.3 ms | **74.4 ms** | 1.37 |

**Restart-to-ready grows about 10% for 50x the stored data** - 67.7 ms to 74.4 ms. At this scale it is
dominated by a constant, not by what is stored. Cold start doubles over the same range, as expected,
because it is re-indexing rather than reconstructing.

## Three measurements were taken before this one, and two were wrong

Recorded because each looked plausible and would have been reportable.

1. **Timing `spawn_nest` and subtracting the cold spawn** gave a reconstruction cost of **zero** - a
   warm spawn is *faster* than a cold one, because the cold path pays to create an empty store, and
   `saturating_sub` clamped the negative to nought. A tidy zero in a results table is exactly the shape
   that gets believed.
2. **Timing spawn-return to view-current** gave a flat **29 µs** at every size, because by the time
   `spawn_nest` returns the reconstruction has already happened inside it.
3. The interval that means anything is **the whole of it**: clock started before `spawn_nest`, stopped
   when the rebuilt view matches what the nest held before shutdown.

The harness now asserts the view settles within 5 ms of `spawn_nest` returning, so if the
reconstruction ever moves *out* of that call the measurement fails loudly instead of quietly measuring
half of it.

## What this does not tell you

**It does not extrapolate to a real nest.** 500 blocks is nothing next to `horizon-nest`'s 10 923
segments, and both #964 and #987 found **segment count dominating everything else** at a realistic
layout. There is no reason to assume startup is exempt, and this harness does not test it - the tape
source builds a small chain, not a large sealed corpus.

**In-process only.** `spawn_nest` to rebuilt view. Process spawn and HTTP `/ready` are excluded because
the tape source is test-only; measuring them here would report something narrower than §11's row names
while appearing to fill it.

**`#[ignore]`d deliberately.** A timing in the CI-critical suite is flaky under contention and
machine-dependent, and would let a slow box fail a test about determinism. `footprint` and `point-read
latency` are separate jobs for the same reason. Unlike those, this has **no ceiling** - it is a recorded
number, not a gate. A gate would need a ceiling derived from a measured mutation, as `point-read`'s was.
