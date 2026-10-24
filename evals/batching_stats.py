"""How Marv's model batches its tool calls (#215): steps per turn, calls per step, and the runs of steps that each made
one read-only call, which could have been one step.

usage: python3 -I evals/batching_stats.py <folder> [<folder> …]

Each folder is searched for trajectories (`*.jsonl`, the records `crates/marv-session/src/trajectory.rs` writes): a
bench run (`evals/results/<run>`), a SWE-bench run's `logs/`, or `~/.marv/trajectories`. One row per folder. Only the
main agent's steps count (subagents have their own prompt).

A call is read-only when it's read_file, grep or glob, or a bash command whose every part is a reader: the
`marv_core::command::is_read_only_command` list plus `sed -n`, awk and nl, with no redirection into a file. It's a
text guess for statistics, never a decision about what to run.
"""
import glob, json, os, re, statistics, sys

READERS = set(
    "echo printf pwd whoami id groups ls cat head tail wc grep egrep fgrep rg ag which type env printenv date uname "
    "hostname stat file du df tree realpath readlink basename dirname test [ true false : sleep sort uniq cut tr diff "
    "cmp less wait nl column xxd od find awk sed cd pushd export set".split()
)
GIT_READERS = set("status log diff show rev-parse ls-files describe blame grep remote branch".split())


def parts(command):
    """The simple commands in a shell line, split on ; | & and newlines outside quotes."""
    out, cur, quote = [], "", None
    for ch in command:
        if quote:
            cur += ch
            quote = None if ch == quote else quote
        elif ch in "\"'":
            quote, cur = ch, cur + ch
        elif ch in ";|&\n":
            out, cur = out + [cur], ""
        else:
            cur += ch
    return [p.strip() for p in out + [cur] if p.strip()]


def read_only_command(command):
    simple = parts(command)
    if not simple:
        return False
    for part in simple:
        part = re.sub(r"\d?>\s*/dev/null|2>&1|\d?>&\d", "", part)
        if ">" in part:
            return False
        words = [w for w in part.split() if not re.match(r"^[A-Za-z_]\w*=", w)]
        if not words:
            continue
        head = words[0].split("/")[-1]
        if head == "git":
            if len(words) < 2 or words[1] not in GIT_READERS:
                return False
        elif head == "sed" and (" -i" in part or "-n" not in part):
            return False
        elif head == "find" and any(x in part for x in ("-delete", "-exec", "-ok")):
            return False
        elif head == "awk" and "system(" in part:
            return False
        elif head not in READERS:
            return False
    return True


def read_only(name, arguments):
    if name in ("read_file", "grep", "glob"):
        return True
    if name == "bash":
        try:
            return read_only_command(json.loads(arguments).get("command", ""))
        except (ValueError, AttributeError):
            return False
    return False


def turns(folder):
    """Each main-agent turn's steps: [{ms, prompt, calls: [(name, arguments)]}]."""
    found = {}
    for path in glob.glob(os.path.join(folder, "**", "*.jsonl"), recursive=True):
        for line in open(path, errors="replace"):
            try:
                record = json.loads(line)
            except ValueError:
                continue
            kind = record.get("type")
            if kind not in ("request", "tool") or record.get("agent", "main") != "main":
                continue
            steps = found.setdefault((path, record.get("session"), record.get("turn")), [])
            if kind == "request":
                usage = record.get("usage") or {}
                steps.append({"ms": record.get("ms", 0), "prompt": usage.get("promptTokens", 0), "calls": []})
            elif steps:
                call = record.get("call") or {}
                steps[-1]["calls"].append((call.get("name"), call.get("arguments", "")))
    return [steps for steps in found.values() if steps]


def row(folder):
    all_turns = turns(folder)
    steps = [s for t in all_turns for s in t]
    with_calls = [s for s in steps if s["calls"]]
    single = [s for s in with_calls if len(s["calls"]) == 1]
    single_ro = [s for s in single if read_only(*s["calls"][0])]
    saved = saved_ms = saved_prompt = 0
    for t in all_turns:
        flags = [len(s["calls"]) == 1 and read_only(*s["calls"][0]) for s in t]
        i = 0
        while i < len(t):
            if not flags[i]:
                i += 1
                continue
            j = i
            while j < len(t) and flags[j]:
                j += 1
            extra = t[i + 1 : j]
            saved += len(extra)
            saved_ms += sum(s["ms"] for s in extra)
            saved_prompt += sum(s["prompt"] for s in extra)
            i = j
    per_turn = [len(t) for t in all_turns] or [0]
    total_ms = sum(s["ms"] for s in steps) or 1
    total_prompt = sum(s["prompt"] for s in steps) or 1
    pct = lambda a, b: 100 * a / b if b else 0
    return {
        "folder": folder,
        "turns": len(all_turns),
        "steps": len(steps),
        "steps/turn": statistics.mean(per_turn),
        "median": statistics.median(per_turn),
        "calls/step": pct(sum(len(s["calls"]) for s in with_calls), len(with_calls)) / 100,
        "1-call %": pct(len(single), len(with_calls)),
        "1-call ro %": pct(len(single_ro), len(single)),
        "foldable %": pct(saved, len(steps)),
        "foldable ms %": pct(saved_ms, total_ms),
        "foldable prompt %": pct(saved_prompt, total_prompt),
    }


def label(folder):
    """A run's name: the folder's, or its parent's for a SWE-bench run's `logs`."""
    path = os.path.normpath(folder)
    return os.path.basename(os.path.dirname(path)) if os.path.basename(path) == "logs" else os.path.basename(path)


def main(folders):
    if not folders:
        sys.exit(__doc__)
    rows = [row(f) for f in folders]
    keys = [k for k in rows[0] if k != "folder"]
    width = max(len(label(r["folder"])) for r in rows)
    print(f"{'run':{width}}  " + "  ".join(f"{k:>8}" for k in keys))
    for r in rows:
        name = label(r["folder"])
        cells = [f"{r[k]:8.0f}" if isinstance(r[k], int) else f"{r[k]:8.1f}" for k in keys]
        print(f"{name:{width}}  " + "  ".join(cells))


if __name__ == "__main__":
    main(sys.argv[1:])
