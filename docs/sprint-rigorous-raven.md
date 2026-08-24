# Sprint: rigorous-raven

Filed 2026-08-24, after quizzical-quail's labelled set closed on the branch (PR #805 still
landing). **Four issues.** Runs **Sunday 2026-09-07 to Sunday 2026-09-14**, or the Monday after
#805 merges, whichever is later.

## Definition of done

Every issue carrying the **`rigorous-raven`** label is closed, and no open PR is for one of
them. That is four issues: #716, #710, #715, #782. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

## The theme

**A check which cannot fail is not a check.**

Owl made `/ready` honest. Peregrine made the defaults and the dollar figure honest. Quail made
the directory lockdown and the cited commit honest. What still greps a sentence, passes when it
never ran, or lives only in GitHub settings the repo cannot see, is the same generator.

Freeze-legal throughout: bug, verification, a gate, a policy. Not RFC-0040.

## The four

### 1. #716 - live-endpoints retries by grepping doctor's prose

**The matcher.** `retry` keys on `getLogs window   up to` (the exact double space). Doctor
reports in prose rather than through its exit status, so a reword of that line condemns every
shipped endpoint. Allowlist-not-denylist is right; coupling it to a sentence is not.

**Acceptance**

1. `nuthatch doctor --json` prints a JSON array of probe objects with `max_window`, `archive`,
   `archive_unknown`. Stdout is JSON only.
2. `live-endpoints.yml` keys retry on those fields (`max_window != null`, `archive == true`),
   not on a grep of the report line.
3. A unit test serialises a failed probe and a healthy one; deleting `max_window` from the
   schema fails it. Rewording `report()` does not.
4. The workflow's URL list matches `src/chains.rs` shipped defaults. Probing a dropped host is
   not a test of the product.

### 2. #710 - network-dependent checks have no policy

**The two answers.** `live-endpoints` hard-fails on a blip. Two e2e tests return green when
`init` never completed (`nothing to judge, not asserting`). Both shipped. Silent skip is how a
gate looks healthy.

**Acceptance**

1. A committed policy: every network-dependent check is one of (a) does not touch the network,
   (b) touches it and is CI-loud on skip, (c) `#[ignore]` / cron, with a documented way to run
   it. Silent skip is not on the list.
2. The two `e2e_bare_help` silent returns panic when `CI` is set.
3. A grep of `nothing to judge` / `not asserting` in `tests/` is empty, or each remaining hit is
   named in the policy as (c).

### 3. #715 - required checks are invisible, new sprint branches start naked

**The scenery.** `reviewed-by signature` ran red onto a list that nothing consumed, for a week.
The required set lives in GitHub settings. `sprint/*` protection is per-branch, by hand.

**Acceptance**

1. `.github/required-checks.txt` lists the contexts `main` requires, one name per line.
2. A check (script and/or test) reads the live protection API when a token is present and reds
   on any difference. Without a token it still asserts the file contains `reviewed-by signature`
   and the rest of the known ten.
3. A script copies that list onto a named branch. A new `sprint/*` ruleset is created if the
   token can, or the PR says plainly that it could not.

### 4. #782 - the doc command gate is blind after `\`

**The copy-paste block.** `check_text` tokenizes one physical line. A `\`-continued invocation
loses every flag on the next line. The gate caught a prose backtick and missed the fenced block
a reader copies.

**Acceptance**

1. A fixture with a `\`-continued invocation whose unreal flag sits on the second line is caught,
   attributed to that line.
2. The same fixture against today's tokenizer (no join) is the mutation this fails under.
3. Real flags on continuation lines (`--record`, `--replay` in RFC-0039) are not allow-listed
   just because they used to be invisible.

## Explicitly not in this sprint

- **RFC-0040.** Design, freeze.
- **#750.** Ops. Swap 2.7.1 on the box.
- **#649 / #638 / #305.** Lodestar product.
- **#744.** A clean tape run, not four tickets.
- **#286.** Hostile-ABI RAM. A measurement.
- **#763 / #776.** Stale prose. File if they fall out.
- **#760 and anything `parked`.**
- **quizzical-quail's four.** They close on #805. Do not restack this on that branch.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.** No proxy signatures.
3. **Acceptance is above.**

Also standing: one worktree per run; never `git add -A`; do not `@`-mention Rowan in GitHub
markdown; `CFLAGS=-std=gnu17` on the Linux box; one merge per CI cycle.

## Context at filing

v2.7.1 is installed. Peregrine is on main. Quail is in review on #805. These four were already
named as next-but-one when quail started.
