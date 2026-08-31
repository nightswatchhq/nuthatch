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


def sql_rows(url: str, query: str, limit: int = 200):
    params = urllib.parse.urlencode({"q": query, "max_rows": str(limit)})
    with urllib.request.urlopen(f"{url}/sql?{params}", timeout=35) as response:
        payload = json.load(response)
    if "error" in payload:
        raise RuntimeError(payload["error"])
    return payload.get("rows", [])


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
    final_rows = []
    if queries:
        try:
            final_rows = sql_rows(args.url, queries[-1])
        except Exception as error:
            print(f"  {question['id']}: final SQL could not be scored: {error}", file=sys.stderr)
    expected = json.loads(question["expect"])
    passed = bool(queries) and results_equal(expected, final_rows)
    return model, {
        "id": question["id"], "class": question["class"], "passed": passed,
        "first_try": passed and len(queries) == 1,
        "sql_attempts": len(queries), "tool_calls": tool_calls,
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
        if set(result) != {"id", "class", "passed", "first_try", "sql_attempts", "tool_calls"}:
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


if __name__ == "__main__":
    main()
