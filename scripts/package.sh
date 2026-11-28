#!/bin/sh
# Assemble a tango package from a built tree: the launcher binary, the Node serve (bin, dist,
# package.json) and the installer, in the layout the launcher expects — the binary three levels
# below the root that holds bin/tango.js (launch.rs repo_root).
#
#   scripts/package.sh <version> <platform> <out-dir> [binary]
#   platform: darwin-arm64 | darwin-x64 | linux-x64 | linux-arm64 | windows-x64 | windows-arm64
#   binary:   default tui/target/release/tango-tui (pass the --target path for a cross build)
# A tarball for macOS and Linux, a zip for Windows (the binary is tango-tui.exe there; the
# script runs under Git Bash on the Windows runner). Prints the path of the package.
set -eu

version=$1
platform=$2
out=$3
binary=${4:-tui/target/release/tango-tui}

root=$(cd "$(dirname "$0")/.." && pwd)
[ -x "$root/$binary" ] || { echo "package: no binary at $binary — build first (cargo build --release in tui/)" >&2; exit 1; }
[ -f "$root/dist/cli.js" ] || { echo "package: no dist/cli.js — build first (npm run build)" >&2; exit 1; }
[ -f "$root/install.sh" ] || { echo "package: install.sh missing" >&2; exit 1; }
[ -f "$root/install.ps1" ] || { echo "package: install.ps1 missing" >&2; exit 1; }

name="tango-$version-$platform"
case "$platform" in
  windows-*) exe=tango-tui.exe ;;
  *) exe=tango-tui ;;
esac
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
pkg="$stage/$name"
mkdir -p "$pkg/tui/target/release" "$pkg/bin"
cp "$root/$binary" "$pkg/tui/target/release/$exe"
cp "$root/bin/tango.js" "$pkg/bin/tango.js"
cp "$root/package.json" "$pkg/package.json"
cp "$root/install.sh" "$root/install.ps1" "$pkg/"
cp -R "$root/dist" "$pkg/dist"
commit=$(git -C "$root" rev-parse --short HEAD 2>/dev/null || echo unknown)
cat > "$pkg/VERSION" <<EOF
name tango
version $version
platform $platform
commit $commit
EOF

mkdir -p "$out"
case "$platform" in
  windows-*)
    # Windows' own bsdtar writes zip by extension (-a); Git Bash's zip is not installed
    /c/Windows/System32/tar.exe -a -cf "$(cygpath -w "$out/$name.zip")" -C "$(cygpath -w "$stage")" "$name"
    echo "$out/$name.zip"
    ;;
  *)
    tar -czf "$out/$name.tar.gz" -C "$stage" "$name"
    echo "$out/$name.tar.gz"
    ;;
esac
