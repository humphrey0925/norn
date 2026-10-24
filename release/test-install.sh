#!/bin/bash
# site/install.sh as `curl ... | sh` runs it, in a home folder of its own,
# with this build's `norn` (target/release) served as a release would be:
# installed, checked and set up for Codex and OpenCode; and refused, with
# nothing changed, when the checksum does not match.
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in Linux) os=linux ;; Darwin) os=darwin ;; esac
case "$(uname -m)" in x86_64 | amd64) arch=x64 ;; aarch64 | arm64) arch=arm64 ;; esac
work="$(mktemp -d)"
trap 'kill "$server" 2>/dev/null || true; rm -rf "$work"' EXIT
mkdir -p "$work/release" "$work/home/.codex" "$work/home/.config/opencode"
cp target/release/norn "$work/release/norn-$os-$arch"
(cd "$work/release" && sha256sum "norn-$os-$arch" 2>/dev/null || shasum -a 256 "norn-$os-$arch") > "$work/release/SHA256SUMS"
port=$((20000 + RANDOM % 20000))
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$work/release" >/dev/null 2>&1 &
server=$!
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$port/SHA256SUMS" >/dev/null && break; sleep 0.2; done

install() {
  env -i PATH=/usr/bin:/bin HOME="$work/home" NORN_DOWNLOAD_URL="http://127.0.0.1:$port/" sh < site/install.sh
}
fail() { echo "test-install: $*" >&2; exit 1; }

install
home="$work/home"
[ "$("$home/.norn/bin/norn" --version)" = "$(target/release/norn --version)" ] || fail "the installed norn is not this build's"
grep -q '^\[mcp_servers\.norn\]' "$home/.codex/config.toml" || fail "no [mcp_servers.norn] in Codex's config"
grep -q '"norn"' "$home/.config/opencode/opencode.json" || fail "no norn in OpenCode's config"
[ -s "$home/.agents/skills/norn/SKILL.md" ] || fail "no skill in ~/.agents/skills/norn"

before="$(cksum < "$home/.norn/bin/norn")"
echo "0000000000000000000000000000000000000000000000000000000000000000  norn-$os-$arch" > "$work/release/SHA256SUMS"
if install; then fail "installed although the checksum did not match"; fi
[ "$(cksum < "$home/.norn/bin/norn")" = "$before" ] || fail "the norn there was changed although the checksum did not match"
echo "test-install: install.sh installs, sets up, and refuses a checksum that does not match"
