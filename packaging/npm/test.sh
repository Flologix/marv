#!/bin/sh
# Checks the npm packages without a registry: builds them from a marv binary, lays them out as npm would
# (node_modules/@flologixai/{marv,marv-linux-x64}), and runs the launcher: --version, an exit code passed through,
# and the message when the platform package is missing.
#
#   packaging/npm/test.sh <marv binary>
set -eu

[ $# -eq 1 ] || {
	echo "usage: $0 <marv binary>" >&2
	exit 1
}
root=$(cd "$(dirname "$0")/../.." && pwd)
binary=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

"$root/packaging/package.sh" x86_64-unknown-linux-musl "$binary" "$work/assets" >/dev/null
"$root/packaging/npm/build.sh" 0.0.0-test "$work/assets" "$work/npm" >/dev/null
modules="$work/node_modules/@flologixai"
mkdir -p "$modules/marv" "$modules/marv-linux-x64"
tar -xzf "$work/npm/flologixai-marv-0.0.0-test.tgz" -C "$modules/marv" --strip-components=1
tar -xzf "$work/npm/flologixai-marv-linux-x64-0.0.0-test.tgz" -C "$modules/marv-linux-x64" --strip-components=1

fail() {
	echo "npm test: $*" >&2
	exit 1
}
grep -q '"@flologixai/marv-linux-x64": "0.0.0-test"' "$modules/marv/package.json" || fail "the launcher doesn't name its platform package"
version=$(node "$modules/marv/bin/marv.js" --version) || fail "--version failed"
[ "$version" = "$("$binary" --version)" ] || fail "--version said '$version'"
set +e
node "$modules/marv/bin/marv.js" --no-such-flag >/dev/null 2>&1
code=$?
set -e
[ "$code" -eq 1 ] || fail "a bad flag exited $code, not marv's 1"
rm -rf "$modules/marv-linux-x64"
set +e
message=$(node "$modules/marv/bin/marv.js" --version 2>&1)
set -e
echo "$message" | grep -q "isn't installed" || fail "no message without the platform package: $message"
echo "npm packages: ok"
