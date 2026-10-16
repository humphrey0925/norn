#!/bin/bash
# The files of binutils the extension's tests run on (vscode/src/test/
# runTest.ts copies them from corpus/c-only), at the commit the tests were
# written against. Files already there are kept.
set -e
cd "$(dirname "$0")/.."
commit=ced7bfccaa63502f0765783dd513d174d415ef9e
for f in bfd/opncls.c bfd/bfd-in2.h bfd/libbfd.c bfd/libbfd.h bfd/cache.c bfd/bfdio.c include/ansidecl.h; do
  out="corpus/c-only/$f"
  [ -s "$out" ] && continue
  mkdir -p "$(dirname "$out")"
  curl -fsSL --retry 3 -o "$out" "https://sourceware.org/git/?p=binutils-gdb.git;a=blob_plain;f=$f;hb=$commit"
done
echo "corpus/c-only: $(find corpus/c-only -type f | wc -l) files"
