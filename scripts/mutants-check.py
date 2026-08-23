#!/usr/bin/env python3
"""Compare cargo-mutants survivors against the checked-in baseline (#768).

Exit 1 on a survivor that is not in `.github/mutants-baseline.toml`. Exit 0 otherwise, and say which
baseline entries no longer survive - a stale exemption is worth removing, and #769's allow-list check
caught exactly that shape the day after it was built.

The baseline is matched on (file, mutation-text) rather than line number, because a line number moves
every time somebody adds a comment above it and a gate that fires on unrelated edits is a gate people
learn to route around.
"""
import re
import sys
from pathlib import Path

MISSED = Path("mutants.out/missed.txt")
BASELINE = Path(".github/mutants-baseline.toml")


def baseline_entries(text):
    """(file, mutation) pairs. A deliberately small parser - the file is ours and its shape is fixed."""
    out = []
    for block in text.split("[[survivor]]")[1:]:
        f = re.search(r'^file\s*=\s*"([^"]+)"', block, re.M)
        m = re.search(r'^mutation\s*=\s*"([^"]+)"', block, re.M)
        if f and m:
            out.append((f.group(1), m.group(1)))
    return out


def survivors(text):
    """cargo-mutants writes `path:line:col: <mutation text>` per line."""
    out = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        m = re.match(r"^([^:]+):\d+:\d+:\s*(.+)$", line)
        if m:
            out.append((m.group(1), m.group(2).strip()))
    return out


def main():
    known = baseline_entries(BASELINE.read_text()) if BASELINE.is_file() else []
    found = survivors(MISSED.read_text()) if MISSED.is_file() else []

    new = [s for s in found if s not in known]
    gone = [k for k in known if k not in found]

    for f, m in gone:
        print(f"note: baseline entry no longer survives, remove it: {f}: {m}")

    if not new:
        print(f"No new survivors. {len(found)} survivor(s), all in the baseline with a reason.")
        return 0

    print()
    print(f"{len(new)} mutation(s) survived that are not in {BASELINE}:")
    for f, m in new:
        print(f"  {f}: {m}")
    print()
    print("A survivor means a test asserted something the code does not have to do to pass.")
    print("Either the behaviour wants a test, or the survivor wants an entry in the baseline with a")
    print("reason a reader can disagree with. Both are fine; silence is not.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
