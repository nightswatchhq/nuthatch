# Sprint: diligent-dunnock

**What the hackathon found, fixed first. Then the one public route into Dune, end to end. Cross-nest SQL
as an RFC.**

## Definition of done

Every issue carrying the **`diligent-dunnock`** label is closed, and no open PR is for one of them. The
label, not this list, is the record of scope. RFC-0056's and #1324's build slices join the label only if
Chief puts them in when he accepts each RFC.

## Order

1. **The hackathon papercuts**, each with its evidence on the issue:
   - #1319: queue a `/sql` request briefly before answering 503 busy. The builder's agent took about 130
     rejections per nest from fan-out.
   - #1318: `--cors` on `dev` and `serve`, so a page on another origin can call a nest without a proxy.
   - #1323: `doctor` probes with an address it finds itself when none is given.
   - #1322: `init` and `add` take a Blockscout root for a chain we have not verified.
   - #1321: `--from-subgraph` accepts a local `subgraph.yaml` and resolves `file:` ABI paths.
2. **#1362, RFC-0056: the Dune row-insert sidecar.** RFC-0055 §4 found it is the only public route in.
   Chief accepts it before any code.
3. **#1359, RFC-0055 S3**: the authored views that translate exactly into DuneSQL, the rest named.
4. **#1324, cross-nest SQL in a multichain runtime, as an RFC.** The motivating case is the hackathon
   protocol that creates on Sepolia and settles on Arc. Design only; build slices are filed on acceptance.

## Rules

- **Public sources only for anything Dune**, as in steadfast-siskin. What the public record does not show
  is listed as unsupported, never guessed.
- **A run is recorded or it did not happen.** Nothing Dune-facing is called supported until it has run
  against a real account and the run is on the issue.
- **Optional stays optional.** The sidecar and CORS make no network call and change no behaviour when
  unconfigured (RFC-0046 §1's deletion test).

## Not in this sprint

- **Blocked:** RFC-0055 S4 #1360 (on RFC-0056's build and a paid Dune account) and RFC-0052 S5 #1263 (on
  warehouse accounts).
- **Deferred:** #1216 and #1222.
- **Parked:** #1313, #1288 and #1280.
- **Board-only:** #1299.
- **Also out:** RFC-0054 and `docs/frozen-for-2027.md`.

Release v3.7.0 is cut when steadfast-siskin closes, before this sprint's first merge.
