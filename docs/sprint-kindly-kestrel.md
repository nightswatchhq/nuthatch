# Sprint: kindly-kestrel

Filed after judicious-jackdaw closed all four of its issues and v2.6.2 shipped. **Four issues.**

## Definition of done

Every issue carrying the **`kindly-kestrel`** label is closed, and no open PR is for one of them.
Work discovered during the sprint is filed as an issue for the board rather than picked up, and
pulling anything into scope needs board approval.

## The theme

**A check that cannot fail is not a check.**

This is the fault class that has appeared *every single day this week*, in five different places, and
it has never once been the subject of a sprint. It has always been something found while fixing
something else:

- **#696** - `chains::all()` returned three chains while `lookup()` knew seven. The test asserted
  every chain in `all()` resolves via `lookup`, which is trivially true of any **subset**. One
  direction of a two-list invariant; the missing direction is the one that rots.
- **#694** - `abi_resolved_lines()` had two unit tests. `print_abi_resolved()` had none. Deleting
  both call sites left the whole suite green.
- **#699** - the sprint-landed gate printed *"Nothing. Every PR is reachable from main"* when its
  query failed. A clean bill of health from a check that never ran.
- **#672** - the reproduction had to be built before anything could be concluded, because four
  identical runs of the same demo measured 2, 15, 28 and 198 events.
- **#684** - a dashboard reading `6 / 6` over six PRs that had shipped in nothing.

Every one of those looked green while proving nothing. This sprint takes the four remaining known
instances and closes them deliberately rather than by accident.

**The discipline for every item: prove the check can go red.** Not that it passes - that it fails when
the thing it guards is broken. State in the PR what you broke and what failed when you broke it. A
green tick is the beginning of the evidence, not the end.

## The four

### 1. #619 - the review gate accepts the word "pending"

**The headline, and it is worse than it reads.** `reviewed-by signature` accepts any non-blank text
after the colon, so it is satisfied by the literal string `pending` - the one value that means the PR
has *not* been reviewed. Observed live on PR #617, which sat mergeable-on-green with an explicitly
unsigned body.

### 2. #691 - the same gate cannot detect a self-signature

It refuses a signature naming the PR's **GitHub login**. Every agent here pushes under one shared
login, `cargopete`, and signs under a different name - `Iris`, `Rowan`, or the shared git identity
`pete-fathom`. Those name spaces do not intersect, so the self-review predicate **can never fire for
us**. It has already let one through: #688 carried a self-applied signature and passed.

Taken together, 1 and 2 mean **the gate the firm's merge authority rests on is decorative**. Merge
authority is safe *because* an independent reviewer signs; a gate that accepts "pending" from the
author is not that. This is the most consequential pair on the board, and it is about the firm's own
safety rather than the product's.

The fix has a design question in it, and the board wants the reasoning more than the patch: what
*can* be checked here, given one shared login? A name allowlist, a mapping from agent name to run
history, separate forge identities (a board decision, not yours), or something else. Say what you
would do and why before doing it.

### 3. #353 - the skill drift gate checks one direction

`tests/skill_refs.rs` asserts every `--flag` and `nuthatch_*` metric the skill files *mention* is
real. It cannot catch the opposite: something real the skill never mentions. That is exactly the shape
of #696, in a different file, and it is the reason the CLI reference did not drift while the rest of
the docs did - the gate is good, it is just half a gate.

Worth noting the gate has already earned its keep, so this is sharpening a working tool rather than
salvaging a broken one.

### 4. #633 - a bad minute reads as a dead endpoint

`live-endpoints.yml` has no retry, so one bad minute at a provider is indistinguishable from a shipped
default that has genuinely died. It failed its scheduled Monday run with two of nine defaults unable
to serve a windowed `getLogs`.

This is the inverse failure to the other three - a check that cries wolf rather than one that sleeps -
and it matters now because the same workflow is what would have caught **#679**, where Polygon's first
endpoint lost archive depth one day after being measured. A probe nobody trusts is a probe nobody
reads.

## Explicitly not in this sprint

- **#649** - the Lodestar parity gaps. Board work.
- **#639** (CI disk) and **#621** (fuzz budget) - real, and a different theme. They are the ones to
  reach for if this sprint finishes early, in that order.
- **The parked capability issues** - revm, traces, ExEx, DataFusion, Turso, tier-4 cache, wildcard
  decode, OBIB, whole-derivation reuse. Frozen for 2026, not cancelled. They still want a `parked`
  label; that is on the board.

## Why four, again

Two of these contain a design question rather than a known edit - #691's "what can be checked given
one shared login", and #353's "what does the other direction even assert" - and those halves are worth
more than the typing.

## Outstanding on the board's side

- **Hardy-heron's audit**, still owed from two sprints ago.
- **The `parked` label** on the frozen capability issues.
- **A 2.6.3**, once #703 and #704 land. #672 alone changes what a new user experiences.
- **Thanks for the overnight filing discipline.** Seven issues found while reviewing your own sprint
  work, every one filed rather than picked up, three of them unfinished halves of what the sprint had
  just shipped. #696 in particular - a two-list invariant with only one direction checked - is the
  issue that gave this sprint its theme.
