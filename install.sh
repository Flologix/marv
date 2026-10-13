#!/bin/sh
# Installs marv-rs as `marv` (issue #19).
#
#   curl -fsSL https://github.com/FlologixAI/marv-rs/releases/latest/download/install.sh | sh
#   sh install.sh [--prefix DIR] [--version vX.Y.Z] [--from FILE] [--uninstall]
#
# The binary goes to $PREFIX/bin/marv (default prefix: ~/.local). It's a static musl build, so nothing else is needed.
#
# The command switch: TS Marv's launcher is a bash script that is also called `marv`, usually in ~/.local/bin. A `marv`
# there that isn't marv-rs is moved to `marv-ts` first (and the move is reported), so the TypeScript Marv keeps working
# under that name. `--uninstall` removes marv-rs and moves `marv-ts` back to `marv`.
#
# Running it again is safe: a `marv` that is already marv-rs is replaced and `marv-ts` is left alone. Nothing is moved
# or replaced until the new binary has been downloaded and its SHA-256 checked, so a failed download changes nothing.
#
# Environment (mostly for tests and mirrors):
#   MARV_RELEASE_URL  the releases base URL (default https://github.com/FlologixAI/marv-rs/releases); file:// works
#   MARV_TARGET       the target triple to download, instead of the one detected from uname
set -eu

RELEASE_URL=${MARV_RELEASE_URL:-https://github.com/FlologixAI/marv-rs/releases}
PREFIX="${HOME}/.local"
VERSION=latest
FROM=
UNINSTALL=0

say() { printf 'marv install: %s\n' "$*"; }
die() {
	printf 'marv install: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<'EOF'
Install marv-rs as `marv`.

Usage: install.sh [options]

  --prefix DIR      install into DIR/bin (default: ~/.local)
  --version vX.Y.Z  install that release (default: the latest)
  --from FILE       install a marv binary you built (target/release/marv) instead of downloading one
  --uninstall       remove marv-rs and move marv-ts back to marv
  -h, --help        show this help

A `marv` already in DIR/bin that isn't marv-rs (TS Marv's launcher) is moved to `marv-ts` first.
EOF
}

while [ $# -gt 0 ]; do
	case $1 in
	--prefix)
		[ $# -ge 2 ] || die "--prefix needs a directory"
		PREFIX=$2
		shift 2
		;;
	--prefix=*)
		PREFIX=${1#--prefix=}
		shift
		;;
	--version)
		[ $# -ge 2 ] || die "--version needs a version (v0.1.0)"
		VERSION=$2
		shift 2
		;;
	--version=*)
		VERSION=${1#--version=}
		shift
		;;
	--from)
		[ $# -ge 2 ] || die "--from needs a file"
		FROM=$2
		shift 2
		;;
	--from=*)
		FROM=${1#--from=}
		shift
		;;
	--uninstall)
		UNINSTALL=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*) die "unknown option: $1 (see --help)" ;;
	esac
done

[ -n "$PREFIX" ] || die "--prefix is empty"
BIN_DIR="${PREFIX%/}/bin"
MARV="$BIN_DIR/marv"
MARV_TS="$BIN_DIR/marv-ts"

# Whether a file is a marv-rs binary, decided without running it (it could be anything). A native executable (ELF, or
# Mach-O for the macOS builds to come) that contains the name of one of marv-rs's crates: the panic locations keep
# the source paths (crates/marv-session/src/...) even in a stripped release build. TS Marv's launcher is a script.
is_marv_rs() {
	[ -f "$1" ] || return 1
	magic=$(od -An -tx1 -N4 "$1" 2>/dev/null | tr -d ' \n')
	case $magic in
	7f454c46 | cffaedfe | feedfacf | cafebabe) ;;
	*) return 1 ;;
	esac
	LC_ALL=C grep -a -q 'marv-session' "$1" 2>/dev/null
}

# -e is false for a dangling symlink, which still occupies the name.
exists() { [ -e "$1" ] || [ -L "$1" ]; }

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "neither sha256sum nor shasum is installed, so the download can't be checked"
	fi
}

fetch() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --proto '=https,file' -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		die "neither curl nor wget is installed"
	fi
}

detect_target() {
	if [ -n "${MARV_TARGET:-}" ]; then
		printf '%s\n' "$MARV_TARGET"
		return
	fi
	os=$(uname -s)
	arch=$(uname -m)
	case $os in
	Linux) ;;
	Darwin) die "there's no macOS build yet; build from source: cargo install --path crates/marv-cli --root ~/.local" ;;
	*) die "no build for $os" ;;
	esac
	case $arch in
	x86_64 | amd64) printf 'x86_64-unknown-linux-musl\n' ;;
	aarch64 | arm64) printf 'aarch64-unknown-linux-musl\n' ;;
	*) die "no build for $os on $arch" ;;
	esac
}

uninstall() {
	if exists "$MARV"; then
		if is_marv_rs "$MARV"; then
			rm -f "$MARV"
			say "removed $MARV"
		else
			say "$MARV isn't marv-rs; leaving it as it is"
			return 0
		fi
	fi
	if exists "$MARV_TS"; then
		mv "$MARV_TS" "$MARV"
		say "moved $MARV_TS back to $MARV"
	elif ! exists "$MARV"; then
		say "nothing to uninstall in $BIN_DIR"
	fi
}

if [ "$UNINSTALL" = 1 ]; then
	uninstall
	exit 0
fi

TMP=$(mktemp -d 2>/dev/null || mktemp -d -t marv-install)
STAGED=
trap 'rm -rf "$TMP"; [ -z "$STAGED" ] || rm -f "$STAGED"' EXIT
trap 'exit 130' INT TERM

# 1. Get the new binary into $TMP and check it, before anything in BIN_DIR is touched.
if [ -n "$FROM" ]; then
	[ -f "$FROM" ] || die "$FROM doesn't exist"
	cp "$FROM" "$TMP/marv"
	SOURCE=$FROM
else
	TARGET=$(detect_target)
	ASSET="marv-$TARGET.tar.gz"
	if [ "$VERSION" = latest ]; then
		BASE="$RELEASE_URL/latest/download"
	else
		case $VERSION in v*) ;; *) VERSION="v$VERSION" ;; esac
		BASE="$RELEASE_URL/download/$VERSION"
	fi
	say "downloading $ASSET ($VERSION)"
	fetch "$BASE/$ASSET" "$TMP/$ASSET" || die "couldn't download $BASE/$ASSET"
	fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "couldn't download $BASE/SHA256SUMS"
	expected=$(awk -v f="$ASSET" '$2 == f || $2 == "*" f { print $1; exit }' "$TMP/SHA256SUMS")
	[ -n "$expected" ] || die "SHA256SUMS has no line for $ASSET"
	actual=$(sha256_of "$TMP/$ASSET")
	[ "$expected" = "$actual" ] || die "checksum mismatch for $ASSET (expected $expected, got $actual); nothing was installed"
	tar -xzf "$TMP/$ASSET" -C "$TMP" marv || die "$ASSET has no marv in it"
	SOURCE="$ASSET ($VERSION)"
fi
is_marv_rs "$TMP/marv" || die "$SOURCE isn't a marv-rs binary; nothing was installed"
chmod 755 "$TMP/marv"

# 2. Stage it beside the target, so a full disk or a read-only BIN_DIR fails before the switch below.
mkdir -p "$BIN_DIR"
STAGED="$BIN_DIR/.marv.install.$$"
cp "$TMP/marv" "$STAGED"

# 3. The command switch: keep TS Marv as marv-ts.
if exists "$MARV" && ! is_marv_rs "$MARV"; then
	if exists "$MARV_TS"; then
		die "$MARV isn't marv-rs and $MARV_TS already exists; move one of them aside and run this again"
	fi
	mv "$MARV" "$MARV_TS"
	say "moved the existing $MARV (not marv-rs) to $MARV_TS; TS Marv still runs as marv-ts"
fi

# 4. Rename over the target: the rename is atomic, and works while an old marv is running.
mv -f "$STAGED" "$MARV"
STAGED=
say "installed $SOURCE as $MARV"

case ":${PATH:-}:" in
*":$BIN_DIR:"*) ;;
*) say "$BIN_DIR isn't on your PATH; add it (export PATH=\"$BIN_DIR:\$PATH\")" ;;
esac
