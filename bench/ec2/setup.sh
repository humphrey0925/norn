#!/bin/bash
# What the benchmark needs on a fresh Ubuntu 24.04 (an EC2 instance, or
# WSL): compilers and clang's Python bindings for the ground truth, the
# tools compared, Rust, and Norn built from this checkout.
#   bench/ec2/setup.sh
set -euo pipefail
REPO=${REPO:-$(cd "$(dirname "$0")/../.." && pwd)}
export DEBIAN_FRONTEND=noninteractive

sudo apt-get update -q
sudo apt-get install -y -q --no-install-recommends \
  build-essential git curl ca-certificates pkg-config rsync bear zlib1g-dev bison flex texinfo \
  clang-18 libclang-18-dev python3-clang-18 clangd-20 \
  universal-ctags cscope global \
  python3 python3-psutil

# clang's Python bindings find libclang by this name.
[ -e /usr/lib/x86_64-linux-gnu/libclang.so ] || sudo ln -sf /usr/lib/llvm-18/lib/libclang.so.1 /usr/lib/x86_64-linux-gnu/libclang.so

# Rust, the version CI uses.
if ! command -v cargo > /dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.99.1
fi
. "$HOME/.cargo/env"

# codebase-memory-mcp, the release compared, checked against its checksum.
CBM=$REPO/tools/cbm
if [ ! -x "$CBM/codebase-memory-mcp" ]; then
  mkdir -p "$CBM"
  curl -fsSL -o /tmp/cbm.tar.gz https://github.com/DeusData/codebase-memory-mcp/releases/download/v0.11.0/codebase-memory-mcp-linux-amd64.tar.gz
  echo "032b33c1833919a2d1de67ff6367fa6ea46aee8689c86ef223c88fae3b6e4536  /tmp/cbm.tar.gz" | sha256sum -c -
  tar -xzf /tmp/cbm.tar.gz -C "$CBM"
  chmod +x "$CBM/codebase-memory-mcp"
fi

(cd "$REPO" && cargo build --release --locked)
echo "ready: $(clang-18 --version | head -1); $(clangd-20 --version | head -1); norn $("$REPO/target/release/norn" --version)"
