#!/usr/bin/env python3
"""Real-model evals (#73): Marv's eval tasks run through `marv -p`, and through `claude -p` or `opencode run` for comparison.

  python3 -I evals/bench.py --verify                     every check fails on the task's repo and passes with its solution
  python3 -I evals/bench.py --harness marv,claude,opencode --repeat 5 [--tasks a,b] [--label haiku] [--budget 1.00]
        [--marv-model anthropic/claude-haiku-5.5] [--claude-model claude-haiku-5-5] [--marv-bin target/release/marv]
        [--concurrency 6] [--timeout 300]
  python3 -I evals/bench.py --report evals/results/<run>  [<run> …]

A task is a folder in evals/tasks: task.md (the request), repo/ (the starting files), check/ (hidden tests, copied in
only after the agent finishes), solution/ (files that make the check pass, for --verify). Each run gets a fresh git
repository in a temp folder. The check is `bun test ./__eval__.test.js`, only ever in bubblewrap (no network, a cleared
environment, an empty home, only the workspace writable): it runs code the model wrote.

Marv runs with a throwaway MARV_CONFIG_DIR (provider OpenRouter, the key from OPENROUTER_API_KEY or ~/.marv), so your
own MCP servers, skills and settings stay out; `--approve files,process,mcp`. Claude Code runs with project settings
only and no MCP servers but the task's. The MCP tasks get the tracker server (`marv-oracle tracker`, built by
`cargo build -p marv-oracle`) on tests/oracle/mcp/<data>. A TypeScript task's repo gets the compiler from
evals/node_modules (`bun install` in evals/, done here when it's missing).

Results go to evals/results/<time>-<label>/: results.jsonl (one line per run) and logs/<harness>/<task>-<rep>/ (the
stream, Marv's trajectory, stderr, the diff). Costs are what OpenRouter reports for Marv and Claude Code's own
estimate for Claude Code (on a subscription that is not a bill).
"""
import argparse, json, os, re, shutil, subprocess, sys, tempfile, threading, time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EVALS = ROOT / "evals"
TASKS = EVALS / "tasks"
MCP_DATA = ROOT / "tests/oracle/mcp"
REPLY_FILE, COMMENTS = "__eval_reply__.md", "__tracker_comments__.json"
CLAUDE_TOOLS = "Read,Edit,Write,Glob,Grep,Bash,TodoWrite,mcp__tracker"
# Tasks that don't measure the same thing for Claude Code. dev-server-claim checks that the reply doesn't tell the
# user a server is up: true of Marv, whose sandbox the server can't leave, false of Claude Code's unsandboxed one.
CLAUDE_SKIP = {"dev-server-claim"}
# opencode has no sandbox either.
SKIP = {"claude": CLAUDE_SKIP, "opencode": CLAUDE_SKIP}
RUNNERS = {"claude": "run_claude", "marv": "run_marv", "opencode": "run_opencode"}


def git(cwd, *args):
    subprocess.run(["git", "-c", "user.name=eval", "-c", "user.email=eval@localhost", "-c", "commit.gpgSign=false", *args],
                   cwd=cwd, check=True, capture_output=True)


def add_typescript(dir):
    """The compiler in the task repo's node_modules, as in a real project (excluded from git, like evals/run.ts did)."""
    mods = dir / "node_modules"
    for pkg in ["typescript", "@typescript"]:
        src = EVALS / "node_modules" / pkg
        if src.exists(): shutil.copytree(src, mods / pkg, symlinks=False)
    (mods / ".bin").mkdir(parents=True, exist_ok=True)
    (mods / ".bin/tsc").symlink_to("../typescript/bin/tsc")


def workspace(task, *extra):
    d = Path(tempfile.mkdtemp(prefix=f"marv-bench-{task}-"))
    for part in ["repo", *extra]: shutil.copytree(TASKS / task / part, d, dirs_exist_ok=True)
    git(d, "init", "-q")
    (d / ".git/info").mkdir(parents=True, exist_ok=True)
    (d / ".git/info/exclude").write_text("node_modules/\n")
    git(d, "add", "-A")
    git(d, "commit", "-q", "-m", "start")
    if (d / "tsconfig.json").exists(): add_typescript(d)
    return d


def tracker_servers(task, tracker, out_file):
    f = TASKS / task / "mcp.json"
    if not f.exists(): return None
    data = json.loads(f.read_text())["data"]
    return {"tracker": {"command": str(tracker), "args": ["tracker", str(MCP_DATA / data), str(out_file)]}}


def parse_lines(text):
    for line in text.splitlines():
        try: yield json.loads(line)
        except ValueError: continue


def run_claude(ws, prompt, servers, logdir, a):
    cmd = ["claude", "-p", prompt, "--model", a.claude_model, "--output-format", "stream-json", "--verbose",
           "--setting-sources", "project", "--strict-mcp-config", "--mcp-config", json.dumps({"mcpServers": servers or {}}),
           "--allowedTools", CLAUDE_TOOLS, "--no-session-persistence"]
    try:
        p = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=a.timeout)
    finally:
        # Claude Code leaves an empty ~/.claude/projects/<workspace>/ behind even without session persistence.
        project = Path.home() / ".claude/projects" / re.sub(r"[^A-Za-z0-9]", "-", str(ws))
        if project.is_dir(): subprocess.run(["find", str(project), "-depth", "-type", "d", "-empty", "-delete"])
    (logdir / "stream.jsonl").write_text(p.stdout)
    (logdir / "stderr.txt").write_text(p.stderr)
    r = dict(tools=Counter(), toolErrors=0, reply="", exit=p.returncode)
    messages = set()
    for e in parse_lines(p.stdout):
        if e.get("type") == "assistant":
            messages.add(e["message"].get("id"))
            for b in e["message"]["content"]:
                if b["type"] == "tool_use": r["tools"][b["name"]] += 1
        elif e.get("type") == "user":
            for b in e["message"].get("content", []):
                if isinstance(b, dict) and b.get("type") == "tool_result" and b.get("is_error"): r["toolErrors"] += 1
        elif e.get("type") == "result":
            u = e.get("usage", {})
            cached, created = u.get("cache_read_input_tokens", 0), u.get("cache_creation_input_tokens", 0)
            r.update(reply=e.get("result") or "", reason=e.get("subtype"), cost=e.get("total_cost_usd") or 0,
                     promptTokens=u.get("input_tokens", 0) + cached + created, cachedTokens=cached,
                     completionTokens=u.get("output_tokens", 0), agentMs=e.get("duration_ms"))
    r["requests"] = len(messages)
    return r


def run_marv(ws, prompt, servers, logdir, a):
    cfg = Path(tempfile.mkdtemp(prefix="marv-bench-config-"))
    (cfg / "config.json").write_text(json.dumps({"provider": "openrouter", "model": a.marv_model, "apiKey": a.key}))
    if servers: (cfg / "mcp.json").write_text(json.dumps({"mcpServers": servers}))
    env = {k: v for k, v in os.environ.items() if k not in ("OPENROUTER_API_KEY", "MARV_MODEL", "MARV_MAX_STEPS")}
    env["MARV_CONFIG_DIR"] = str(cfg)
    cmd = [a.marv_bin, "-p", prompt, "-m", a.marv_model, "--output", "stream-json", "--max-steps", "50",
           "--approve", "files", "--approve", "process", "--approve", "mcp"]
    try:
        p = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=a.timeout, env=env)
        (logdir / "stream.jsonl").write_text(p.stdout)
        (logdir / "stderr.txt").write_text(p.stderr)
        for t in (cfg / "trajectories").rglob("*.jsonl"): shutil.copy(t, logdir / "trajectory.jsonl")
    finally:
        shutil.rmtree(cfg, ignore_errors=True)
    r = dict(tools=Counter(), toolErrors=0, reply="", exit=p.returncode)
    for e in parse_lines(p.stdout):
        if e.get("type") == "tool_end":
            r["tools"][e.get("tool")] += 1
        elif e.get("type") == "result":
            u = e.get("usage") or {}
            r.update(reply=e.get("reply") or "", reason=e.get("reason"), requests=e.get("steps", 0),
                     promptTokens=u.get("promptTokens", 0), cachedTokens=u.get("cachedTokens", 0),
                     completionTokens=u.get("completionTokens", 0), cost=u.get("cost") or 0)
            if e.get("refused"): r["refused"] = e["refused"]
    traj = logdir / "trajectory.jsonl"
    if traj.exists():
        for e in parse_lines(traj.read_text()):
            if e.get("type") == "tool" and e.get("isError"): r["toolErrors"] += 1
            if e.get("type") == "agent_end" and e.get("agent") == "main": r["agentMs"] = e.get("ms")
    return r


def opencode_bin():
    """The opencode binary itself. mise's wrapper script (`mise x opencode`) reads mise's config from XDG_CONFIG_HOME,
    which each run points at a throwaway folder, so through it every run reinstalls opencode first (~50 s, and the
    parallel installs race)."""
    try:
        p = subprocess.run(["mise", "which", "opencode"], capture_output=True, text=True)
        if p.returncode == 0 and p.stdout.strip(): return p.stdout.strip()
    except FileNotFoundError:
        pass
    return "opencode"


def opencode_bin():
    """The opencode binary itself. mise's wrapper script (`mise x opencode`) reads mise's config from XDG_CONFIG_HOME,
    which each run points at a throwaway folder, so through it every run reinstalls opencode first (~50 s, and the
    parallel installs race)."""
    try:
        p = subprocess.run(["mise", "which", "opencode"], capture_output=True, text=True)
        if p.returncode == 0 and p.stdout.strip(): return p.stdout.strip()
    except FileNotFoundError:
        pass
    return "opencode"


def run_opencode(ws, prompt, servers, logdir, a):
    """opencode on the same OpenRouter model as Marv. Its config and data folders are throwaway (your own config,
    plugins, MCP servers and sessions stay out), `--pure` skips external plugins and `--auto` approves everything: it has
    no sandbox. The cost is opencode's own estimate from its price list."""
    tmp = Path(tempfile.mkdtemp(prefix="marv-bench-opencode-"))
    env = {k: v for k, v in os.environ.items() if not k.startswith("OPENCODE")}
    env.update(OPENROUTER_API_KEY=a.key, XDG_CONFIG_HOME=str(tmp / "config"), XDG_DATA_HOME=str(tmp / "data"),
               XDG_STATE_HOME=str(tmp / "state"), XDG_CACHE_HOME=str(tmp / "cache"))
    config = {"$schema": "https://opencode.ai/config.json", "autoupdate": False, "share": "disabled"}
    if servers:
        config["mcp"] = {n: {"type": "local", "command": [s["command"], *s["args"]], "enabled": True} for n, s in servers.items()}
    env["OPENCODE_CONFIG_CONTENT"] = json.dumps(config)
    cmd = [a.opencode_bin, "run", "--pure", "--auto", "--format", "json", "-m", f"openrouter/{a.marv_model}", "--dir", str(ws), prompt]
    try:
        p = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=a.timeout, env=env)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    (logdir / "stream.jsonl").write_text(p.stdout)
    (logdir / "stderr.txt").write_text(p.stderr)
    r = dict(tools=Counter(), toolErrors=0, reply="", exit=p.returncode, requests=0, promptTokens=0, cachedTokens=0,
             completionTokens=0, cost=0.0, reason="end" if p.returncode == 0 else "error")
    texts, first = [], None
    for e in parse_lines(p.stdout):
        part, kind = e.get("part") or {}, e.get("type")
        if first is None and e.get("timestamp"): first = e["timestamp"]
        if kind == "tool_use":
            r["tools"][part.get("tool")] += 1
            if (part.get("state") or {}).get("status") == "error": r["toolErrors"] += 1
        elif kind == "text":
            texts.append(part.get("text") or "")
        elif kind == "step_finish":
            t = part.get("tokens") or {}
            cache = t.get("cache") or {}
            r["requests"] += 1
            r["promptTokens"] += (t.get("input") or 0) + (cache.get("read") or 0) + (cache.get("write") or 0)
            r["cachedTokens"] += cache.get("read") or 0
            r["completionTokens"] += (t.get("output") or 0) + (t.get("reasoning") or 0)
            r["cost"] += part.get("cost") or 0
            if first and e.get("timestamp"): r["agentMs"] = e["timestamp"] - first
        elif kind == "error":
            r["reason"] = "error"
    r["reply"] = texts[-1] if texts else ""
    return r


def write_fresh(path, text):
    """Writes `text` to `path`, replacing whatever is there: a symlink the model planted under that name is removed,
    never written through (it could point at a file of yours), and the write refuses anything that appears meanwhile."""
    if path.is_symlink() or path.is_file(): path.unlink()
    elif path.exists(): shutil.rmtree(path)
    with open(path, "x") as f: f.write(text)


def sandboxed(cmd, ws):
    """`cmd` in bubblewrap: no network, a cleared environment, an empty home and /tmp, and only the workspace writable
    (the check runs code the model wrote). Bun's own folder comes back read-only when it lives under the home."""
    home = Path.home()
    out = ["bwrap", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", str(home)]
    path = []
    # Bun runs the check, node runs tsc (`#!/usr/bin/env node`) in the TypeScript tasks, git takes the diff.
    for tool in ("bun", "node", "git"):
        found = shutil.which(tool)
        if not found: continue
        exe = Path(found).resolve()
        path.append(str(exe.parent))
        if home in exe.parents:
            install = exe.parent.parent  # ~/.bun, ~/.local/share/mise/installs/node/<v>: that install only
            out += ["--ro-bind", str(install), str(install)]
    return out + ["--bind", str(ws), str(ws), "--unshare-all", "--die-with-parent", "--clearenv",
                  "--setenv", "PATH", ":".join([*path, "/usr/bin", "/bin"]), "--setenv", "HOME", str(home),
                  "--chdir", str(ws), *cmd]


def check(ws, task):
    """Copies the hidden check in (replacing whatever the model left under its names) and runs it."""
    for entry in (TASKS / task / "check").iterdir():
        dst = ws / entry.name
        if dst.is_symlink() or dst.is_file(): dst.unlink()
        elif dst.exists(): shutil.rmtree(dst)
        (shutil.copytree if entry.is_dir() else shutil.copy)(entry, dst)
    # By its real path: a link to it under the home is hidden in the sandbox.
    cmd = sandboxed([str(Path(shutil.which("bun")).resolve()), "test", "./__eval__.test.js"], ws)
    try:
        p = subprocess.run(cmd, cwd=ws, capture_output=True, text=True, timeout=120)
        return p.returncode == 0, (p.stdout + p.stderr)[-2000:]
    except subprocess.TimeoutExpired:
        return False, "the check timed out"


class Run:
    def __init__(self, a, out):
        self.a, self.out, self.lock, self.spent, self.done = a, out, threading.Lock(), 0.0, 0

    def one(self, job, total):
        harness, task, rep = job
        with self.lock:
            if self.spent >= self.a.budget: return
        logdir = self.out / "logs" / harness / f"{task}-{rep}"
        logdir.mkdir(parents=True, exist_ok=True)
        ws = workspace(task)
        tdir = Path(tempfile.mkdtemp(prefix="marv-bench-tracker-"))
        servers = tracker_servers(task, self.a.tracker, tdir / "out.json")
        prompt = (TASKS / task / "task.md").read_text().strip()
        res = dict(harness=harness, model=self.a.claude_model if harness == "claude" else self.a.marv_model,
                   task=task, rep=rep, label=self.a.label, pass_=False, tools=Counter())
        started = time.time()
        try:
            res.update(globals()[RUNNERS[harness]](ws, prompt, servers, logdir, self.a))
        except subprocess.TimeoutExpired:
            res["reason"] = "timeout"
        res["ms"] = int((time.time() - started) * 1000)
        # What the agent changed, new files too (staged), before the check runs anything. Git runs sandboxed: the
        # repository's config is the agent's to write (a core.fsmonitor or filter command would run on the host).
        git = str(Path(shutil.which("git")).resolve())
        git_in_ws = lambda *args: subprocess.run(sandboxed([git, *args], ws), cwd=ws, capture_output=True, text=True)
        staged, diff = git_in_ws("add", "-A"), git_in_ws("diff", "--cached", "HEAD")
        failed = next((p for p in (staged, diff) if p.returncode != 0), None)
        (logdir / "diff.patch").write_text(diff.stdout if not failed else f"# the diff failed: {failed.stderr.strip()}\n")
        if servers:
            recorded = tdir / "out.json"
            write_fresh(ws / COMMENTS, recorded.read_text() if recorded.exists() else "[]")
        # Only for a check that reads the reply: anywhere else it's one more file in the repo (it would still name
        # what a rename task removed, say).
        if res.get("reply") and any(REPLY_FILE in f.read_text() for f in (TASKS / task / "check").rglob("*.js")):
            write_fresh(ws / REPLY_FILE, res["reply"])
        res["pass_"], output = check(ws, task)
        if not res["pass_"]: res["checkOutput"] = output
        shutil.rmtree(ws, ignore_errors=True)
        shutil.rmtree(tdir, ignore_errors=True)
        res["tools"] = dict(res["tools"])
        res.pop("reply", None)
        with self.lock:
            self.spent += res.get("cost") or 0
            self.done += 1
            with open(self.out / "results.jsonl", "a") as f: f.write(json.dumps(res) + "\n")
            print(f"[{self.done}/{total}] ${self.spent:.3f}  {'PASS' if res['pass_'] else 'fail'}  {harness:8} {task}#{rep}  "
                  f"{res['ms'] / 1000:.0f}s · {res.get('requests', 0)} requests · {sum(res['tools'].values())} tools · {res.get('reason')}",
                  flush=True)


def report(dirs):
    rows = [json.loads(l) for d in dirs for l in (Path(d) / "results.jsonl").read_text().splitlines() if l]
    groups = defaultdict(list)
    for r in rows: groups[(r["harness"], r.get("model", ""))].append(r)
    med = lambda xs: sorted(xs)[len(xs) // 2] if xs else 0
    print(f"{'harness':8} {'model':28} {'runs':>4} {'pass':>5} {'med s':>6} {'req':>5} {'tools':>6} {'tool err':>8} "
          f"{'in tok':>8} {'cached':>7} {'out tok':>8} {'$/run':>7}")
    for (h, m), rs in sorted(groups.items()):
        n = len(rs)
        avg = lambda k: sum(r.get(k) or 0 for r in rs) / n
        prompt = sum(r.get("promptTokens") or 0 for r in rs)
        print(f"{h:8} {m:28} {n:4} {sum(r['pass_'] for r in rs) / n:5.0%} {med([r['ms'] / 1000 for r in rs]):6.1f} "
              f"{avg('requests'):5.1f} {sum(sum(r['tools'].values()) for r in rs) / n:6.1f} {sum(r.get('toolErrors', 0) for r in rs):8} "
              f"{avg('promptTokens'):8.0f} {(sum(r.get('cachedTokens') or 0 for r in rs) / prompt if prompt else 0):7.0%} "
              f"{avg('completionTokens'):8.0f} {avg('cost'):7.4f}")
    print("\nTool calls:")
    for (h, m), rs in sorted(groups.items()):
        c = Counter()
        for r in rs: c.update(r["tools"])
        print(f"  {h} {m}: " + ", ".join(f"{k} {v}" for k, v in c.most_common()))
    print("\nPer task (passes/runs, median seconds):")
    keys = sorted(groups)
    print(f"  {'':22}" + "".join(f"{(h + ' ' + m)[:24]:>26}" for h, m in keys))
    for t in sorted({r["task"] for r in rows}):
        cells = []
        for k in keys:
            rs = [r for r in groups[k] if r["task"] == t]
            cells.append(f"{sum(r['pass_'] for r in rs)}/{len(rs)} {med([r['ms'] / 1000 for r in rs]):4.0f}s" if rs else "-")
        print(f"  {t:22}" + "".join(f"{c:>26}" for c in cells))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--verify", action="store_true")
    ap.add_argument("--report", nargs="+", metavar="RUN_DIR")
    ap.add_argument("--harness", default="marv")
    ap.add_argument("--tasks")
    ap.add_argument("--repeat", type=int, default=1)
    ap.add_argument("--label", default="run")
    ap.add_argument("--budget", type=float, default=1.0, help="stop starting runs once this many dollars are spent")
    ap.add_argument("--concurrency", type=int, default=4)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--marv-model", default="anthropic/claude-haiku-5.5")
    ap.add_argument("--claude-model", default="claude-haiku-5-5")
    ap.add_argument("--marv-bin", default=str(ROOT / "target/release/marv") if (ROOT / "target/release/marv").exists() else "marv")
    ap.add_argument("--opencode-bin", default=opencode_bin())
    ap.add_argument("--tracker", default=str(ROOT / "target/debug/marv-oracle"))
    a = ap.parse_args()
    if a.report: return report(a.report)

    for tool in ("bun", "bwrap", "git"):
        if not shutil.which(tool): sys.exit(f"bench.py needs {tool} (the checks run model-written code, in bubblewrap only).")
    if not (EVALS / "node_modules/typescript").exists():
        subprocess.run(["bun", "install", "--frozen-lockfile"], cwd=EVALS, check=True)
    all_tasks = sorted(p.name for p in TASKS.iterdir() if (p / "task.md").exists())
    tasks = a.tasks.split(",") if a.tasks else all_tasks
    for t in tasks:
        if t not in all_tasks: sys.exit(f'No task "{t}" (have: {", ".join(all_tasks)})')

    if a.verify:
        ok = True
        for t in tasks:
            # A solution brings its own reply or tracker comments where its check reads them.
            before, after = workspace(t), workspace(t, "solution")
            (b, _), (s, output) = check(before, t), check(after, t)
            shutil.rmtree(before, ignore_errors=True)
            shutil.rmtree(after, ignore_errors=True)
            good = not b and s
            ok &= good
            note = (" (passes before any change)" if b else "") + ("" if s else " (solution fails)\n" + output)
            print(f"{'ok ' if good else 'BAD'} {t}{note}")
        sys.exit(0 if ok else 1)

    harnesses = a.harness.split(",")
    for h in harnesses:
        if h not in RUNNERS: sys.exit(f'No harness "{h}" (have: {", ".join(RUNNERS)})')
    if "marv" in harnesses or "opencode" in harnesses:
        a.key = os.environ.get("OPENROUTER_API_KEY") or json.loads((Path.home() / ".marv/config.json").read_text()).get("apiKey")
        if not a.key: sys.exit("No OpenRouter key: set OPENROUTER_API_KEY or run marv's /setup.")
    if any((TASKS / t / "mcp.json").exists() for t in tasks) and not Path(a.tracker).exists():
        sys.exit(f"No tracker at {a.tracker}: run `cargo build -p marv-oracle` (or pass --tracker).")
    jobs = [(h, t, rep) for rep in range(a.repeat) for t in tasks for h in harnesses if t not in SKIP.get(h, ())]
    out = EVALS / "results" / f"{time.strftime('%Y-%m-%dT%H-%M-%S')}-{a.label}"
    out.mkdir(parents=True)
    print(f"{len(jobs)} runs ({', '.join(harnesses)} × {len(tasks)} tasks × {a.repeat}), budget ${a.budget} → {out}", flush=True)
    run = Run(a, out)
    with ThreadPoolExecutor(a.concurrency) as pool:
        for f in [pool.submit(run.one, j, len(jobs)) for j in jobs]: f.result()
    if run.done < len(jobs): print(f"\nBudget reached: {len(jobs) - run.done} runs not started.")
    print()
    report([out])
    print(f"\nSpent ${run.spent:.3f}. Results: {out}")


if __name__ == "__main__":
    main()
