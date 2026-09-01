# Tyre-kicking pass - 2026-08-31

This is a fresh, **partial** execution of [#790](https://github.com/nightswatchhq/nuthatch/issues/790),
not a replacement for the 2026-08-23 evaluation saved under
`evaluation-2026-08-23/`.  That earlier paid-arm result was invalidated by the
endpoint identifying its plan as free when asked for the historical range.

Binary: `nuthatch 3.0.0-alpha.1`, built 2026-08-28.  The working tree's only
newer commit was documentation-only.  Runs used one supplied Chainstack mainnet
endpoint passed explicitly with `--rpc`; each generated config contained that
endpoint alone.  Fresh nests were created beneath `/private/tmp` and are not
part of this report's artefacts.

## Results

| Prediction | Command and observation | Result |
|---|---|---|
| 1. USDC scaffolds cleanly | `init 0xa0…eb48 --chain mainnet --rpc <paid>` completed in 6 s.  It resolved proxy implementation `0x4350…02dd` through Blockscout and warned, usefully, that pre-upgrade history may need an earlier ABI. | **HIT** for scaffolding.  No full-history under-two-minute claim was tested. |
| 2. An unhandled proxy silently gives an empty scaffold | stETH `init 0xae7…fe84` completed in 4 s, resolved implementation `0x0282…cdb0`, then a paid-RPC `dev --backfill 100` indexed 36 events in 1 s, including 13 `Transfer` rows. | **MISS**, favourably, for the current tip window.  This does not establish Aragon-era historical completeness. |
| 8p. Paid endpoint avoids retry amplification | USDC's first 100 blocks took 4 s, served 11,490 `Transfer` rows, and recorded 67 endpoint requests with 0 failures and 0 retries.  A caught-up 30 s sample later made 44 requests, or 126,720/day when linearly extrapolated, again with 0 retries. | **HIT** for this endpoint and sample.  It is not a monthly bill. |
| 11. Kill and resume | A 10,000-block USDC backfill had 20,265 transfers through block 25,866,021 when the process was sent `SIGKILL`.  Restart rebuilt its view and resumed from the persisted checkpoint, not deployment.  The later prefix held 124,214 rows and 124,214 distinct `(tx_hash, log_index)` pairs through block 25,867,748. | **HIT** for the observed prefix.  This is not a completed-history identity proof. |
| 14/15. `/sql` refuses known file read/write shapes | A quoted `read_csv('/etc/hosts')` request and a stacked `COPY ... TO <temporary-file>; SELECT 1` request both returned HTTP 400.  The temporary write target did not exist afterwards. | **HIT** for these two regressions.  Not a claim that no novel bypass exists. |
| 16. Local operator surface | `lsof` showed the two test APIs listening only on `127.0.0.1`; both `/ready` endpoints were caught up and returned `ready: true`. | No adverse finding in this limited check. |

## Friction observed

Before a cold backfill begins, `dev` reports every declared table as having no
data and says the event has "likely never fired on this chain".  In both USDC
and stETH runs the `Transfer` table populated seconds later.  The text does add
that the table starts populating when the event fires, but its first sentence is
misleading during a normal cold start.

## Not run

The following #790 work remains deliberately unscored: a deployment-to-tip
backfill and its under-two-minute claim; ENS/subgraph parity; provider-cap and
factory-path testing; an hour-long cost sample and actual provider billing;
the free-tier arm; reorg, changed-config and disk-pressure tests; webhook and
admin-UI checks; and a broader `/sql` adversary pass.  In particular, the
126,720 requests/day figure is a 30-second mainnet extrapolation, not evidence
for or against the published monthly cost model.
