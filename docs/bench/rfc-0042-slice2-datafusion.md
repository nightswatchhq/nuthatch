# RFC-0042 slice 2: the DataFusion gate, re-run (#956)

Measured 2026-08-29 on Apple Silicon. **DataFusion 55.0.0 / Arrow 59.2.0** against **DuckDB 1.10501.0
bundled**, over the same query, same fixture and same spike RFC-0013 used on 2026-08-02.

The spike was preserved as source at `docs/bench/rfc-0013-datafusion-gate.rs` and is now a standalone
crate at `tools/df-gate`, **deliberately outside nuthatch's dependency graph**: putting DataFusion in
`Cargo.toml` would place it in the release graph, in `deny.toml`, and in every clean-build timing slice
0 established - contaminating the numbers this RFC rests on.

## The version matters, and nearly did not

`datafusion = "54"` resolves to **54.1.0 - the exact version RFC-0013 measured.** Running that would
have produced a ratio and taught us nothing about the engine having moved, while looking like a
re-measurement. Pinned to `"55"` with the reason in the manifest.

RFC-0013 used DF 54.1.0 with Arrow 58.3.0; DF 55 moved to Arrow 59. The **fixture writer stays on Arrow
58**, which is what nuthatch actually seals - the point of the gate is that DataFusion reads our
segments, not a file written to suit it.

## Result

| rows | RFC-0013 (DF 54.1) | now (DF 55.0) | change |
| ---: | ---: | ---: | --- |
| 2 M | 1.85x | **0.84x** | **inverted - DataFusion is now faster** |
| 8 M | 2.57x | 2.38-2.78x | unchanged |
| 20 M | 2.65x | 2.56-2.60x | unchanged |

Both orderings run at every size; the ratios agree within noise, so this is about the engines and not
about who warmed the page cache. **Parity identical at every size** - 509, 506 and 508 addresses - which
is the acceptance criterion before any timing counts.

Five repeats at 2 M: `0.94, 0.84, 0.84, 0.84, 0.81`. Median **0.84x**, DuckDB flat at 31 ms every run.

### Both engines got faster, by almost exactly the same factor

| 20 M | 2026-08-02 | now | speedup |
| --- | ---: | ---: | ---: |
| DuckDB | 229 ms | 103 ms | 2.2x |
| DataFusion | 606 ms | 268 ms | 2.3x |

That is the reason the large-size ratio is unchanged: a year of both projects' work, and neither pulled
ahead. **RFC-0013's core finding survives** - the gap widens with size, and at the sizes it measured,
DataFusion is still ~2.6x slower.

## What is genuinely new

**At 2 M rows DataFusion now wins**, where it lost by 1.85x. The crossover sits somewhere between 2 M
and 8 M rows.

That interacts with RFC-0041 in a way worth stating carefully. Entities removed the *large* scanning
queries - the Lodestar panel went p50 2.15 s to 87.7 ms and stopped scanning raw history. If the
remaining general-SQL surface has moved toward the small end, the engine comparison has moved with it.
**But that is a hypothesis about our workload, not a measurement of it**, and this gate does not test it.

## The limitation that matters most

**This fixture is one segment. A real nest has 10,923.**

#889 measured `horizon-nest`: 80% of segments under 20 KB, a 6.3 KB median, and a file costing
0.14-0.18 ms on a `COUNT(*)`. A thousand-segment table is a different planning and open problem from a
single 20 M-row file, and the two engines may not scale the same way across it. **Nothing here says
which handles many-small-files better**, and on our real layout that could dominate everything above.

That is the next measurement, and it needs #947's decision first, because a comparison over a layout we
are about to change measures a layout we are about to change.

## Not measured

The other five DuckDB roles. This gate covers general SQL only - the admissible function vocabulary,
the AST lowering, the graft engine string and the segment-binding oracle are untouched by a DataFusion
port, and slice 0 §4a records that a port addresses **one of six roles, partially**.

Overflow semantics. Appendix A notes DataFusion does not error on integer overflow by default; this
fixture's values force i128 and the two engines agreed, but agreement on valid data is not the same as
agreeing to *refuse* invalid data. That wants its own case.

Concurrency. Every figure is one client. The concurrent floor (`docs/bench/noise-floor.md`) says the
median stops being the right statistic there.
