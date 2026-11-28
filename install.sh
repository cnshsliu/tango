#!/bin/sh
# Install tango from a GitHub release: pick the package for this OS and CPU, check its
# SHA-256 against the release's SHA256SUMS, unpack it under the tango home and link `tango`
# into ~/.local/bin. Node 20 or newer must be on PATH: the serve is Node, and the launcher
# runs it from there.
#
#   curl -fsSL https://raw.githubusercontent.com/cnshsliu/tango/main/install.sh | sh
#   TANGO_VERSION=v0.2.0 … | sh           a given release (default: the latest)
#   TANGO_INSTALL_DIR=/usr/local/bin …    where the `tango` link goes (default ~/.local/bin)
#   TANGO_HOME=~/.tango                   where releases/ goes (the tango home, as for tango)
#   TANGO_RELEASES_URL=…                  download base (default: this repo's GitHub releases)
#   TANGO_NO_SHA=1                        skip the checksum check (not recommended)
#   GITHUB_TOKEN=…                        for the latest-release lookup (optional)
set -eu

repo="cnshsliu/tango"
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$arch" in
  arm64 | aarch64) arch=arm64 ;;
  x86_64 | amd64) arch=x64 ;;
  *) echo "tango: unsupported CPU: $arch" >&2; exit 1 ;;
esac
case "$os" in
  darwin | linux) ;;
  *) echo "tango: unsupported OS: $os (macOS and Linux)" >&2; exit 1 ;;
esac
platform="$os-$arch"

command -v node >/dev/null 2>&1 || { echo "tango: node 20 or newer is needed — https://nodejs.org" >&2; exit 1; }
node_major=$(node -p 'process.versions.node.split(".")[0]')
[ "$node_major" -ge 20 ] || { echo "tango: node 20 or newer is needed, found $(node --version)" >&2; exit 1; }

base="${TANGO_RELEASES_URL:-https://github.com/$repo/releases/download}"
version="${TANGO_VERSION:-}"
if [ -z "$version" ]; then
  set -- -fsSL -H 'X-GitHub-Api-Version: 2022-11-28'
  [ -n "${GITHUB_TOKEN:-}" ] && set -- "$@" -H "Authorization: Bearer $GITHUB_TOKEN"
  version=$(curl "$@" "https://api.github.com/repos/$repo/releases/latest" 2>/dev/null |
    sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/v\1/p' | head -n 1)
fi
[ -n "$version" ] || { echo "tango: no release found — pass TANGO_VERSION=v0.2.0" >&2; exit 1; }
case "$version" in v*) ;; *) version="v$version" ;; esac

asset="tango-${version#v}-$platform.tar.gz"
url="$base/$version/$asset"
home="${TANGO_HOME:-$HOME/.tango}"
dest="$home/releases/tango-${version#v}-$platform"
bindir="${TANGO_INSTALL_DIR:-$HOME/.local/bin}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "tango: downloading $url"
curl -fsSL -o "$tmp/$asset" "$url" || { echo "tango: no package $asset in release $version" >&2; exit 1; }
if [ -z "${TANGO_NO_SHA:-}" ]; then
  curl -fsSL -o "$tmp/SHA256SUMS" "$base/$version/SHA256SUMS" || { echo "tango: SHA256SUMS missing for $version" >&2; exit 1; }
  expected=$(grep " \*\{0,1\}$asset\$" "$tmp/SHA256SUMS" | awk '{print $1}')
  [ -n "$expected" ] || { echo "tango: $asset is not in SHA256SUMS" >&2; exit 1; }
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$asset" | awk '{print $1}')
  else
    actual=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')
  fi
  [ "$actual" = "$expected" ] || { echo "tango: checksum mismatch for $asset (expected $expected, got $actual)" >&2; exit 1; }
fi

rm -rf "$dest"
mkdir -p "$dest" "$bindir"
tar -xzf "$tmp/$asset" -C "$dest" --strip-components=1
chmod +x "$dest/tui/target/release/tango-tui"
ln -sfn "$dest/tui/target/release/tango-tui" "$bindir/tango"
ln -sfn "$dest/tui/target/release/tango-tui" "$bindir/tang"

case ":$PATH:" in
  *":$bindir:"*) ;;
  *) echo "tango: add $bindir to your PATH" ;;
esac
echo "tango ${version#v} installed: $bindir/tango"
