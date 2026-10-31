#!/usr/bin/env python3
"""Why runs failed: every failed run of one or more bench result folders, sorted into a cause, with the line that
shows it. The question each cause answers is "what would have to change for this run to pass", so the harness and the
bench get fixed where the fault is theirs and the model's misses are counted as such.

  python3 -I evals/failures.py evals/results/<run> [<run> …]          the table and every failure's evidence
  python3 -I evals/failures.py --json evals/results/<run> [<run> …]   one JSON line per failed run

Causes, first match wins (Marv's stream is read; other harnesses are judged on results.jsonl alone):
  refused       a tool call was refused for want of an approval ("no one to ask", "refused"): the bench's flags, or
                a guard doing its job
  announced     the run ended on a reply that announced a step and made no tool call after it (Marv #90)
  no-tools      the run ended without a single tool call
  step-limit    the run hit --max-steps
  timeout       the run hit --timeout
  error         the harness exited with an error (provider, crash)
  edit-errors   edit tool errors in a run that then failed its check: the edits didn't land as meant
  wrong-result  the run ended normally and the hidden check failed: the model's answer was wrong
"""
import json, re, sys
from collections import Counter, defaultdict
from pathlib import Path

REFUSED = re.compile(r"needs the user's approval|there's no one to ask|^refused |was refused", re.I)
EDIT_TOOLS = {"edit_file", "write_file", "apply_patch", "Edit", "Write", "MultiEdit", "edit", "write", "patch"}


def stream_events(log: Path):
    path = log / "stream.jsonl"
    if not path.exists():
        return []
    events = []
    for line in path.read_text(errors="replace").splitlines():
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return events


def last_line(text: str) -> str:
    lines = [l.strip() for l in (text or "").splitlines() if l.strip()]
    return lines[-1][:160] if lines else ""


def classify(r: dict, log: Path):
    """(cause, evidence) for one failed run."""
    events = stream_events(log) if r["harness"] == "marv" else []
    ends = [e for e in events if e.get("type") == "tool_end"]
    for e in ends:
        if e.get("error") and REFUSED.search(str(e.get("result", ""))):
            return "refused", f"{e.get('tool')}: {str(e.get('result'))[:140]}"
    reason = r.get("reason")
    if reason == "timeout":
        return "timeout", f"{r.get('ms', 0) / 1000:.0f}s"
    if reason in ("max_steps", "error_max_turns"):
        return "step-limit", f"{r.get('requests', 0)} requests"
    if reason in ("error", "error_during_execution"):
        return "error", last_line((log / "stderr.txt").read_text(errors="replace")) if (log / "stderr.txt").exists() else ""
    calls = sum((r.get("tools") or {}).values())
    reply = r.get("reply") or next((e.get("reply", "") for e in reversed(events) if e.get("type") == "result"), "")
    if events:
        starts = [i for i, e in enumerate(events) if e.get("type") == "tool_start"]
        text_after = any(e.get("type") == "text_delta" for e in events[(starts[-1] if starts else 0):])
        if reply and text_after and ANNOUNCE.search(last_line(reply)):
            return "announced", last_line(reply)
    if calls == 0:
        return "no-tools", last_line(reply)
    edit_errors = [e for e in ends if e.get("error") and e.get("tool") in EDIT_TOOLS]
    if edit_errors or (r.get("toolErrors", 0) and any(t in EDIT_TOOLS for t in (r.get("tools") or {}))):
        what = f"{edit_errors[0].get('tool')}: {str(edit_errors[0].get('result'))[:120]}" if edit_errors else f"{r.get('toolErrors')} tool errors"
        return "edit-errors", what
    check = (r.get("checkOutput") or "").splitlines()
    failed = next((l.strip() for l in check if l.strip().startswith("(fail)")), last_line(r.get("checkOutput") or ""))
    return "wrong-result", failed[:160]


# The tail of a reply that says what it will do next (a looser cousin of marv-agent's announce.rs: this only labels
# finished runs, so a false match costs nothing but a mislabel).
ANNOUNCE = re.compile(
    r"(:\s*\**$)|^(now |next,? |first,? |then,? )?let me |^now \w+ing\b|^(starting|beginning|proceeding|moving on)\b|^i('ll| will) (now |start|begin)",
    re.I,
)


def main(argv):
    as_json = "--json" in argv
    dirs = [Path(a) for a in argv if a != "--json"]
    if not dirs:
        sys.exit(__doc__)
    rows = []
    for d in dirs:
        for line in (d / "results.jsonl").read_text().splitlines():
            r = json.loads(line)
            if r.get("pass_"):
                rows.append((r, None, None))
                continue
            log = d / "logs" / r["harness"] / f"{r['task']}-{r['rep']}"
            cause, evidence = classify(r, log)
            rows.append((r, cause, evidence))
    if as_json:
        for r, cause, evidence in rows:
            if cause:
                print(json.dumps({"harness": r["harness"], "model": r.get("model"), "label": r.get("label"),
                                  "task": r["task"], "rep": r["rep"], "cause": cause, "evidence": evidence}))
        return
    groups = defaultdict(list)
    for r, cause, evidence in rows:
        groups[(r["harness"], r.get("model") or "")].append((r, cause, evidence))
    causes = ["refused", "announced", "no-tools", "step-limit", "timeout", "error", "edit-errors", "wrong-result"]
    print(f"{'harness':9} {'model':32} {'pass':>9}  " + " ".join(f"{c:>12}" for c in causes))
    for (h, m), rs in sorted(groups.items()):
        n, ok = len(rs), sum(1 for r, c, _ in rs if c is None)
        count = Counter(c for _, c, _ in rs if c)
        print(f"{h:9} {m[:32]:32} {ok:>3}/{n:<3}{ok / n:4.0%}  " + " ".join(f"{count.get(c, 0) or '-':>12}" for c in causes))
    for (h, m), rs in sorted(groups.items()):
        failed = [(r, c, e) for r, c, e in rs if c]
        if not failed:
            continue
        print(f"\n{h} {m}:")
        for r, cause, evidence in sorted(failed, key=lambda x: (causes.index(x[1]), x[0]["task"], x[0]["rep"])):
            print(f"  {cause:12} {r['task']}#{r['rep']:<3} {evidence}")


if __name__ == "__main__":
    main(sys.argv[1:])
