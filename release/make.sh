#!/bin/bash
# Build the server and the extension and pack them as
# release/si-<version>-windows-x64.7z, the one file a release is.
# The version is the extension's (vscode/package.json).
set -e
cd "$(dirname "$0")/.."
version=$(node -p "require('./vscode/package.json').version")
seven="${SEVENZIP:-/c/Program Files/7-Zip/7z.exe}"
cargo build --release
# The server goes inside the extension, under the name of what it runs on.
rm -rf vscode/bin && mkdir -p vscode/bin/win32-x64
cp target/release/si.exe vscode/bin/win32-x64/si.exe
(cd vscode && npm run package)
unzip -l out/si-code-index.vsix | grep -q "extension/bin/win32-x64/si.exe" || { echo "the package has no server in it"; exit 1; }
packed=$(unzip -p out/si-code-index.vsix extension/package.json | node -p "JSON.parse(require('fs').readFileSync(0)).version")
[ "$packed" = "$version" ] || { echo "the package is $packed, not $version"; exit 1; }
stage=out/release-stage
rm -rf "$stage" && mkdir -p "$stage"
cp out/si-code-index.vsix "$stage/si-code-index-$version.vsix"
cp target/release/si.exe "$stage/si-windows-x64.exe"
cp release/README.md "$stage/README.md"
out="release/si-$version-windows-x64.7z"
rm -f release/si-*-windows-x64.7z
(cd "$stage" && "$seven" a -t7z -mx=9 "../../$out" . > /dev/null)
"$seven" l "$out" | tail -6
