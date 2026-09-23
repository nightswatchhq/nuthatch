# RFC-0061: 256-bit values stay decimal text - the C1b decision, measured

**Status:** **Accepted 2026-09-23 (Chief).** The decision `durable-dipper` item 2 asks for. Its only
deliverable, the per-engine conversion in `reading-segments.md`, shipped with it in #1447.
Tracking #1222.

**Date:** 2026-09-21

**Author:** Pete (cargopete)

**Depends on:** RFC-0047 (C1, C2 and §6), RFC-0052 (the mirrored nest, and its rule that an untested
recipe is not listed as supported), RFC-0055 (the Dune emitter), and
[reading-segments.md](../reading-segments.md).

**Answers:** RFC-0047 §2.1, *"What is a format version, and is not implied by the rest"*, and §6,
*"Whether FLBA32 is worth a format version at all"*.

**Nature:** a decision **not** to change the segment format. No writer, reader or catalogue change.
One documentation change (§4).

## Decision

1. **The physical type of 256-bit values does not change.** Canonical decimal text in `Utf8` stays
   the storage contract. There is no segment-format version, no new `writer_profile` and no
   `manifest_version` bump.
2. That answers #1222's three questions directly:
   - **The new type** is none.
   - **Old segments** are read exactly as they are today.
   - **An external reader crosses no boundary.**
3. **The gap RFC-0047 found is closed by documentation, not bytes.** That gap is that the checked
   projection (`c_dec`, `c_overflow`) lives only in DuckDB views. A published mirror already ships
   `schema.json` with its hash, and that says which text columns are 256-bit integers.
   `reading-segments.md` gains the checked cast for each engine a recipe has actually been run on
   (§4).
4. **Any later physical change must meet the conditions in §5,** so the question is not reopened
   from nothing.

## §1 The measurement

Run on 2026-09-21 by `tools/rfc-0061-u256-encodings`, whose output is in its `results.txt`.

- **Crates:** nuthatch's own `arrow` and `parquet` 58.3.0, writing ZSTD level 3.
- **Data:** two real columns from the Arbitrum Network corpus: `graph_token__approval.value` (2,843,980
  rows) and `graph_token__transfer.value` (6,587,086 rows).
- **Read-back:** DuckDB **1.5.4**, the version nuthatch bundles.

Each column was written four ways:

| | Encoding |
|---|---|
| A | today's `Utf8` decimal text |
| B | `FIXED_LEN_BYTE_ARRAY(32)`, big-endian, no logical type |
| C | Parquet `DECIMAL(76,0)` on 32 bytes, the widest decimal 32 bytes can hold |
| D | A plus RFC-0047's view columns made physical: `DECIMAL(38,0)` and a boolean overflow flag |

**The data first.**

- **Approvals:** 1,333,963 values (46.9%) have more than 38 digits, and **1,324,091 (46.6%) have
  more than 76.** 404,331 are exactly `2^256 − 1`: the unlimited-allowance sentinel, and the
  allowance left after spending from one.
- **Transfers:** not one of the 6.59M values exceeds 38 digits.

**Size, relative to A:**

| | approvals | transfers |
|---|---|---|
| B FLBA32 | 0.87x | 0.87x |
| C `DECIMAL(76,0)` | 0.37x, **because 46.6% of the column cannot be stored and is null** | 0.87x |
| D text + `DECIMAL(38,0)` + flag | 1.37x | **1.86x** |

**What DuckDB 1.5.4 does on read:**

| | Reads as | Consequence |
|---|---|---|
| A | `VARCHAR` | exact; arithmetic by checked cast |
| B | `BLOB` | exact and byte-ordered; **no arithmetic at all** |
| C | **`DOUBLE`** | **all 6,587,086 transfer values changed on read**, e.g. `18024260660084595035534` became `1.8024260660084594e+22` |
| D | `VARCHAR` + `DECIMAL(38,0)` | exact; `SUM` of the companion is native and exact |

**Footer statistics** (`min_value` / `max_value`):

- **A** records the lexicographic maximum `999…9` (54 digits), which is numerically wrong.
- **B** records `0x00…` to `0xFF…`, which is numerically right.
- **D's companion** records a correct numeric range, while its text column in the same file records
  the wrong one.

## §2 The options, against the measurement

| | Lossless for all of `uint256` | Readable by any engine without a UDF | Size (approvals / transfers) | Numeric statistics |
|---|---|---|---|---|
| **A text** (today) | yes | yes, as text; checked cast to `DECIMAL(38,0)` | 1.00 / 1.00 | no |
| B FLBA32 | yes (unsigned; signed values need an offset encoding to sort) | as binary only | 0.87 / 0.87 | yes |
| C `DECIMAL(76,0)` | **no: 46.6% of approvals** | read as `DOUBLE`, lossy | n/a | yes |
| D text + companions | yes (the text) | yes, natively up to 38 digits | 1.37 / 1.86 | yes, for the companion |

## §3 Why A

- **It is the only encoding that is both lossless over the whole range and readable everywhere
  without a user-defined function.** Every engine RFC-0052 names can read text, and every one has a
  checked cast.
- **B buys numeric statistics and 13% of one column,** and pays with a format version, a dual-read in
  every external reader, and the loss of arithmetic in nuthatch's own engine. The statistics would
  rarely prune anything: `zstd-bloom-v1` sorts rows by `(block_number, log_index)`, so a segment's
  value range is close to the whole range.
- **C fails twice.** It cannot hold nearly half of a real allowance column, and DuckDB turns every
  value it does hold into a float.
- **D stores a cast.** Nuthatch's query layer already derives `c_dec` and `c_overflow` from the text.
  So D's bytes, 1.86x on transfer amounts, serve only external readers, all of whom have `TRY_CAST`.
- **Overflow is concentrated, not general.** On this corpus `c_overflow` is essentially the
  unlimited-allowance sentinel; amounts fit `DECIMAL(38,0)`. That is worth saying on the page readers
  use, and §4 says it.

## §4 What changes: one page

`reading-segments.md`'s section on `c_dec` and `c_overflow` gains three things:

- the checked cast for **DuckDB** (`TRY_CAST(c AS DECIMAL(38,0))`, verified here);
- the cast for **DuneSQL**: `cast(c as uint256)`, or `int256` for a signed column. RFC-0055's emitter
  already generates it, and it is exact over the whole range because DuneSQL's 256-bit types hold
  every value;
- RFC-0052 S5's rule restated: a recipe for another engine is listed once it has been run, not
  before;
- the observation that overflow concentrates in allowance columns, where `2^256 − 1` is a sentinel.

Nothing else changes. RFC-0047's status line records C1b as decided by this RFC once it is accepted,
and #1222 closes.

## §5 What any later physical change must meet

This decision holds until one of three things happens:

1. **A named external consumer needs native numeric 256-bit columns and cannot cast.** Then the shape
   is D, because it is the only one that stays lossless and native.
2. **A real query is shown to need value-range pruning on a 256-bit column.**
3. **Parquet gains a 256-bit integer logical type that DuckDB reads losslessly.**

Whatever triggers it, the change is made this way, which is the `durable-dipper` rule turned into a
checklist:

- **Per segment:** a new `writer_profile` name, not a `manifest_version` bump. That is how the
  catalogue already records a change of bytes.
- **Additive:** the exact text column stays in every segment, so an old segment and a new one are
  both read by the text.
- **Readers first:** the reader recipe in `reading-segments.md`, and `read_table_rows`, handle both
  profiles before the first writer emits the new one.
- **Nothing already sealed changes.**

## §6 Unresolved

- **Signed columns.** The corpus measured here has no `int256` column. The text rule is sign-agnostic,
  so under A this is moot. It becomes live only under B.
- **DuckDB's own decimal ceiling.** DuckDB's widest decimal is 38 digits, so even nuthatch's query
  layer cannot hold a full 256-bit value natively. That is RFC-0047 §5's "native 256-bit arithmetic",
  still out of scope and still engine work.
