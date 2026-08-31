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
import os
import signal
import shlex
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


def start_fixture_chain(scenario) -> tuple[subprocess.Popen, str, int]:
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
    return proc, f"{base}/", port


# --- the subject ---------------------------------------------------------------------------------
def sandbox_argv(template: str, workdir: Path, rpc_port: int) -> list[str]:
    """Build the sandbox command for *this* run's workdir and RPC port.

    A **template**, not a fixed prefix. Review of #1050 found the previous interface unusable rather
    than merely weak: the operator supplied a static `docker run -v "$WORKDIR:/w"` before the runner
    had created any workdir, so the prefix pointed at a directory that did not exist, the probe
    could not read its own control file, and a container that did start could reach neither the ABI
    nor the loopback RPC. Every keyed run would have been rejected or unable to author anything.

    `{workdir}` and `{rpc_port}` are substituted here, so the confinement is expressed against the
    real directory and port. **The workdir must be mounted at the same absolute path**, because the
    subject is handed host paths for the ABI and the nest and they have to remain valid inside; the
    probe below proves that rather than trusting it.
    """
    return shlex.split(template.format(workdir=workdir, rpc_port=rpc_port))


def verify_sandbox(template: str, rpc_port: int) -> None:
    """Prove the sandbox confines, by probing it - positively and negatively.

    Requiring `--sandbox` to be non-empty was not enforcement: `--sandbox env` satisfied it while
    handing the subject the whole filesystem, so the boundary rested on operator honesty, which is
    what a mechanical score exists to replace.

    Three things must hold together, and any one alone is worthless:

      1. the sandbox can read a file in the subject's workdir **at its host path** - otherwise the
         ABI and nest paths the subject is given are meaningless inside it;
      2. it cannot read this repository - probed at several paths, not one. The answer is not only
         in `eval/authoring.toml`: `tests/authoring_eval_board.rs` carries the same fixture values,
         the expected total, and the canned query verbatim, so hiding one file proves nothing;
      3. it can reach the fixture RPC on loopback, or the subject cannot index at all.
    """
    with tempfile.TemporaryDirectory(prefix="nuthatch-sandbox-check-") as tmp:
        workdir = Path(tmp)
        (workdir / "allowed").write_text("readable")
        argv = sandbox_argv(template, workdir, rpc_port)

        def run(script: str) -> tuple[int, str]:
            try:
                out = subprocess.run([*argv, "sh", "-c", script], cwd=workdir,
                                     capture_output=True, text=True, timeout=180)
                return out.returncode, out.stdout
            except Exception as error:
                return 1, f"<{type(error).__name__}: {error}>"

        code, body = run(f"cat {workdir / 'allowed'}")
        if code != 0 or "readable" not in body:
            die(
                f"the sandbox cannot read {workdir / 'allowed'} - the subject's own workdir, at its\n"
                "host path. The ABI and nest paths handed to the subject would be meaningless inside\n"
                "it, so it could not author anything. Mount the workdir at the SAME absolute path,\n"
                f"e.g. --sandbox 'docker run --rm -v {{workdir}}:{{workdir}} -w {{workdir}} ...'\n"
                f"  got: {body.strip()[:200]}"
            )

        # (2) The repository, probed broadly. Any one of these is enough to score 3/3 by reading.
        leaks = []
        for path in (SCENARIO, ROOT / "tests/authoring_eval_board.rs", ROOT / "eval/run-authoring.py",
                     ROOT / "eval/questions.toml"):
            code, body = run(f"cat {path}")
            if code == 0 and body.strip():
                leaks.append(str(path))
        code, body = run(f"ls {ROOT}")
        if code == 0 and body.strip():
            leaks.append(f"{ROOT} (listable)")
        if leaks:
            die(
                "the sandbox does not isolate this repository. The subject can read:\n  "
                + "\n  ".join(leaks)
                + "\n\nEach of these gives away the answer - the scenario file carries the expected\n"
                "result, and the board test carries the same fixture values, the expected total and\n"
                "the canned query verbatim. An agent that finds any one scores 3/3 without building\n"
                "a nest, and the report records the run as sandboxed.\n"
                "\n"
                "A prefix that merely execs (`env`, `nice`) is not a sandbox. Use one that confines\n"
                "the filesystem - a container, `bwrap --ro-bind`, `sandbox-exec -f`."
            )

        # (3) Loopback must reach the fixture chain, or nothing can be indexed from inside.
        code, body = run(
            f"sh -c 'command -v curl >/dev/null && curl -fsS -m 5 "
            f"http://127.0.0.1:{rpc_port}/control/state || echo NOCURL'"
        )
        if "NOCURL" not in body and (code != 0 or not body.strip()):
            die(
                f"the sandbox cannot reach the fixture RPC on 127.0.0.1:{rpc_port}. The subject\n"
                "would be unable to index, and every run would fail criteria 2 and 3 for a reason\n"
                "that has nothing to do with the agent. Give the sandbox host loopback, e.g.\n"
                "  --sandbox 'docker run --rm --network host -v {workdir}:{workdir} -w {workdir} ...'"
            )


def spawn_group(command, **kwargs) -> tuple[subprocess.Popen, int]:
    """Start a process as the leader of a new group, and capture that group id **now**.

    Capturing it at spawn is the whole point. Reaping via `os.getpgid(proc.pid)` after the fact
    looks equivalent and is not: once the leader has exited and been waited on, that call raises
    `ProcessLookupError` and the surviving children are never signalled at all. Found by writing a
    self-test that backgrounds a real child and checks whether it is still running - it was, on the
    normal-exit path, which is the common one.
    """
    proc = subprocess.Popen(command, start_new_session=True, **kwargs)
    return proc, os.getpgid(proc.pid)


def reap_group(proc: subprocess.Popen, pgid: int, grace: float = 5.0) -> None:
    """Kill a subject's entire process group and wait for it to be gone.

    `SIGTERM` first so a `dev` gets to close its store cleanly, then `SIGKILL` for anything that
    ignores it. `ProcessLookupError` is swallowed because a group that is already gone is the
    outcome wanted, not an error.
    """
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError):
            pass
        try:
            proc.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            continue
        # The leader is gone; confirm the group is too before returning, since a stray `dev` in it
        # would still hold the redb lock.
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return
    try:
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass


def subject_run(scenario, workdir: Path, rpc: str, abi: Path, nest: Path, rpc_port: int, args):
    """One agent with the builder skill and a shell, and nothing else it is not given.

    It receives the task statement and the paths. It must **not** be able to reach the criteria, the
    expected result, or this repository - and that is a property of the sandbox, not of this
    function. Review of #1050 caught the first version claiming the boundary in a docstring while
    doing nothing to enforce it: the subject ran with `cwd` set to a temporary directory and no
    filesystem restriction whatever, so it could simply read `eval/authoring.toml`, take the
    expected result, and score three out of three by discovering the repository rather than by
    knowing how to build a nest.

    `--sandbox` is therefore **required**, and `main` refuses to run without it. Changing a working
    directory is not isolation, and an eval that can be solved by reading the answer key measures
    nothing.
    """
    statement = scenario["task"]["statement"].format(
        contract=scenario["contract"], chain=scenario["chain"],
        rpc=rpc, abi=abi, dir=nest,
    )
    skill = ROOT / "skills" / "nuthatch-builder"
    if not skill.is_dir():
        die(f"the builder skill is not at {skill}; there is nothing to evaluate")
    shutil.copytree(skill, workdir / ".claude" / "skills" / "nuthatch-builder")

    command = sandbox_argv(args.sandbox, workdir, rpc_port) + [
        args.claude, "-p", "--model", args.model, "--no-session-persistence",
        "--output-format", "stream-json", "--verbose",
        "--max-budget-usd", str(args.max_budget_usd),
        "--system-prompt",
        "You are working in a shell. The nuthatch builder skill is installed; consult it.",
        "--", statement,
    ]
    # Its own process group, **and actually killed**.
    #
    # The previous version passed `start_new_session=True` and a comment saying anything the subject
    # backgrounded "is reaped when its turn ends". It is not: that flag *creates* a session, it does
    # not end one, and `subprocess.run(timeout=...)` raises without touching the group either. So a
    # subject that backgrounded `nuthatch dev` left it alive holding the nest's exclusive redb lock;
    # the scorer's `serve` could then not open the store at all, and criterion 2 would sit in a
    # 180-second poll before failing for a reason that had nothing to do with the agent.
    #
    # Killing the group on **every** path - normal exit, timeout, and anything else - is what makes
    # one run isolated from the next. Reaped before scoring, so the lock is provably gone.
    proc, pgid = spawn_group(
        command, cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    timed_out = False
    try:
        stdout, stderr = proc.communicate(timeout=args.timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        stdout, stderr = "", f"subject exceeded {args.timeout}s"
    finally:
        reap_group(proc, pgid)
    completed = subprocess.CompletedProcess(
        command, -1 if timed_out else (proc.returncode or 0), stdout or "", stderr or ""
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
def score(scenario, nest: Path, api: str, unservable: str | None = None):
    # **Is there anything to read at all?** Decided once, here, rather than per criterion.
    #
    # Review of #1050 caught the previous version claiming in a comment that an unindexed nest
    # scores criteria 2 and 3 false, while criterion 3 went to `/tables` regardless - against a
    # `serve` that had refused to start - and the resulting `ScoringUnavailable` aborted the whole
    # run. So a perfectly ordinary agent failure (scaffolded a nest, never indexed it) produced no
    # score at all, and the comment asserted a behaviour the code did not have.
    #
    # A nest with no hot store is a *finished fact*, not a scorer problem: the agent did not index.
    # It scores false, honestly, and the run continues.
    indexed = (nest / "nuthatch.redb").exists()
    unreadable = unservable if indexed else "the nest has no hot store: it was scaffolded but never indexed"
    results = []
    for criterion in scenario["criterion"]:
        if unreadable and criterion["kind"] != "nest-exists":
            results.append({"id": criterion["id"], "passed": False, "detail": unreadable})
            continue
        kind, cid = criterion["kind"], criterion["id"]
        if kind == "nest-exists":
            missing = [f for f in ("nuthatch.toml", "schema.json") if not (nest / f).exists()]
            results.append({"id": cid, "passed": not missing,
                            "detail": "ok" if not missing else f"missing {missing}"})
        elif kind == "sealed-through":
            got = wait_for("the nest to seal", 180, lambda: (
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
                # A verdict, not an absence of one - and here it means something sharper than in the
                # RFC-0016 runner, where the SQL is the *agent's*. This query is **ours**, and
                # `tests/authoring_eval_board.rs` proves it runs against a reference nest on every
                # commit. So a rejection at eval time says the agent built something structurally
                # different - no `value_dec`, a different decode - which is an authoring failure and
                # scores as one, with the reason kept.
                results.append({"id": cid, "passed": False, "detail": f"query error: {payload['error'][:120]}",
                                "query_error": str(payload["error"])})
                continue
            # Validate the whole shape, not merely the presence of a key: `{"rows": null}` and
            # `{"rows": "x"}` would otherwise reach the comparison and read as an ordinary wrong
            # answer. Same fault, same fix, as the sibling runner (#1051).
            rows = payload.get("rows")
            if not isinstance(rows, list) or not all(isinstance(r, dict) for r in rows):
                raise ScoringUnavailable(
                    f"/sql returned a 'rows' that is not an array of objects: {type(rows).__name__}"
                )
            expected = json.loads(criterion["expect"])
            passed = results_equal(expected, rows)
            results.append({"id": cid, "passed": passed,
                            "detail": "ok" if passed else f"got {json.dumps(rows)[:200]}",
                            "final_rows": None if passed else rows})
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
    chain, rpc, rpc_port = start_fixture_chain(scenario)
    dev = None
    try:
        with tempfile.TemporaryDirectory(prefix="nuthatch-authoring-") as tmp:
            work = Path(tmp)
            abi = work / "erc20.json"
            abi.write_text(ERC20_TRANSFER_ABI)
            nest = work / "nest"

            unservable = None
            model, completed = subject_run(scenario, work, rpc, abi, nest, rpc_port, args)
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
                    # `serve` refusing is the **agent's** result, not the harness's.
                    #
                    # The previous version raised `ScoringUnavailable` whenever a `nuthatch.redb`
                    # existed, reasoning that something must still hold the lock. Review of #1050
                    # pointed out it cannot know that: a subject can create the store and then die
                    # mid-initialisation, leaving one that is empty, truncated or otherwise
                    # unservable - and the run would abort rather than record the perfectly ordinary
                    # `init-succeeds=true, reaches-pinned-tip=false, canned-question=false`.
                    #
                    # The reason it cannot be a lock is one line up: `subject_run` reaps the
                    # subject's whole process group before returning, so nothing the agent started
                    # is still alive to hold one. Having removed the harness's own contribution, an
                    # unservable nest is an authoring failure and scores as one, with the reason
                    # kept rather than thrown away.
                    unservable = (
                        f"`serve` could not open the nest the subject built "
                        f"(exit {dev.returncode if dev.poll() is not None else 'still running'})"
                    )
            return model, score(scenario, nest, api, unservable)
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

    # The case review found the code not doing what its own comment claimed: a subject that
    # scaffolded a nest and never indexed it must **score**, with criteria 2 and 3 false, rather
    # than reaching for an API that is not there and aborting the run. The scorer is pointed at a
    # deliberately dead port, so if any criterion tries to talk to it this check fails.
    with tempfile.TemporaryDirectory(prefix="nuthatch-authoring-selftest-") as tmp:
        nest = Path(tmp) / "nest"
        nest.mkdir()
        (nest / "nuthatch.toml").write_text("")
        (nest / "schema.json").write_text("{}")
        assert not (nest / "nuthatch.redb").exists()
        try:
            results = score(scenario, nest, "http://127.0.0.1:9")
            passed = {r["id"]: r["passed"] for r in results}
            check("an unindexed nest scores rather than aborting",
                  passed == {"init-succeeds": True, "reaches-pinned-tip": False,
                             "canned-question": False},
                  str(passed))
        except ScoringUnavailable as error:
            check("an unindexed nest scores rather than aborting", False,
                  f"aborted the run: {error}")

    import unittest.mock as mock
    for label, body in [("rows is null", {"rows": None}), ("rows is a string", {"rows": "x"}),
                        ("a row is not an object", {"rows": [1]})]:
        with tempfile.TemporaryDirectory(prefix="nuthatch-shape-") as tmp:
            nest = Path(tmp) / "nest"
            nest.mkdir()
            (nest / "nuthatch.toml").write_text("")
            (nest / "schema.json").write_text("{}")
            (nest / "nuthatch.redb").write_text("")  # "indexed", so scoring reaches the query
            with mock.patch(f"{__name__}.http_json") as fetch, \
                    mock.patch(f"{__name__}.resolve_table", return_value="t"), \
                    mock.patch(f"{__name__}.wait_for", return_value=scenario["finalized"]):
                fetch.return_value = body
                try:
                    score(scenario, nest, "http://example.invalid")
                    check(f"a malformed /sql shape is fatal ({label})", False, "scored it instead")
                except ScoringUnavailable:
                    check(f"a malformed /sql shape is fatal ({label})", True)

    # A prefix that merely execs is not a sandbox, and saying so is not enough: `--sandbox env`
    # satisfied the old non-empty check while handing the subject the whole filesystem. The property
    # is tested against the operator's own prefix, with the file that would give the game away.
    try:
        verify_sandbox("env", 9)
        check("a pass-through prefix is rejected as a sandbox", False, "accepted `env`")
    except SystemExit as exit_:
        check("a pass-through prefix is rejected as a sandbox",
              "does not isolate" in str(exit_), f"refused for another reason: {exit_}")

    # ...and the probe must be two-sided, or a sandbox that cannot run at all reads as isolation.
    try:
        verify_sandbox("false", 9)
        check("an unusable sandbox is rejected, not mistaken for isolation", False, "accepted it")
    except SystemExit as exit_:
        check("an unusable sandbox is rejected, not mistaken for isolation",
              "the subject's own workdir" in str(exit_),
              f"refused for the wrong reason: {exit_}")

    # A sandbox that hides the *answer key* but exposes the rest of the repository must still be
    # refused. Review of #1050: `tests/authoring_eval_board.rs` carries the same fixture values, the
    # expected total 3600 and the canned query verbatim, so blocking `authoring.toml` alone gives
    # away exactly as much. Probing one path is not evidence of the boundary.
    with tempfile.TemporaryDirectory(prefix="nuthatch-leakbox-") as tmp:
        gate = Path(tmp) / "leaky.sh"
        gate.write_text(
            "#!/bin/sh\n"
            "# Hides the scenario file and nothing else - the partial sandbox that must not pass.\n"
            "case \"$*\" in *authoring.toml*) exit 13 ;; esac\n"
            "exec \"$@\"\n"
        )
        gate.chmod(0o755)
        try:
            verify_sandbox(f"{gate} ", 9)
            check("hiding only the answer key is not isolation", False, "accepted a partial sandbox")
        except SystemExit as exit_:
            check("hiding only the answer key is not isolation",
                  "does not isolate this repository" in str(exit_),
                  f"refused for another reason: {str(exit_)[:160]}")

    # **The positive case, with no escape hatch.**
    #
    # Everything above proves the runner *rejects* things. None of it proves it *accepts* a genuine
    # sandbox - and a `verify_sandbox` that refused everything would satisfy the lot while making
    # the eval unrunnable, which is the very defect review found in the interface itself.
    #
    # The first attempt at this check was worse than none: it ran against port 9 with no chain
    # there, so a correctly confined wrapper reached the RPC leg and raised, and the check counted
    # that specific failure as success. A regression rejecting every sandbox would have passed it
    # unchanged. So a **real fixture chain** is started and `verify_sandbox` must return cleanly.
    chain, _, port = start_fixture_chain(scenario)
    try:
        with tempfile.TemporaryDirectory(prefix="nuthatch-fakebox-") as tmp:
            gate = Path(tmp) / "gate.sh"
            gate.write_text(
                "#!/bin/sh\n"
                "# A real confinement, in the only terms available everywhere: refuse any command\n"
                "# that names the repository. Not what an operator should use - enough to prove the\n"
                "# contract is satisfiable at all.\n"
                f"case \"$*\" in *{ROOT}*) exit 13 ;; esac\n"
                "exec \"$@\"\n"
            )
            gate.chmod(0o755)
            try:
                verify_sandbox(f"{gate} ", port)
                check("a confining sandbox is accepted", True)
            except SystemExit as exit_:
                check("a confining sandbox is accepted", False,
                      f"rejected a working sandbox: {str(exit_)[:200]}")
    finally:
        chain.kill()
        chain.wait()

    # The eval's integrity property: a run must be impossible without isolation. Review of #1050
    # found the subject boundary asserted in a docstring and enforced nowhere - the agent could read
    # `eval/authoring.toml`, lift the expected result, and score 3/3 by discovering this repository.
    # A refusal is the only honest default, because the failure is silent and flattering.
    import unittest.mock as mock

    class _NoSandbox:
        self_test, runs, sandbox = False, 3, None
        nuthatch = Path(sys.executable)  # exists, so the sandbox check is what refuses

    with mock.patch(f"{__name__}.argparse.ArgumentParser.parse_args", return_value=_NoSandbox()):
        try:
            main()
            check("a run without a sandbox is refused", False, "it ran anyway")
        except SystemExit as exit_:
            check("a run without a sandbox is refused", "--sandbox is required" in str(exit_),
                  f"exited for another reason: {exit_}")

    # A store that exists but cannot be served - the subject died mid-initialisation - is the
    # agent's failure, not the harness's, and must score rather than abort. The subject's process
    # group is reaped before scoring, so there is no lock left that could excuse it.
    with tempfile.TemporaryDirectory(prefix="nuthatch-corrupt-") as tmp:
        nest = Path(tmp) / "nest"
        nest.mkdir()
        (nest / "nuthatch.toml").write_text("")
        (nest / "schema.json").write_text("{}")
        (nest / "nuthatch.redb").write_text("not a database")
        try:
            results = score(scenario, nest, "http://127.0.0.1:9", "`serve` could not open it")
            passed = {r["id"]: r["passed"] for r in results}
            check("an unservable nest scores rather than aborting",
                  passed == {"init-succeeds": True, "reaches-pinned-tip": False,
                             "canned-question": False}, str(passed))
        except ScoringUnavailable as error:
            check("an unservable nest scores rather than aborting", False, f"aborted: {error}")

    # The high-severity case review found: a subject that backgrounds a long-lived child must not
    # leave it alive holding the nest's redb lock. `start_new_session=True` alone does not do this -
    # it creates the group, it does not end it.
    #
    # **With a positive control**, because the first version of this check was inert: the child was
    # spawned through nested `-c` quoting, never actually started, and the marker therefore never
    # appeared whether or not anything was killed. Neutering `os.killpg` left it green. So the check
    # now proves the child is running *before* it proves it stopped.
    for label, timeout in [("normal exit", 30.0), ("timeout", 1.0)]:
        with tempfile.TemporaryDirectory(prefix="nuthatch-reap-") as tmp:
            marker = Path(tmp) / "alive"
            child = Path(tmp) / "child.py"
            child.write_text(
                "import pathlib, time\n"
                f"m = pathlib.Path({str(marker)!r})\n"
                "while True:\n    m.touch()\n    time.sleep(0.05)\n"
            )
            parent = Path(tmp) / "parent.py"
            parent.write_text(
                "import subprocess, sys, time\n"
                f"subprocess.Popen([sys.executable, {str(child)!r}])\n"
                f"time.sleep({30 if timeout < 5 else 0.4})\n"
            )
            proc, pgid = spawn_group([sys.executable, str(parent)],
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

            # Positive control: the backgrounded child must genuinely be running, or this proves
            # nothing about reaping.
            alive = wait_for("the backgrounded child to start", 10, lambda: marker.exists() or None)
            check(f"the reap check has a live child to kill ({label})", alive is not None,
                  "the child never started, so the reap assertion below would be vacuous")

            try:
                proc.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                pass
            finally:
                reap_group(proc, pgid)

            marker.unlink(missing_ok=True)
            time.sleep(0.5)
            check(f"a backgrounded child is reaped ({label})", not marker.exists(),
                  "the child is still running and would hold the redb lock")

    print("self-test: " + ("PASS" if not failures else f"FAIL ({', '.join(failures)})"))
    return 1 if failures else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument(
        "--sandbox", default=None,
        help="REQUIRED. A command TEMPLATE that confines the subject, with {workdir} and "
             "{rpc_port} substituted per run - e.g. \"docker run --rm --network host "
             "-v {workdir}:{workdir} -w {workdir} <image>\". The workdir must be mounted at the "
             "SAME absolute path, because the subject is handed host paths for the ABI and nest. "
             "Verified before every run, not taken on trust. Recorded in the report.",
    )
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
    if not args.sandbox:
        die(
            "--sandbox is required. Without it the subject can read eval/authoring.toml, take the\n"
            "expected result, and score 3/3 by discovering this repository instead of by knowing\n"
            "how to build a nest. A temporary working directory is not isolation.\n"
            "\n"
            "Pass a command prefix that confines the subject to its workdir, e.g.\n"
            "  --sandbox docker run --rm -v $WORKDIR:/w -w /w <image>"
        )
    if not args.nuthatch.is_file():
        die(f"{args.nuthatch} is not there; build it first")

    scenario = tomllib.load(open(SCENARIO, "rb"))
    # Verified against a live fixture chain, because loopback reachability is part of the property.
    probe_chain, _, probe_port = start_fixture_chain(scenario)
    try:
        verify_sandbox(args.sandbox, probe_port)
    finally:
        probe_chain.kill()
        probe_chain.wait()

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
        # Recorded because the score is only as trustworthy as the isolation: a reader must be able
        # to see what confined the subject, not take it on faith.
        "sandbox": args.sandbox,
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
