#!/bin/sh
# Norn for AI agents, on Linux and macOS:
#
#   curl -fsSL https://norn-c.com/install.sh | sh
#
# Downloads the `norn` executable for this machine from the latest release
# on GitHub, checks it against the release's SHA256SUMS, puts it at
# ~/.norn/bin/norn, and runs `norn setup`: Claude Code, Codex and OpenCode
# found on this machine get Norn's tools and its skill.
#
#   NORN_VERSION=0.26.0   a release other than the latest
#   NORN_NO_SETUP=1       put the executable there, set no agent up
set -eu

repo="https://github.com/humphrey0925/norn/releases"
case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) echo "norn: $(uname -s) is not Linux or macOS; on Windows: irm https://norn-c.com/install.ps1 | iex" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) echo "norn: no release for $(uname -m)" >&2; exit 1 ;;
esac
if [ -n "${NORN_VERSION:-}" ]; then
  base="$repo/download/v${NORN_VERSION#v}"
else
  base="$repo/latest/download"
fi
asset="norn-$os-$arch"

dir="$HOME/.norn/bin"
mkdir -p "$dir"
tmp="$(mktemp "$dir/.norn-download.XXXXXX")"
trap 'rm -f "$tmp" "$tmp.sums"' EXIT

fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$1" -O "$2"
  else
    echo "norn: neither curl nor wget is here to download with" >&2
    exit 1
  fi
}

echo "norn: downloading $asset from $base"
fetch "$base/$asset" "$tmp"
fetch "$base/SHA256SUMS" "$tmp.sums"

want="$(awk -v f="$asset" '$2 == f { print $1 }' "$tmp.sums")"
if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$tmp" | awk '{ print $1 }')"
else
  got="$(shasum -a 256 "$tmp" | awk '{ print $1 }')"
fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "norn: $asset does not match the release's SHA256SUMS; nothing installed" >&2
  exit 1
fi

chmod +x "$tmp"
# (A running copy is renamed away, not written over.)
[ -f "$dir/norn" ] && mv -f "$dir/norn" "$dir/norn.old"
mv -f "$tmp" "$dir/norn"
rm -f "$dir/norn.old"
echo "norn: $("$dir/norn" --version) is at $dir/norn"

if [ -n "${NORN_NO_SETUP:-}" ]; then
  exit 0
fi
if "$dir/norn" setup --help >/dev/null 2>&1; then
  "$dir/norn" setup
else
  echo "norn: this release has no \`norn setup\` yet; set the agents up by hand: https://github.com/humphrey0925/norn/blob/master/docs/agents.md"
fi
