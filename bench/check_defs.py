"""Recall of function definitions against a parser-independent estimate.

GNU-style C puts the name of a function being defined in column 0 and its
opening brace in column 0 too:

    static bool
    elf_foo (bfd *abfd)
    {

A regex finds those without parsing anything, so it is an independent check
on what the index extracted. It is an estimate, not ground truth: it only
understands this layout, so use it on code that follows it (binutils, gdb).

    python bench/check_defs.py <corpus> <index dir> [--worst N]
"""
import argparse
import collections
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SI = ROOT / "target" / "release" / "si.exe"
NAME = re.compile(rb"^([A-Za-z_]\w*) ?\(")
KEYWORDS = {b"if", b"while", b"for", b"switch", b"return", b"sizeof", b"defined"}


def expected(path):
    """Names defined GNU-style in one file: `name (` in column 0, then `{` in column 0."""
    names = set()
    pending, depth = None, 0
    for line in path.read_bytes().split(b"\n"):
        if pending is None:
            m = NAME.match(line)
            if m and m.group(1) not in KEYWORDS and not m.group(1).isupper():
                pending, depth = m.group(1), 0
            else:
                continue
        elif line.startswith(b"{") and depth <= 0:
            names.add(pending)
            pending = None
            continue
        elif line.startswith(b"#") or (depth <= 0 and line.strip() and not line[:1].isspace()):
            pending = None
            continue
        depth += line.count(b"(") - line.count(b")")
        if depth <= 0 and line.rstrip().endswith(b";"):
            pending = None
    return names


def recall(corpus, index):
    """(expected, found, {file: (missing count, expected count, missing names)})."""
    corpus = Path(corpus)
    found = collections.defaultdict(set)
    out = subprocess.run([str(SI), "symbols", str(index)], capture_output=True, check=True).stdout
    for row in out.split(b"\n"):
        cols = row.rstrip(b"\r").split(b"\t")
        if len(cols) == 5 and cols[2] == b"function" and cols[3] == b"def":
            found[cols[0].decode()].add(cols[4])

    total = hit = 0
    missing = {}
    for path in sorted(corpus.rglob("*.[ch]")):
        rel = path.relative_to(corpus).as_posix()
        want = expected(path)
        miss = want - found.get(rel, set())
        total += len(want)
        hit += len(want) - len(miss)
        if miss:
            missing[rel] = (len(miss), len(want), sorted(miss))
    return total, hit, missing


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus")
    ap.add_argument("index")
    ap.add_argument("--worst", type=int, default=15)
    args = ap.parse_args()
    total, hit, missing = recall(args.corpus, args.index)
    print(f"function definitions: expected {total}, found {hit}, recall {hit / total:.4%}, "
          f"missing {total - hit} in {len(missing)} files")
    for rel, (n, of, names) in sorted(missing.items(), key=lambda kv: -kv[1][0])[:args.worst]:
        print(f"  {n:5d}/{of:<5d} {rel}  e.g. {b', '.join(names[:3]).decode()}")


if __name__ == "__main__":
    main()
