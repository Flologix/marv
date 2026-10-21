#!/bin/sh
# Builds the npm packages from the release assets: @flologixai/marv (the launcher, launcher.js) and one
# @flologixai/marv-<os>-<cpu> per binary (linux-x64, linux-arm64, darwin-x64, darwin-arm64), each packed with
# `npm pack` into <out>. A missing tarball is skipped (and left out of the launcher's optionalDependencies).
#
#   packaging/npm/build.sh <version> <dir with marv-<target>.tar.gz> <out dir>
#
# Publish the platform packages first, then the launcher (it names them as optionalDependencies at <version>):
#   npm publish <out>/flologixai-marv-linux-x64-<version>.tgz --access public   (and linux-arm64, darwin-x64, darwin-arm64)
#   npm publish <out>/flologixai-marv-<version>.tgz --access public
set -eu

[ $# -eq 3 ] || {
	echo "usage: $0 <version> <assets dir> <out dir>" >&2
	exit 1
}
version=$1
assets=$(cd "$2" && pwd)
mkdir -p "$3"
out=$(cd "$3" && pwd)
root=$(cd "$(dirname "$0")/../.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

repo='{ "type": "git", "url": "git+https://github.com/FlologixAI/marv.git" }'
optional=""
for triple in x86_64-unknown-linux-musl:linux:x64 aarch64-unknown-linux-musl:linux:arm64 \
	x86_64-apple-darwin:darwin:x64 aarch64-apple-darwin:darwin:arm64; do
	target=${triple%%:*}
	rest=${triple#*:}
	platform=${rest%%:*}
	cpu=${rest#*:}
	case $platform in
	linux) name="Linux" ;;
	darwin) name="macOS" ;;
	esac
	tarball="$assets/marv-$target.tar.gz"
	[ -f "$tarball" ] || {
		echo "build.sh: $tarball is missing; skipping $platform-$cpu" >&2
		continue
	}
	dir="$stage/$platform-$cpu"
	mkdir -p "$dir/bin"
	tar -xzf "$tarball" -C "$dir/bin" marv
	chmod 755 "$dir/bin/marv"
	cp "$root/LICENSE" "$dir/"
	cat >"$dir/package.json" <<JSON
{
  "name": "@flologixai/marv-$platform-$cpu",
  "version": "$version",
  "description": "The marv binary for $name $cpu (installed by @flologixai/marv).",
  "license": "MIT",
  "repository": $repo,
  "os": ["$platform"],
  "cpu": ["$cpu"],
  "files": ["bin/marv", "LICENSE"],
  "preferUnplugged": true
}
JSON
	(cd "$dir" && npm pack --silent --pack-destination "$out" >/dev/null)
	optional="$optional${optional:+,
}    \"@flologixai/marv-$platform-$cpu\": \"$version\""
done
[ -n "$optional" ] || {
	echo "build.sh: no binaries in $assets" >&2
	exit 1
}

dir="$stage/marv"
mkdir -p "$dir/bin"
cp "$root/packaging/npm/launcher.js" "$dir/bin/marv.js"
chmod 755 "$dir/bin/marv.js"
cp "$root/LICENSE" "$root/README.md" "$dir/"
cat >"$dir/package.json" <<JSON
{
  "name": "@flologixai/marv",
  "version": "$version",
  "description": "Marv: a coding agent for your terminal, sandboxed.",
  "license": "MIT",
  "repository": $repo,
  "homepage": "https://github.com/FlologixAI/marv",
  "bin": { "marv": "bin/marv.js" },
  "files": ["bin/marv.js", "README.md", "LICENSE"],
  "engines": { "node": ">=18" },
  "optionalDependencies": {
$optional
  }
}
JSON
(cd "$dir" && npm pack --silent --pack-destination "$out" >/dev/null)
ls -1 "$out"
