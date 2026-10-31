#!/usr/bin/env bash
# Terminal-Bench 2.0 (89 tasks, no GPUs) through Harbor, for Marv and the agents Harbor ships (codex, opencode,
# claude-code), on one model. Needs Docker and Harbor (`uv tool install harbor`).
#
#   evals/terminal_bench/run.sh marv     deepseek/deepseek-v4.1-flash [harbor args…]
#   evals/terminal_bench/run.sh opencode openrouter/deepseek/deepseek-v4.1-flash [harbor args…]
#   evals/terminal_bench/run.sh marv     z-ai/glm-5.3 -i 'terminal-bench/sqlite-*' -l 5    # a few tasks first
#
# Marv runs the static build at target/x86_64-unknown-linux-musl/release/marv (or MARV_BIN); see marv_agent.py.
# Jobs go to evals/results/terminal-bench/ (`harbor view jobs -o evals/results/terminal-bench` to browse them).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
agent="${1:?agent: marv, opencode, codex or claude-code}"
model="${2:?model, e.g. deepseek/deepseek-v4.1-flash}"
shift 2
if [ "$agent" = marv ]; then
    agent="evals.terminal_bench.marv_agent:MarvAgent"
    export PYTHONPATH="$repo${PYTHONPATH:+:$PYTHONPATH}"
fi
exec harbor run -d terminal-bench/terminal-bench-2 -e docker -a "$agent" -m "$model" \
    -o "$repo/evals/results/terminal-bench" -n "${TB_CONCURRENCY:-4}" "$@"
