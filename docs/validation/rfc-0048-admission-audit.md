# RFC-0048 admission audit, 2026-09-13

The implementation in `fbd8939b` and `997946c7` does not yet satisfy RFC-0048.
Issues #1226 and #1227 have been reopened. Their earlier closure and completion claims were
premature. The code is on topic branches, not evidence that the default branch has shipped it.

## Corrections under test

- Named-query hot snapshot failures now abort instead of entering the ordinary cold-only recovery
  path. A handler regression warms the ordinary query memo and then checks that a named request
  exceeding its byte budget still refuses with no rows.
- Ordinary SQL uses the original row-bounded backend method. Adding the byte-snapshot trait method
  must not make a backend without that optional method silently lose its hot data on free queries.
- Hot source bytes are counted before parsing each stored value, including malformed values.
- Unknown physical operators and operators capable of rescanning are refused. The previous walker
  counted only `READ_PARQUET` and silently ignored everything else.
- The estimator applies the statement-stacking and filesystem-access guards before preparing SQL.

## Outstanding acceptance gaps

- Payment is currently recorded before admission. Rejected requests must not consume authorisations.
- The preliminary estimator opens a separate connection with no hot rows. Execution needs a bound
  from its own connection after bounded hot materialisation, with the same catalogue and settings.
- The estimator must bind the exact catalogue snapshot and account for scans per source, including
  supported non-chain relations. Multiplying all reachable bytes by scan count is not the specified
  per-operator candidate-segment accounting.
- Quote catalogue retention, expiry, snapshot hashes and retry semantics are not implemented.
- Named-query planning must run under the cursor concurrency gate and wall-clock deadline.
- The 512 MiB threshold has no workload measurement supporting it. The RFC explicitly requires one.
- Maintained entity copies need explicit accounting within admission.
- The settler requires a concurrency/crash audit: queue replacement can race a serving process
  appending authorisations, and an external submission can finish before its outcome is journalled.

Passing existing analytics and serving tests does not prove these requirements. Each needs direct
evidence before the sprint issues can be closed again.
