#!/bin/sh
# A stand-in for the GitHub CLI (#47): tests put it on PATH as `gh` (marv_tools::github::fake), so the github tool
# runs it exactly as it would run gh, and nothing reaches GitHub. It logs every argv, its folder and a few
# environment variables to $FAKE_GH_LOG, answers `auth status` per $FAKE_GH_LOGGED_IN ("1": logged in; $FAKE_GH_HANG:
# never answers), `repo view --json nameWithOwner` with o/r ($FAKE_GH_NO_REPO=1: fails), and anything else with
# $FAKE_GH_OUTPUT (default: its argv), $FAKE_GH_STDERR on stderr, and exit code $FAKE_GH_EXIT.
# Ports Marv's tests/fixtures/fake-gh.ts (a shell script here: no runtime to start).
# The github tool runs reads in parallel: a record is built first and appended under a lock (mkdir is atomic, and
# flock isn't on macOS), or two calls' lines could interleave. A lock left by a killed call is waited out after ~5 s.
if [ -n "$FAKE_GH_LOG" ]; then
  record=$(
    printf 'call\n'
    printf 'cwd %s\n' "$(pwd -P)"
    for name in GH_HOST GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN OPENROUTER_API_KEY MARV_SECRET GH_PROMPT_DISABLED GH_PAGER NO_COLOR GH_DEBUG GIT_DIR GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GIT_CONFIG_KEY_1 GIT_CONFIG_VALUE_1; do
      if eval "[ -n \"\${$name+x}\" ]"; then eval "printf 'env %s=%s\n' \"$name\" \"\$$name\""; fi
    done
    for a in "$@"; do printf 'arg %s\n' "$a"; done
    printf 'end'
  )
  tries=0
  while ! mkdir "$FAKE_GH_LOG.lock" 2>/dev/null && [ "$tries" -lt 500 ]; do
    tries=$((tries + 1))
    sleep 0.01
  done
  printf '%s\n' "$record" >> "$FAKE_GH_LOG"
  rmdir "$FAKE_GH_LOG.lock" 2>/dev/null
fi
if [ "$1" = auth ] && [ "$2" = status ]; then
  if [ -n "$FAKE_GH_HANG" ]; then exec sleep 30; fi
  if [ "$FAKE_GH_LOGGED_IN" = 1 ]; then exit 0; fi
  exit 1
fi
if [ "$1" = repo ] && [ "$2" = view ]; then
  for a in "$@"; do
    if [ "$a" = nameWithOwner ]; then
      if [ "$FAKE_GH_NO_REPO" = 1 ]; then exit 1; fi
      echo o/r
      exit 0
    fi
  done
fi
printf '%s\n' "${FAKE_GH_OUTPUT-fake gh $*}"
if [ -n "$FAKE_GH_STDERR" ]; then printf '%s\n' "$FAKE_GH_STDERR" >&2; fi
exit "${FAKE_GH_EXIT:-0}"
