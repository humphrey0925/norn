#!/usr/bin/env python3
"""Ground truth from clang: function definitions and direct calls of a built tree.

Runs where the tree was configured and built (Linux, `bear -- make`), over
every entry of compile_commands.json, and writes one JSON file:

  defs      [usr, name, file, line, static]
  calls     [caller file, caller name, caller line, callee usr, callee name,
             callee file or null, site file, site line]

The callee's file is given when its definition is in the same translation
unit. Otherwise it has to be found by USR, which names one function per
program: `main` and other names that several programs define have several.
  files     {file: {"tus": n, "inactive": [[first, last], ...]}}

`file` is relative to the source root; generated files are `<build>/...`.
A line is inactive when every translation unit that read the file skipped it
(`#if` not taken), so nothing can be expected of an indexer there.
"""
import argparse
import collections
import ctypes
import json
import multiprocessing
import os
import sys

from clang import cindex
from clang.cindex import CursorKind, TranslationUnit


class RangeList(ctypes.Structure):
    _fields_ = [("count", ctypes.c_uint), ("ranges", ctypes.POINTER(cindex.SourceRange))]


LIB = cindex.conf.lib
LIB.clang_getAllSkippedRanges.argtypes = [cindex.TranslationUnit]
LIB.clang_getAllSkippedRanges.restype = ctypes.POINTER(RangeList)
LIB.clang_disposeSourceRangeList.argtypes = [ctypes.POINTER(RangeList)]

DROP_WITH_VALUE = {"-o", "-MT", "-MF", "-MQ"}
DROP = {"-c", "-MD", "-MP", "-MMD"}
SRC = BUILD = None


def rel(path):
    path = os.path.realpath(path)
    if path.startswith(SRC + "/"):
        return path[len(SRC) + 1:]
    if path.startswith(BUILD + "/"):
        return "<build>/" + path[len(BUILD) + 1:]
    return None


def flags(entry):
    out, args = [], iter(entry["arguments"][1:])
    for a in args:
        if a in DROP_WITH_VALUE:
            next(args)
        elif a not in DROP and a != entry["file"] and os.path.basename(a) != os.path.basename(entry["file"]):
            out.append(a)
    return out


def calls_in(fn, caller, out, stats):
    stack = list(fn.get_children())
    while stack:
        c = stack.pop()
        if c.kind == CursorKind.CALL_EXPR:
            callee = c.referenced
            if callee is not None and callee.kind == CursorKind.FUNCTION_DECL:
                loc = c.location
                site = rel(loc.file.name) if loc.file else None
                d = callee.get_definition()
                home = rel(d.location.file.name) if d is not None and d.location.file else None
                out.add((*caller, callee.get_usr(), callee.spelling, home, site, loc.line))
            else:
                stats["indirect"] += 1
        stack.extend(c.get_children())


def one(entry):
    os.chdir(entry["directory"])
    path = os.path.join(entry["directory"], entry["file"])
    try:
        tu = cindex.Index.create().parse(path, flags(entry), options=TranslationUnit.PARSE_DETAILED_PROCESSING_RECORD)
    except cindex.TranslationUnitLoadError:
        return {"file": entry["file"], "failed": True}
    errors = [d.spelling for d in tu.diagnostics if d.severity >= cindex.Diagnostic.Error]
    defs, calls, stats = set(), set(), collections.Counter()
    for c in tu.cursor.get_children():
        if c.kind != CursorKind.FUNCTION_DECL or not c.is_definition() or c.location.file is None:
            continue
        file = rel(c.location.file.name)
        if file is None:
            continue
        defs.add((c.get_usr(), c.spelling, file, c.location.line, c.storage_class == cindex.StorageClass.STATIC))
        calls_in(c, (file, c.spelling, c.location.line), calls, stats)
    read = {rel(path)} | {rel(i.include.name) for i in tu.get_includes()}
    read.discard(None)
    skipped = collections.defaultdict(list)
    ranges = LIB.clang_getAllSkippedRanges(tu)
    for i in range(ranges.contents.count):
        r = ranges.contents.ranges[i]
        file = rel(r.start.file.name) if r.start.file else None
        if file:
            skipped[file].append((r.start.line, r.end.line))
    LIB.clang_disposeSourceRangeList(ranges)
    return {"file": entry["file"], "errors": errors[:3], "defs": list(defs), "calls": list(calls), "read": list(read),
            "skipped": skipped, "indirect": stats["indirect"]}


def main():
    global SRC, BUILD
    ap = argparse.ArgumentParser()
    ap.add_argument("src")
    ap.add_argument("build")
    ap.add_argument("out")
    args = ap.parse_args()
    SRC, BUILD = os.path.realpath(args.src), os.path.realpath(args.build)
    entries = json.load(open(os.path.join(BUILD, "compile_commands.json")))
    entries = list({(e["directory"], e["file"]): e for e in entries}.values())

    defs, calls, indirect, failed, with_errors = set(), set(), 0, [], []
    tus = collections.Counter()
    skips = collections.defaultdict(collections.Counter)
    with multiprocessing.Pool() as pool:
        for n, r in enumerate(pool.imap_unordered(one, entries, chunksize=4)):
            if r.get("failed"):
                failed.append(r["file"])
                continue
            if r["errors"]:
                with_errors.append((r["file"], r["errors"]))
            defs.update(map(tuple, r["defs"]))
            calls.update(map(tuple, r["calls"]))
            indirect += r["indirect"]
            for f in r["read"]:
                tus[f] += 1
            for f, ranges in r["skipped"].items():
                lines = set()
                for a, b in ranges:
                    lines.update(range(a, b + 1))
                skips[f].update(lines)
            if n % 100 == 0:
                print(f"  {n}/{len(entries)}", file=sys.stderr, flush=True)

    files = {}
    for f, n in tus.items():
        dead = sorted(line for line, k in skips[f].items() if k == n)
        runs = []
        for line in dead:
            if runs and runs[-1][1] == line - 1:
                runs[-1][1] = line
            else:
                runs.append([line, line])
        files[f] = {"tus": n, "inactive": runs}
    json.dump({"translation_units": len(entries), "failed": failed, "with_errors": with_errors, "indirect_calls": indirect,
               "defs": sorted(defs), "calls": sorted(calls, key=str), "files": files},
              open(args.out, "w"))
    print(f"{len(entries)} TUs, {len(failed)} failed, {len(with_errors)} with errors, "
          f"{len(defs)} definitions, {len(calls)} call sites, {indirect} indirect calls")


if __name__ == "__main__":
    main()
