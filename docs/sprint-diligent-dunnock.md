# Sprint: diligent-dunnock

**What the hackathon found, fixed first. Then Dune end to end: the row-insert sidecar, the rest of
RFC-0055, and a recorded run. The warehouse recipes RFC-0052 left unrun. Cross-nest SQL as an RFC.**

**Closed 2026-09-16.** Every issue carrying `diligent-dunnock` is closed and no open PR carries the
label. The Dune sidecar and its recorded run (#1362, #1360), the warehouse runs (#1263), and
cross-nest SQL (#1324) were parked on Chief's call and removed from the scope before close. Their
draft RFCs and acceptance evidence remain intact; parked means deliberately not now, not done.

## Definition of done

Every issue carrying the **`diligent-dunnock`** label is closed, and no open PR is for one of them. The
label, not this list, is the record of scope. On 2026-09-14 Chief put the whole Dune path in this sprint,
so RFC-0056's build slices join the label when they are filed on its acceptance. #1324's build slices
join only if he puts them in when he accepts it.

## Order

1. **The hackathon papercuts**, each with its evidence on the issue:
   - #1319: queue a `/sql` request briefly before answering 503 busy. The builder's agent took about 130
     rejections per nest from fan-out.
   - #1318: `--cors` on `dev` and `serve`, so a page on another origin can call a nest without a proxy.
   - #1323: `doctor` probes with an address it finds itself when none is given.
   - #1322: `init` and `add` take a Blockscout root for a chain we have not verified.
   - #1321: `--from-subgraph` accepts a local `subgraph.yaml` and resolves `file:` ABI paths.
   - #1369: `adoptable` reads the registry hash a candidate's store recorded instead of recomputing it.
     Since #1364 changed the hash, a recomputed value can disagree with the store it describes.
2. **Dune, end to end, closing RFC-0055.** Chief's call on 2026-09-14, on a paid account.
   - #1362, RFC-0056: the row-insert sidecar. RFC-0055 §4 found it is the only public route in. It is
     written and accepted before any code, and its build slices are filed on acceptance.
   - The RFC-0056 build, slice by slice as accepted.
   - #1359, RFC-0055 S3: the authored views that translate exactly into DuneSQL, the rest named. It needs
     nothing from the sidecar, so it runs beside it.
   - #1360, RFC-0055 S4: the emitted queries run in Dune over a real nest's rows loaded by the sidecar,
     and recorded. The last slice, and the one that closes RFC-0055.
3. **#1263, RFC-0052 S5**: the Snowflake, BigQuery and Databricks recipes, run against a real published
   nest and recorded. Independent of the Dune path.
4. **The two costs every sprint pays.** #1283, the RPC width test that fails under full-suite load and
   forces reruns, and #1372, porting `scripts/pr-review.py` off Python under the house rule.
5. **#1324, cross-nest SQL in a multichain runtime, as an RFC.** The motivating case is the hackathon
   protocol that creates on Sepolia and settles on Arc. Design only; build slices are filed on acceptance.

## Rules

- **Public sources only for anything Dune**, as in steadfast-siskin. What the public record does not show
  is listed as unsupported, never guessed.
- **A run is recorded or it did not happen.** Nothing Dune-facing or warehouse-facing is called supported
  until it has run against a real account and the run is on the issue.
- **Accounts and keys stay with the operator.** The Dune API key and the warehouse credentials are
  supplied at run time, never committed, and never printed in a log or on an issue. Uploads to Dune are
  public below Enterprise, so the S4 run uses a nest whose rows are public chain data.
- **Optional stays optional.** The sidecar and CORS make no network call and change no behaviour when
  unconfigured (RFC-0046 §1's deletion test).

## Not in this sprint

- **Deferred:** #1216 and #1222.
- **Parked:** #1313, #1288 and #1280.
- **Board-only:** #1299.
- **Also out:** RFC-0054 and `docs/frozen-for-2027.md`.

v3.7.0 was cut on 2026-09-13 (#1378) and rolled out to the Helsinki nests and the site (#1365) before
this sprint's first merge.
