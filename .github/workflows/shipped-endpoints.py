#!/usr/bin/env python3
"""Print `<chain>\t<rpc url>` for every endpoint `src/chains.rs` actually ships.

`live-endpoints.yml` probes what this prints. It used to carry its own copy of the list, and a copy
goes stale in both directions: on 2026-09-07 (#1196) the workflow was still probing three endpoints
that had been removed from the registry weeks earlier, and probing none of the five that BSC,
Optimism and Gnosis ship. One of those five had been unable to serve a backfill for at least
nineteen days with nothing to notice.

Parsing Rust with a regex is a small sin, and the alternative - a `--list-endpoints` flag - is
product surface added for CI's benefit. The shape here is fixed by `Chain`'s definition, and the
workflow fails loudly if a chain yields no endpoints, so a parser that breaks reds the job rather
than quietly judging nothing.
"""

import re
import sys

CHAIN = re.compile(r"const\s+\w+:\s*Chain\s*=\s*Chain\s*\{(.*?)\n\};", re.S)
NAME = re.compile(r'name:\s*"([^"]+)"')
URLS = re.compile(r"rpc_urls:\s*&\[(.*?)\]", re.S)
# A line comment, and not the `//` inside `https://`.
COMMENT = re.compile(r"(?<!:)//.*$")
URL = re.compile(r'"(https://[^"]+)"')


def main() -> int:
    source = open("src/chains.rs", encoding="utf-8").read()
    rows = 0
    for chain in CHAIN.finditer(source):
        body = chain.group(1)
        name = NAME.search(body)
        urls = URLS.search(body)
        if not name or not urls:
            continue
        for line in urls.group(1).splitlines():
            for url in URL.findall(COMMENT.sub("", line)):
                print(f"{name.group(1)}\t{url}")
                rows += 1
    if rows == 0:
        print("no endpoints extracted from src/chains.rs", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
