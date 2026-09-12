# Documentation guide

The documentation is split between the product's current contracts and dated records of decisions.
Start with the current material below; use historical records to understand why a decision was made,
not to infer work that remains.

## Current work

- [Backlog guide](backlog.md) - GitHub is the live queue; this explains its labels and settled
  decisions.
- [Sprint: brisk-brambling](sprint-brisk-brambling.md) - drop-in closed; bugs that are true for everyone; then 0048.
- [Frozen for 2027](frozen-for-2027.md) - capability work deliberately deferred during the 2026
  feature freeze. These issues are closed, with an explicit reopening rule.
- [RFC index](rfcs/README.md) - design and implementation status for each RFC.

## Porting a subgraph

- [The Graph compatibility surface: what it is, and what it is not](graph-compatibility-what-it-is.md) -
  **read this first.** Whether a nest can serve your client, and why a coverage percentage overstates it.
- [The accepted query dialect](graph-query-dialect-accepted.md) - every shape that compiles, every
  refusal and its reason.
- [Schema generation rules](graph-schema-generation-rules.md) - how the Graph-shaped schema is derived.

## Operating and verifying nuthatch

- [Production readiness](prod-readiness.md)
- [Verification](verification.md)
- [Production guide](production.md)
- [Operator reference](operators.md)
- [Reading sealed segments without nuthatch](reading-segments.md)
- [Release notes](releases/README.md)

## Dated records

- [Progress log](progress-log.md)
- `sprint-*.md` - completed and historical sprint scopes. Sprint labels remain on GitHub issues as
  provenance; the documents explain the scope as it stood at the time.
- [July-August 2026 roadmap](high-level-roadmap-jul-aug-2026.md)
- [2027 direction](roadmap-2027.md)

When a document and a live issue disagree about outstanding work, the issue wins. If the disagreement
is substantive, correct the document as well. Otherwise the attic fills up, and before long it starts
answering questions nobody asked.
