# Sprint: mighty-moorhen

**Eleven issues.** A sprint is a labelled set, not a calendar. This is deliberately the large cleanup
sprint after the backlog reconciliation: product correctness, measurement, documentation, and two
external decisions which need to stay visible rather than being mistaken for code tasks.

## Definition of done

Every issue carrying the **`mighty-moorhen`** label is closed, and no open PR is for one of them.
That is #296, #750, #790, #814, #815, #1006, #1025, #1026, #1027, #1028 and **#1046**. Work
discovered in flight is filed **unlabelled**. Pulling it into scope needs a board reply.

**#1046 was pulled in by board reply, 2026-08-31**, having been filed unlabelled while measuring
#296. It is the finding #296 turned into: the hot store's RSS is governed by redb's cache size,
which nuthatch has never set, and not by the row encoding. It belongs to this sprint's theme -
*the numbers we quote about the running product* - and it is performance rather than capability,
so the freeze permits it.

## The theme

**The running product, the numbers we quote about it, and the small controls that make both
believable.**

This is not a capability sprint. Compact rows and SQL concurrency are performance work on shipped
paths; read-only readiness and tip-lag are existing operational surfaces; the ABI, BOM and metric
checks make stated facts remain true. The Lodestar host and keyed AI evaluations are explicitly
external tracks, not work to be faked from a repository checkout.

## The eleven

1. **#296 - compact binary rows.** ~~Measure the hot-store cost and choose a versioned migration or
   an explicit rebuild-on-upgrade contract.~~ **Closed 2026-08-31: neither.** The prototype
   (#1045) falsified the premise - RSS tracks redb's cache setting, not the file, and the two
   encodings land on identical RSS at every cache size below both. The no-resync promise was not
   traded away because there was nothing to trade it for. The saving became #1046; the encoding's
   surviving 3.4x decode win is a latency argument and was not carried forward under this issue.
2. **#750 - production follow-up.** Finish the fresh post-stop accounting and remaining host cleanup.
   Board-only: the VPS and credentials remain outside the firm.
3. **#790 - tyre-kicking pass.** A fresh operator runs the registered product predictions blind and
   files one issue per actual finding.
4. **#814 - COR-6 and COR-8.** Decide the reserved-column refusal/namespace rule and the honest
   treatment of values beyond `i128`.
5. **#815 - keyed AI evaluation.** Board-only: run the two evaluations without retaining keys in the
   firm, and record the result or the decision not to run them.
6. **#1006 - SQL concurrency.** Measure the throughput/RSS curve on the enforcing production box,
   then set or retain the bound with a per-cursor explanation and a gate that can fail.
7. **#1025 - read-only readiness.** A sealed-history nest must be ready by its sealed state, not by
   a poll it intentionally never performs.
8. **#1026 - ABI-floor assertion.** Bind the documented runtime requirement to the supported-platform
   floor rather than merely searching for nearby words.
9. **#1027 - Cargo timing discovery.** Recognise the normal unsuffixed `cargo-timing.html` report.
10. **#1028 - rendered tip lag.** Exercise the per-nest Prometheus value the operator actually sees.
11. **#1046 - redb's unset cache ceiling.** Make the cache size settable (default unchanged, a
    no-op), then measure RSS, point-read p50/p99 and tip lag on the enforcing production box at
    1 GiB / 512 / 256 MiB and pick a default from that. **The harness figures must not be used to
    pick the value**: both boxes had the store in OS page cache, so a miss cost a memcpy rather
    than a disk read.

## Explicitly not in this sprint

- Every `frozen` issue. The 2026 freeze remains intact.
- New engine, chain, extraction, or AI capability.
- New findings discovered while doing these ten, unless the board adds them explicitly.

## How this sprint runs

Standing rule: one issue per PR closure keyword, no `git add -A`, and verification names the actual
surface a user or operator reaches. A green internal getter is not evidence about an exported gauge;
a number from a convenient laptop is not a production ceiling.
