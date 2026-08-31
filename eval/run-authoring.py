#!/usr/bin/env python3
"""Run RFC-0017's authoring evaluation: can an agent build a working nest from nothing?

RFC-0016 §1 measures **runtime** knowledge - an agent with MCP tools querying a nest that already
exists. This measures **authoring** knowledge: the builder skill plus a shell, a contract address,
and nothing else. An agent with only MCP cannot scaffold a nest; an agent with only the skill cannot
say what a table means as of block N.

RFC-0017 fixes the three criteria and this file does not reinvent them: `init` succeeds, `dev`
reaches the pinned tip, one canned question answers correctly - "scored mechanically (exit codes +
result comparison)". Nothing here reads the agent's prose. Three facts about the filesystem and one
result set.

The board is proven before anyone plays on it: `tests/authoring_eval_board.rs` walks this same
scenario with a scripted reference solution and must be green in CI first. A 0/3 against an
unproven board is a number about nothing.

Fully offline: `scripts/fixture_rpc.py` serves the chain over loopback and the ABI is a local file.

    python3 eval/run-authoring.py --nuthatch target/release/nuthatch --runs 3
    python3 eval/run-authoring.py --self-test          # no key, no model, no network
"""

import argparse
import json
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCENARIO = ROOT / "eval" / "authoring.toml"
REPORT = ROOT / "eval" / "authoring-report.json"

ERC20_TRANSFER_ABI = json.dumps([{
    "anonymous": False, "name": "Transfer", "type": "event",
    "inputs": [
        {"indexed": True, "name": "from", "type": "address"},
        {"indexed": True, "name": "to", "type": "address"},
        {"indexed": False, "name": "value", "type": "uint256"},
    ],
}])


def die(message: str) -> None:
    raise SystemExit(f"authoring runner: {message}")


class ScoringUnavailable(RuntimeError):
    """The scorer could not obtain a verdict, as distinct from obtaining a failing one.

    The same distinction #1051 drew for the RFC-0016 runner, made here on the first day rather than
    after a published zero: an agent that built nothing and a scorer that could not look are not the
    same result, and a criterion must never be marked failed because the scorer broke.
    """


# --- comparison, ported verbatim from tests/eval_harness.rs -------------------------------------
# Kept in lock-step deliberately: a score must not acquire a second definition of correctness
# merely because it belongs to a different eval.
def scalar_string(value):
    return value if isinstance(value, str) else json.dumps(value, separators=(",", ":"))


def number(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            return None
    return None


def values_equal(left, right):
    a, b = number(left), number(right)
    if a is not None and b is not None:
        return abs(a - b) <= 1e-9 * max(abs(a), abs(b), 1.0)
    return scalar_string(left) == scalar_string(right)


def row_matches(expected, actual):
    if not isinstance(expected, dict):
        return expected == actual
    return isinstance(actual, dict) and all(
        key in actual and values_equal(value, actual[key]) for key, value in expected.items()
    )


def results_equal(expected, actual):
    if not isinstance(actual, list) or len(expected) != len(actual):
        return False
    remaining = list(actual)
    for want in expected:
        for index, got in enumerate(remaining):
            if row_matches(want, got):
                del remaining[index]
                break
        else:
            return False
    return True


# --- the fixture chain and the nest under test ---------------------------------------------------
def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def http_json(url: str, timeout: float = 8.0):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return json.load(response)
    except Exception as error:
        raise ScoringUnavailable(f"{type(error).__name__}: {error}") from error


def post(url: str, payload: dict, timeout: float = 5.0) -> None:
    request = urllib.request.Request(url, data=json.dumps(payload).encode(), method="POST")
    with urllib.request.urlopen(request, timeout=timeout):
        pass


def wait_for(what: str, seconds: float, probe):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = probe()
            if value is not None:
                return value
        except Exception:
            pass
        time.sleep(0.25)
    return None


def start_fixture_chain(scenario) -> tuple[subprocess.Popen, str]:
    port = free_port()
    proc = subprocess.Popen(
        [sys.executable, str(ROOT / "scripts/fixture_rpc.py"),
         "--port", str(port), "--contract", scenario["contract"]],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    base = f"http://127.0.0.1:{port}"
    if wait_for("the fixture chain", 30, lambda: http_json(f"{base}/control/state")) is None:
        proc.kill()
        die("the fixture chain never came up; nothing can be scored")
    post(f"{base}/control/tip", {"number": scenario["tip"]})
    post(f"{base}/control/finalized", {"number": scenario["finalized"]})
    return proc, f"{base}/"


# --- the subject ---------------------------------------------------------------------------------
def subject_run(scenario, workdir: Path, rpc: str, abi: Path, nest: Path, args):
    """One isolated agent with the builder skill and a shell, and nothing else.

    It is given the task statement and the paths; it is **not** given the criteria, the expected
    result, or this repository. What it does with a shell is its business - the score is taken from
    the artefacts afterwards.
    """
    statement = scenario["task"]["statement"].format(
        contract=scenario["contract"], chain=scenario["chain"],
        rpc=rpc, abi=abi, dir=nest,
    )
    skill = ROOT / "skills" / "nuthatch-builder"
    if not skill.is_dir():
        die(f"the builder skill is not at {skill}; there is nothing to evaluate")
    shutil.copytree(skill, workdir / ".claude" / "skills" / "nuthatch-builder")

    command = [
        args.claude, "-p", "--model", args.model, "--no-session-persistence",
        "--output-format", "stream-json", "--verbose",
        "--max-budget-usd", str(args.max_budget_usd),
        "--system-prompt",
        "You are working in a shell. The nuthatch builder skill is installed; consult it.",
        "--", statement,
    ]
    # Its own process group, so anything the subject backgrounded - a `dev` it forgot to stop -
    # is reaped when its turn ends. Without this a stray `dev` keeps the exclusive redb lock and
    # the scorer cannot open the store at all.
    completed = subprocess.run(
        command, cwd=workdir, capture_output=True, text=True, timeout=args.timeout,
        start_new_session=True,
    )
    model = None
    for line in completed.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "system" and event.get("subtype") == "init":
            model = event.get("model")
    return model, completed


# --- scoring: three facts, no prose --------------------------------------------------------------
def score(scenario, nest: Path, api: str):
    results = []
    for criterion in scenario["criterion"]:
        kind, cid = criterion["kind"], criterion["id"]
        if kind == "nest-exists":
            missing = [f for f in ("nuthatch.toml", "schema.json") if not (nest / f).exists()]
            results.append({"id": cid, "passed": not missing,
                            "detail": "ok" if not missing else f"missing {missing}"})
        elif kind == "sealed-through":
            # No store means nothing to wait for: a nest that was never indexed is a fact already,
            # and polling it for three minutes only makes the run slower and the failure vaguer.
            got = None if not (nest / "nuthatch.redb").exists() else wait_for(
                "the nest to seal", 180, lambda: (
                    http_json(f"{api}/sql?q=select%201").get("provenance", {}).get("sealed_through")
                ))
            results.append({"id": cid, "passed": got == criterion["value"],
                            "detail": f"sealed_through={got}, wanted {criterion['value']}"})
        elif kind == "sql":
            table = resolve_table(api)
            if table is None:
                results.append({"id": cid, "passed": False, "detail": "no table to query"})
                continue
            query = criterion["sql"].replace("{table}", table)
            payload = http_json(f"{api}/sql?" + urllib.parse.urlencode({"q": query}))
            if "error" in payload:
                results.append({"id": cid, "passed": False, "detail": f"query error: {payload['error'][:120]}"})
                continue
            if "rows" not in payload:
                # Never degrade an unexpected shape to "no rows" - #1051's silent path.
                raise ScoringUnavailable(f"/sql response has no 'rows' key: {sorted(payload)!r}")
            expected = json.loads(criterion["expect"])
            passed = results_equal(expected, payload["rows"])
            results.append({"id": cid, "passed": passed,
                            "detail": "ok" if passed else f"got {json.dumps(payload['rows'])[:200]}",
                            "final_rows": None if passed else payload["rows"]})
        else:
            die(f"unknown criterion kind {kind!r}")
    return results


def resolve_table(api: str):
    """The agent names its own alias, so the table is discovered rather than assumed.

    `init` derives the table name from the contract alias, which defaults to `c0` but is the agent's
    to choose. Hardcoding `c0__transfer` would fail a perfectly good nest for picking a nicer name -
    measuring obedience rather than authoring. The scenario has exactly one table by construction.
    """
    listing = http_json(f"{api}/tables")
    names = [t.get("name") or t.get("table") for t in listing.get("tables", [])]
    names = [n for n in names if n]
    return names[0] if len(names) == 1 else None


def one_run(scenario, args):
    chain, rpc = start_fixture_chain(scenario)
    dev = None
    try:
        with tempfile.TemporaryDirectory(prefix="nuthatch-authoring-") as tmp:
            work = Path(tmp)
            abi = work / "erc20.json"
            abi.write_text(ERC20_TRANSFER_ABI)
            nest = work / "nest"

            model, completed = subject_run(scenario, work, rpc, abi, nest, args)
            if completed.returncode != 0:
                print(f"  subject exited {completed.returncode}: "
                      f"{completed.stderr.strip()[:200]}", file=sys.stderr)

            # --- read the store back WITHOUT touching it ----------------------------------------
            #
            # `serve`, never `dev`. This is the whole correctness of the eval and review of #1050
            # caught it: the first version started `dev` unconditionally after the subject finished,
            # which broke the score in *both* directions.
            #
            #   * A subject that complied and left `dev` running lost the exclusive redb lock fight
            #     with the scorer's second `dev`, which then died before serving - so the better the
            #     agent behaved, the worse it scored.
            #   * A subject that ran only `init` and stopped had its indexing done **for it**: the
            #     scorer's `dev` walked the chain and the agent collected `reaches-pinned-tip` and
            #     `canned-question` it had not earned.
            #
            # `nuthatch serve` is the RFC-0022 query-FE role: it owns no cursor and never advances
            # one. It can only report what the subject actually indexed. A nest that was never run
            # reads `sealed_through = 0` and fails criterion 2 honestly, and there is no arrangement
            # of the scorer that can rescue it.
            api_port = free_port()
            api = f"http://127.0.0.1:{api_port}"
            if (nest / "nuthatch.toml").exists():
                dev = subprocess.Popen(
                    [str(args.nuthatch), "serve", "--dir", str(nest),
                     "--listen", f"127.0.0.1:{api_port}"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                if wait_for("the nest API", 60, lambda: http_json(f"{api}/tables")) is None:
                    # `serve` refusing is TWO different facts and they must not share a verdict -
                    # #1051's lesson, and the reason this is not a bare `raise`:
                    #
                    #   * **no redb at all** - the subject scaffolded a nest and never indexed it.
                    #     `serve` says so itself ("no hot store to serve from ... `serve` never
                    #     creates or writes to the local redb, only reads it"). That is an honest
                    #     agent failure, and criteria 2 and 3 must simply score false. Aborting the
                    #     run here would throw away a real result.
                    #   * **a redb that will not open** - something else holds the exclusive lock,
                    #     almost certainly a `dev` that outlived its subject. The scorer cannot
                    #     look, which is a harness problem and never an agent's fault.
                    if not (nest / "nuthatch.redb").exists():
                        pass  # scored below, honestly, as a nest that was never indexed
                    elif dev.poll() is not None:
                        raise ScoringUnavailable(
                            f"`serve` exited {dev.returncode} against a nest that HAS a hot store; "
                            "something still holds the redb lock, so the score would be about the "
                            "harness rather than the agent"
                        )
            return model, score(scenario, nest, api)
    finally:
        for proc in (dev, chain):
            if proc is not None:
                proc.kill()
                proc.wait()


def self_test() -> int:
    """Prove the runner's properties with no key, no model and no network.

    Written on day one rather than after a published zero, because #1051 is what the alternative
    looks like: two defects that were invisible precisely because nothing exercised them.
    """
    failures = []

    def check(name, ok, detail=""):
        print(f"  {'ok  ' if ok else 'FAIL'} {name}{'' if ok else ' - ' + detail}")
        if not ok:
            failures.append(name)

    scenario = tomllib.load(open(SCENARIO, "rb"))
    ids = [c["id"] for c in scenario["criterion"]]
    check("the scenario carries RFC-0017's three criteria",
          ids == ["init-succeeds", "reaches-pinned-tip", "canned-question"], f"got {ids}")

    tip_criterion = next(c for c in scenario["criterion"] if c["kind"] == "sealed-through")
    check("the sealed-through criterion agrees with the chain's finality pin",
          tip_criterion["value"] == scenario["finalized"],
          f"criterion {tip_criterion['value']} vs chain {scenario['finalized']}")

    # An unreachable scorer must raise, never mark a criterion failed - #1051's lesson, applied here
    # before it could cost anything.
    try:
        http_json("http://127.0.0.1:9/tables")
        check("an unreachable scorer raises", False, "returned instead of raising")
    except ScoringUnavailable:
        check("an unreachable scorer raises ScoringUnavailable", True)
    except Exception as error:
        check("an unreachable scorer raises ScoringUnavailable", False, type(error).__name__)

    # The comparison must be the same one Tier A uses, including the DECIMAL-as-string tolerance the
    # canned question depends on: `total` comes back as "3600", not 3600.
    expected = json.loads(next(c for c in scenario["criterion"] if c["kind"] == "sql")["expect"])
    # `"3600"` against `3600` proves nothing: with numeric comparison removed entirely the string
    # fallback still matches them, because `json.dumps(3600)` is `"3600"`. Found by mutating
    # `values_equal` dead and watching this check stay green. A float discriminates - `3600.0`
    # stringifies to `"3600.0"` - and floats are exactly what arrives from an AVG or a division.
    check("a DECIMAL string equals its number",
          results_equal(expected, [{"n": 8, "lo": 1, "hi": 8, "total": 3600.0}])
          and values_equal("3600", 3600.0)
          and not values_equal("3600", "3600.0000001e3"))
    check("a wrong total does not pass",
          not results_equal(expected, [{"n": 8, "lo": 1, "hi": 8, "total": 3599}]))
    check("a missing column does not pass",
          not results_equal(expected, [{"n": 8, "lo": 1, "hi": 8}]))

    check("the builder skill the subject is given exists",
          (ROOT / "skills" / "nuthatch-builder" / "SKILL.md").is_file())

    print("self-test: " + ("PASS" if not failures else f"FAIL ({', '.join(failures)})"))
    return 1 if failures else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--nuthatch", type=Path, default=ROOT / "target/release/nuthatch")
    ap.add_argument("--claude", default="claude")
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--timeout", type=float, default=900)
    ap.add_argument("--max-budget-usd", type=float, default=5.0)
    ap.add_argument("--report", type=Path, default=REPORT)
    args = ap.parse_args()

    if args.self_test:
        raise SystemExit(self_test())

    if args.runs < 3:
        die("refusing to publish fewer than three runs")
    if not args.nuthatch.is_file():
        die(f"{args.nuthatch} is not there; build it first")

    scenario = tomllib.load(open(SCENARIO, "rb"))
    commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                            capture_output=True, text=True).stdout.strip()

    runs, model = [], None
    for n in range(args.runs):
        print(f"run {n + 1}/{args.runs}")
        model, results = one_run(scenario, args)
        for r in results:
            print(f"  {'PASS' if r['passed'] else 'FAIL'} {r['id']}: {r['detail']}")
        runs.append(results)

    per_criterion = {
        c["id"]: sum(1 for run in runs if next(r for r in run if r["id"] == c["id"])["passed"])
        / len(runs)
        for c in scenario["criterion"]
    }
    report = {
        "date": time.strftime("%Y-%m-%d"), "commit": commit, "model": model,
        "runs": args.runs, "scenario": "authoring (RFC-0017)",
        "summary": {
            "end_to_end_pass_rate": sum(
                1 for run in runs if all(r["passed"] for r in run)) / len(runs),
            "by_criterion": per_criterion,
        },
        "runs_detail": runs,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(f"wrote {args.report}")


if __name__ == "__main__":
    main()
