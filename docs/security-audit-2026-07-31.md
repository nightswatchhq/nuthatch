# Security audit, 2026-07-31 (pre-1.0)

A deliberate adversary pass over the untrusted surfaces, rather than the incidental findings that turn
up while building. It produced **one exploitable vulnerability in the first ten minutes**, which is the
argument for doing this before 1.0 rather than after.

Method: probe, then **verify against the real component** — the confirmed vulnerability was proven by
executing it against a live DuckDB, not by reading the guard and reasoning about it. Every "safe"
verdict below was likewise tested, and several turned out to be safe *because of a dependency's
default* rather than anything we wrote. Those are recorded as such, because a guarantee you did not
write is one you can lose in a version bump without noticing.

## Findings

| # | Finding | Severity | State |
|---|---|---|---|
| 1 | Arbitrary file read via `/sql` — a **quoted function name** evaded the denylist | **High** | **Fixed** (0.9.3) |
| 2 | `duckdb_settings()` / `duckdb_extensions()` disclose absolute paths incl. OS username | Low–Medium | **Fixed** (0.9.3) |
| 3 | The `allowed_directories` lockdown is **empty** — the documented second layer is not engaged | Informational | **Won't fix, measured** |
| 4 | Extension-gated file readers are absent from the denylist | Latent | **Fixed** (0.9.3) |
| 5 | The denylist is an allowlist-shaped problem | Design | **Addressed** (0.9.3) |
| 6 | `init --from` relies on git's `protocol.ext.allow` default | Defence-in-depth | **Fixed** (0.9.3) |
| 7 | Bundle outbound-URL warning does not distinguish link-local / metadata targets | Hardening | **Fixed** (0.9.3) |

All seven are closed. Three of the fixes are worth reading past their status, because *how* they were
closed differs: finding 3 is closed as **not ours to fix** rather than fixed, finding 5 by a new control
alongside the old one rather than replacing it, and finding 4 by a rule that immediately caught its own
test being obsolete. Details below.

### 1. Arbitrary file read via `/sql` — **fixed**

```sql
SELECT * FROM "read_csv"('/etc/passwd')
```

`reject_file_access` matched a forbidden name only when the next non-space character was `(`. DuckDB
accepts a **quoted** function name, where the next character is `"`. Both guards passed it and DuckDB
executed it — confirmed against a live connection, returning the contents of `/etc/hosts`.

Exploitable on any `/sql` reachable by an untrusted party, in every released version.

**Same class as the stacked-`COPY TO` arbitrary *write* (#153):** the guard was correct about the shape
it imagined, and the shape had another spelling.

Fixed by stripping `"` before the scan, which normalises every placement at once — `"read_csv"(`,
`read"_"csv(`, anything quoting can do to break a name apart — rather than patching one spelling.
Stripping can only make the denylist match *more*; over-refusing is the safe direction here. Both
directions are tested (`a_quoted_function_name_cannot_evade_the_denylist`,
`ordinary_quoted_identifiers_still_work`, since this product quotes `"from"`/`"to"` constantly), and
the fix is mutation-verified.

**This needs an advisory.** See "Disclosure" below.

### 2. Environment disclosure via `duckdb_settings()`

Both `duckdb_settings()` and `duckdb_extensions()` pass the guards and execute. Measured output:

```
secret_directory    = "/Users/<user>/.duckdb/stored_secrets"
temp_directory      = ".tmp"
allowed_directories = "[]"
```

An untrusted `/sql` caller learns the **absolute home path and OS username**, and the exact state of
the sandbox. Not a file read; free reconnaissance for anyone looking for one.

**Fixed.** Both, plus `getenv`, are in the denylist, and the AST allowlist of finding 5 refuses them a
second time as unrecognised table functions. Reconnaissance is the cheap half of an attack and the half
that leaves no trace; there is no analytical query over blockchain data that needs to know our
`secret_directory`.

### 3. The second layer is not engaged

`allowed_directories = "[]"` confirms what the code only suspected. `analytics.rs` says the directory
lockdown is *"defense-in-depth (its runtime enforcement is version-dependent in the bundled DuckDB)"*.
It is **not enforcing at all**, so the denylist is not the primary control — it is the *only* one.

That matters mostly for how finding 1 should be read: there was no second layer to catch it.

**Closed as "not ours to fix", not as fixed** — the distinction is the point. The setting is passed to
DuckDB correctly; the bundled build does not enforce it. We cannot make it work from here, and quietly
dropping it would remove the free upgrade if upstream ever starts enforcing.

_(2026-08-24, #289: it was ours. `allowed_directories` is a restriction only when
`enable_external_access` is false, a startup-only flag the 31 July pass never set. quizzical-quail
sets it on the `/sql` connection. The dated finding above is kept as what we believed that day.)_

What *was* wrong was the belief attached to it. `the_directory_lockdown_blocks_an_out_of_allowlist_file_read`
now pins which control does the work, so nobody can weaken the denylist on the assumption that something
sits behind it. `lock_configuration` is real and does hold — a query cannot widen the setting — but an
empty allowlist that nothing enforces is a comment, not a control. A defence-in-depth layer nobody has
measured is worse than none, because it is budgeted for in decisions about the layer in front of it.

### 4. Extension-gated readers are absent from the denylist

`read_xlsx`, `st_read`, `iceberg_scan`, `delta_scan`, `postgres_scan`, `sqlite_scan` all pass the
guards. They fail today only because those extensions are not in the bundled build — i.e. we are safe
by build configuration, not by policy. Bundling any extension, or a DuckDB release that promotes one to
core, converts this to a live file read with no code change on our side.

**Fixed**, and the fix promptly demonstrated the finding it came from. A reachability test probed the
guard using `read_xlsx` — chosen *because the denylist did not list it* — and adding it here made that
test fail. The probe was right and the vocabulary had moved underneath it, in a single afternoon,
which is finding 5 in miniature. It now probes `read_some_future_format`, a name DuckDB will never
have, so it tests the guard rather than the list.

### 5. The denylist is an allowlist-shaped problem

Findings 1 and 4 are the same root cause seen twice: a **denylist over an evolving vocabulary**. It has
now been wrong about *spelling* (1) and about *coverage* (4), and every DuckDB release can add a table
function that touches the filesystem or network. The failure mode is silent and the feedback loop is
"someone exploits it".

Worth considering before 1.0 whether the guard should invert — parse the statement and permit only
known-safe table references, rather than enumerating the unsafe ones. That is a larger change than a
fix and should be argued on its own terms, but it is the finding with the longest tail.

**Addressed, by adding the allowlist rather than replacing the denylist.** `reject_unknown_table_refs`
asks DuckDB's own parser (`json_serialize_sql`) what a statement references and refuses anything it does
not recognise: a table function must be one of three (`generate_series`, `range`, `unnest`), and a base
table must be named like an identifier — which is what catches a *replacement scan*, `FROM
'/x.parquet'`, that the AST otherwise reports as an ordinary table whose name happens to be a path.

Finding 1 is not expressible against it: `read_csv(…)` and `"read_csv"(…)` parse to the same
`TABLE_FUNCTION` node, so quoting collapses for free instead of needing to be normalised. Finding 4 is
not expressible either — a new DuckDB file reader is unrecognised by default rather than permitted by
default, which inverts the failure mode from silent to loud.

**Both controls remain.** The allowlist fails *open* when a parse is unavailable, because
`json_serialize_sql` is a DuckDB feature and this is the newer of the two: a parse failure must not take
down `/sql` while the denylist that has guarded this surface since RFC-0008 is still in front of it. Two
controls with different failure modes, not one replacing the other. The denylist's tail is now finite.

### 6. `init --from` relies on git's default

`is_git_source` accepts anything ending in `.git`, so `ext::sh -c … .git` reaches `git clone`. Git's
`ext::` transport executes commands; the attempt is refused only by git's own
`protocol.ext.allow=never` default (git ≥ 2.12).

**An operator with `protocol.ext.allow=always` in their gitconfig — not unheard of in CI images — turns
`nuthatch init --from <url>` into remote code execution.** Validating the URL scheme ourselves costs
nothing and removes the dependence.

Option injection (`--upload-pack=…`) is separately blocked by clap refusing leading-`-` values.

**Fixed.** `is_git_source` now requires one of four transports (`http://`, `https://`, `ssh://`,
`git://`) and rejects anything carrying `::`, so a transport helper is not a git source regardless of
what the operator's gitconfig permits. Depending on someone else's default for the difference between
"clones a repository" and "executes a command" was the whole problem; the check costs one string
comparison.

### 7. Outbound-URL warning does not rank its targets

`warn_outbound_urls` lists every non-loopback webhook, alert sink and RPC URL a freshly installed
bundle declares, with credentials redacted — a good warning. It does not single out **link-local and
cloud-metadata addresses** (`169.254.169.254`, `fd00:ec2::254`), which are the high-value SSRF targets,
and it is warn-and-proceed with no way to refuse.

A bundle is fetched from a URL, so this is untrusted input; "the endpoint is the allowlist" holds only
as far as the operator reads the warning. Today's other findings suggest that is not far.

**Fixed.** `classify_outbound` ranks each URL and link-local/metadata targets are logged at `error`
while ordinary ones stay at `warn`, so the one line that matters is not the twelfth of twelve. Still
warn-and-proceed: refusing a bundle outright is a policy decision an operator should make, and a nest
legitimately pointing at a metadata address is imaginable. Classification is returned rather than only
logged, so a test asserts the ranking instead of asserting that a log line was formatted.

## Verified safe (and why that is not the same as "we made it safe")

- **Bundle symlink escape — refused.** A tar carrying a symlink out of the destination plus a file
  written through it is rejected by `tar-rs` ("trying to unpack outside of destination path"). Our
  `checked_join` is lexical and cannot see symlinks, so **the guarantee is the dependency's**. Now
  pinned by `a_bundle_cannot_escape_its_destination_through_a_symlink` so a version bump or a stray
  `set_overwrite` fails a test.
- **Token comparison is constant-time**, shared by the admin and control-plane surfaces; the control
  plane refuses to bind off-localhost without a token.
- **The `SELECT`/`WITH` gate holds** where it matters: forbidden functions in SELECT position, inside a
  CTE, and inside a scalar subquery are all refused.
- **Option injection into `git clone` is blocked** — by clap, not by us.

## Disclosure

Finding 1 is exploitable in released versions and warrants a **GHSA**, published together with
**GHSA-jvjx-5528-r6mm** (fixed in 0.6.2, still unpublished).

A project asking to be taken seriously at 1.0 does not fix security bugs silently. Two advisories
published together, with the fixed versions named, is a better first impression than a quiet patch —
and the second one is already overdue.

> **Resolved 2026-08-02.** Both were published together as recommended: **GHSA-393p-f3vr-rf2r**
> (arbitrary file read, quoted function name) and **GHSA-jvjx-5528-r6mm** (arbitrary file write,
> statement stacking), each naming its fixed version. Neither is listed in GitHub's *global* advisory
> database — that needs a package in a supported registry, and nuthatch ships as a binary rather than
> a crates.io package, so there is no coordinate to attach one to. A consequence of how we distribute,
> not an omission.
