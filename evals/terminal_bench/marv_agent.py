"""Marv as a Harbor agent, for Terminal-Bench 2.0 (https://www.tbench.ai).

    harbor run -d terminal-bench/terminal-bench-2 -e docker \\
        --agent-import-path evals.terminal_bench.marv_agent:MarvAgent ...   (see run.sh: it sets PYTHONPATH)

Each trial's container gets the static build under test (MARV_BIN, default target/x86_64-unknown-linux-musl/release/marv:
`cargo zigbuild --release -p marv-cli --target x86_64-unknown-linux-musl`, as the release does), a private config with the
OpenRouter key, and runs `marv -p` in the task's working folder with `--dangerously-run-unsandboxed`: the container is the
sandbox, and bubblewrap can't run inside it. Files, processes and the network are approved too, as a benchmark run has no
one to ask, and `--approve-tainted`: an unsandboxed command's output is untrusted (it had the network), so after the first
one every command counts as egress from a tainted session, and without it the run stalls (0/5 on the first smoke test). The stream (`stream.jsonl`), Marv's trajectory (`trajectory.jsonl`) and stderr land in the trial's agent log
folder; the config (it holds the key) never does.

Environment: OPENROUTER_API_KEY (else ~/.marv/config.json's apiKey), MARV_BIN, MARV_MAX_STEPS (default 0: none, the task's timeout governs),
MARV_CONFIG_JSON (extra config keys as a JSON object, e.g. '{"editTools": "patch"}' for an A/B).

Model: `-m deepseek/deepseek-v4.1-flash` (an OpenRouter id; a leading `openrouter/` is dropped).
"""

import functools
import json
import os
import shlex
import subprocess
import tomllib
from pathlib import Path
from typing import override

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

REPO = Path(__file__).resolve().parents[2]
REMOTE_BIN = "/installed-agent/marv"
REMOTE_CONFIG_DIR = "/installed-agent/marv-config"
REMOTE_INSTRUCTION = "/installed-agent/instruction.txt"
LOGS = "/logs/agent"
BLOCKED_HOSTS = (
    "tbench.ai", "www.tbench.ai", "harborframework.com", "hub.harborframework.com", "www.harborframework.com",
    "duckduckgo.com", "html.duckduckgo.com", "lite.duckduckgo.com", "api.duckduckgo.com", "www.google.com", "google.com",
    "www.bing.com", "bing.com", "search.brave.com", "search.yahoo.com", "www.startpage.com", "searx.be", "kagi.com",
)
# Sent with -c after a dropped connection: the conversation so far is intact, only the last request failed.
CONTINUE_PROMPT = "The connection to the model dropped and has been restored. Continue the task from where you left off."


def _api_key() -> str:
    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        try:
            key = json.loads((Path.home() / ".marv/config.json").read_text()).get("apiKey")
        except (OSError, ValueError):
            key = None
    if not key:
        raise RuntimeError("No OpenRouter key: set OPENROUTER_API_KEY or run marv's /setup.")
    return key


def local_binary() -> Path:
    """The Marv build under test, on this machine (uploaded into each task's container)."""
    return Path(os.environ.get("MARV_BIN") or REPO / "target/x86_64-unknown-linux-musl/release/marv")


@functools.cache
def supports(binary: str, flag: str) -> bool:
    """Whether this Marv build takes `flag`, from its --help (it's static, so it runs here too). An older build, kept to
    re-run a baseline, doesn't know the newer flags: passing one fails every run with 'unexpected argument' (all 7
    baseline re-runs on 2026-10-31)."""
    try:
        out = subprocess.run([binary, "--help"], capture_output=True, text=True, timeout=30)
        return flag in out.stdout
    except (OSError, subprocess.SubprocessError):
        return False


def task_time_limit(logs_dir: Path) -> int | None:
    """The task's agent timeout in seconds, for `marv -p --time-limit`: Harbor passes it only to its oracle agent, so
    it's read from the task's `task.toml` in Harbor's cache, found by the name and hash in the trial's `config.json`
    (the trial folder holds the agent's log folder). None when it can't be found (then Marv runs without a limit)."""
    try:
        task = json.loads((logs_dir.parent / "config.json").read_text())["task"]
        digest = task["ref"].removeprefix("sha256:")
        toml = Path.home() / ".cache/harbor/tasks/packages" / task["name"] / digest / "task.toml"
        timeout = tomllib.loads(toml.read_text())["agent"]["timeout_sec"]
        return int(timeout)
    except (OSError, KeyError, ValueError, TypeError):
        return None


class MarvAgent(BaseInstalledAgent):
    """Marv (https://github.com/Flologix/marv), the build under test, run headless in the task container."""

    @staticmethod
    @override
    def name() -> str:
        return "marv"

    @override
    def get_version_command(self) -> str | None:
        return f"{REMOTE_BIN} --version"

    @override
    def parse_version(self, stdout: str) -> str:
        return stdout.strip().removeprefix("marv").strip()

    def _model(self) -> str:
        if not self.model_name:
            raise ValueError("Give a model with -m, an OpenRouter id such as deepseek/deepseek-v4.1-flash")
        return self.model_name.removeprefix("openrouter/")

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        local = local_binary()
        if not local.is_file():
            raise RuntimeError(f"No Marv build at {local}: build the static binary (see this file's header) or set MARV_BIN")
        await environment.upload_file(local, REMOTE_BIN)
        owner = shlex.quote(str(environment.default_user)) if environment.default_user is not None else None
        chown = f" && chown -R {owner} {REMOTE_CONFIG_DIR}" if owner else ""
        await self.exec_as_root(
            environment,
            command=f"chmod 755 {REMOTE_BIN} && mkdir -p {REMOTE_CONFIG_DIR} && chmod 700 {REMOTE_CONFIG_DIR}{chown}",
        )
        # The benchmark's own sites and the search engines don't resolve in the container: no looking up the answers.
        # Hosts only (GitHub stays, for packages); leaks.py flags whatever still reaches the benchmark's sources.
        hosts = "".join(f"0.0.0.0 {h}\n:: {h}\n" for h in BLOCKED_HOSTS)
        await self.exec_as_root(environment, command=f"printf {shlex.quote(hosts)} >> /etc/hosts")
        config = {
            "provider": "openrouter",
            "model": self._model(),
            "apiKey": _api_key(),
            "sandbox": False,
            # Nothing of this machine's: no checks that need the sandbox, no checkpoints to a store nobody reads.
            "diagnostics": False,
            "checks": False,
            **json.loads(os.environ.get("MARV_CONFIG_JSON") or "{}"),
        }
        await self._upload_config_text(
            environment, content=json.dumps(config), remote_path=f"{REMOTE_CONFIG_DIR}/config.json", filename="config.json"
        )
        await self.exec_as_root(environment, command=f"chmod 600 {REMOTE_CONFIG_DIR}/config.json{chown}")

    @override
    @with_prompt_template
    async def run(self, instruction: str, environment: BaseEnvironment, context: AgentContext) -> None:
        # 0: no step limit, the task's own timeout (15 to 60 minutes) ends a run, as for the other agents. At 100,
        # 2 of 5 smoke-test tasks stopped mid-work at the limit (2026-10-31).
        steps = int(os.environ.get("MARV_MAX_STEPS") or 0)
        # No `web` scope, so no web_fetch: with it, Marv searched for the benchmark and fetched tasks' solutions and
        # tests (5 passes of the 2026-11-01 TB 2.1 run). The network stays for packages, as for the other agents.
        approve = " ".join(f"--approve {s}" for s in ("files", "process", "network", "git_write"))
        # The task's own time limit, less a little for Marv's start and the logs (MARV_TIME_LIMIT overrides it).
        limit = int(os.environ.get("MARV_TIME_LIMIT") or 0) or (task_time_limit(self.logs_dir) or 0)
        budget = max(limit - 20, 60) if limit and supports(str(local_binary()), "--time-limit") else 0
        # The limit is shell code (`$left`): a continuation gets the time left of the budget, not the whole of it. With
        # the whole, a run that reconnected thought it had time the task no longer gave it (install-windows was killed
        # with 47 minutes left on Marv's clock, 2026-11-05).
        time_limit = '--time-limit "$left" ' if budget else ""
        # The task goes in on stdin (`-p -`), never in argv: a process's argv is what `pkill -f` matches, and the task
        # names its own scripts. With it in argv, `pkill -f pystan_analysis.py` killed Marv itself (and the bash -c
        # around it): rstan-to-pystan, exit 143, 2026-11-06.
        await self._upload_config_text(
            environment, content=instruction, remote_path=REMOTE_INSTRUCTION, filename="instruction.txt"
        )
        await self.exec_as_root(environment, command=f"chmod 644 {REMOTE_INSTRUCTION}")
        marv = (
            f"MARV_CONFIG_DIR={REMOTE_CONFIG_DIR} {REMOTE_BIN} --output stream-json --max-steps {steps} {time_limit}"
            f"--trust-project --dangerously-run-unsandboxed --approve-tainted {approve}"
        )
        command = (
            f"mkdir -p {LOGS}; start=$(date +%s); left={budget}; "
            # Each run's stream goes to last.jsonl (judged alone) and onto stream.jsonl (all of them) as it comes, so a
            # run the task's timeout kills is in both (copying after the run lost the timed-out runs' streams).
            f"{marv} -p - < {REMOTE_INSTRUCTION} 2>> {LOGS}/stderr.txt | tee {LOGS}/last.jsonl >> {LOGS}/stream.jsonl; "
            "status=${PIPESTATUS[0]}; "
            # A dropped connection (DNS, Wi-Fi) ends a run after the provider's own retries (about 5 s). Wait and continue
            # the same conversation (-c: its history intact), at most 5 times; only for that error, never for the
            # model's own ending.
            # Only a run Marv ended on that error counts: exit 1, last line a result with reason "error", and an error
            # event (its own line, keys sorted so it starts with "message") saying the provider can't be reached. Never
            # a substring anywhere: `ps aux` output in a tool result carried this command line, so 5 normal endings
            # were continued (2026-11-06). The pattern is split so this command line never holds it whole.
            "for retry in 1 2 3 4 5; do "
            "  [ \"$status\" = 1 ] || break; "
            f"  case \"$(tail -n 1 {LOGS}/last.jsonl)\" in '{{\"reason\":\"error\",'*'\"type\":\"result\"'*) ;; *) break ;; esac; "
            "  dropped='^{\"message\":\"Can.t'; "
            f"  grep -q \"$dropped re\"ach {LOGS}/last.jsonl || break; "
            f"  echo \"marv-agent: the connection dropped; continuing ($retry/5)\" >> {LOGS}/stderr.txt; sleep 30; "
            # Less than a minute left isn't worth a continuation: the task's own timeout is about to end it.
            + (f"  left=$(( {budget} - $(date +%s) + start )); [ \"$left\" -ge 60 ] || break; " if budget else "")
            + f"  {marv} -c -p {shlex.quote(CONTINUE_PROMPT)} 2>> {LOGS}/stderr.txt | tee {LOGS}/last.jsonl >> {LOGS}/stream.jsonl; "
            "  status=${PIPESTATUS[0]}; "
            "done; "
            # Marv's trajectory, for failures.py and the trajectory stats; never the config folder itself (the key).
            f"cat {REMOTE_CONFIG_DIR}/trajectories/*/*.jsonl {REMOTE_CONFIG_DIR}/trajectories/*.jsonl > {LOGS}/trajectory.jsonl 2>/dev/null; "
            # Exit 2 (step limit, loop) and 3 (an approval it couldn't get) are results, not crashes: the verifier
            # decides. Anything else is passed on.
            'case "$status" in 0|2|3) exit 0 ;; *) exit "$status" ;; esac'
        )
        await self.exec_as_agent(environment, command=f"bash -c {shlex.quote(command)}")

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        stream = self.logs_dir / "stream.jsonl"
        if not stream.exists():
            return
        # One result per marv run: the first, and one per continuation after a dropped connection. Summed.
        totals = {"promptTokens": 0, "cachedTokens": 0, "completionTokens": 0, "cost": 0.0}
        found = False
        for line in stream.read_text(errors="replace").splitlines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") != "result":
                continue
            found = True
            for key in totals:
                totals[key] += (event.get("usage") or {}).get(key) or 0
        if found:
            context.n_input_tokens = totals["promptTokens"]
            context.n_cache_tokens = totals["cachedTokens"]
            context.n_output_tokens = totals["completionTokens"]
            context.cost_usd = totals["cost"]
