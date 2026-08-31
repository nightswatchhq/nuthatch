#!/usr/bin/env python3
"""Run RFC-0016's Tier-B MCP evaluation without leaking its oracle to the subject.

The runner may read questions.toml.  Each subject is a new, restricted Claude
process rooted in a freshly-created directory, configured with only nuthatch's
MCP server.  Its prompt is assembled from a generic instruction and the
question field, never sql, expect, class, fixture details, or repository source.
"""

import argparse
import concurrent.futures
import datetime as dt
import json
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import tomllib
import urllib.parse
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
QUESTIONS = ROOT / "eval" / "questions.toml"
REPORT = ROOT / "eval" / "eval-report.json"


def die(message: str) -> None:
    raise SystemExit(f"tier-b runner: {message}")


# These are a direct port of tests/eval_harness.rs:results_equal, row_matches,
# and values_equal.  Keep the equivalence in lock-step with Tier A: a score
# cannot acquire a second definition of correctness merely because it is keyed.
def scalar_string(value):
    return value if isinstance(value, str) else json.dumps(value, separators=(",", ":"))


def number(value):
    if isinstance(value, bool) or value is None:
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
    lnum, rnum = number(left), number(right)
    if lnum is not None and rnum is not None:
        return abs(lnum - rnum) < 1e-9
    return scalar_string(left) == scalar_string(right)


def row_matches(expected, actual):
    if not isinstance(expected, dict) or not isinstance(actual, dict):
        return values_equal(expected, actual)
    return all(key in actual and values_equal(value, actual[key]) for key, value in expected.items())


def results_equal(expected, actual):
    if len(expected) != len(actual):
        return False
    used = [False] * len(actual)
    for row in expected:
        match = next((i for i, candidate in enumerate(actual)
                      if not used[i] and row_matches(row, candidate)), None)
        if match is None:
            return False
        used[match] = True
    return True


class ScoringUnavailable(RuntimeError):
    """The scorer could not obtain a verdict - as distinct from obtaining a wrong one.

    This exists because the two were indistinguishable. `evaluate_question` used to catch every
    exception from `sql_rows`, leave `final_rows` empty, and score the question **failed**; an
    unreachable URL, a timeout, an HTTP error or a server that never came up therefore produced a
    schema-valid, publishable **0/15** with nothing in the report to say why. That is the one way a
    fabricated-looking number could arrive entirely by accident, in a file whose whole premise is
    that published numbers are real.

    A scoring failure is now fatal to the run. A zero must be earned.
    """


class QueryRejected(RuntimeError):
    """The server answered, and the answer is that the subject's SQL is wrong.

    The opposite of `ScoringUnavailable`, and the distinction review of #1051 insisted on. An
    invented table name or a syntax error comes back as a well-formed `{"error": ...}` from a
    perfectly healthy nest: the scorer looked, and what it saw was a bad query. That is an ordinary
    failed verdict - and it is the *most* diagnostic one there is, since "the agent invented a table
    name" is exactly what `final_query` exists to reveal.

    Making it fatal was an over-correction: this file swung from scoring every failure as a zero to
    aborting the run on the commonest agent mistake, and both extremes lose the same information.
    """


def sql_rows(url: str, query: str, limit: int = 200):
    params = urllib.parse.urlencode({"q": query, "max_rows": str(limit)})
    try:
        with urllib.request.urlopen(f"{url}/sql?{params}", timeout=35) as response:
            payload = json.load(response)
    except ScoringUnavailable:
        raise
    except Exception as error:
        raise ScoringUnavailable(f"{type(error).__name__}: {error}") from error
    # Validate the whole shape, not just the presence of a key (review of #1051). Checking only
    # `"rows" not in payload` still let `{"rows": null}` and `{"rows": "not rows"}` through to the
    # comparison, where they read as an ordinary wrong answer; and a bare JSON `null` body died on
    # `"error" in payload` with a `TypeError` rather than following the fatal path. Every response
    # that is not the shape we asked for is the scorer failing to obtain a verdict, and they must
    # all leave by the same door.
    if not isinstance(payload, dict):
        raise ScoringUnavailable(f"response is {type(payload).__name__}, not an object")
    if "error" in payload:
        # The server is fine; the query is not. A verdict, not an absence of one.
        raise QueryRejected(str(payload["error"]))
    if "rows" not in payload:
        raise ScoringUnavailable(f"response has no 'rows' key: {sorted(payload)!r}")
    rows = payload["rows"]
    if not isinstance(rows, list):
        raise ScoringUnavailable(f"'rows' is {type(rows).__name__}, not an array")
    if not all(isinstance(row, dict) for row in rows):
        raise ScoringUnavailable("'rows' contains an entry that is not an object")
    return rows


def median(values):
    return statistics.median(values)


def subject_run(question: str, args):
    config = json.dumps({"mcpServers": {"nuthatch": {
        "command": str(args.nuthatch),
        "args": ["mcp", "--url", args.url],
    }}})
    prompt = (
        "Use the nuthatch MCP tools to answer this question. Use the sql tool "
        "to obtain the data. Give the result only after you have queried it.\n\n"
        + question
    )
    # This directory is intentionally neither the repository nor the fixture.
    # --restricted removes built-in code/command tools; --tools '' keeps it so.
    with tempfile.TemporaryDirectory(prefix="nuthatch-eval-subject-") as cwd:
        command = [
            args.claude, "-p", "--model", args.model, "--restricted",
            "--strict-mcp-config", "--tools", "", "--allowed-tools",
            "mcp__nuthatch__*", "--no-session-persistence", "--output-format",
            "stream-json", "--verbose", "--max-budget-usd", str(args.max_budget_usd),
            "--system-prompt", "Use only the supplied MCP tools to answer the user's question.",
            "--mcp-config", config, "--", prompt,
        ]
        completed = subprocess.run(command, cwd=cwd, capture_output=True, text=True, timeout=args.timeout)
    if completed.returncode != 0:
        raise RuntimeError(completed.stderr.strip() or "subject process failed")

    tool_calls, sql_queries, model = 0, [], None
    for line in completed.stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "system" and event.get("subtype") == "init":
            model = event.get("model")
        message = event.get("message", {})
        for content in message.get("content", []):
            if content.get("type") != "tool_use":
                continue
            tool_calls += 1
            if content.get("name") == "mcp__nuthatch__sql":
                query = content.get("input", {}).get("query")
                if isinstance(query, str):
                    sql_queries.append(query)
    return model, tool_calls, sql_queries


def evaluate_question(question, args):
    """Run one isolated subject and score only its final SQL invocation."""
    model, tool_calls, queries = subject_run(question["question"], args)
    final_rows, query_error = [], None
    if queries:
        try:
            # `ScoringUnavailable` is deliberately **not** caught. A scorer that cannot reach the
            # nest has not discovered that the agent is wrong; it has discovered that it cannot
            # tell, and those must not share a verdict.
            final_rows = sql_rows(args.url, queries[-1])
        except QueryRejected as rejected:
            # A rejected query *is* the verdict, and recording why is the point of this change.
            query_error = str(rejected)
    expected = json.loads(question["expect"])
    passed = bool(queries) and query_error is None and results_equal(expected, final_rows)
    return model, {
        "id": question["id"], "class": question["class"], "passed": passed,
        "first_try": passed and len(queries) == 1,
        "sql_attempts": len(queries), "tool_calls": tool_calls,
        # Why a zero is a zero. Without these the baseline says all fifteen are wrong and cannot say
        # whether the agent invented a table name, tripped the `value` / `value_dec` big-int footgun
        # the fixture exists to probe, or fell over the `"from"` / `"to"` reserved words. RFC-0016
        # S1's premise is that the MCP surface is a context-engineering problem *to be fixed*, and a
        # score without a diagnosis gives the slices that follow nothing to aim at. It cannot be
        # recovered later either - the transcripts are gone once the run ends.
        "final_query": queries[-1] if queries else None,
        "final_rows": final_rows if not passed else None,
        "query_error": query_error,
    }


def validate_report(report):
    required = {"date", "commit", "model", "temperature", "question_set_hash", "runs", "summary", "results"}
    if set(report) != required:
        die("generated report does not match its top-level schema")
    if report["runs"] < 3:
        die("refusing to publish fewer than three runs")
    summary = report["summary"]
    for key in ("first_try_pass_rate", "overall_pass_rate", "mean_sql_attempts"):
        if key not in summary or not isinstance(summary[key], (int, float)):
            die(f"generated report has invalid summary.{key}")
    for result in report["results"]:
        if set(result) != {"id", "class", "passed", "first_try", "sql_attempts", "tool_calls",
                           "final_query", "final_rows", "query_error"}:
            die("generated result does not match its schema")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True, help="nuthatch serve URL")
    parser.add_argument("--nuthatch", type=Path, default=ROOT / "target/debug/nuthatch")
    parser.add_argument("--claude", default="claude")
    parser.add_argument("--model", default="sonnet", help="Claude model selector")
    parser.add_argument("--temperature", type=float, default=1.0,
                        help="provider default used by Claude Code; the CLI has no sampling override")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--max-budget-usd", type=float, default=1.0)
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--workers", type=int, default=1,
                        help="independent subject processes to run concurrently")
    parser.add_argument("--report", type=Path, default=REPORT)
    args = parser.parse_args()
    if args.runs < 3:
        die("a published baseline requires at least three runs")
    if not args.nuthatch.is_file():
        die(f"MCP binary does not exist: {args.nuthatch}")

    with QUESTIONS.open("rb") as source:
        questions = tomllib.load(source)["question"]
    question_hash = subprocess.check_output(["shasum", "-a", "256", str(QUESTIONS)], text=True).split()[0]
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    all_runs, observed_models = [], set()

    for run in range(args.runs):
        print(f"run {run + 1}/{args.runs}", flush=True)
        outcomes = [None] * len(questions)
        # Processes are separate subjects whether concurrent or not. One is the
        # published default: the MCP SQL surface allows two concurrent queries,
        # and an eval must not manufacture guard rejections by load testing it.
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as pool:
            futures = {pool.submit(evaluate_question, question, args): position
                       for position, question in enumerate(questions)}
            for future in concurrent.futures.as_completed(futures):
                position = futures[future]
                model, outcome = future.result()
                if model:
                    observed_models.add(model)
                outcomes[position] = outcome
                print(f"  {outcome['id']}: {'pass' if outcome['passed'] else 'fail'} "
                      f"({outcome['sql_attempts']} SQL, {outcome['tool_calls']} tools)", flush=True)
        all_runs.append(outcomes)

    if len(observed_models) != 1:
        die(f"model selector was not pinned: observed {sorted(observed_models)}")
    results = []
    for position, question in enumerate(questions):
        samples = [run[position] for run in all_runs]
        results.append({
            "id": question["id"], "class": question["class"],
            "passed": bool(median([sample["passed"] for sample in samples])),
            "first_try": bool(median([sample["first_try"] for sample in samples])),
            "sql_attempts": int(median([sample["sql_attempts"] for sample in samples])),
            "tool_calls": int(median([sample["tool_calls"] for sample in samples])),
        })
    run_summaries = []
    for outcomes in all_runs:
        run_summaries.append({
            "first": sum(item["first_try"] for item in outcomes) / len(outcomes),
            "overall": sum(item["passed"] for item in outcomes) / len(outcomes),
            "attempts": sum(item["sql_attempts"] for item in outcomes) / len(outcomes),
        })
    classes = sorted({question["class"] for question in questions})
    by_class = {}
    for cls in classes:
        rates = [sum(item["passed"] for item in run if item["class"] == cls) /
                 sum(item["class"] == cls for item in run) for run in all_runs]
        by_class[cls] = median(rates)
    report = {
        "date": dt.datetime.now(dt.timezone.utc).date().isoformat(), "commit": commit,
        "model": observed_models.pop(), "temperature": args.temperature,
        "question_set_hash": question_hash, "runs": args.runs,
        "summary": {
            "first_try_pass_rate": median([item["first"] for item in run_summaries]),
            "overall_pass_rate": median([item["overall"] for item in run_summaries]),
            "mean_sql_attempts": median([item["attempts"] for item in run_summaries]),
            "by_class": by_class,
        },
        "results": results,
    }
    validate_report(report)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(f"wrote {args.report}")


def self_test() -> int:
    """Prove the two properties #1051 is about, without a key, a nest, or a model.

    Both defects this replaces were invisible precisely because nothing exercised them: the runner
    scored a *verdict it never obtained* as a failure, and the report recorded no evidence for the
    zero it published. A fix for that class must not itself ship unexercised.
    """
    failures = []

    def check(name, condition, detail=""):
        print(f"  {'ok  ' if condition else 'FAIL'} {name}{'' if condition else ' - ' + detail}")
        if not condition:
            failures.append(name)

    # 1. An unreachable scorer must raise, not return an empty result set that scores as a failure.
    try:
        sql_rows("http://127.0.0.1:9", "SELECT 1")
        check("unreachable scorer raises", False, "returned instead of raising")
    except ScoringUnavailable:
        check("unreachable scorer raises ScoringUnavailable", True)
    except Exception as error:
        check("unreachable scorer raises ScoringUnavailable", False, f"raised {type(error).__name__}")

    # 2. A response with no `rows` key must raise rather than degrade to "no rows". This is the
    #    silent path: the old `payload.get("rows", [])` never threw at all.
    import io
    import unittest.mock as mock

    def served(body):
        """Answer one request with `body` and report how sql_rows reacted."""
        with mock.patch("urllib.request.urlopen") as opener:
            opener.return_value.__enter__ = lambda self_: io.StringIO(json.dumps(body))
            opener.return_value.__exit__ = lambda *_: False
            try:
                return ("returned", sql_rows("http://example.invalid", "SELECT 1"))
            except ScoringUnavailable:
                return ("fatal", None)
            except Exception as error:
                return (type(error).__name__, None)

    # Every malformed shape must leave by the same door. `{"rows": null}` and `{"rows": "x"}` used
    # to reach the comparison and read as a wrong answer; a bare `null` died with a TypeError.
    for label, body in [
        ("no rows key", {"unexpected": 1}),
        ("rows is null", {"rows": None}),
        ("rows is a string", {"rows": "not rows"}),
        ("a row is not an object", {"rows": [1, 2]}),
        ("body is not an object", None),
        ("body is a list", [{"n": 1}]),
    ]:
        outcome, _ = served(body)
        check(f"shape mismatch raises ({label})", outcome == "fatal", f"got {outcome}")

    # ...and a well-formed response must still be returned, so the checks above are not passing
    # because everything raises.
    outcome, rows = served({"rows": [{"n": 8}]})
    check("a well-formed response is returned", outcome == "returned" and rows == [{"n": 8}],
          f"got {outcome} {rows}")

    # 3. The regression itself, at the site it actually lives. Everything above exercises
    #    `sql_rows`; re-adding the old try/except around its *call* in `evaluate_question` would
    #    turn `ScoringUnavailable` back into a failed question while every probe above still
    #    passed. Review of #1051 found exactly that hole, which is the same shape as the defect
    #    this change exists to remove - a guard that does not cover the thing it names.
    import unittest.mock as mock

    class _Args:
        url = "http://127.0.0.1:9"

    question = {"id": "q", "class": "c", "question": "how many?", "expect": "[]"}
    with mock.patch(f"{__name__}.subject_run", return_value=("m", 1, ["SELECT 1"])):
        try:
            evaluate_question(question, _Args())
            check("evaluate_question propagates a scoring failure", False,
                  "scored it instead of raising")
        except ScoringUnavailable:
            check("evaluate_question propagates a scoring failure", True)
        except Exception as error:
            check("evaluate_question propagates a scoring failure", False,
                  f"raised {type(error).__name__}")

    # The commonest agent mistake - an invented table name - must produce a *diagnosed failure*,
    # not an aborted run. Review of #1051 caught this file over-correcting into exactly that: it
    # swung from scoring every failure as a zero to killing the run on a bad query, and both
    # extremes throw away the same information.
    with mock.patch(f"{__name__}.subject_run", return_value=("m", 1, ["SELECT * FROM invented"])), \
            mock.patch(f"{__name__}.sql_rows",
                       side_effect=QueryRejected("Table with name invented does not exist")):
        try:
            _, result = evaluate_question(question, _Args())
            check("a rejected query is a diagnosed failure, not an aborted run",
                  result["passed"] is False
                  and result["final_query"] == "SELECT * FROM invented"
                  and "invented" in (result["query_error"] or ""),
                  str(result))
        except Exception as error:
            check("a rejected query is a diagnosed failure, not an aborted run", False,
                  f"raised {type(error).__name__}: {error}")

    # ...and a reachable scorer must still produce a verdict, so the check above is not passing
    # merely because everything raises.
    with mock.patch(f"{__name__}.subject_run", return_value=("m", 1, ["SELECT 1"])), \
            mock.patch(f"{__name__}.sql_rows", return_value=[{"n": 1}]):
        question_ok = dict(question, expect='[{"n": 1}]')
        _, result = evaluate_question(question_ok, _Args())
        check("a reachable scorer still yields a verdict",
              result["passed"] and result["final_query"] == "SELECT 1", str(result))

    # 4. A report whose results omit the diagnostic fields must be refused, so a future run cannot
    #    publish another undiagnosable zero.
    stub = {
        "date": "2026-01-01", "commit": "0" * 40, "model": "m", "temperature": 1.0,
        "question_set_hash": "0" * 64, "runs": 3,
        "summary": {"first_try_pass_rate": 0.0, "overall_pass_rate": 0.0,
                    "mean_sql_attempts": 1.0, "by_class": {}},
        "results": [{"id": "q", "class": "c", "passed": False, "first_try": False,
                     "sql_attempts": 1, "tool_calls": 1}],
    }
    try:
        validate_report(stub)
        check("report without final_query is refused", False, "accepted it")
    except SystemExit:
        check("report without final_query is refused", True)

    # 5. ...and accepted once they are present, so (4) is not passing for the wrong reason.
    stub["results"][0].update({"final_query": "SELECT 1", "final_rows": [], "query_error": None})
    try:
        validate_report(stub)
        check("report with final_query is accepted", True)
    except SystemExit as error:
        check("report with final_query is accepted", False, str(error))

    print("self-test: " + ("PASS" if not failures else f"FAIL ({', '.join(failures)})"))
    return 1 if failures else 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        raise SystemExit(self_test())
    main()
