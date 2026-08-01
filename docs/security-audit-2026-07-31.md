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
| 1 | Arbitrary file read via `/sql` — a **quoted function name** evaded the denylist | **High** | **Fixed** |
| 2 | `duckdb_settings()` / `duckdb_extensions()` disclose absolute paths incl. OS username | Low–Medium | Open |
| 3 | The `allowed_directories` lockdown is **empty** — the documented second layer is not engaged | Informational | Open |
| 4 | Extension-gated file readers are absent from the denylist | Latent | Open |
| 5 | The denylist is an allowlist-shaped problem | Design | Open |
| 6 | `init --from` relies on git's `protocol.ext.allow` default | Defence-in-depth | Open |
| 7 | Bundle outbound-URL warning does not distinguish link-local / metadata targets | Hardening | Open |

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

### 3. The second layer is not engaged

`allowed_directories = "[]"` confirms what the code only suspected. `analytics.rs` says the directory
lockdown is *"defense-in-depth (its runtime enforcement is version-dependent in the bundled DuckDB)"*.
It is **not enforcing at all**, so the denylist is not the primary control — it is the *only* one.

That matters mostly for how finding 1 should be read: there was no second layer to catch it.

### 4. Extension-gated readers are absent from the denylist

`read_xlsx`, `st_read`, `iceberg_scan`, `delta_scan`, `postgres_scan`, `sqlite_scan` all pass the
guards. They fail today only because those extensions are not in the bundled build — i.e. we are safe
by build configuration, not by policy. Bundling any extension, or a DuckDB release that promotes one to
core, converts this to a live file read with no code change on our side.

### 5. The denylist is an allowlist-shaped problem

Findings 1 and 4 are the same root cause seen twice: a **denylist over an evolving vocabulary**. It has
now been wrong about *spelling* (1) and about *coverage* (4), and every DuckDB release can add a table
function that touches the filesystem or network. The failure mode is silent and the feedback loop is
"someone exploits it".

Worth considering before 1.0 whether the guard should invert — parse the statement and permit only
known-safe table references, rather than enumerating the unsafe ones. That is a larger change than a
fix and should be argued on its own terms, but it is the finding with the longest tail.

### 6. `init --from` relies on git's default

`is_git_source` accepts anything ending in `.git`, so `ext::sh -c … .git` reaches `git clone`. Git's
`ext::` transport executes commands; the attempt is refused only by git's own
`protocol.ext.allow=never` default (git ≥ 2.12).

**An operator with `protocol.ext.allow=always` in their gitconfig — not unheard of in CI images — turns
`nuthatch init --from <url>` into remote code execution.** Validating the URL scheme ourselves costs
nothing and removes the dependence.

Option injection (`--upload-pack=…`) is separately blocked by clap refusing leading-`-` values.

### 7. Outbound-URL warning does not rank its targets

`warn_outbound_urls` lists every non-loopback webhook, alert sink and RPC URL a freshly installed
bundle declares, with credentials redacted — a good warning. It does not single out **link-local and
cloud-metadata addresses** (`169.254.169.254`, `fd00:ec2::254`), which are the high-value SSRF targets,
and it is warn-and-proceed with no way to refuse.

A bundle is fetched from a URL, so this is untrusted input; "the endpoint is the allowlist" holds only
as far as the operator reads the warning. Today's other findings suggest that is not far.

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
