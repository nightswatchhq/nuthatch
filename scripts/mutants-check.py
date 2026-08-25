#!/usr/bin/env python3
"""Compare cargo-mutants survivors against the checked-in baseline (#768).

Exit 1 on a survivor that is not in `.github/mutants-baseline.toml`. Exit 0 otherwise, and say which
baseline entries no longer survive - a stale exemption is worth removing, and #769's allow-list check
caught exactly that shape the day after it was built.

The baseline is matched on (file, mutation-text) rather than line number, because a line number moves
every time somebody adds a comment above it and a gate that fires on unrelated edits is a gate people
learn to route around.

**A missing or truncated run is a failure, not a pass (#841).** This script used to read an absent
`missed.txt` as an empty survivor list and print "No new survivors", so a `cargo mutants` that never
ran, died, or was killed at the job timeout reported a clean bill of health. Every nightly run
between 2026-08-23 and 2026-08-25 was cancelled at `timeout-minutes: 300`, and this script is the
reason nobody found out from the job itself. "I could not tell" must never render as "nothing found".

Usage:
    mutants-check.py [--file src/foo.rs]

`--file` scopes both the survivors and the baseline entries to one source file, so a matrix job that
mutates only `src/seal.rs` does not report every `src/chunker.rs` baseline entry as newly stale.
"""
import argparse
import json
import re
import sys
from pathlib import Path

OUT = Path("mutants.out")
MISSED = OUT / "missed.txt"
OUTCOMES = OUT / "outcomes.json"
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


def run_is_complete():
    """Did cargo-mutants finish, and did it actually mutate anything?

    Returns (ok, reason). A truncated sweep - killed at the job timeout, or one that enumerated zero
    mutants because a `--file` path stopped matching after a rename - is indistinguishable from a
    clean one by `missed.txt` alone: both are empty.
    """
    if not OUTCOMES.is_file():
        return False, f"{OUTCOMES} is absent - cargo-mutants did not get far enough to write it"
    try:
        d = json.loads(OUTCOMES.read_text())
    except (json.JSONDecodeError, OSError) as e:
        return False, f"{OUTCOMES} is unreadable ({e}) - treating the run as untrustworthy"
    total = d.get("total_mutants")
    if not total:
        return False, (
            f"{OUTCOMES} reports total_mutants={total!r}. A sweep that enumerated no mutants proves "
            "nothing - check the --file paths still match the tree."
        )
    recorded = len(d.get("outcomes") or [])
    if d.get("end_time") is None:
        return False, (
            f"{OUTCOMES} has no end_time: cargo-mutants was killed before it finished "
            f"({recorded} scenario(s) recorded of {total} mutant(s)). The results are a prefix, not "
            "a verdict - raise the job timeout or narrow the scope."
        )
    return True, f"{total} mutant(s), run completed"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--file", help="scope survivors and baseline entries to this source file")
    args = ap.parse_args()

    ok, reason = run_is_complete()
    if not ok:
        print(f"FAIL: {reason}")
        print()
        print("This is a failure and not a pass. An absent or truncated mutation run means the")
        print("coverage question was not answered - it does not mean the answer was yes.")
        return 1
    print(f"run: {reason}")

    if not MISSED.is_file():
        print(f"FAIL: {MISSED} is absent although the run completed - cargo-mutants always writes it.")
        return 1

    known = baseline_entries(BASELINE.read_text()) if BASELINE.is_file() else []
    found = survivors(MISSED.read_text())
    if args.file:
        known = [k for k in known if k[0] == args.file]
        found = [s for s in found if s[0] == args.file]
        print(f"scope: {args.file}")

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
