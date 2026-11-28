#!/bin/sh
# Assemble a tango package from a built tree: the launcher binary, the Node serve (bin, dist,
# package.json) and install.sh, in the layout the launcher expects — the binary three levels
# below the root that holds bin/tango.js (launch.rs repo_root).
#
#   scripts/package.sh <version> <platform> <out-dir> [binary]
#   platform: darwin-arm64 | darwin-x64 | linux-x64 | linux-arm64
#   binary:   default tui/target/release/tango-tui (pass the --target path for a cross build)
# Prints the path of the tarball: <out-dir>/tango-<version>-<platform>.tar.gz
set -eu

version=$1
platform=$2
out=$3
binary=${4:-tui/target/release/tango-tui}

root=$(cd "$(dirname "$0")/.." && pwd)
[ -x "$root/$binary" ] || { echo "package: no binary at $binary — build first (cargo build --release in tui/)" >&2; exit 1; }
[ -f "$root/dist/cli.js" ] || { echo "package: no dist/cli.js — build first (npm run build)" >&2; exit 1; }
[ -f "$root/install.sh" ] || { echo "package: install.sh missing" >&2; exit 1; }

name="tango-$version-$platform"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/$name"
mkdir -p "$pkg/tui/target/release" "$pkg/bin"
cp "$root/$binary" "$pkg/tui/target/release/tango-tui"
cp "$root/bin/tango.js" "$pkg/bin/tango.js"
cp "$root/package.json" "$pkg/package.json"
cp "$root/install.sh" "$pkg/install.sh"
cp -R "$root/dist" "$pkg/dist"
commit=$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)
cat > "$pkg/VERSION" <<EOF
name tango
version $version
platform $platform
commit $commit
EOF

mkdir -p "$out"
tar -czf "$out/$name.tar.gz" -C "$stage" "$name"
echo "$out/$name.tar.gz"
