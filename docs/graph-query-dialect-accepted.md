# The Graph query dialect a nest accepts

RFC-0053 S2 (#1266). What `/graphql` lowers to SQL, what it refuses, and why each refusal is a
refusal rather than an approximation.

**The governing rule is RFC-0053 §Design: "It must never silently approximate a value and call the
endpoint drop-in."** Applied to a query, that means an argument this compiler does not implement is an
error in the Graph envelope, never a query quietly run without it. A dropped `where` clause returns
*more* rows than the caller asked for, and a caller who cannot tell that from a correct answer is
worse off than one who got an error.

Every refusal below names the thing refused, so a caller can act on it.

## Accepted

### Roots

| shape | SQL |
|---|---|
| `pools { … }` | `SELECT … FROM "pool" ORDER BY "id" ASC LIMIT 100 OFFSET 0` |
| `pool(id: "0x…") { … }` | the same with `WHERE "id" = '0x…' LIMIT 1`, answered as an object |
| `_meta { block { number } … }` | not compiled at all: answered from the nest's own head |

The two names come from two different derivations, both taken from the live graph-node reference:
`lower_first` for the singular, `plural` for the collection. `modifyLiquidity` pluralises to
`modifyLiquidities`, and getting that wrong means the client's query names do not exist here.

### Arguments

`first` (default 100, **refused** outside 0..=1000 rather than clamped, so a caller who asked for
1,500 rows is told rather than quietly given 1,000), `skip` (default 0), `orderBy`, `orderDirection`,
`where`, `id` on a singular root, and `subgraphError`, which is **accepted and ignored**: a nest runs
no mapping, so `allow` and `deny` cannot differ.

### `where` operators

Bare, `_not`, `_gt`, `_gte`, `_lt`, `_lte`, `_in`, `_not_in`.

`_in []` lowers to `FALSE` and `_not_in []` to `TRUE`, because SQL has no empty `IN` list and the
empty set is a legitimate thing for a client to send. Suffixes are matched longest-first, so
`_not_in` is not read as `_not` followed by a field called `in`.

### Variables

A named operation with a variable definition list, `$name` in any argument position, values from the
request body's `variables`, and header defaults where the request omits one:

```graphql
query Pools($first: Int!, $min: BigInt = "5") {
  pools(first: $first, where: { liquidity_gt: $min }) { id }
}
```

This is the shape **every generated client sends**, so a compiler that refuses variables refuses in
practice every query a client produces, whatever it can do with a hand-written one.

The operation header is parsed rather than skipped to the first `{`. A variable default may itself be
an object - `$w: Pool_filter = { id: "0xaaa" }` - and skipping to the first brace lands inside it,
after which the whole operation reads as garbage.

## Refused, by name

| refused | why it is not approximated |
|---|---|
| `block:` / `block_gte:` | needs a block-ranged entity store the nest has not got (#1267). Answering as of head while the caller named a past block is a wrong answer that looks right |
| nested selections (`pools { token0 { symbol } }`) | lowers to a join, which is the next slice. An N+1 walk would answer, slowly and with a different transaction view per row |
| `_contains`, `_starts_with`, `_ends_with`, the `_nocase` family | `LIKE` with correct escaping wants its own slice and its own tests. A caller's `%` must not become a wildcard by accident |
| `and:` / `or:` | a nested boolean tree, and precedence got wrong silently changes which rows come back |
| fragments and `...` | not implemented |
| `orderBy` that traverses a relation (`token0__symbol`) | needs the same join as a nested selection |
| `mutation`, `subscription` | a nest has no mappings, so it has nothing to mutate and nothing to stream |
| directives on an operation | skipping one silently is the same class of mistake as a dropped filter |
| an unbound `$name` | neither the request nor the header supplies a value. Dropping the argument would widen the filter |
| a fractional number in `variables` | `BigInt` and `BigDecimal` travel as strings over GraphQL precisely because a float loses them, so a fractional JSON number is refused rather than rounded into a filter |

## What S2 is done when

- A canonical client query against the reference schema returns rows that match the reference
  deployment, modulo the declared divergence list.
- A mutation that drops any single argument, operator, default or bound above makes a test fail.
  `mut/m-s2-http.sh` is the current set.

## Where the surface is reachable

`POST /graphql`, `POST /subgraphs/id/{id}`, `POST /subgraphs/name/{*name}`. The three shapes exist so
an existing client's URL can be swapped host-for-host without rewriting the path.

Queries run through `run_sql_query`, so they inherit the node's admission bounds and row cap rather
than opening a second unmetered way into DuckDB.
