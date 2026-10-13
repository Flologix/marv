#!/usr/bin/env bash
# Tests for install.sh and packaging/package.sh (issue #19). Everything happens under a temp dir: a --prefix there, a
# fake TS Marv launcher (a bash script, as the real one is), and a fake release served over file://. The real
# ~/.local/bin is never touched.
#
#   packaging/test-install.sh                       # with a fake marv-rs binary (an ELF header and a crate name)
#   MARV_TEST_BINARY=target/release/marv packaging/test-install.sh   # with a real build; also runs it
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
install="$root/install.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

passed=0
failed=0
current=
fail() {
	echo "  FAIL ($current): $*" >&2
	failed=$((failed + 1))
}
check() { # check <description> <command...>
	if "${@:2}"; then :; else fail "$1"; fi
}
begin() {
	current=$1
	echo "- $1"
	prefix="$work/$1"
	bin="$prefix/bin"
	mkdir -p "$prefix"
}
end() { passed=$((passed + 1)); }

# A marv-rs binary: the real one if given, else the smallest file install.sh's check accepts.
if [[ -n ${MARV_TEST_BINARY:-} ]]; then
	rs="$MARV_TEST_BINARY"
	real=1
else
	rs="$work/marv-rs"
	printf '\177ELF\002\001\001\000\377\376fake build: \200crates/marv-session/src/lib.rs\n' >"$rs"
	chmod 755 "$rs"
	real=0
fi
rs_sum=$(sha256sum "$rs" | cut -d' ' -f1)

ts_launcher() { # a TS Marv launcher at $1
	mkdir -p "$(dirname "$1")"
	cat >"$1" <<'EOF'
#!/usr/bin/env bash
echo "ts marv $*"
EOF
	chmod 755 "$1"
}

lacks() { ! grep -q "$1" <<<"$2"; }
same_as_rs() { [[ $(sha256sum "$1" | cut -d' ' -f1) == "$rs_sum" ]]; }
runs_ts() { [[ $("$1" hi) == "ts marv hi" ]]; }

# A release in the layout GitHub serves: <base>/latest/download/<asset> and <base>/download/<tag>/<asset>.
release="$work/release"
host_target=x86_64-unknown-linux-musl
for dir in "$release/latest/download" "$release/download/v0.0.1"; do
	"$root/packaging/package.sh" "$host_target" "$rs" "$dir" >/dev/null
	cat "$dir"/*.sha256 >"$dir/SHA256SUMS"
done
export MARV_RELEASE_URL="file://$release" MARV_TARGET="$host_target"

# --- package.sh ---
begin package
check "the asset holds marv at its root" bash -c "tar -tzf '$release/latest/download/marv-$host_target.tar.gz' | grep -qx marv"
check "the .sha256 is sha256sum's format" bash -c "cd '$release/latest/download' && sha256sum -c --quiet marv-$host_target.tar.gz.sha256"
"$root/packaging/package.sh" "$host_target" "$rs" "$work/again" >/dev/null
check "the same binary packs to the same bytes" cmp -s "$work/again/marv-$host_target.tar.gz" "$release/latest/download/marv-$host_target.tar.gz"
end

# --- install into an empty prefix ---
begin fresh
out=$("$install" --prefix "$prefix" 2>&1)
check "marv installed" same_as_rs "$bin/marv"
check "marv is executable" test -x "$bin/marv"
check "no marv-ts made" test ! -e "$bin/marv-ts"
check "says what it installed" grep -q "installed marv-$host_target.tar.gz (latest) as $bin/marv" <<<"$out"
check "no staging file left" bash -c "! ls -A '$bin' | grep -q '^\.marv'"
end

# --- the command switch: TS Marv's launcher moves to marv-ts ---
begin switch
ts_launcher "$bin/marv"
ts_sum=$(sha256sum "$bin/marv" | cut -d' ' -f1)
out=$("$install" --prefix "$prefix" 2>&1)
check "marv is marv-rs" same_as_rs "$bin/marv"
check "marv-ts is the old launcher" bash -c "[[ \$(sha256sum '$bin/marv-ts' | cut -d' ' -f1) == $ts_sum ]]"
check "marv-ts still runs TS Marv" runs_ts "$bin/marv-ts"
check "says it moved it" grep -q "moved the existing $bin/marv (not marv-rs) to $bin/marv-ts" <<<"$out"

# Idempotent: a second run replaces marv-rs and leaves marv-ts alone.
out=$("$install" --prefix "$prefix" 2>&1)
check "second run: marv is marv-rs" same_as_rs "$bin/marv"
check "second run: marv-ts untouched" runs_ts "$bin/marv-ts"
check "second run: nothing moved" lacks moved "$out"
end

# --- a symlinked launcher (npm's bin) moves as a link ---
begin symlink
ts_launcher "$prefix/lib/marv-ts-real"
mkdir -p "$bin"
ln -s "$prefix/lib/marv-ts-real" "$bin/marv"
"$install" --prefix "$prefix" >/dev/null 2>&1
check "marv-ts is the link" test -L "$bin/marv-ts"
check "marv-ts runs" runs_ts "$bin/marv-ts"
check "marv is a file" test ! -L "$bin/marv"
check "the link's target untouched" runs_ts "$prefix/lib/marv-ts-real"
end

# --- --version picks the tagged release; a bare number gets its v ---
begin version
"$install" --prefix "$prefix" --version 0.0.1 >/dev/null 2>&1
check "installed from download/v0.0.1" same_as_rs "$bin/marv"
check "an unknown version fails" bash -c "! '$install' --prefix '$prefix/x' --version v9.9.9 >/dev/null 2>&1"
check "and installs nothing" test ! -e "$prefix/x/bin/marv"
end

# --- a bad checksum changes nothing ---
begin checksum
bad="$work/bad-release/latest/download"
mkdir -p "$bad"
cp "$release/latest/download/marv-$host_target.tar.gz" "$bad/"
echo "0000000000000000000000000000000000000000000000000000000000000000  marv-$host_target.tar.gz" >"$bad/SHA256SUMS"
ts_launcher "$bin/marv"
if out=$(MARV_RELEASE_URL="file://$work/bad-release" "$install" --prefix "$prefix" 2>&1); then
	fail "a bad checksum installed"
fi
check "reports the mismatch" grep -q "checksum mismatch" <<<"$out"
check "TS marv untouched" runs_ts "$bin/marv"
check "no marv-ts made" test ! -e "$bin/marv-ts"
# A SHA256SUMS without the asset's line is refused too.
echo "abc  some-other.tar.gz" >"$bad/SHA256SUMS"
check "a missing line fails" bash -c "! MARV_RELEASE_URL='file://$work/bad-release' '$install' --prefix '$prefix' >/dev/null 2>&1"
check "TS marv still untouched" runs_ts "$bin/marv"
end

# --- a non-marv-rs marv and an existing marv-ts: refuse, change nothing ---
begin conflict
ts_launcher "$bin/marv"
ts_launcher "$bin/marv-ts"
echo '# edited' >>"$bin/marv-ts"
before=$(cat "$bin/marv-ts")
if out=$("$install" --prefix "$prefix" 2>&1); then fail "installed over a conflict"; fi
check "says why" grep -q "already exists" <<<"$out"
check "marv untouched" runs_ts "$bin/marv"
check "marv-ts untouched" bash -c "[[ \$(cat '$bin/marv-ts') == '$before' ]]"
check "no staging file left" bash -c "! ls -A '$bin' | grep -q '^\.marv'"
end

# --- --from: a local build; anything that isn't marv-rs is refused ---
begin from
"$install" --prefix "$prefix" --from "$rs" >/dev/null 2>&1
check "installed the local file" same_as_rs "$bin/marv"
ts_launcher "$work/not-rs"
check "a script is refused" bash -c "! '$install' --prefix '$prefix/y' --from '$work/not-rs' >/dev/null 2>&1"
check "and installs nothing" test ! -e "$prefix/y/bin/marv"
printf '\177ELF\002\001\001\000some other program\n' >"$work/other-elf"
check "another ELF is refused" bash -c "! '$install' --prefix '$prefix/y' --from '$work/other-elf' >/dev/null 2>&1"
end

# --- --uninstall restores marv-ts, and is idempotent ---
begin uninstall
ts_launcher "$bin/marv"
"$install" --prefix "$prefix" >/dev/null 2>&1
out=$("$install" --prefix "$prefix" --uninstall 2>&1)
check "marv is TS Marv again" runs_ts "$bin/marv"
check "marv-ts is gone" test ! -e "$bin/marv-ts"
check "says it moved it back" grep -q "moved $bin/marv-ts back to $bin/marv" <<<"$out"
out=$("$install" --prefix "$prefix" --uninstall 2>&1)
check "second uninstall leaves TS Marv" runs_ts "$bin/marv"
check "second uninstall says so" grep -q "isn't marv-rs; leaving it" <<<"$out"
end

begin uninstall-alone
"$install" --prefix "$prefix" >/dev/null 2>&1
"$install" --prefix "$prefix" --uninstall >/dev/null 2>&1
check "marv removed" test ! -e "$bin/marv"
out=$("$install" --prefix "$prefix" --uninstall 2>&1)
check "nothing to do" grep -q "nothing to uninstall" <<<"$out"
end

# --- arguments and targets ---
begin args
check "an unknown option fails" bash -c "! '$install' --bogus >/dev/null 2>&1"
check "--help exits 0" "$install" --help >/dev/null
out=$(MARV_TARGET=riscv64-unknown-linux-musl "$install" --prefix "$prefix" 2>&1) && fail "an asset that doesn't exist installed"
check "a missing asset says what it tried" grep -q "couldn't download .*marv-riscv64-unknown-linux-musl.tar.gz" <<<"$out"
end

# --- the real binary runs (only with MARV_TEST_BINARY) ---
if [[ $real == 1 ]]; then
	begin real
	"$install" --prefix "$prefix" >/dev/null 2>&1
	check "marv --version" bash -c "'$bin/marv' --version | grep -q '^marv '"
	end
fi

echo
echo "$passed groups passed, $failed checks failed"
[[ $failed == 0 ]]
