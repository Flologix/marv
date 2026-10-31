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

import json
import os
import shlex
from pathlib import Path
from typing import override

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

REPO = Path(__file__).resolve().parents[2]
REMOTE_BIN = "/installed-agent/marv"
REMOTE_CONFIG_DIR = "/installed-agent/marv-config"
LOGS = "/logs/agent"


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
        local = Path(os.environ.get("MARV_BIN") or REPO / "target/x86_64-unknown-linux-musl/release/marv")
        if not local.is_file():
            raise RuntimeError(f"No Marv build at {local}: build the static binary (see this file's header) or set MARV_BIN")
        await environment.upload_file(local, REMOTE_BIN)
        owner = shlex.quote(str(environment.default_user)) if environment.default_user is not None else None
        chown = f" && chown -R {owner} {REMOTE_CONFIG_DIR}" if owner else ""
        await self.exec_as_root(
            environment,
            command=f"chmod 755 {REMOTE_BIN} && mkdir -p {REMOTE_CONFIG_DIR} && chmod 700 {REMOTE_CONFIG_DIR}{chown}",
        )
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
        approve = " ".join(f"--approve {s}" for s in ("files", "process", "network", "git_write", "web"))
        command = (
            f"mkdir -p {LOGS}; "
            f"MARV_CONFIG_DIR={REMOTE_CONFIG_DIR} {REMOTE_BIN} -p {shlex.quote(instruction)} "
            f"--output stream-json --max-steps {steps} --trust-project --dangerously-run-unsandboxed --approve-tainted {approve} "
            f"2> {LOGS}/stderr.txt | tee {LOGS}/stream.jsonl; status=${{PIPESTATUS[0]}}; "
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
        for line in reversed(stream.read_text(errors="replace").splitlines()):
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if event.get("type") != "result":
                continue
            usage = event.get("usage") or {}
            context.n_input_tokens = usage.get("promptTokens")
            context.n_cache_tokens = usage.get("cachedTokens")
            context.n_output_tokens = usage.get("completionTokens")
            context.cost_usd = usage.get("cost")
            return
