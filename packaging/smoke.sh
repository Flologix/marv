#!/bin/sh
# The release smoke test (#167): a packed release asset installed the two ways a user installs it, and a sandboxed
# turn run with each. It stands in for trying a release candidate by hand on a Mac (the release workflow runs it on
# GitHub's macOS runners, and on Linux; CI runs it on a debug build). It never touches the network or a real model.
#
#   packaging/smoke.sh <target> <dir with marv-<target>.tar.gz and its .sha256>
#
# 1. install.sh, pointed at the asset over file:// (the target detected from uname, the SHA-256 checked), into a temp
#    prefix; `marv --version`.
# 2. `marv -p` against packaging/stub_provider.py, which makes one `bash` call: it writes a file in the project and
#    tries to write one outside it. The run must exit 0, the project's file must be there, and the outside one must
#    not (on macOS Seatbelt refuses the write; bwrap's /tmp is its own). A warning that commands run unsandboxed
#    fails it.
# 3. The npm packages (packaging/npm/build.sh): the platform package and the launcher, `npm install -g` from the .tgz
#    files into a temp prefix; `marv --version` and the same turn through the launcher. Skipped without npm.
set -eu

[ $# -eq 2 ] || {
	echo "usage: $0 <target> <assets dir>" >&2
	exit 1
}
target=$1
assets=$(cd "$2" && pwd)
root=$(cd "$(dirname "$0")/.." && pwd)
asset="marv-$target.tar.gz"
[ -f "$assets/$asset" ] || {
	echo "smoke: $assets/$asset is missing" >&2
	exit 1
}

# The real path: macOS's temp folder is behind the /var symlink, and Seatbelt's rules are on real paths.
work=$(cd "$(mktemp -d 2>/dev/null || mktemp -d -t marv-smoke)" && pwd -P)
stub=
cleanup() {
	[ -z "$stub" ] || kill "$stub" 2>/dev/null || true
	rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

fail() {
	echo "smoke: FAIL: $*" >&2
	exit 1
}

# --- 1. install.sh from the asset ---
rel="$work/release/latest/download"
mkdir -p "$rel"
cp "$assets/$asset" "$assets/$asset.sha256" "$rel/"
cat "$rel/$asset.sha256" >"$rel/SHA256SUMS"
env -u MARV_TARGET MARV_RELEASE_URL="file://$work/release" sh "$root/install.sh" --prefix "$work/prefix" ||
	fail "install.sh didn't install $asset"
version=$("$work/prefix/bin/marv" --version) || fail "marv --version failed"
case $version in marv\ *) ;; *) fail "marv --version said '$version'" ;; esac
echo "smoke: install.sh installed $version"

# --- 2. a sandboxed turn ---
outside="$work/outside"
mkdir -p "$outside"
port_file="$work/port"
command="echo made > made-here.txt; if (echo x > '$outside/escaped') 2>/dev/null; then echo outside-write-allowed; else echo outside-write-refused; fi"
echo "smoke: $(command -v python3): $(python3 -c 'import sys; print(sys.version.split()[0])' 2>&1)"
stub_log="$work/stub.log"
python3 "$root/packaging/stub_provider.py" "$port_file" "$command" >"$stub_log" 2>&1 &
stub=$!
stub_failed() {
	echo "smoke: the stub provider's output:" >&2
	sed 's/^/  | /' "$stub_log" >&2
	fail "$1"
}
# Up to 30 s (a cold python3 on a fresh runner can be slow), but not a moment longer than the stub lives.
i=0
while [ ! -s "$port_file" ]; do
	kill -0 "$stub" 2>/dev/null || stub_failed "the stub provider exited before it was listening"
	i=$((i + 1))
	[ $i -lt 300 ] || stub_failed "the stub provider didn't start in 30 s"
	sleep 0.1
done
port=$(cat "$port_file")

turn() { # turn <name> <marv command>
	t="$work/turn-$1"
	mkdir -p "$t/project" "$t/home" "$t/config" "$t/run"
	chmod 700 "$t/run"
	cat >"$t/config/config.json" <<EOF
{"provider": "openrouter", "baseUrl": "http://127.0.0.1:$port/v1", "apiKey": "smoke-dummy-key", "model": "smoke/stub",
 "trajectories": false, "diagnostics": false, "checks": false}
EOF
	set +e
	(cd "$t/project" && env -i PATH="$PATH" HOME="$t/home" MARV_CONFIG_DIR="$t/config" XDG_RUNTIME_DIR="$t/run" \
		MARV_GITHUB=off TERM=dumb "$2" -p "run the smoke command" </dev/null >"$t/stdout" 2>"$t/stderr")
	code=$?
	set -e
	echo "smoke: [$1] marv -p exited $code; it printed:"
	sed 's/^/  | /' "$t/stdout"
	[ ! -s "$t/stderr" ] || {
		echo "smoke: [$1] stderr:"
		sed 's/^/  | /' "$t/stderr"
	}
	[ "$code" -eq 0 ] || fail "[$1] marv -p exited $code"
	! grep -qi unsandboxed "$t/stderr" "$t/stdout" || fail "[$1] the command ran unsandboxed"
	[ -f "$t/project/made-here.txt" ] || fail "[$1] the bash command didn't run in the project"
	[ ! -e "$outside/escaped" ] || fail "[$1] the sandbox let the command write outside the project"
	if [ "$(uname -s)" = Darwin ]; then
		grep -q outside-write-refused "$t/stdout" || fail "[$1] Seatbelt didn't refuse the write outside the project"
	fi
	echo "smoke: [$1] the turn ran sandboxed"
}
turn install.sh "$work/prefix/bin/marv"

# --- 3. npm ---
if ! command -v npm >/dev/null 2>&1; then
	echo "smoke: no npm; the npm packages weren't tried"
	exit 0
fi
npm_assets="$work/npm-assets"
mkdir -p "$npm_assets"
cp "$assets/$asset" "$npm_assets/"
"$root/packaging/npm/build.sh" 0.0.0-smoke "$npm_assets" "$work/npm" >/dev/null 2>&1 || fail "npm/build.sh failed"
platform=$(ls "$work/npm" | grep -v '^flologixai-marv-0.0.0-smoke.tgz$')
[ "$(printf '%s\n' "$platform" | wc -l | tr -d ' ')" = 1 ] || fail "expected one platform package, got: $platform"
# The platform package first, by path, so the launcher's optional dependency is already there (0.0.0-smoke isn't on
# the registry).
npm install -g --prefix "$work/npm-prefix" --no-audit --no-fund --loglevel=error \
	"$work/npm/$platform" "$work/npm/flologixai-marv-0.0.0-smoke.tgz" || fail "npm install -g failed"
npm_version=$("$work/npm-prefix/bin/marv" --version) || fail "the npm launcher's marv --version failed"
[ "$npm_version" = "$version" ] || fail "the npm launcher said '$npm_version', install.sh's marv '$version'"
echo "smoke: npm installed $platform and the launcher: $npm_version"
turn npm "$work/npm-prefix/bin/marv"
echo "smoke: ok"
