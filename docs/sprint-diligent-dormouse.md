# Sprint: diligent-dormouse

Membership is defined by the `diligent-dormouse` label on GitHub, not
by this file. If the two disagree, the label is right and this file is stale.

Follows [circumspect-capybara](sprint-circumspect-capybara.md), which closed 13 of 13 and shipped
[v2.1.0](https://github.com/nightswatchhq/nuthatch/releases/tag/v2.1.0).

## Definition of done

Unchanged, and both halves matter:

1. **The Linux dev box and the Hetzner box.** CI green proves neither. An issue is not finished
   until the behaviour has been observed on hardware.
2. **A mutation, stated in the PR.** Say what you broke and what failed when you broke it. A
   criterion that still passes with the mechanism removed proves nothing, and this project has
   shipped that mistake more than once - most recently four gates in one sprint that could not fail.

## The theme, and why this scope

Last sprint built the gates. It also proved that four of them could not fail, which the firm found
itself, after shipping them.

This sprint is about **the answer being wrong rather than absent** - the failure mode where nothing
errors, nothing is red, and the number served is simply not true. Six of the thirteen are that
shape. It is the hardest class to find and the only one a green suite cannot rule out.

## How this is ordered

Same ranking as last time, and it is worth restating because it decides everything below: a live
defect, then a CLAUDE.md non-negotiable at risk, then a published claim that is unproven, then cost
against leverage.

### 1. A live defect

**#432 - a contract-free nest asks the node for every log on the chain.** `p0`. An `eth_getLogs`
with an empty address *and* empty topic filter is not "no logs", it is the whole chain. #429 fixed
two of the four paths; the **live tip loops** are the two it did not. This is OBIB case 3 - a nest
with `blocks = true` and no contracts - so it is a shipped configuration, not a hypothetical.

### 2. Cold data that vanishes or lies

One cluster, four issues, all found while reviewing #430. Together they are the sprint's centre of
gravity, because every one of them is a *wrong answer served confidently*.

- **#434** - a big-int column declared in `schema.json` but carried by no sealed segment **deletes
  the table** from `/sql`. Same user-visible symptom as #419, entirely different cause, no corrupt
  file involved.
- **#433** - a page-corrupt segment whose footer is intact fails the *whole* query with
  `don't know what type:`. #430 handles the footer-corrupt half; this is the other half of the class.
- **#435** - when cold data *is* reduced, `/sql` tells the caller nothing. The query succeeds and
  the answer is quietly incomplete. Absent data rendering as healthy is the house failure mode.
- **#413** - `nuthatch sql` creates the store it is probing for, then queries the empty thing it just
  made. A probe that answers its own question.

### 3. Non-negotiables at risk

- **#289** - DuckDB `allowed_directories` is not enforced on the build we ship. The sandbox we
  describe is not the sandbox that runs.
- **#400** - the nest mount API's two guards have no test, neither the `admin_enabled` check nor the
  token. Mounting is full control of what the runtime serves.
- **#393** - the per-cursor RSS projection is ~13x pessimistic and it gates a **refusal**. A wrong
  estimator does not merely mis-report; it declines mounts that would have fit, which makes the
  ≤2 GB budget read as stricter than it is.

### 4. Published claims that are unproven

- **#291** - reorg property tests at depths we have not tried. CLAUDE.md names these as a correctness
  rule; the depths we actually exercise are narrower than the claim.
- **#424** - the point-read gate measures a 256-row hot store, not a realistic nest. It is the one
  condition of #283's design call the shipped gate does not meet.
- **#385** - `docs/bench/point-read.json` still records the dev box's numbers as the baseline for a
  runner-enforced gate. The ceiling was corrected; the committed artifact was not.
- **#415** - adoption's "holds data" fixtures are unreadable stores rather than stores with data, so
  the tests pass without exercising the thing they name.
- **#414** - a nest hot-mounted into a running runtime never gets the early cutoff, so it re-indexes
  data it already has.

## Not in scope, deliberately

**Left for outside contributors.** `good first issue`: #376, #377, #383, #409. `help wanted` covers
another ten. Two external contributors landed work last sprint - #421 and #440 - and the lane stays
open. **If one of these arrives as a PR, we thank them, we fix what is missing ourselves, and we
merge under their name.** We do not send a contributor away to do mechanics.

**Board-only.** #441 (v2.1.0 is unverified on the production box) and #428 (the live `/nest`
disclosure probe). Both need credentials and machine access the firm must never hold. Agents must
not pick these up; if one is assigned, that is the board's mistake.

**Parked.** Thirteen RFC-scoped items remain parked with reasons on each. Parked is a decision, not
a backlog.

## The shape of it

Thirteen issues, one week, and the previous sprint suggests that is not the binding constraint - the
firm closed thirteen in about a day and a half of wall time. What took the rest of the time was
everything around the work: gates that could not fail, a review convention the forge cannot enforce,
and a board that was slower than the team it was directing.

So the honest expectation is that the code lands early and the sprint is decided by whether the
**mutations** are real. Last sprint the firm caught its own four; the measure of this one is whether
that happens without the board asking.
