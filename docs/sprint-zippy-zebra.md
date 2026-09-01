# Sprint: zippy-zebra

Filed after zealous-zosterops (#831) opened. **One issue.** A sprint is a labelled set. It has no
calendar.

## Definition of done

Every issue carrying the **`zippy-zebra`** label is closed, and no open PR is for it. That is one
issue: #818. It ends with an explicit **go** or **park** decision for RFC-0041. Work discovered in
flight is filed **unlabelled**. Pulling it into scope needs a board reply.

## The theme

**Prove that authored incremental entities fit inside the binary before building the product around
them.**

RFC-0041 is Nuthatch 3.0's headline feature: an author may declare a keyed `entities/*.sql`
relation which is maintained as facts arrive, rather than recomputed from raw history at every
query. That promise is valuable only if its compiler, DBSP circuit, retractions and resource cost
fit the existing single-binary, offline and 2 GB cursor contracts.

The first question is therefore not authoring syntax, serving routes, or documentation. It is
whether one useful Lodestar relation can be lowered from DuckDB's AST into a dynamic DBSP plan with
no external compiler, service, JVM, Cargo invocation or downloaded toolchain. If it cannot, RFC-0041
parks. There is no disguised fallback which turns Nuthatch into a small platform estate.

Freeze-legal: this RFC was explicitly approved and unparked on 2026-08-24. The sprint implements
only its slice-zero decision gate.

## The one

### #818 - RFC-0041 slice 0: prove authored SQL can become an embedded DBSP circuit

Choose one real Lodestar relation by captured raw-history scan cost, not by which example is most
polite. The throwaway vertical spike must include a filter, exact arithmetic, `GROUP BY`, an inner
equijoin, declared key, runtime lowering from DuckDB's serialised AST, and both `+1` insertion and
`-1` retraction batches.

**Acceptance** is #818's. The non-negotiable evidence is:

1. DuckDB and DBSP match byte-for-byte over a fixed finalized Lodestar corpus after canonical key
   ordering.
2. Randomized apply, retract and replacement sequences converge to a clean replay.
3. At declared `max_rows`, whole-cursor RSS stays within 2 GB, with fixed circuit and per-row costs
   recorded.
4. Release-binary size delta and ingestion throughput are measured on the same recorded input, and
   throughput meets the current ingest floor.
5. Mutations independently break retraction, expression lowering and join keys, and each makes a
   discriminator fail.
6. The outcome is an explicit go or park decision. Failure parks the RFC. It does not authorise a
   Feldera service, JVM, mount-time Rust compilation or runtime toolchain download.

## Explicitly not in this sprint

- **#820, #821 and #822.** They are slices 1-3, and remain blocked by #818's go decision. No
  `entities.toml`, lifecycle, serving route or Lodestar product claim lands first.
- **#357.** Durable per-entity grafting is post-v1 RFC-0033 work, reopened only if a measured
  restart-rebuild cost after slice 3 warrants persistent entity state.
- **Changing README copy to claim general authored IVM.** The current copy is intentionally narrow
  until the complete RFC ships and proves the claim.
- **#638.** Broader Lodestar migration ownership is separate. This sprint uses one relation only as
  a measured compiler boundary.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; one merge per CI cycle. `Closes` is one keyword per issue, not a
comma list - squash only honours the first.
