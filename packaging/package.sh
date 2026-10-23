#!/bin/sh
# Packs one release asset (issue #19): marv-<target>.tar.gz (marv, LICENSE, README.md at its root) and
# marv-<target>.tar.gz.sha256 in sha256sum's format. The release job concatenates the .sha256 files into SHA256SUMS,
# which install.sh checks the download against.
#
#   packaging/package.sh <target> <path to the marv binary> <out dir>
#
# The asset name has no version in it, so https://github.com/<repo>/releases/latest/download/marv-<target>.tar.gz
# always names the newest build (install.sh's default).
set -eu

[ $# -eq 3 ] || {
	echo "usage: $0 <target> <marv binary> <out dir>" >&2
	exit 1
}
target=$1
binary=$2
out=$3
root=$(cd "$(dirname "$0")/.." && pwd)

asset="marv-$target.tar.gz"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$binary" "$stage/marv"
chmod 755 "$stage/marv"
cp "$root/LICENSE" "$root/README.md" "$stage/"
mkdir -p "$out"
# Fixed order, owner and times (gzip -n leaves the time out of its header), so the same binary packs to the same
# bytes. GNU tar where there is one (gtar on macOS, if installed); else macOS's bsdtar, with the times set on the
# files themselves and the files named in order.
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
	gnutar=tar
elif command -v gtar >/dev/null 2>&1; then
	gnutar=gtar
else
	gnutar=
fi
if [ -n "$gnutar" ]; then
	"$gnutar" -C "$stage" --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf - marv LICENSE README.md
else
	TZ=UTC touch -t 197001010000.00 "$stage/marv" "$stage/LICENSE" "$stage/README.md"
	tar -C "$stage" --format=ustar --uid 0 --gid 0 --uname '' --gname '' -cf - LICENSE README.md marv
fi | gzip -n -9 >"$out/$asset"
# sha256sum's format ("<hash>  <file>"); macOS has shasum, which writes the same.
if command -v sha256sum >/dev/null 2>&1; then
	(cd "$out" && sha256sum "$asset" >"$asset.sha256")
else
	(cd "$out" && shasum -a 256 "$asset" >"$asset.sha256")
fi
cat "$out/$asset.sha256"
