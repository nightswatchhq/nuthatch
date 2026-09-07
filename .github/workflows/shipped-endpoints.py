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
    chains = 0
    for chain in CHAIN.finditer(source):
        chains += 1
        body = chain.group(1)
        name = NAME.search(body)
        urls = URLS.search(body)
        # A `Chain` block this cannot read is a hard failure, never a skip. Skipping is the shape of
        # bug this whole change exists to remove: the omitted chain vanishes from the probe list *and*
        # from the coverage gate that is supposed to notice a chain nobody probes, so the job goes
        # green while a shipped chain is unprobed. If a registry entry ever expresses its endpoints
        # through a constant or a macro, this stops the run and someone teaches it that shape.
        if not name or not urls:
            print(
                f"cannot read the Chain block at offset {chain.start()} in src/chains.rs: "
                f"{'no name' if not name else 'no rpc_urls'}. Teach this script that shape rather "
                "than letting the chain drop out of the probe list.",
                file=sys.stderr,
            )
            return 1
        found = 0
        for line in urls.group(1).splitlines():
            for url in URL.findall(COMMENT.sub("", line)):
                print(f"{name.group(1)}\t{url}")
                found += 1
        if found == 0:
            print(
                f"{name.group(1)} declares rpc_urls and this script read none of them",
                file=sys.stderr,
            )
            return 1
        rows += found
    if chains == 0 or rows == 0:
        print("no chains extracted from src/chains.rs", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
