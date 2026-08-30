#!/usr/bin/env python3
"""RFC-0042 slice 0: attribute clean-build time to dependency groups, from a cargo --timings report.

#975: this used to be four lines that read one developer's absolute path and picked whichever
timing file sorted last. Both are ways of publishing a number nobody else can reproduce or check:

  * `/home/pepe/bom/target/cargo-timings` exists on exactly one machine, so anyone else got a
    `FileNotFoundError` and anyone running it *on* that machine got whatever happened to be there;
  * `sorted(...)[-1]` is a lexicographic pick, not a chosen run. `cargo-timing-<ts>.html` names sort
    by timestamp only while the format stays fixed, and `cargo-timing.html` - the unsuffixed copy
    cargo also writes - sorts before every dated one, so the "latest" could be an old file.

The numbers this produces are cited in RFC-0042 §14 and slice 0's BOM, so provenance is part of the
output rather than something to reconstruct afterwards: the selected file, its mtime, its size and
the rustc/commit it came from are printed with the table.

Usage:
    scripts/bom-timings.py <path to cargo-timing-*.html>
    scripts/bom-timings.py --dir <cargo-timings dir>     # newest by mtime, and says which
    scripts/bom-timings.py --json                        # machine-readable, for a doc generator

With no argument it looks in `$CARGO_TARGET_DIR/cargo-timings` then `target/cargo-timings`,
relative to the repository this script lives in - never an absolute path belonging to one person.
"""

import argparse
import datetime as _dt
import json
import os
import pathlib
import re
import subprocess
import sys

# Attribution groups. Order matters only for reading; a unit may match several predicates and is
# counted in each, which is why the shares do not sum to 100%.
GROUPS = {
    "duckdb (incl. libduckdb-sys)": lambda n: "duckdb" in n,
    "wasmtime + cranelift + wasm*": lambda n: n.startswith(
        ("wasmtime", "cranelift", "wasm", "wast", "wit-")
    ),
    "dbsp + feldera": lambda n: n.startswith(("dbsp", "feldera")),
    "arrow + parquet": lambda n: n.startswith(("arrow", "parquet")),
    "ring / zstd / mimalloc / ittapi": lambda n: n.startswith(
        ("ring", "zstd", "mimalloc", "ittapi")
    ),
}


def repo_root() -> pathlib.Path:
    return pathlib.Path(__file__).resolve().parent.parent


def default_dirs() -> list[pathlib.Path]:
    root = repo_root()
    out = []
    if target := os.environ.get("CARGO_TARGET_DIR"):
        out.append(pathlib.Path(target) / "cargo-timings")
    out.append(root / "target" / "cargo-timings")
    return out


def pick_report(explicit: str | None, directory: str | None) -> pathlib.Path:
    if explicit:
        p = pathlib.Path(explicit)
        if not p.is_file():
            sys.exit(f"not a file: {p}")
        return p

    dirs = [pathlib.Path(directory)] if directory else default_dirs()
    for d in dirs:
        if not d.is_dir():
            continue
        # By mtime, not by name: a lexicographic pick is not a choice, and `cargo-timing.html`
        # (unsuffixed) sorts before every dated file.
        reports = sorted(d.glob("cargo-timing-*.html"), key=lambda f: f.stat().st_mtime)
        if reports:
            return reports[-1]
    sys.exit(
        "no cargo-timing-*.html found in "
        + ", ".join(str(d) for d in dirs)
        + "\nRun `cargo build --release --locked --timings` first, or pass a path explicitly."
    )


def provenance(report: pathlib.Path) -> dict:
    st = report.stat()
    out = {
        "report": str(report),
        "mtime_utc": _dt.datetime.fromtimestamp(st.st_mtime, _dt.timezone.utc).isoformat(),
        "bytes": st.st_size,
    }
    for key, cmd in (
        ("commit", ["git", "-C", str(repo_root()), "log", "--oneline", "-1"]),
        ("rustc", ["rustc", "--version"]),
    ):
        try:
            out[key] = subprocess.run(
                cmd, capture_output=True, text=True, timeout=10, check=True
            ).stdout.strip()
        except Exception:
            out[key] = "unavailable"
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("report", nargs="?", help="path to a cargo-timing-*.html")
    ap.add_argument("--dir", help="directory of cargo-timing reports; newest by mtime is used")
    ap.add_argument("--json", action="store_true", help="emit JSON instead of a table")
    args = ap.parse_args()

    report = pick_report(args.report, args.dir)
    meta = provenance(report)

    m = re.search(r"UNIT_DATA = (\[.*?\]);", report.read_text(), re.S)
    if not m:
        sys.exit(f"{report} has no UNIT_DATA block - is it a cargo --timings report?")
    data = json.loads(m.group(1))
    if not data:
        sys.exit(f"{report} contains no build units")
    total = sum(u["duration"] for u in data)
    if total <= 0:
        sys.exit(f"{report} reports zero total build time")

    rows = []
    for name, pred in GROUPS.items():
        units = [u for u in data if pred(u["name"])]
        secs = sum(u["duration"] for u in units)
        rows.append(
            {
                "group": name,
                "seconds": round(secs, 1),
                "share_pct": round(100 * secs / total, 1),
                "units": len(units),
            }
        )

    if args.json:
        print(json.dumps({"provenance": meta, "total_seconds": round(total), "unit_count": len(data), "groups": rows}, indent=2))
        return

    print(f"report:  {meta['report']}")
    print(f"written: {meta['mtime_utc']}  ({meta['bytes']} bytes)")
    print(f"commit:  {meta['commit']}")
    print(f"rustc:   {meta['rustc']}")
    print()
    print("%-34s %9s %7s  %s" % ("group", "seconds", "share", "units"))
    for r in rows:
        print("%-34s %9.1f %6.1f%%  %d" % (r["group"], r["seconds"], r["share_pct"], r["units"]))
    print()
    print("total summed unit time: %.0fs across %d units" % (total, len(data)))
    print("note: groups overlap, so shares do not sum to 100%")


if __name__ == "__main__":
    main()
