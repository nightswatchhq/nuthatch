# graph-node's generated schema, derived from a live reference

RFC-0053 S1 (#1265). What `nuthatch` must generate from an imported `schema.graphql` so that a
generated client validates against it.

**Every rule below was extracted mechanically from a recorded introspection of a real graph-node**,
not inferred from documentation. The reference is
`tests/fixtures/graph-node/graph-node-introspection-uniswap-v4.json`, captured 2026-09-10 from
deployment `Qmda2K4NcKWXB2AqyGUZEU35DgxSqFFRhkCmrJ8oC9po7i` (Uniswap V4, Ethereum mainnet, 19
entities, 231 fields). Re-record it with the query in `## Re-recording` below; the golden test diffs
against it.

Totals for that schema: **80 types** - 25 `OBJECT`, 24 `ENUM`, 21 `INPUT_OBJECT`, 10 `SCALAR` - and
**40 `Query` root fields**.

## Scalars

Ten, of which five are graph-node's own additions on top of GraphQL's built-ins:

| built-in | graph-node adds |
|---|---|
| `String`, `Int`, `Float`, `Boolean`, `ID` | `BigInt`, `BigDecimal`, `Bytes`, `Int8`, `Timestamp` |

## Non-entity object types

`Query`, `_Meta_`, `_Block_`, `_Log_`, `_LogMeta_`, `_LogArgument_`.

```graphql
type _Meta_ { block: _Block_  deployment: String  hasIndexingErrors: Boolean }
```

## Query root

For every `@entity` type `E`, two fields - and note the two different name derivations:

```graphql
e(id: ID!, block: Block_height, subgraphError: _SubgraphErrorPolicy_ = deny): E
es(skip: Int = 0, first: Int = 100, orderBy: E_orderBy, orderDirection: OrderDirection,
   where: E_filter, block: Block_height, subgraphError: _SubgraphErrorPolicy_ = deny): [E!]!
```

Plus `_meta(block: Block_height): _Meta_` and `_logs`. For 19 entities: 19x2 + 2 = 40.

**Pluralisation is a real rule, not `+ "s"`.** Observed: `modifyLiquidity` → `modifyLiquidities`,
`poolDayData` → `poolDayDatas`, `poolHourData` → `poolHourDatas`, `subscribe` → `subscribes`,
`transfer` → `transfers`, `pool` → `pools`. A `y` after a consonant becomes `ies`. Getting this wrong
means the client's generated query names do not exist on our schema, which is a hard failure before a
single row is read.

**`first` defaults to 100 and `skip` to 0.** RFC-0053 §Query semantics says the maximum is 1000.

## Filters: `E_filter`

One input object per entity. Operator sets are **per scalar type and they are not uniform** - this is
the part most likely to be got wrong by generalising from `String`.

| field type | operators |
|---|---|
| `BigInt`, `BigDecimal`, `ID` | bare, `_not`, `_gt`, `_gte`, `_lt`, `_lte`, `_in`, `_not_in` |
| `Bytes` | bare, `_not`, `_gt`, `_gte`, `_lt`, `_lte`, `_contains`, `_not_contains`, `_in`, `_not_in` |
| `Boolean` | bare, `_not`, `_in`, `_not_in` |
| `String` | all of the numeric set, **plus** `_contains`, `_contains_nocase`, `_not_contains`, `_not_contains_nocase`, `_starts_with`, `_starts_with_nocase`, `_not_starts_with`, `_not_starts_with_nocase`, `_ends_with`, `_ends_with_nocase`, `_not_ends_with`, `_not_ends_with_nocase` |

**`Bytes` is not `String`.** It has no `_starts_with`, no `_ends_with` and no `_nocase` variant
anywhere. `String` carries 18 operators; `Bytes` carries 10. Measured on `Swap.sender` and
`Swap.origin`, both `Bytes`.

`_in` and `_not_in` take the **list** form of the field type (`[BigInt]`, `[Bytes]`, `[String]`, …).

A field that references another entity gets both a scalar comparison on the id **and** a nested filter:

```graphql
token0: String        token0_: Token_filter        # plus the String operator set on token0_*
```

And every filter carries:

```graphql
and: [E_filter]   or: [E_filter]   _change_block: BlockChangedFilter
```

## Ordering: `E_orderBy`

An enum over the entity's own fields **and one level of relation traversal**, joined by a double
underscore. `Pool_orderBy` has **65 values: 35 plain and 30 nested** - `token0__id`,
`token0__symbol`, `token0__decimals`, `token0__volume` and so on.

One level only. There is no `token0__whitelistPools__id`.

## Supporting types

```graphql
input Block_height { hash: Bytes  number: Int  number_gte: Int }
input BlockChangedFilter { number_gte: Int }
enum OrderDirection { asc  desc }
enum _SubgraphErrorPolicy_ { allow  deny }
```

## `@derivedFrom` fields

Rendered as a list with collection arguments, but **no `block`** - a derived field inherits the
parent query's block:

```graphql
Transaction.swaps(skip: Int, first: Int, orderBy: Swap_orderBy,
                  orderDirection: OrderDirection, where: Swap_filter): [Swap!]!
```

**The field name is the schema author's, not the pluraliser's.** The reference schema declares
`modifyLiquiditys` and graph-node renders `Transaction.modifyLiquiditys` - a plain `s` - while the
*root* field for the same entity is `modifyLiquidities`. Two different naming rules in one schema, and
conflating them breaks either the root or the traversal.

## What S1 is done when

- Generated introspection diffs clean against the recorded reference, modulo a reviewed,
  machine-readable divergence list (RFC-0053 §Acceptance).
- A mutation that changes any single operator, default, pluralisation or nesting depth above makes
  that diff fail.

## Re-recording

```sh
curl -s -X POST https://api.thegraph.com/subgraphs/id/<CID> \
  -H 'content-type: application/json' -H 'User-Agent: nuthatch-s1' \
  --data-binary @tests/fixtures/graph-node/introspection-query.gql.json
```

No API key is needed for that endpoint. It answers an auth failure and a pruned-block refusal with
HTTP **200** and the error in the body, so check for an `errors` array rather than the status.
