#!/usr/bin/env python3
"""Review one pull request's diff with GPT-5.6 Luna and print a Markdown comment.

The repo takes fifteen pull requests a day and every one of them already carries a `Reviewed-by:`
signature from an agent. What it has never had is a reviewer with no stake in the sprint. We have at
least two recorded cases of a double-dispatched run approving a PR and arming auto-merge while
missing a real defect, and a second opinion from the same firm is not a second opinion.

This is that outside reader. It is deliberately advisory: it posts a comment with a confidence score
and findings, it is not in `.github/required-checks.txt`, and it blocks nothing. Promote it to a
gate only once its comments have been read for a while and found to be worth reading.

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
    pr-review.py --diff pr.diff --title "feat: ..." [--body-file body.txt] [--json]

Reads `OPENAI_API_KEY` from the environment. Prints Markdown on stdout, diagnostics on stderr.
"""
import argparse
import json
import os
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
# quietly costing a hundred times what a normal review costs. Truncation is reported in the comment
# so nobody reads a partial review as a whole one.
MAX_DIFF_CHARS = 400_000

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
                "required": ["severity", "file", "line", "title", "detail"],
                "properties": {
                    "severity": {"type": "string", "enum": ["high", "medium", "low"]},
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


def render(review, model, truncated):
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
    if truncated:
        lines.append(
            f"> The diff was truncated at {MAX_DIFF_CHARS:,} characters, so this review did not see "
            "all of it."
        )
        lines.append("")
    lines.append(
        f"<sub>Jules · {model} · advisory, blocks nothing · comment `/re-review` to run again</sub>"
    )
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--diff", required=True, type=Path, help="unified diff of the pull request")
    ap.add_argument("--title", default="", help="pull request title")
    ap.add_argument("--body-file", type=Path, help="file holding the pull request description")
    ap.add_argument("--model", default=MODEL)
    ap.add_argument("--json", action="store_true", help="print the raw structured review instead")
    args = ap.parse_args()

    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        raise SystemExit("pr-review: OPENAI_API_KEY is not set")

    diff = args.diff.read_text(errors="replace")
    if not diff.strip():
        raise SystemExit("pr-review: the diff is empty - nothing to review")
    truncated = len(diff) > MAX_DIFF_CHARS
    if truncated:
        diff = diff[:MAX_DIFF_CHARS]

    body = args.body_file.read_text(errors="replace") if args.body_file else ""
    claude_md = CLAUDE_MD.read_text() if CLAUDE_MD.exists() else "(not available)"

    user = (
        f"Pull request title: {args.title}\n\n"
        f"Description:\n{body or '(none)'}\n\n"
        f"Diff:\n```diff\n{diff}\n```"
    )
    review = call_model(api_key, args.model, SYSTEM.format(claude_md=claude_md), user)

    if args.json:
        print(json.dumps(review, indent=2))
    else:
        print(render(review, args.model, truncated))


if __name__ == "__main__":
    main()
