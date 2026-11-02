#!/usr/bin/env python3
"""Which Terminal-Bench trials looked up the benchmark: every tool call (Marv's trajectory) whose arguments name the
benchmark in a URL (its repos and mirrors, Harbor's hub, tbench.ai, a web search for it). Plain web searches are listed,
not flagged. A flagged trial's pass doesn't count: the clean score below counts it as failed.

  python3 -I evals/terminal_bench/leaks.py evals/results/terminal-bench/<job>           flagged trials and the call
  python3 -I evals/terminal_bench/leaks.py --json evals/results/terminal-bench/<job>    one JSON line per flagged trial
"""
import json, re, sys
from pathlib import Path
from urllib.parse import unquote

URL = re.compile(r"(?:https?://|git@|\b)(?:[\w-]+\.)+[a-z]{2,}/[^\s\"'<>)]*", re.I)
# A URL of the benchmark's own, or one whose path or query names it (a repo, a mirror, a search for it). Task files
# carry the canary string and "Terminal-Bench" in comments; those aren't URLs, so they don't count.
BENCH = re.compile(r"harbor-framework|laude-institute|terminal[-_ +]?bench|tbench|harborframework|TBENCH-SALT", re.I)
SEARCH = re.compile(r"duckduckgo|google\.[a-z.]+/search|bing\.com|search\.brave|search\.yahoo|startpage|searx|kagi\.com", re.I)


def calls(trajectory: Path):
    for line in trajectory.read_text(errors="replace").splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("type") == "tool" and isinstance(event.get("call"), dict):
            yield event["call"].get("name", "?"), str(event["call"].get("arguments", ""))


def reward(trial: Path) -> float:
    try:
        return json.loads((trial / "result.json").read_text())["verifier_result"]["rewards"]["reward"] or 0.0
    except (OSError, ValueError, KeyError, TypeError):
        return 0.0


def main(argv: list[str]) -> None:
    as_json = "--json" in argv
    trials = passed = clean = 0
    for job in (Path(a) for a in argv if a != "--json"):
        for trial in sorted(p for p in job.iterdir() if (p / "result.json").exists()):
            trials += 1
            ok = reward(trial) >= 1
            hits = []
            trajectory = trial / "agent" / "trajectory.jsonl"
            if trajectory.exists():
                hits = [(name, args) for name, args in calls(trajectory) if any(BENCH.search(u) for u in URL.findall(unquote(args)))]
                searches = sum(1 for _, args in calls(trajectory) if SEARCH.search(unquote(args)))
                if searches and not hits and not as_json:
                    print(f"{trial.name}  searched the web {searches}× (not about the benchmark)")
            passed += ok
            clean += ok and not hits
            if not hits:
                continue
            if as_json:
                print(json.dumps({"trial": trial.name, "passed": ok, "calls": [{"tool": n, "args": a} for n, a in hits]}))
            else:
                print(f"{trial.name}  {'PASS' if ok else 'fail'}  {len(hits)} call(s)")
                for name, args in hits[:3]:
                    print(f"    {name}: {unquote(args)[:200]}")
    if not as_json:
        print(f"\n{trials} trials: {passed} passed, {clean} passed without looking up the benchmark")


if __name__ == "__main__":
    main(sys.argv[1:])
