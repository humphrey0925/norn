#!/bin/bash
# Build the server and the extension and pack them as
# release/norn-<version>-windows-x64.7z, the one file a release is.
# The version is the extension's (vscode/package.json).
set -e
cd "$(dirname "$0")/.."
version=$(node -p "require('./vscode/package.json').version")
seven="${SEVENZIP:-/c/Program Files/7-Zip/7z.exe}"
cargo build --release
# The server goes inside the extension, under the name of what it runs on.
rm -rf vscode/bin && mkdir -p vscode/bin/win32-x64
cp target/release/norn.exe vscode/bin/win32-x64/norn.exe
(cd vscode && npm run package)
unzip -l out/norn.vsix | grep -q "extension/bin/win32-x64/norn.exe" || { echo "the package has no server in it"; exit 1; }
packed=$(unzip -p out/norn.vsix extension/package.json | node -p "JSON.parse(require('fs').readFileSync(0)).version")
[ "$packed" = "$version" ] || { echo "the package is $packed, not $version"; exit 1; }
stage=out/release-stage
rm -rf "$stage" && mkdir -p "$stage"
cp out/norn.vsix "$stage/norn-$version.vsix"
cp target/release/norn.exe "$stage/norn-windows-x64.exe"
cp release/README.md "$stage/README.md"
out="release/norn-$version-windows-x64.7z"
rm -f release/si-*-windows-x64.7z release/norn-*-windows-x64.7z
(cd "$stage" && "$seven" a -t7z -mx=9 "../../$out" . > /dev/null)
"$seven" l "$out" | tail -6
