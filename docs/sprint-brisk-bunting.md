# Sprint: brisk-bunting

**Scoped 2026-10-05. Starts now: agile-avocet is closed bar #1851, whose PR Chief opens.**

**Nothing in production depends on someone remembering to run it.**

agile-avocet made the roll obey its gates and made float answers reproducible. Running the
product since has turned up the jobs that are still done by hand or assumed rather than measured:

- **The epoch table is hand-run.** The allocations nest's exact epoch starts come from a script run
  by hand; its table reached epoch 1401 on 2026-10-05, and parity's signalled_tokens gate fails the
  day after a missed run. 4.6.0 shipped the means to derive them (`[extract] l1_blocks`), but only on
  a fresh index.
- **Reproducibility is claimed, not shown.** Exact float sums shipped, yet nobody has compared two
  fresh servers' bytes on the real nests, and the gate still rounds floats.
- **Tip lag is unmeasured**, though CLAUDE.md lists it.
- **A newcomer's first run** waits 20 s on Linux probing keyless endpoints, and the installer still
  points at the example the README moved away from.
- **Exact sums cost QoS 7 to 17%** over 4.5.0.

## Definition of done

Every issue labelled **`brisk-bunting`** in `nightswatchhq/nuthatch` and `nightswatchhq/burrmill` is
closed, with no open PR for one. The allocations nest runs with no hand-maintained table. Two fresh
servers agree byte for byte on every production gate set.

## Order

1. **The allocations nest derives its epochs: #1882.** A fresh index with `l1_blocks`, staged beside
   production on Helsinki, swapped with a rollback path; then the table and its script go. Until it
   lands, extending the table is a daily job with an owner.
2. **Reproducibility shown: #1883.** Two fresh servers per nest, bytes compared; the gate's float
   tolerance and the float volatile tags come off.
3. **Tip lag in CI: #1884.** Measured on every PR; gated or not by its noise, as #1723 decided
   throughput.
4. **The first run: #1885, #1886.** Parallel endpoint probing; the installer and site at 4.7.0 and
   pointing at the README's example.
5. **Engine cost: #1887, burrmill#79, burrmill#7, burrmill#6.** The exact sum's overhead on QoS, the
   all-match join filter, the mangled missing-column message, build time and arrow's unused features.
6. **Release**, gated and rolled by the roll that checks its gates.

## Alongside, not in the definition of done

- **#1851**, the mirror's provisional segments: done on `pete/avocet-mirror-provisional`; Chief opens
  the PR.
- **The Balancer archive nest** (GTM), when Chief picks it up.

## Rules

As before: a gate is done when it has failed; operations work is committed; every fix lands with its
missing test, red first and mutation-checked; a memory figure is four Linux runs; a wrong answer
outranks a refusal; a check that needs a person to run it is not finished. Added: **a script that
edits a production unit prechecks every source the binary reads and restores the unit itself if it
does not come back** (the 2026-10-04 audit outage).
