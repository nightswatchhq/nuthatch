# Sprint: bashful-bittern

Filed after assiduous-avocet. **Three issues.** A sprint is a labelled set. It has no calendar.

Deliberately small: it clears the tail assiduous-avocet's audit turned up, and the next thing after
it is RFC-0041 slice 2 (#821), which is the real work.

## Definition of done

Every issue carrying the **`bashful-bittern`** label is closed, and no open PR is for one of them.
That is three issues: #853, #843, #847. Work discovered in flight is filed **unlabelled**. Pulling it
into scope needs a board reply.

## The theme

**Neither caught nor missed.**

assiduous-avocet was about instruments that report success without measuring. These three are its
tail, and they share a narrower fault: each has an **edge it does not classify at all**. A mutant
that times out is neither a survivor nor a kill and appears in neither list. A copy guard that names
three files says nothing whatsoever about the other two. A notice whose condition is unsatisfiable is
neither shown nor removed.

None of them reports a falsehood. Each has a region it is silent about while looking complete, which
is the harder version of the same problem: a wrong answer can be caught, and an absent one has to be
gone looking for.

Freeze-legal throughout: correctness and observability of capabilities already shipped.

## The three

### 1. #853 - three chunker mutants time out, and the gate reports on neither outcome

`mutants.out/missed.txt` is what `mutants-check.py` reads, so a **Timeout** outcome appears in
neither the survivor list nor the baseline. Three of 39 scoped mutants - about 8% - are currently in
a state the gate says nothing about in either direction.

They are also most of the cost: 39.7, 33.4, 23.5 and 16.3 minutes against a ~7 minute median, so
they are roughly half the reason #841 had to split the job.

**The interesting half is not the reporting.** All three mutations plausibly send a *retry loop*
into an unbounded spin rather than an assertion into a failure - `window -> 0` means a window that
never advances, `is_result_too_large -> false` means a provider cap never recognised so the narrowing
retry never narrows. If that is what is happening, the finding is that the window controller's tests
**hang rather than fail** on a controller that makes no progress, and a test that hangs on bad input
is a test that would hang in production on the same input. This is the controller #672 took five
attempts to settle, and #672's own failure mode was "fails whole and retries forever".

Read `mutants.out/log/` for those three first. That says immediately whether it is a spin or a slow
build, and the answer decides whether this is a ten-line reporting fix or a real defect.

### 2. #843 - `launch_copy` scans three of the five launch docs

```rust
let files = ["show-hn.md", "port-queue-nest.md", "community.md"];
```

`docs/launch/` also holds `home-turf.md` and `strategy-review-2026-08-19.md`. The exact banned phrase
appended to `home-turf.md` leaves the suite green - measured, not inferred.

`ivm_claims` solved this one file away and says why in its own comment: a second pass over every
markdown file, "so a new page cannot reintroduce the claim without being added to SURFACES first".
Walking `docs/launch/**` instead of naming three files is a four-line change.

The second half of #843 - that both copy guards are exact-phrase denylists, so the same false claim
in new words passes - is **not** in scope. That is a known limitation, honestly scoped, and changing
it means guessing at English. Fix the file coverage; leave the mechanism.

### 3. #847 - the bench's "flag has no effect here" notice cannot print

`BackfillPath` has four variants, so the guard is an eight-row truth table and every row is false.
`effective_window_adaptive` never returns `false` while `requested` is `true`, because the only two
paths that ignore the flag ignore it *upward*. The condition asks "was adaptivity requested and
refused", which the function never does; the situation it means to describe is "requested, and it
changed nothing":

```rust
if args.window_adaptive && matches!(path, BackfillPath::Factory | BackfillPath::Pipelined)
```

Cosmetic - the JSON report is correct and no published number is wrong. It is here because of where
it sits: the commit whose entire purpose was that the bench must not misdescribe its own run added a
line to explain a discrepancy, and that line is dead.

## Explicitly not in this sprint

- **#821, #822 and the RFC-0041 slice-zero evidence (#835, #837, #839).** The next thing after this
  sprint, not part of it. #821 is p1 and carries all three.
- **#849, RFC-0042.** Unfrozen 2026-08-25 and sequenced behind RFC-0041.
- **#829 and #830.** Release integrity, still their own pair.
- **The phrase-denylist half of #843**, per above.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; `Closes` is one keyword per issue, not a comma list - and not
`Closes part of #N`, which GitHub does not parse at all and which left #841 open through its own
merge.

From assiduous-avocet, and it earned its place four times over that sprint: **prove the mutation
applied before believing it went green.** Print the `diff --stat` and read it.
