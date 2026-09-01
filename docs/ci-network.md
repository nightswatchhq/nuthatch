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

## The two protection scripts, and which one runs itself (#845)

Branch protection lives in GitHub's settings, not in the tree, so nothing about it appears in a diff.
Two scripts exist because of that, and they are not the same kind of thing.

| script | kind | runs itself? |
|---|---|---|
| `scripts/check-required-contexts.sh` | **(b)** - reads live protection; a skip is a failure | Yes: `required-contexts.yml`, nightly and on any change to `.github/required-checks.txt` |
| `scripts/protect-branch.sh` | Manual operator tool. Writes protection onto a branch. | **No, and deliberately** |

`check-required-contexts.sh` compares `.github/required-checks.txt` against what GitHub actually
enforces on `main`. **Without a token it exits 1**, because the comparison is its entire job and a
tokenless run has compared nothing; `--offline` is how a caller asks for the file-only check and gets
told, in the output, that no drift check happened. Its Actions job needs
`permissions: administration: read` - reading `branches/main/protection` is an admin-scoped read that
the default `GITHUB_TOKEN` does not carry.

It is **not** a required context, and that is on purpose: it reads a setting rather than the tree, so
a PR author cannot satisfy it by changing their PR, and a required check nobody can fix is a trap.

`protect-branch.sh` is the other direction - it *writes* `main`'s context list onto a named branch.
It stays manual because it mutates repository settings, which is not a thing a schedule should do
unsupervised. The hazard it addresses: a new `sprint/*` branch starts unprotected, and PRs onto it
are gated by nothing until someone runs this.

`apply-required-contexts.sh` is the third of the set and the one that touches `main`. It PATCHes the
required-context list alone from `.github/required-checks.txt`, reading `strict` back and re-sending
it unchanged, and it is a dry run unless given `--apply`. `protect-branch.sh` deliberately is not
used for this: it PUTs a whole protection object, so on `main` it would also write `strict` and
`enforce_admins` that the caller never mentioned.

Together the three are read, write-new-branch, write-main - and the committed list is the source of
truth for all of them. Note that the drift checker cannot currently run in CI at all (#1095): the
`PROTECTION_READ_TOKEN` secret has never existed, so it has failed on `main` every day since
2026-08-28 without blocking anything.

Since 2026-08-20 sprint work has gone straight to `main` (a sprint is a labelled set of issues, not a
branch - #810), so in practice this script is only needed if that changes back.
