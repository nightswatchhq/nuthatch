# Independent audit results - 2026-09-01

Audited against `docs/audits/2026-09-plan.md` on commit `324056cd`.

## Findings filed

| issue | finding |
| --- | --- |
| [#1084](https://github.com/nightswatchhq/nuthatch/issues/1084) | The scheduled Lodestar Horizon activity cache is a gateway-backed continuous ingestion path outside the nine `src/lib/ingest/*` modules. It fetches chain-derived delegation and provision events, but is omitted from the migration table and completion denominator. |
| [#1086](https://github.com/nightswatchhq/nuthatch/issues/1086) | #1078 omits eight direct public API consumers of the network subgraph. They remain unavailable, or empty in the `token-metrics` fallback case, without `GRAPH_API_KEY`; replacing only the listed ingestion seam cannot make the stated completion claim true. |

Each issue contains the failure scenario, reproducing commands, and the existing test or inventory boundary that failed to cover it.

## Examined and found sound

- `/sql` is guarded both textually and from DuckDB's parsed table references. The checked controls refuse statement stacking, `WITH`-prefixed DML, filesystem table functions, replacement scans, unrecognised table functions, oversized query text, concurrent excess, result rows and result bytes. `cargo test --test e2e_query_allowlist` passed: 4 tests.
- The admin surface is composed only when enabled. On a non-loopback bind it is absent without `NUTHATCH_ADMIN_TOKEN`, and a configured token gates it. `cargo test --test e2e_fe_admin_exposure` passed: 5 tests.
- Mount aliases, tenants and NIDs are validated before becoming route or filesystem segments. NIDs are exactly 64 hexadecimal characters; aliases and tenants permit only alphanumeric, `_`, and `-` characters.
- The two required segment determinism properties held in the end-to-end suite: identical runs, and identical segments despite changed fetch-window/concurrency shape. `cargo test --test e2e_seal_determinism` passed: 2 tests.
- Entity derivation converged after clean replay and at arbitrary exercised reorg depth; reorg isolation, maintained-relation SQL, and restart seeding were also covered. `cargo test --test e2e_entity_reorg` passed: 16 tests.
- Runtime lifecycle tests rejected malformed NIDs before path resolution, enforced lifecycle admin authentication, and preserved mount isolation. `cargo test --test e2e_runtime_lifecycle` passed: 8 tests.
- The checked IVM, launch and verification documentation-claim suites passed: `ivm_claims` (4), `launch_copy` (1), and `verification_non_claims` (2).
- `cargo deny check advisories licenses bans sources` passed. It emitted duplicate-version warnings only; advisories, licence policy, banned crates and source policy all passed.

## Limits of this pass

No clean result here establishes production-scale performance, a clean-machine 90-second setup, slow-loris behaviour, or a profile of the two-cursor RSS anomaly. Those need their stated environments and measurements, not a symbolic nod from a repository checkout. The Lodestar findings were inspected in `~/Projects/lodestar`; no Lodestar files were changed.
