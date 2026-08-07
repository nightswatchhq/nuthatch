# views/ - this nest's authored logic (RFC-0018 §1)

Drop `*.sql` files here, each a `CREATE VIEW …` over your nest's tables (the live tip ∪ sealed
history, one surface - recomputed per query, never materialised). Query a view by name with
`nuthatch sql` or the MCP `sql` tool. Files load in sorted filename order (`10-…`, `20-…`), so
a later view can build on an earlier one. Describe what a view *means* in `semantic.toml` under
`[view.<name>]` so an agent sees it. A broken/drifted view fails `nuthatch check` loudly.
