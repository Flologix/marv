#!/usr/bin/env bash
# Pulls task images before a run, retrying each until it lands: over a flaky connection a large image (2.5 GB for
# mteb-retrieve) otherwise fails Harbor's environment start ("connection reset by peer") and the task is lost before
# the agent runs. Docker resumes the layers it already has.
#   evals/terminal_bench/prepull.sh alexgshaw/regex-log:20251031 …
set -u
for image in "$@"; do
    for attempt in $(seq 1 20); do
        if docker pull -q "$image" >/dev/null 2>&1; then echo "ok    $image"; continue 2; fi
        echo "retry $image ($attempt)"; sleep 15
    done
    echo "FAILED $image"
done
