# #1006: SQL_MAX_CONCURRENCY, measured where the budget is enforced

Measured 2026-08-31 on the **Hetzner Lodestar VPS** - 4 cores, 7.7 GB, six live production nests -
because that is the surface that enforces the per-cursor RAM budget. The prior figure came from a
32-core ThinkPad, and the whole reason #1006 was held back a sprint was that a ceiling set from the
convenient box is how a ceiling lands in the wrong place.

Binary: `nuthatch 3.0.1` built from `pete/1006-sql-concurrency-knob`, one binary at four settings via
`NUTHATCH_SQL_MAX_CONCURRENCY`. Corpus: a copy of `graph-staking-legacy-history`, **100 MB, 54
sealed segments**. Load: 12 concurrent clients, 12 s per level,
`SELECT COUNT(*) FROM staking_legacy__stake_delegated`.

## Measured

| permits | successful qps | 503s refused | peak RSS | p50 |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 26.1 | 1,678 | 70 MB | 68 ms |
| **2 (shipped)** | **36.2** | **1,256** | **79 MB** | 84 ms |
| 4 | 54.2 | 810 | 93 MB | 98 ms |
| 8 | **81.9** | 157 | **119 MB** | 104 ms |

## The RAM argument is much weaker here than the dev-box figure implied

RFC-0042 §14 recorded **1,313 MB at 32 clients unbounded, 64% of one cursor's 2 GB**, and that number
is why the default was left alone and this work deferred.

On the enforcing box, **8 permits cost 40 MB over the shipped 2** - 119 MB total, about **6% of the
per-cursor budget**, not 64%. The curve is close to linear in permits and nowhere near the ceiling.

**This contradicts the figure that motivated the caution, and that is the point of having measured
it here.** The 1,313 MB reading was 32 clients on 32 cores against a 2 M-row synthetic fixture; it
described that arrangement, not this product on this hardware.

## The finding the throughput column hides

At the shipped **2 permits, 1,256 of 1,690 requests were refused** with `503` - about **74%**.
`serve.rs` acquires the gate with `try_acquire_owned`, so a caller past the limit is refused in
microseconds rather than queued. That is deliberate and it is self-protection, not a fault. But it
means the practical concurrency of the analytical surface under a dozen callers is **two**, and the
other ten get an error rather than a slow answer.

Raising to 8 takes refusals from 1,256 to 157 and throughput from 36.2 to 81.9 qps - **2.26x** - for
40 MB. Not the 4.8x the dev box suggested, and worth having the real number rather than the
extrapolated one.

## What this does not establish

- **Corpus size.** 100 MB and 54 segments. The intended run was against `graph-staking-nest`
  (926 MB); **it could not be done**, and the reason is worth recording rather than hiding: `cp -a`
  of a *live* nest's `redb` produces a corrupt copy (`DB corrupted: Failed to repair database`),
  because the file is being written while it is read. Only a nest that is not indexing can be hot
  copied. Measuring the large nest needs either a stopped service - not acceptable on production - or
  a consistent snapshot, and neither was in scope. **RSS on a larger corpus is unmeasured, and 119 MB
  should not be read as an upper bound.**
- **Client count.** 12, not 32. Refusal counts scale with offered load and are not a constant.
- **Query shape.** A `COUNT(*)` over one table. A heavy analytical query would move RSS more.
- **Cores.** 4. At 8 permits the box is oversubscribed, which is part of why throughput gains taper.

## Recommendation, and what it is not

The evidence supports **raising the default above 2**, and it does not support picking 8 from this
run alone: the RSS headroom is real but measured on a small corpus, and the per-cursor budget is
shared across every nest on the cursor, so a dense multi-nest runtime spends it faster than one nest
does.

The narrow, defensible change is **4** - 1.5x the throughput of the shipped value, 14 MB more, half
the refusals, comfortably inside the budget on the box that enforces it. Going to 8 wants the
large-corpus number first.

**Not changed in this branch.** The default stays 2 and the knob is capped at 16. Changing a
self-protection default is a board decision, and it should be taken against a number rather than
against an argument - which is what this document is for.
