#!/bin/bash
# Build the server and the extension and pack them as
# out/norn-<version>-windows-x64.7z (not committed: releases are on GitHub).
# The version is the extension's (vscode/package.json).
set -e
cd "$(dirname "$0")/.."
version=$(node -p "require('./vscode/package.json').version")
seven="${SEVENZIP:-/c/Program Files/7-Zip/7z.exe}"
# (CARGO_TARGET_DIR: build elsewhere, when target/ is in use, by a benchmark say.)
target="${CARGO_TARGET_DIR:-target}"
node release/build.js
# The server goes inside the extension, under the name of what it runs on.
rm -rf vscode/bin && mkdir -p vscode/bin/win32-x64
cp "$target/release/norn.exe" vscode/bin/win32-x64/norn.exe
# The change log goes in too (VS Code shows it on the extension's page),
# and the notices of what others made that the release includes.
cp CHANGELOG.md vscode/CHANGELOG.md
cp LICENSE NOTICE vscode/
node release/notices.js vscode/THIRD_PARTY_NOTICES.txt
(cd vscode && npm run package)
unzip -l out/norn.vsix | grep -q "extension/bin/win32-x64/norn.exe" || { echo "the package has no server in it"; exit 1; }
# What the extension needs once installed is in the package: checked in
# the package itself, then by running it as installed (vscode/src/test/
# package.ts and packaged.ts). A file left out shows only after installing.
node vscode/out/test/package.js out/norn.vsix
(cd vscode && node out/test/runTest.js packaged ../out/norn.vsix) > out/packaged.log 2>&1 || { tail -20 out/packaged.log; echo "the package does not work as installed"; exit 1; }
packed=$(unzip -p out/norn.vsix extension/package.json | node -p "JSON.parse(require('fs').readFileSync(0)).version")
[ "$packed" = "$version" ] || { echo "the package is $packed, not $version"; exit 1; }
stage=out/release-stage
rm -rf "$stage" && mkdir -p "$stage"
cp out/norn.vsix "$stage/norn-$version.vsix"
cp "$target/release/norn.exe" "$stage/norn-windows-x64.exe"
cp release/README.md "$stage/README.md"
cp CHANGELOG.md "$stage/CHANGELOG.md"
cp vscode/THIRD_PARTY_NOTICES.txt "$stage/THIRD_PARTY_NOTICES.txt"
cp LICENSE NOTICE "$stage/"
out="out/norn-$version-windows-x64.7z"
rm -f out/norn-*-windows-x64.7z
(cd "$stage" && "$seven" a -t7z -mx=9 "../../$out" . > /dev/null)
"$seven" l "$out" | tail -6
