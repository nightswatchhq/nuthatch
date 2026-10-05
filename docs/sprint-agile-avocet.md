# Sprint: agile-avocet

**Closed 2026-10-05.** Every agile-avocet issue is closed except #1851, done on a branch whose PR
Chief opens. 4.6.0 and 4.7.0 shipped; all six production nests run 4.7.0. The README quickstart was
run as written on a clean macOS environment and a clean ubuntu:24.04 container: 6,844 and 6,980 WETH
transfers counted. Next: brisk-bunting.

**Scoped 2026-10-04, not started. Starts when every `zealous-zebra` issue is closed and its release
is cut.**

**Production stays right without someone remembering to check, and a newcomer's first run works.**

The last three sprints built the checks: the release gate, the answer comparison, parity against the subgraph, monitoring and the sealed-history audit. Each has caught a
real defect. Each still depends on a person in the loop at the moment that matters:

- **Nothing enforces the gates.** The roll never reads the gate's status. On 2026-10-04 an agent
  believed it did.
- **The epoch table needs a daily rerun.** Without one, parity's boundary shifts come back.
- **Some floats aren't reproducible.** A view that sums a DOUBLE answers differently from one server
  process to the next, so the gate has to round floats or skip the statement.

Meanwhile the Balancer archive is about to send new people to the README. Its quickstart fails on
stock macOS, and its first example opens with a warning.

## Definition of done

Every issue carrying the **`agile-avocet`** label, in `nightswatchhq/nuthatch` and in
`nightswatchhq/burrmill`, is closed, and no open PR is for one of them. A release whose gate is red
cannot be rolled without a recorded override. The README's quickstart runs as written on a clean
macOS and a clean Linux.

## Order

1. **The roll enforces the gates: #1848.** Before it touches a unit, the roll checks that the release
   commit carries success on `release-gate/<nest>` for every nest. Rolling anyway needs
   an explicit `--override` with a reason.
2. **Derived state maintains itself: #1839.** nuthatch keeps `l1_block_number` on the log-bearing
   blocks a nest already reads, so the allocations nest derives each epoch from the formula and drops
   its hand-run table. Then signalled tokens becomes a parity gate (#1841, held until this lands).
3. **Answers are reproducible: #1849.** Money stays in exact types until presentation, or burrmill
   sums DOUBLE in a fixed order. The gate then compares these statements without a float rule.
4. **A newcomer's first ten minutes: #1842, #1843, #1844, #1828.**
   - #1842: the quickstart works on stock macOS.
   - #1843: the hero example opens with a result, not a warning.
   - #1844: the README's figures are current and sourced.
   - #1828: no stale DuckDB references remain in comments, strings or docs.
5. **The rest of the test-strategy list:** #1723, #1741, #1722, #1724.
   - #1723: the backfill throughput floor becomes a gate, not a number in CLAUDE.md.
   - #1741: a crash kill point mid-move, driven through the binary.
   - #1722: component tests.
   - #1724: `yatr ci` matches CI.
6. **What went red:** the scheduled mutants run (#1837). Also #1787: after a split, a long empty tail
   costs thousands of calls.
7. **Release**, rolled by a roll that checks its gates.

## Alongside, not in the definition of done

- **The Balancer archive nest.** This is a GTM job with its own estimate, waiting on Chief's choice
  of RPC. It is the first outside test of item 4.
- **The sealed-history audit in production (#1786).** It needs a second RPC provider per nest, also
  Chief's choice.

## Rules

As before:

- A gate is done when it has failed.
- Operations work is committed.
- Every fix lands with its missing test, red first and mutation-checked.
- A memory figure is four Linux runs.
- A wrong answer outranks a refusal.

Added:

- **A check that needs a person to run it is not finished.** Say who runs it, and when, or automate
  it.
