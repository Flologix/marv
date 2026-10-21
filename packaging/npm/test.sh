#!/bin/sh
# Checks the npm packages without a registry: builds them from a marv binary for this machine (Linux or macOS, x64 or
# arm64), lays them out as npm would (node_modules/@flologixai/{marv,marv-<os>-<cpu>}), and runs the launcher:
# --version, an exit code passed through, and the message when the platform package is missing. The same binary is
# packed under the other targets' names too, to check the launcher names all four platform packages.
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

fail() {
	echo "npm test: $*" >&2
	exit 1
}
host=$(node -p 'process.platform + "-" + process.arch')
case $host in
linux-x64 | linux-arm64 | darwin-x64 | darwin-arm64) ;;
*) fail "no npm package for $host" ;;
esac
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin; do
	"$root/packaging/package.sh" "$target" "$binary" "$work/assets" >/dev/null
done
"$root/packaging/npm/build.sh" 0.0.0-test "$work/assets" "$work/npm" >/dev/null
modules="$work/node_modules/@flologixai"
mkdir -p "$modules/marv" "$modules/marv-$host"
tar -xzf "$work/npm/flologixai-marv-0.0.0-test.tgz" -C "$modules/marv" --strip-components=1
tar -xzf "$work/npm/flologixai-marv-$host-0.0.0-test.tgz" -C "$modules/marv-$host" --strip-components=1

for pkg in linux-x64 linux-arm64 darwin-x64 darwin-arm64; do
	grep -q "\"@flologixai/marv-$pkg\": \"0.0.0-test\"" "$modules/marv/package.json" ||
		fail "the launcher doesn't name @flologixai/marv-$pkg"
done
os=${host%-*}
grep -q "\"os\": \[\"$os\"\]" "$modules/marv-$host/package.json" || fail "marv-$host isn't limited to $os"
version=$(node "$modules/marv/bin/marv.js" --version) || fail "--version failed"
[ "$version" = "$("$binary" --version)" ] || fail "--version said '$version'"
set +e
node "$modules/marv/bin/marv.js" --no-such-flag >/dev/null 2>&1
code=$?
set -e
[ "$code" -eq 1 ] || fail "a bad flag exited $code, not marv's 1"
rm -rf "$modules/marv-$host"
set +e
message=$(node "$modules/marv/bin/marv.js" --version 2>&1)
set -e
echo "$message" | grep -q "isn't installed" || fail "no message without the platform package: $message"
node --test "$root/packaging/npm/launcher.test.js" >/dev/null || fail "the launcher's unit tests failed"
echo "npm packages: ok"
