# Frozen for 2027

These items were closed from the GitHub board during the 2026 feature freeze. Closure means
**deferred and recorded**, not delivered, rejected, or forgotten. Reconsider them in 2027 against
the current product, measurements and operator demand rather than treating this list as an inherited
implementation plan.

## Capability and architecture

- **#269 - RFC-0023 tier 4, hosted/shared call cache.** Only after a local tier-3 executor exists;
  it must remain opt-in and preserve the no-phone-home rule.
- **#271 - factory-child retirement.** Add an end/expiry shape only when long-lived factory demand
  justifies managing an unbounded watch set.
- **#272 - wildcard-address decode.** Needs a separate volume and RAM-budget design before decoding
  all topic-matching contracts.
- **#276 - real reth ExEx tip mode.** Requires a colocated node, the node binary, and an honest
  tip-latency result.
- **#277 - trace and state-diff extraction.** Own-node/ExEx work, sequenced after #276; public-RPC
  `debug_*` remains a non-goal unless deliberately revisited.
- **#278 - revm demand-driven state engine.** Reconsider only after derive-first and simple RPC
  tier-3 evidence leaves a material residue.
- **#280 - Turso hot store.** Requires a permissive production-ready release and a measured win over
  redb that federation does not already provide.
- **#308 - blocks and transactions tables.** The unfinished OBIB cases need a scoped volume-bound
  design and published artefacts.
- **#309 - OBIB traces.** Re-decide the `debug_*` non-goal before treating trace benchmark coverage as
  a missing implementation task.
- **#357 - whole-derivation reuse.** Revisit when durable materialised entity checkpoints make reuse
  measurable rather than criterion scenery.
- **#760 - parameterised-call volume bound.** The RFC record is corrected; the still-unbuilt guard
  needs `max_calls` and explicit operator opt-in before row-driven calls can be claimed bounded.

## Measurements and evidence

- **#274 - Polygon finality trust.** Establish the finality criterion and sustained live-cursor
  evidence before calling immutable Polygon sealing fully trusted.
- **#282 - tracked RPC tip lag.** Publish notification-to-row-queryable latency, distinct from merely
  exporting a lag gauge.
- **#285 - current release backfill number.** Publish a reproducible run tied to a reachable release
  SHA, provider, hardware and adaptive-window data.
- **#298 - wider performance suite.** Extend measured CI thresholds beyond the existing workload set.
- **#306 - complete OBIB coverage.** Revisit the remaining benchmark cases when their required
  capability and provider decisions are in scope.

## Reopening rule

Reopen one issue, rather than treating this document as a batch. The reopening proposal must name
the new demand or evidence, its compatibility with the then-current roadmap, and an acceptance
criterion that can fail. A feature freeze is not a substitute for thought, but it is an excellent
way to stop a backlog from disguising speculation as an order book.
