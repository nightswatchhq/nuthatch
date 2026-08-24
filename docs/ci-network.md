# Network-dependent checks

A check which cannot fail is not a check. Every test or job that talks to the network picks
**exactly one** of:

| | Kind | When skip happens |
|---|---|---|
| **(a)** | Does not touch the network. Fixtures, tapes, stubs. | It does not. |
| **(b)** | Touches the network, and a skip in CI is a failure. Locally, absence of the dependency may skip. `NUTHATCH_REQUIRE_PG=1` is the Postgres form; `CI=true` plus a panic on skip is the RPC form. | Local only. |
| **(c)** | `#[ignore]`, or a cron workflow, with a documented command to run it. | Always, until someone runs it. |

**Silent skip is not on the list.** `eprintln!("offline? - nothing to judge, not asserting"); return;`
is how a gate looks healthy.

`live-endpoints.yml` is (b) with retry-then-fail: a blip is absorbed, exhausting the retries is a
red job. It is not on the pull_request path, so a provider's bad minute does not redden an
unrelated PR.

See #710.
