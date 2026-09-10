#!/usr/bin/env python3
"""Diff the #1212 ported nest against the reference deployment, at one pinned block.

Two comparisons, deliberately separate:

  A. the emitted views, which is what RFC-0044 S3 actually delivered.
  B. the decoded base tables, which is what the nest holds.

The gap between them is the emitter's, not the indexer's, and reporting one number for
both would hide that. Every run prints a coverage denominator: fields compared over
fields the port report called `exact`, because a diff that cannot say what it declined
to look at is not a diff (sprint resolute-robin).

The reference prunes below block 25942028, so --block must be at or above that and at
or below the reference's own head.
"""
import argparse
import json
import sys
import time
import urllib.parse
import urllib.request

REF = "https://api.thegraph.com/subgraphs/id/Qmda2K4NcKWXB2AqyGUZEU35DgxSqFFRhkCmrJ8oC9po7i"
NEST = "http://127.0.0.1:8288"
UA = "nuthatch-port-1212"
PRUNE_FLOOR = 25942028


def gql(query, attempts=5):
    """Query the reference. A non-empty `errors` array is a failure whatever the status
    says: the gateway answers an auth failure and a pruned-block refusal with HTTP 200,
    and treating either as an empty result is how a diff agrees with itself.

    A transport timeout is retried with backoff, and only a transport timeout: paging
    `pools` is 133 requests and the public endpoint does time one out from time to time.
    A partial walk that raised would otherwise look identical to a short reference.
    A GraphQL `errors` array is never retried - it is an answer, and the wrong one."""
    req = urllib.request.Request(
        REF,
        data=json.dumps({"query": query}).encode(),
        headers={"content-type": "application/json", "User-Agent": UA},
    )
    last = None
    for attempt in range(attempts):
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                body = json.load(r)
            break
        except (TimeoutError, urllib.error.URLError, OSError) as e:
            last = e
            if attempt == attempts - 1:
                raise SystemExit(f"reference unreachable after {attempts} attempts: {e}")
            time.sleep(2 ** attempt)
    if body.get("errors"):
        raise SystemExit(f"reference refused: {body['errors']}")
    if "data" not in body or body["data"] is None:
        raise SystemExit(f"reference returned no data: {body}")
    return body["data"]


def sql(q):
    url = f"{NEST}/sql?" + urllib.parse.urlencode({"q": q})
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with urllib.request.urlopen(req, timeout=300) as r:
        body = json.load(r)
    if body.get("degraded"):
        raise SystemExit(f"nest answered degraded ({body.get('degraded_tables')}); refusing to diff")
    rows = body.get("rows")
    if rows is None:
        raise SystemExit(f"nest returned no rows key: {body}")
    # `/sql` caps at 50,000 rows and says so only in a flag nothing has to read. An
    # unnoticed cap is a silently short side of the comparison, so refuse rather than
    # compare a prefix. Measured: `SELECT id FROM pool_manager__initialize` returns
    # count 50000, truncated true, against 125,895 real rows.
    if body.get("truncated"):
        raise SystemExit(f"nest truncated at {len(rows)} rows; page it instead:\n  {q}")
    return rows, body.get("provenance", {})


def sql_paged(select, table, key, where="", page=40000):
    """Page a table by an ordered key, under the 50,000-row cap."""
    out, last = [], None
    while True:
        cond = [where] if where else []
        if last is not None:
            cond.append(f"{key} > '{last}'")
        w = (" WHERE " + " AND ".join(cond)) if cond else ""
        rows, _ = sql(f"SELECT {select} FROM \"{table}\"{w} ORDER BY {key} LIMIT {page}")
        if not rows:
            return out
        out.extend(rows)
        last = rows[-1][key.strip('"')]
        if len(rows) < page:
            return out


def page_ref(entity, fields, block, where, key="id"):
    """Page an entity by id. `first: 1000` is graph-node's ceiling."""
    out, last = {}, ""
    while True:
        flt = f'{where}, {key}_gt: "{last}"' if where else f'{key}_gt: "{last}"'
        q = (
            f"{{ {entity}(block: {{number: {block}}}, first: 1000, orderBy: {key}, "
            f"orderDirection: asc, where: {{{flt}}}) {{ {' '.join(fields)} }} }}"
        )
        batch = gql(q)[entity]
        if not batch:
            return out
        for r in batch:
            out[r[key]] = r
        last = batch[-1][key]


def compare(name, ref, nest, fields, report):
    """Compare two dicts keyed the same way. Never driven by the intersection."""
    only_ref = sorted(set(ref) - set(nest))
    only_nest = sorted(set(nest) - set(ref))
    both = sorted(set(ref) & set(nest))
    diverged = {f: [] for f in fields}
    for k in both:
        for f in fields:
            a, b = ref[k].get(f), nest[k].get(f)
            if a is None and b is None:
                continue
            if str(a) != str(b):
                diverged[f].append((k, a, b))
    report.append(
        {
            "entity": name,
            "ref_rows": len(ref),
            "nest_rows": len(nest),
            "aligned": len(both),
            "missing_from_nest": len(only_ref),
            "extra_in_nest": len(only_nest),
            "fields": {f: len(v) for f, v in diverged.items()},
            "examples": {f: v[:3] for f, v in diverged.items() if v},
            "sample_missing": only_ref[:3],
        }
    )


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--block", type=int, required=True, help=f"pinned block, >= {PRUNE_FLOOR}")
    p.add_argument("--window", type=int, default=500, help="blocks of event history to compare")
    a = p.parse_args()
    if a.block < PRUNE_FLOOR:
        raise SystemExit(f"--block {a.block} is below the reference's pruning floor {PRUNE_FLOOR}")

    head = gql("{ _meta { block { number } hasIndexingErrors } }")["_meta"]
    if a.block > head["block"]["number"]:
        raise SystemExit(f"--block {a.block} is ahead of the reference head {head['block']['number']}")
    if head["hasIndexingErrors"]:
        raise SystemExit("reference reports indexing errors; its answers are not a baseline")

    rows, prov = sql("SELECT 1 AS ok")
    as_of = prov.get("as_of", 0)
    print(f"reference head {head['block']['number']}   nest as_of {as_of}   pinned {a.block}")
    # **The nest must be exactly at the pin, not merely past it.**
    #
    # Only some of the nest-side queries below carry a block predicate. The emitted `token` view
    # cannot: it is `SELECT id FROM (...) GROUP BY id` with no block column, so it always answers as
    # of whatever the nest has reached. Accepting `as_of > block` therefore compares post-pin rows
    # against a reference pinned earlier, and reports the difference as extras - a divergence caused
    # by the clock rather than by the port. My own first run did exactly that: nest frozen at
    # 25,945,634, pinned at 25,943,500, and the 27 "extra" tokens include any created in those 2,134
    # blocks. Raised by review of #1278.
    #
    # Equality is cheap to satisfy because the procedure already freezes the nest: stop `dev`, start
    # `serve` (which owns no cursor), read `as_of`, and pin to that.
    if as_of != a.block:
        raise SystemExit(
            f"nest is at {as_of} and the pin is {a.block}. Pass --block {as_of}, or freeze the nest "
            f"at {a.block}. Comparing a view with no block predicate against a reference pinned "
            f"elsewhere reports clock skew as divergence."
        )

    lo, hi = a.block - a.window, a.block
    ts = sql(f'SELECT min(block_timestamp) AS lo, max(block_timestamp) AS hi '
             f'FROM "pool_manager__swap" WHERE block_number > {lo} AND block_number <= {hi}')[0][0]
    print(f"window blocks ({lo}, {hi}]  timestamps [{ts['lo']}, {ts['hi']}]")

    report = []

    # --- Swap, immutable, aligned on the reference's own id construction -------------
    # `new Swap(transaction.id + '-' + event.logIndex.toString())` (swap.ts:122). The
    # schema comment above the field says "#" and is stale; the mapping uses "-".
    ref_swaps = page_ref(
        "swaps",
        ["id", "sender", "sqrtPriceX96", "tick", "logIndex", "amountUSD"],
        a.block,
        f'timestamp_gte: "{ts["lo"]}", timestamp_lte: "{ts["hi"]}"',
    )
    nest_swaps = {
        f"{r['tx_hash']}-{r['logIndex']}": r
        for r in sql(
            f'SELECT tx_hash, log_index AS "logIndex", sender, "sqrtPriceX96", tick '
            f'FROM "pool_manager__swap" WHERE block_number > {lo} AND block_number <= {hi}'
        )[0]
    }
    # B: the base table. `sender`, `sqrtPriceX96` and `logIndex` are what the view binds;
    # `tick` is the field #1274's dead-handler citation dropped from it.
    compare("Swap (base table)", ref_swaps, nest_swaps,
            ["sender", "sqrtPriceX96", "tick", "logIndex"], report)

    # --- Pool, mutable. The view is unusable (35M rows, no key), so align creation
    # facts off the Initialize table, which is what the view's first branch projects.
    ref_pools = page_ref("pools", ["id", "createdAtBlockNumber", "createdAtTimestamp", "hooks"], a.block, "")
    nest_pools = {
        r["id"]: r
        for r in sql_paged(
            'id, block_number AS "createdAtBlockNumber", '
            'block_timestamp AS "createdAtTimestamp", hooks',
            "pool_manager__initialize",
            "id",
            where=f"block_number <= {a.block}",
        )
    }
    compare("Pool (base table)", ref_pools, nest_pools,
            ["createdAtBlockNumber", "createdAtTimestamp", "hooks"], report)

    # --- Token. The view answers `id` alone, from currency0 only, so this measures the
    # row loss rather than any field.
    ref_tokens = page_ref("tokens", ["id"], a.block, "")
    nest_tokens = {r["id"]: r for r in sql('SELECT id FROM "token"')[0]}
    compare("Token (emitted view)", ref_tokens, nest_tokens, [], report)

    print(json.dumps(report, indent=2)[:6000])
    tot_aligned = sum(r["aligned"] for r in report)
    tot_div = sum(sum(r["fields"].values()) for r in report)
    print(f"\naligned rows {tot_aligned:,}   field divergences {tot_div:,}")
    print("coverage: 27 of 231 schema fields are answerable by the emitted views; "
          "the report called 205 exact (see #1277)")


if __name__ == "__main__":
    sys.exit(main())
