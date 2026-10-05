# Reproducibility of the production gate sets, 2026-10-05

For #1883. On the ThinkPad (x86_64), against each production nest's gate copy under
`~/release-gate/<nest>` and that nest's production environment, nuthatch 4.7.0 was started fresh
with `serve`, sent every statement in the nest's kittiwake set, stopped, and started again as a new
process for a second run. Each run was done at concurrency 1 and at concurrency 2 (the gate's pairs),
and every raw `/sql` body was kept and compared byte for byte. All 464 requests answered 200.

| Nest | Statements | With floats | Rows identical, c1 | Rows identical, c2 | c1 = c2 | Bodies identical (c1 + c2 pairs) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| alloc-nest | 74 | 1 | 74 | 74 | 74 | 120 of 148 |
| qos-nest | 9 | 5 | 9 | 9 | 9 | 11 of 18 |
| gns-nest | 9 | 0 | 9 | 9 | 9 | 14 of 18 |
| dips-nest | 16 | 0 | 16 | 16 | 16 | 31 of 32 |
| data-services-nest | 5 | 0 | 5 | 5 | 5 | 8 of 10 |
| staking-archive-nest | 3 | 2 | 3 | 3 | 3 | 6 of 6 |
| **Total** | **116** | **8** | **116** | **116** | **116** | **190 of 232** |

"With floats" counts statements whose answer carries a non-integer JSON number; the eight carry
32,533 float values between them, all identical to the last digit. "Rows identical" compares each
row's bytes as served, keys sorted, rows in order where the statement has a top-level `ORDER BY` and
as a sorted set where it has none, which is the gate's canonical form without its float rule.

Every body that differed differed in row order only, and only for a statement with no top-level
`ORDER BY`, where the order is the engine's choice and the gate already sorts:

- alloc-nest: indexer.revenue_by_deployment, indexer.allocation_shares, deployments.by_fee_ids,
  deployments.directory, deployments.fees_window, deployments.signal_stake, search.figures,
  search.deployment_ids, refresh.active_allocations_all, refresh.closed_allocations_90d,
  refresh.data_service_counts, refresh.exchange_rates_30d, refresh.exchange_rates_90d,
  qos.allocation_shares_all (c1 and c2)
- qos-nest: indexer_day, seconds_behind_day, network_rows (c1 and c2), freshness (c1)
- gns-nest: metadata_cids, current_metadata (c1 and c2)
- dips-nest: current_allocation (c2)
- data-services-nest: registry.dispatch (c1 and c2)

No ordered statement's body ever differed. The copies' provenance (`as_of` / `sealed_through`):
alloc 511814660 / 511736171, qos 48595262 / 48595039, gns 511815773 / 511811613, dips 511815782 /
511728296, data-services 511815765 / 511812218, staking-archive 497860300 / 497855497.

What this does not cover: binaries before 4.6.0, which summed DOUBLE in arrival order (burrmill#77),
and statements outside these sets.
