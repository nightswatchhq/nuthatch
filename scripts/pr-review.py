#!/usr/bin/env python3
"""Review one pull request's diff with GPT-5.6 Luna and print a Markdown comment.

The repo takes fifteen pull requests a day and what it has never had is a reviewer with no stake in
the sprint. We have at least two recorded cases of a double-dispatched run approving a PR and arming
auto-merge while missing a real defect, and a second opinion from the same firm is not a second
opinion. The `Reviewed-by:` signature gate that used to sit alongside this was retired in favour of
it: a line of text a party types about itself is not review, and the workflow enforcing it recorded
in its own comments that it detected approximately none of the forgeries it was built for.

This is that outside reader. Jules publishes an App-owned `Jules approval` check as well as the
comment. The check is green only for a `ship` verdict. A finding, a failed review, or malformed
model output is red until the author addresses it and asks for `/re-review`.

Model: `gpt-5.6-luna`, $0.20/$1.20 per MTok since 2026-07-30. A review of a typical diff runs about
four pence, so the whole repo's traffic is under twenty dollars a month. That price is the reason
this exists as a bespoke script rather than an off-the-shelf bot: for the cost of a takeaway we get
a prompt we control, aimed at the non-negotiables in CLAUDE.md rather than at trailing commas.

**A review that did not happen must never render as a clean one.** This is the `mutants-check.py`
lesson (#841) and it applies with more force here, because the output is prose a human skims. An API
error, a truncated response, a malformed structured output: all exit non-zero and print nothing that
could be mistaken for a verdict. The only silent success path is a review that actually came back.

**The reviewer has a name and a manner, and both are fenced.** A review nobody reads is a review
that did not happen, and fifteen of these a day is a lot of identical prose to skim past; a voice
with some grit in it gets read. But the voice is confined to the `summary` field by the system
prompt, and the prompt says in terms that the manner must never move the verdict. `title` and
`detail` stay flat and technical, because those are what somebody acts on at two in the morning.
If a review ever reads as though it is doing a bit while telling you about a security hole, that
fence has failed and the prompt is wrong, not the finding.

**The whole of CLAUDE.md goes into the system prompt, deliberately.** It is around ten thousand
characters, which is a fifth of a penny at Luna's input rate, and it is the difference between a
reviewer that knows a second cursor per chain is forbidden and one that suggests adding a mutex.
Reading the file rather than embedding a distilled copy means the rules cannot go stale here while
staying current there.

Usage:
    pr-review.py --diff pr.diff --title "feat: ..." [--body-file body.txt]
                 [--json] [--json-output review.json]

Reads `OPENAI_API_KEY` from the environment. Prints Markdown on stdout, diagnostics on stderr.
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ENDPOINT = "https://api.openai.com/v1/chat/completions"
MODEL = "gpt-5.6-luna"
CLAUDE_MD = Path("CLAUDE.md")

# A cap on what we send, not on what we can afford. Luna's context window is 1.05M tokens, so this
# is nowhere near the model's limit - it is a guard against one 40,000-line generated-code PR
# quietly costing a hundred times what a normal review costs. Elision is reported in the comment
# so nobody reads a partial review as a whole one.
#
# **Spent per file, not off the end.** Cutting the diff at this many characters spends the whole
# budget in `git diff` path order, so one large file evicts every file sorted after it. Measured on
# #1282: an 847,499-character recorded introspection fixture was 73% of a 1,161,244-character diff,
# the cut landed inside it, and `tests/graph_schema_golden.rs` was invisible - so the reviewer read a
# renderer with none of the thirteen assertions that prove it correct, and raised the same finding
# four times while every reply cited tests it could not see. `budget_diff` caps the largest files
# instead, which on that diff leaves 63 of 65 files whole.
MAX_DIFF_CHARS = 400_000

# Prior reviews are context, not the subject. Bounded so a long-lived pull request cannot crowd the
# diff out of the window with its own history.
MAX_PRIOR_CHARS = 40_000

# Marks our comments so a re-review can find its predecessors, and so a human scrolling a long PR
# can tell the outside reader from the firm's own.
MARKER = "<!-- pr-review:luna -->"

SYSTEM = """You are Jules, the outside reviewer for the nuthatch repository. You came up through \
Mechanical. You have no stake in the sprint, you did not write this change, and you are not \
required to find something. A short review that says "this is fine, here is the one thing I \
checked hardest" is a good review.

How you talk. Blunt, unhurried, no ceremony. You do not open with praise and you do not thank \
anyone for their contribution. You would rather take the generator down and fix it properly than \
keep patching it while everyone assures you it is fine, and you say so in those terms. You do not \
trust the official story: a comment claiming a thing is safe is a claim, not evidence, and where \
the code and the comment disagree you say which one you believe and why. You are not rude. You are \
just not interested in softening anything. Short sentences. No exclamation marks, no emoji, no \
praise for the author, no jokes about the code.

The voice lives in `summary` and nowhere else. Every `title` and `detail` stays plain, precise and \
technical, because those are the parts somebody has to act on at two in the morning. Never let the \
manner change the judgement: do not invent a finding to sound rigorous, do not soften a real one \
to sound easy-going, and never let `confidence` drift to suit the tone of the sentence beside it. \
A reviewer who performs is worse than no reviewer at all.

The project's standing brief follows. Its non-negotiables are not suggestions and not stylistic \
preferences: a change that threatens the RAM budget, adds a phone-home, puts LLM output in the \
runtime data path, multiplexes two chains behind one cursor, mutates a sealed segment, or pulls in \
a copyleft dependency is a defect regardless of how well it is written.

--- CLAUDE.md ---
{claude_md}
--- end CLAUDE.md ---

**A release is a range, and the range includes what is already on the base.** You are told which \
commits sit on the base branch since the previous release, as well as which sit on this branch. A \
release pull request legitimately contains nothing but a version bump, a release note and the \
documentation that moves with it - the fix it announces landed earlier, on its own pull request, and \
was reviewed there. "The implementation is not in this diff" is a description of a diff, not a \
finding about a release: check the two commit lists before you write it, and if the change is named \
in either, the release contains it. It is a finding only when it appears in neither list, or when no \
lists were supplied at all - and then say which of those two it is.

**A file already on the default branch is not this pull request's work.** You are told which files \
this branch changes relative to the default branch. A file that appears in the diff but not on that \
list is byte-identical to the default branch: it arrived because this branch merged the default \
branch in, or because it is stacked on another branch that did, and it was reviewed on the pull \
request that landed it. Do not raise a finding against it. Merging this branch does not change it. \
This is the third route to the same mistake #1056 and #1201 fixed: a `high` was raised against \
`src/analytics_budget.rs` on a subgraph-tooling pull request, on the same day and by this reviewer, \
having shipped that exact file at 88/100 on the pull request it belonged to.

**You have reviewed this pull request before, and those verdicts are evidence.** Your previous \
reviews are supplied when they exist. Read them first.

- A finding you already raised that has been addressed is **closed**. Say so in `summary` and do \
  not raise it again in another form.
- A verdict may not move from `ship` to `changes-requested` unless the diff changed in the place \
  the new finding is about. Say which commit or hunk changed your mind. A branch that has only had \
  defects removed since you approved it has not become less safe, and a review that says otherwise \
  is a review that is not converging. One pull request took thirteen passes, returned `ship` at \
  91/100 on the seventh, and then went back to `changes-requested` four times running while every \
  finding it raised was being fixed.
- A fresh medium on every pass is a smell in **you**, not in the branch. If this pass finds nothing \
  that the previous pass would have called blocking, the honest verdict is `ship`, and "there is \
  always one more thing" is not a reason to withhold it.

**A shortened file is a file you have partly seen, not a file without the rest.** An oversized file is \
cut to fit the budget and carries a marker saying so where it was cut. Treat what follows the marker as \
unknown, not as absent: do not report a thing missing from a shortened file, and do not raise a finding \
whose evidence would be in the part you were not shown. Say in `summary` that the file was shortened. \
This is #1282's lesson about *you*: a recorded 847,499-character fixture was 73% of that diff, the old \
flat cut landed inside it, and the whole test file proving the change correct was invisible - so the same \
finding was raised four passes running, each time against code whose tests were in the part not sent. \
The budget is now spent per file so that cannot recur, but a shortened file can still mislead you.

**You cannot run anything.** You have the diff, not a test runner, not a debugger, and not the rest \
of the file. So:

- Never state that a named test fails, panics or passes. You did not run it. Say what in the source \
  leads you to expect that, and quote the lines that do. A review claimed at certainty 99 that a \
  named test "panics instead of passing"; the test passed, because a value the reviewer could not \
  see was recovered elsewhere.
- The direction a value flows, the order two functions run in, and what a loop actually reaches are \
  claims about behaviour, not about text. Quote the lines you are reading them from. A review \
  asserted at certainty 94 that a propagation ran caller to callee; it runs callee to caller, and \
  three lines of the function say so.
- Certainty above 90 is for something the diff itself proves - a missing bound, a wrong constant, a \
  swapped argument you can point at. An inference about runtime behaviour from partial source caps \
  at 80 however sure it feels.

Review the diff you are given. Judge the change that is there, not the change you would have made. \
Rank correctness above style; a naming quibble is not a finding. Prefer one concrete failure \
scenario - specific inputs or state producing a specific wrong result - over three vague concerns. \
If a finding depends on code you cannot see in the diff, say so rather than assuming.

`confidence` is how confident you are that this change is safe to merge as it stands, 0 to 100. \
Reserve below 50 for a change you believe carries a real defect. A clean, small, well-tested diff \
should score high; do not manufacture doubt to look rigorous."""

SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["confidence", "verdict", "summary", "findings"],
    "properties": {
        "confidence": {
            "type": "integer",
            "minimum": 0,
            "maximum": 100,
            "description": "0-100, confidence this change is safe to merge as it stands.",
        },
        "verdict": {"type": "string", "enum": ["ship", "comment", "changes-requested"]},
        "summary": {
            "type": "string",
            "description": "Two or three sentences: what the change does and how it reads.",
        },
        "findings": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": False,
                "required": ["severity", "file", "line", "title", "detail", "certainty"],
                "properties": {
                    "severity": {"type": "string", "enum": ["high", "medium", "low"]},
                    "certainty": {
                        "type": "integer",
                        # Bounded, not merely typed. `certainty: 150` satisfied "integer" and
                        # rendered as a score, against a contract that says 0-100 (review of #1056).
                        "minimum": 0,
                        "maximum": 100,
                        "description": "0-100, how sure you are THIS FINDING is real - a different "
                        "question from the merge-safety `confidence` above. A correct high-severity "
                        "finding makes a PR unsafe and drives that score down, so the two moved "
                        "together and a reader could not use either to triage. Be honest and "
                        "willing to be low: a finding you are 40 sure of is worth raising and worth "
                        "labelling as such.",
                    },
                    "file": {"type": "string"},
                    "line": {"type": "integer", "description": "0 if not tied to one line."},
                    "title": {"type": "string"},
                    "detail": {
                        "type": "string",
                        "description": "The concrete failure scenario: inputs or state, and the wrong result.",
                    },
                },
            },
        },
    },
}


def call_model(api_key, model, system, user, attempts=3):
    """POST to chat/completions with a strict JSON schema. Raises on anything short of a parsed body."""
    payload = {
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "pr_review", "strict": True, "schema": SCHEMA},
        },
        "max_completion_tokens": 8000,
    }
    req = urllib.request.Request(
        ENDPOINT,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    last = None
    for attempt in range(1, attempts + 1):
        try:
            with urllib.request.urlopen(req, timeout=300) as resp:
                body = json.loads(resp.read().decode())
            break
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode(errors="replace")[:2000]
            last = f"HTTP {exc.code}: {detail}"
            # 4xx other than rate-limiting will not improve on a retry.
            if exc.code not in (408, 409, 429) and exc.code < 500:
                raise SystemExit(f"pr-review: {last}")
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as exc:
            last = f"{type(exc).__name__}: {exc}"
        if attempt < attempts:
            wait = 2**attempt
            print(f"pr-review: attempt {attempt} failed ({last}), retrying in {wait}s", file=sys.stderr)
            time.sleep(wait)
    else:
        raise SystemExit(f"pr-review: {attempts} attempts failed, last was {last}")

    choice = body["choices"][0]
    if choice.get("finish_reason") not in (None, "stop"):
        # A response cut off at the token ceiling is a partial review, and a partial review that
        # renders as a whole one is exactly the failure this script refuses to have.
        raise SystemExit(f"pr-review: response did not finish cleanly ({choice.get('finish_reason')})")
    content = choice["message"].get("content")
    if not content:
        raise SystemExit("pr-review: model returned an empty message")
    usage = body.get("usage", {})
    print(
        f"pr-review: {usage.get('prompt_tokens', '?')} in, {usage.get('completion_tokens', '?')} out",
        file=sys.stderr,
    )
    return json.loads(content)


SEVERITY_MARK = {"high": "**high**", "medium": "medium", "low": "low"}


def render(review, model, elided):
    """Markdown comment. The score goes first because it is the bit anyone actually reads."""
    score = review["confidence"]
    bar = "█" * (score // 10) + "░" * (10 - score // 10)
    lines = [
        MARKER,
        f"### Jules · confidence {score}/100",
        "",
        f"`{bar}` · verdict: **{review['verdict']}**",
        "",
        review["summary"],
        "",
    ]
    findings = review["findings"]
    if findings:
        lines.append(f"#### {len(findings)} finding{'s' if len(findings) != 1 else ''}")
        lines.append("")
        order = {"high": 0, "medium": 1, "low": 2}
        for f in sorted(findings, key=lambda f: order.get(f["severity"], 3)):
            where = f["file"] + (f":{f['line']}" if f["line"] else "")
            lines.append(f"- {SEVERITY_MARK.get(f['severity'], f['severity'])} · `{where}` - **{f['title']}**")
            lines.append(f"  {f['detail']}")
        lines.append("")
    else:
        lines.append("No findings.")
        lines.append("")
    if elided:
        shown = ", ".join(f"`{name}` ({kept:,} of {size:,} chars)" for name, kept, size in elided)
        lines.append(
            f"> The diff exceeded {MAX_DIFF_CHARS:,} characters, so the largest files were shortened "
            f"and this review saw them only in part: {shown}. Every other file was whole."
        )
        lines.append("")
    lines.append(
        f"<sub>Jules · {model} · required approval · push a fix or comment `/re-review` to run again</sub>"
    )
    return "\n".join(lines)


def budget_diff(diff: str, budget: int) -> tuple[str, list[tuple[str, int, int]]]:
    """Fit a diff into `budget` characters by shortening its largest files.

    A flat `diff[:budget]` spends the whole allowance in path order, so a single large file evicts
    every file after it - and a recorded fixture or a lock file is exactly the sort of large file a
    review least needs and most often sits early in the order. Here every file keeps at least as much
    as every smaller file keeps, which is the most even split available: find the largest per-file cap
    whose total fits, and apply it.

    Returns the diff and, for each shortened file, `(path, kept, original)`.
    """
    sections = re.split(r"(?m)^(?=diff --git )", diff)
    head, files = ("", sections) if sections[0].startswith("diff --git ") else (sections[0], sections[1:])
    if not files:
        return (diff[:budget], [(("(whole diff)"), budget, len(diff))] if len(diff) > budget else [])
    room = max(budget - len(head), 0)
    sizes = [len(f) for f in files]
    if sum(sizes) <= room:
        return (diff, [])
    # The largest cap C with sum(min(size, C)) <= room. Bisected rather than solved, because the
    # closed form has to special-case ties and this runs once per review.
    lo, hi = 0, max(sizes)
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if sum(min(n, mid) for n in sizes) <= room:
            lo = mid
        else:
            hi = mid - 1
    cap = lo
    out, elided = [head], []
    for section in files:
        if len(section) <= cap:
            out.append(section)
            continue
        path = section.split("\n", 1)[0].removeprefix("diff --git ").split(" b/")[-1]
        note = f"\n[pr-review: this file was shortened to fit the review budget; {len(section) - cap:,} characters are not shown]\n"
        keep = max(cap - len(note), 0)
        out.append(section[:keep] + note)
        elided.append((path, keep, len(section)))
    return ("".join(out), elided)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--diff", required=True, type=Path, help="unified diff of the pull request")
    ap.add_argument("--title", default="", help="pull request title")
    ap.add_argument("--body-file", type=Path, help="file holding the pull request description")
    ap.add_argument(
        "--commits-file",
        type=Path,
        help="file holding the PR's commit subjects, one per line. Without it a reviewer can only "
             "see this diff, and reports a change absent from the diff as absent from the branch - "
             "which is how the 3.1.0 release PR was told its security fix was missing (#1056).",
    )
    ap.add_argument("--base-file", type=Path, help="file holding the PR's base branch name")
    ap.add_argument(
        "--base-commits-file",
        type=Path,
        help="file holding the commit subjects already on the base since the previous release, one "
             "per line. The commit list above covers this branch only, which is not the same "
             "question: a release branch cut *after* its fix merged carries the version bump and "
             "nothing else, and the reviewer then reports the fix missing from a release that "
             "contains it. That is #1056 again by another route, and it is what blocked 3.6.1.",
    )
    ap.add_argument(
        "--base-range",
        default="",
        help="what the base commit list covers, e.g. 'v3.6.0...main (12 of 12 commits listed)'. "
             "Stated rather than implied, because a truncated range must not pass for a whole one.",
    )
    ap.add_argument(
        "--own-files-file",
        type=Path,
        help="file holding the paths this branch changes relative to the DEFAULT branch, one per "
             "line. A stacked branch that merged `main` in has a diff against its own base that "
             "carries all of main with it, and the reviewer then raises findings against files it "
             "reviewed and shipped elsewhere. #1056 and #1201 fixed the same blindness from the "
             "commit-list side; this is the file side of it.",
    )
    ap.add_argument(
        "--prior-reviews-file",
        type=Path,
        help="file holding this reviewer's previous reviews of this pull request, newest last. "
             "Without them every pass re-derives from nothing and finds a fresh medium: one PR took "
             "thirteen passes and reversed its own `ship` four times while its findings were being "
             "fixed.",
    )
    ap.add_argument("--model", default=MODEL)
    ap.add_argument("--json", action="store_true", help="print the raw structured review instead")
    ap.add_argument(
        "--dry-run",
        action="store_true",
        help="print the prompt that would be sent and exit, without calling the model or needing a "
             "key. This is how the harness is tested: #1056 exists because nobody could see what "
             "the reviewer was actually given, and 'we pass the commits now' is a claim until "
             "something prints them.",
    )
    ap.add_argument(
        "--json-output",
        type=Path,
        help="also write the raw structured review to this file (for the App check conclusion)",
    )
    args = ap.parse_args()

    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key and not args.dry_run:
        raise SystemExit("pr-review: OPENAI_API_KEY is not set")

    diff = args.diff.read_text(errors="replace")
    if not diff.strip():
        raise SystemExit("pr-review: the diff is empty - nothing to review")
    diff, elided = budget_diff(diff, MAX_DIFF_CHARS)

    body = args.body_file.read_text(errors="replace") if args.body_file else ""
    claude_md = CLAUDE_MD.read_text() if CLAUDE_MD.exists() else "(not available)"

    commits = args.commits_file.read_text(errors="replace").strip() if args.commits_file else ""
    base = args.base_file.read_text(errors="replace").strip() if args.base_file else ""
    base_commits = (
        args.base_commits_file.read_text(errors="replace").strip()
        if args.base_commits_file
        else ""
    )
    own_files = (
        args.own_files_file.read_text(errors="replace").strip() if args.own_files_file else ""
    )
    # Newest last, and bounded: a long-lived PR accumulates these, and the point is the trend and
    # the open findings, not every word of every pass.
    prior = (
        args.prior_reviews_file.read_text(errors="replace").strip()
        if args.prior_reviews_file
        else ""
    )
    if len(prior) > MAX_PRIOR_CHARS:
        prior = "(earlier reviews elided)\n\n" + prior[-MAX_PRIOR_CHARS:]
    user = (
        f"Pull request title: {args.title}\n\n"
        f"Description:\n{body or '(none)'}\n\n"
        f"Base branch: {base or '(not supplied)'}\n\n"
        f"Commits on this branch ({len(commits.splitlines())}):\n{commits or '(not supplied)'}\n\n"
        # Merging this branch ships both lists. Rendered separately from the branch's own commits so
        # the reviewer can tell "this pull request wrote it" from "this release contains it".
        f"Already on the base, and therefore in this release "
        f"[{args.base_range or 'range not supplied'}] "
        f"({len(base_commits.splitlines())}):\n{base_commits or '(not supplied)'}\n\n"
        # Byte-identical to the default branch means merging this branch does not change it, so a
        # finding against it is a finding about somebody else's merged pull request.
        f"Files this branch changes relative to the default branch "
        f"({len(own_files.splitlines())}). Anything in the diff and not on this list came from a "
        f"merge and is already on the default branch:\n{own_files or '(not supplied)'}\n\n"
        f"Your previous reviews of this pull request, oldest first:\n"
        f"{prior or '(none - this is your first pass)'}\n\n"
        f"Diff:\n```diff\n{diff}\n```"
    )
    if args.dry_run:
        print(user)
        return
    review = call_model(api_key, args.model, SYSTEM.format(claude_md=claude_md), user)

    if args.json_output:
        args.json_output.write_text(json.dumps(review, indent=2) + "\n")

    if args.json:
        print(json.dumps(review, indent=2))
    else:
        print(render(review, args.model, elided))


if __name__ == "__main__":
    main()
