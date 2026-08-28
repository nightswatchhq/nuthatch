# Sprint: dogged-dunlin

Filed after candid-curlew. **Four issues.** A sprint is a labelled set. It has no calendar.

The 2026 freeze applies again in full. candid-curlew was a carve-out and RFC-0041 spent it; this
sprint is squarely inside what the freeze allows - security, a bug, and maintenance. Nothing here
adds a capability.

## Definition of done

Every issue carrying the **`dogged-dunlin`** label is closed, and no open PR is for one of them.
That is four: #829, #830, #863, #913. Work discovered in flight is filed **unlabelled**. Pulling it
into scope needs a board reply.

## The theme

**The release we are publishing this week, and the checks that were never checking.**

Two of these four name their own deadline in their own text - #829 calls itself "a pre-3.0
supply-chain hardening item", #830 says "Before 3.0". 3.0.0-alpha.1 is being cut as this is filed, so
those deadlines are now. The other two are the bill for two days of measuring: a loop that cannot
stop, and a set of gates that were not watching.

There is a thread running through all four. Each is a thing that *reports success by default* -
an unpinned action that runs whatever it was retagged to, a checksum that proves the file matches
itself, a retry that keeps retrying, a workflow that never validated. None of them fails loudly when
it is wrong. That is the shape 3.0.0-alpha's own defects had too: a relation silently keeping
`groups mod 10,000`, an update cost growing unnoticed, a nest dying with a diagnosis that was never
true.

## The four pieces

### 1. #829 - pin GitHub Actions to immutable commit SHAs

`p1`, security. The release and CI workflows execute six third-party actions by **mutable tag** -
`actions/checkout@v4`, `softprops/action-gh-release@v2`, `EmbarkStudios/cargo-deny-action@v2` and
others - while holding repository and release credentials. A retag is a code-execution primitive
against our release pipeline.

Done when every action is pinned to a full commit SHA with the version retained as a comment, and
Dependabot is advancing the pins.

### 2. #830 - verifiable provenance for release binaries

`p1`, security, and the other half of #829. We attach SHA-256 sidecars and nothing else. A checksum
downloaded from the same release as the binary establishes only that the file matches its own
hash - it cannot detect a compromised release credential or a swapped action, which is exactly the
failure #829 describes.

Done when releases carry signed checksums or GitHub artifact attestations, the install path documents
how to verify, and the release workflow verifies artifact identity *before* publishing.

Sequence these two together and #829 first: provenance signed by a pipeline that runs unpinned
third-party code is provenance for whatever that code produced.

### 3. #863 - the backfill loop has no no-progress guard

`p2`, bug, and it acquired a real instance this week. #903 was precisely this shape: a tip race
misclassified as a provider cap, the chunker narrowing a window that could never help, and nothing to
notice that the narrowing was achieving nothing - until it reached a single block and gave up with a
message describing a condition that was never true.

#860 closed the zero-width case with an invariant. The general case is open: a retry that never
narrows should be a named failure, not a spin.

### 4. #913 - the gates do not watch what they claim

`tech-debt`, and the evidence is six of them in two days: a lifecycle vocabulary with no word for
two statuses in use, an assertion that outlived its fact, a canonical metric set that stopped being
the whole exposition, a workflow that never once validated, a pipe swallowing an exit status, and a
test pinning the one recovery path that does not exist.

Every one was found because something *else* broke. The method is mutation rather than inspection -
three of these were read while broken and looked fine.

## Explicitly not in this sprint

- Anything `parked` or `frozen`. RFC-0013, 0014, 0024, 0003, 0033, 0031, 0009 §wildcard, 0023 tier 4.
  The freeze is back and the carve-out is spent.
- **RFC-0042** (#849, #891). Sequenced behind RFC-0041, which has only just shipped, and it needs a
  freeze decision of its own before a slice starts. Not by drift.
- **The 3.0.0-alpha stress test.** It runs on wall-clock, not on sprint scope - items 2, 3 and 4 of
  `docs/releases/3.0.0-alpha-stresstest.md` need days of uptime, not engineering. Findings from it
  are filed unlabelled like any other discovered work.
- #296, #889 (performance) and #698 (website). Real, in scope for the freeze, and not this sprint.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; `Closes` is one keyword per issue, and never `Closes part of #N`,
which GitHub does not parse.

From candid-curlew: prove the fixture can distinguish a pass from a failure before quoting what it
measured. This sprint's own subject matter sharpens it into the harder rule - **mutate the guarded
thing, not the assertion.** Deleting an assertion leaves a test green by construction and proves
nothing, which I nearly recorded as evidence on #910 this week.
