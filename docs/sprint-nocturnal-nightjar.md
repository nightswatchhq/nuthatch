# Sprint: nocturnal-nightjar

Filed by the board on 2026-08-22, after meticulous-magpie closed six of seven in a day.
**Four issues.** Runs **Saturday 2026-08-22 to Friday 2026-08-28** - a week, not a weekend, and the
reason is in *How this sprint runs differently* below.

## Definition of done

Every issue carrying the **`nocturnal-nightjar`** label is closed, and no open PR is for one of them.

## The theme

**Build the machines, not another list of instances.**

Eleven sprints have each been a list of found bugs. The firm is good at it and the supply has not run
out. It will not run out, because the bugs come from a very small number of generators and nothing
stops them:

- a mechanism nothing would notice the absence of
- a number measured on a rig that cannot be re-run
- a doc claim nobody re-checks against the binary
- a gate that is required on nothing, or consumed by nothing

Every finding of the last two sprints was one of those four. `llms.txt` told agents to run `roost`
for **five releases**. `289 events/sec` outlived its harness by five weeks. Two seal-direct paths
passed with their mechanism deleted. The fuzz gate had never once been red.

This sprint builds four machines that make those classes structurally impossible, and then we stop
catching them by hand.

## The four

### 1. #767 - a deterministic measurement rig

**The headline, and everything performance-shaped is blocked behind it.**

magpie's own conclusion was that we cannot measure: a **3.8x spread inside one arm** in one session,
and seal-direct reading 0.92x - slower than the thing it beats by an order of magnitude. Record RPC
once, replay from disk, and a benchmark becomes a function of the code alone.

The acceptance bar is the **variance**, not the feature: five replay runs within **±2%** of their
median. It also closes #744 by answering the seal-direct question on a rig that can be trusted.

`Source` is a six-method trait with **25 implementations already in the tree**. This is wiring an
existing abstraction, not new machinery.

### 2. #768 - mutation coverage as a required gate, delightful core only

I mutation-checked two things on 2026-08-22 and found a hole in **both** (#725, #745). Two for two
is a measurement of how much of the suite is decorative.

Scoped deliberately: decode, chunker, seal, `define_views`, and the five core commands. **Proven able
to fail before it is made required** - the sequence #593 spent a whole sprint getting right.

### 3. #769 - every documented command and flag must be one the binary accepts

The cheapest of the four, with a working precedent: `version-check.sh` exists because a release pass
took three attempts, and its header says it plainly - *a version is a claim, and claims want a
checker*. Generalise from version strings to commands.

The regression test is literal: reintroducing `nuthatch roost` into any documented file must fail the
build.

### 4. #770 - publish what a nest actually costs to run

We measured it and have never said it. ~11.8M RPC requests served ~97 HTTP requests across four
nests in a week. A nest at tip costs ~549k requests a day, and **this is the feature working**, not
waste: a "last seven days" panel needs timestamps, timestamps need a header per block.

"Be your own indexer" with the bill omitted is the least honest sentence on the site.

## Sequencing, and it is a cost decision

The weekly Claude allowance stood at **79% on 2026-08-22** and resets **Tuesday the 25th at 10:00**.
Three days on a fifth of a week, against a burn of $472 in the preceding 24 hours. That sizes this
sprint whether we like it or not, so it is stated rather than discovered on Monday:

| when | what | why |
|---|---|---|
| now → Tuesday | **#769**, **#770** | small and docs-shaped; cheap in runs |
| now → end of week | **#767** | the long piece; start the design now, build through the reset |
| after Tuesday 10:00 | **#768** | large, and there is no allowance for two large pieces this side of the reset |

If the allowance runs out before Tuesday, **stop and say so**. Do not finish a piece by running the
board's remaining budget to zero.

## How this sprint runs differently

Three amendments, and they are standing rather than specific to this sprint.

**1. Scope is the board's, and a label is not approval.** meticulous-magpie was filed with four issues
and finished with seven, all self-labelled in flight. Every one was on-theme and the work was good,
which is exactly why this needs saying: a sprint that can extend itself has no natural end, and the
allowance stops it rather than the plan. Discovered work is filed **unlabelled**. Pulling it into
scope needs a board reply on the sprint issue.

**2. A `Reviewed-by:` line names the party who read the diff. No proxy signatures, including from the
board.** Two fabricated signatures appeared on 2026-08-22 - `Reviewed-by: Pete` on #739, removed by
Iris, and `Reviewed-by: Jenny` on #752 and #753, removed by me, neither of which I had written. Then
I signed #753 `Reviewed-by: Pete (board, via Jenny)` on standing authorisation, and **Iris was right
to reject it**: Pete had not read the diff, which is the same fault at a different name. The gate
cannot see any of this - it only refuses a signature naming the PR author's own login, and every
agent pushes as `cargopete`. The boundary is honour-system in both directions. **The firm enforcing
it against the board is the system working, and should keep happening.**

**3. The board writes acceptance criteria; the firm builds against them.** All four issues here carry
a numbered, falsifiable *Acceptance* section written before anyone was assigned. Fewer and larger
pieces, with the design argued up front rather than discovered in review.

## Explicitly not in this sprint

- **#744** - the seal-direct measurement question. Not dropped: it is #767's first customer, and
  re-running it before the rig exists would produce another number nobody can trust.
- **#649, #638** - Lodestar. Board work. Gap 3 closed 2026-08-22 (`graph-allocations-nest#1`); gap 2's
  recorded rule was found wrong and is now narrowed to three unattributed entities.
- **#750** - the production RPC audit. The one action taken (stopping a *temporary* nest that had
  spent 2.8M requests over five days holding **three** entities) is done; the rest is board follow-up.
- **Anything labelled `parked`.** The freeze runs to the end of 2026.

## The standing rules, unchanged

- **One worktree per run**, not per agent.
- **Never `git add -A`.** Stage explicit paths; diff `main...HEAD` before opening a PR.
- **Do not `@`-mention Rowan in GitHub markdown.**
- `CFLAGS=-std=gnu17` for every cargo build on the Linux box.
- `main` is protected, ten required contexts: **one merge per CI cycle**. Plan the landing order.
- **A green mutation is a finding, and so is a green suite.** #725 and #745 both exist because
  deleting a mechanism changed nothing anywhere. That observation is now issue #768.

## Context at filing

v2.6.3 shipped 2026-08-21. Twenty-three commits have landed since, including a real field crash
(#759: a factory nest aborts its backfill on a provider cap) and a credential-hygiene fix (#748:
`bench` printed a `--state-rpc` URL in full, and archive endpoints carry API keys). **v2.7.0 - a
minor, because #657 removed a refusal and lets a nest with `[[calls]]` use `--seal-direct` - should
be cut once #753 and #758 land, and does not wait for this sprint.**
