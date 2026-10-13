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
  icalls    [caller file, caller name, caller line, kind, record, field,
             site file, site line]
  binds     [record, field, function usr, function name, function file or
             null, site file, site line]
  files     {file: {"tus": n, "inactive": [[first, last], ...]}}

An indirect call (`icalls`) goes through a struct field (`kind` "field",
with the record and field names), a parameter, a variable, or something
else. `binds` are the places a function is stored in a field: by an
initializer, positional or designated, or by an assignment. Joining the two
on (record, field) gives the calls a field-based, type-aware analysis finds;
it is an upper bound on what runs, not a record of it.

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


OTHER = ("other", "", "")
WRAPPERS = {CursorKind.UNEXPOSED_EXPR, CursorKind.PAREN_EXPR, CursorKind.CSTYLE_CAST_EXPR, CursorKind.UNARY_OPERATOR,
            CursorKind.ARRAY_SUBSCRIPT_EXPR}


def site(loc):
    return rel(loc.file.name) if loc.file else None


def slot_of(field):
    return (field.semantic_parent.spelling, field.spelling)


def target_of(call):
    """What an indirect call goes through: (kind, record, name)."""
    kids = list(call.get_children())
    c = kids[0] if kids else None
    while c is not None:
        if c.kind == CursorKind.MEMBER_REF_EXPR:
            f = c.referenced
            return ("field", *slot_of(f)) if f is not None and f.kind == CursorKind.FIELD_DECL else OTHER
        if c.kind == CursorKind.DECL_REF_EXPR:
            d = c.referenced
            if d is not None and d.kind == CursorKind.PARM_DECL:
                return ("param", "", d.spelling)
            if d is not None and d.kind == CursorKind.VAR_DECL:
                return ("var", "", d.spelling)
            return OTHER
        if c.kind not in WRAPPERS:
            return OTHER
        kids = list(c.get_children())
        # A cast's operand is its last child; the others lead with theirs.
        c = (kids[-1] if c.kind == CursorKind.CSTYLE_CAST_EXPR else kids[0]) if kids else None
    return OTHER


def functions_named_in(expr):
    """Functions whose address the expression takes (not ones it calls)."""
    stack = [expr]
    while stack:
        c = stack.pop()
        if c.kind == CursorKind.CALL_EXPR:
            continue
        if c.kind == CursorKind.DECL_REF_EXPR:
            d = c.referenced
            if d is not None and d.kind == CursorKind.FUNCTION_DECL:
                yield d
            continue
        stack.extend(c.get_children())


def bind(slot, expr, out):
    for fn in functions_named_in(expr):
        d = fn.get_definition()
        home = rel(d.location.file.name) if d is not None and d.location.file else None
        out.add((*slot, fn.get_usr(), fn.spelling, home, site(expr.location), expr.location.line))


def init_list(lst, owner, out):
    """Bind what a braced initializer stores. `owner` is the field the list
    itself initializes, which is what the elements of an array belong to."""
    t = lst.type.get_canonical()
    fields = list(t.get_fields()) if t.kind == cindex.TypeKind.RECORD else None
    pos = 0
    for ch in lst.get_children():
        slot, value = owner, ch
        kids = list(ch.get_children())
        if ch.kind == CursorKind.UNEXPOSED_EXPR and kids and kids[0].kind == CursorKind.MEMBER_REF:
            f, value = kids[0].referenced, kids[-1]
            if f is not None and f.kind == CursorKind.FIELD_DECL:
                slot = slot_of(f)
                names = [x.spelling for x in fields or []]
                if f.spelling in names:
                    pos = names.index(f.spelling) + 1
        elif fields is not None:
            slot = slot_of(fields[pos]) if pos < len(fields) else None
            pos += 1
        elif ch.kind == CursorKind.UNEXPOSED_EXPR and kids and kids[-1].kind == CursorKind.INIT_LIST_EXPR:
            value = kids[-1]  # `[i] = { ... }`
        if value.kind == CursorKind.INIT_LIST_EXPR:
            init_list(value, slot, out)
        elif slot is not None:
            bind(slot, value, out)


def binds_in(root, out):
    stack = [root]
    while stack:
        c = stack.pop()
        if c.kind == CursorKind.INIT_LIST_EXPR:
            init_list(c, None, out)
            continue
        if c.kind == CursorKind.BINARY_OPERATOR:
            kids = list(c.get_children())
            if len(kids) == 2:
                lhs = kids[0]
                while lhs.kind == CursorKind.PAREN_EXPR:
                    lhs = next(lhs.get_children())
                f = lhs.referenced if lhs.kind == CursorKind.MEMBER_REF_EXPR else None
                # An assignment has the type of its left side; a comparison does not.
                if f is not None and f.kind == CursorKind.FIELD_DECL and c.type.spelling == kids[0].type.spelling:
                    bind(slot_of(f), kids[1], out)
        stack.extend(c.get_children())


def calls_in(fn, caller, out, icalls, stats):
    stack = list(fn.get_children())
    while stack:
        c = stack.pop()
        if c.kind == CursorKind.CALL_EXPR:
            callee = c.referenced
            if callee is not None and callee.kind == CursorKind.FUNCTION_DECL:
                loc = c.location
                d = callee.get_definition()
                home = rel(d.location.file.name) if d is not None and d.location.file else None
                out.add((*caller, callee.get_usr(), callee.spelling, home, site(loc), loc.line))
            else:
                stats["indirect"] += 1
                icalls.add((*caller, *target_of(c), site(c.location), c.location.line))
        stack.extend(c.get_children())


def one(entry):
    os.chdir(entry["directory"])
    path = os.path.join(entry["directory"], entry["file"])
    try:
        tu = cindex.Index.create().parse(path, flags(entry), options=TranslationUnit.PARSE_DETAILED_PROCESSING_RECORD)
    except cindex.TranslationUnitLoadError:
        return {"file": entry["file"], "failed": True}
    errors = [d.spelling for d in tu.diagnostics if d.severity >= cindex.Diagnostic.Error]
    defs, calls, icalls, binds, stats = set(), set(), set(), set(), collections.Counter()
    for c in tu.cursor.get_children():
        if c.location.file is None or c.kind not in (CursorKind.FUNCTION_DECL, CursorKind.VAR_DECL):
            continue
        file = rel(c.location.file.name)
        if file is None:
            continue
        binds_in(c, binds)
        if c.kind != CursorKind.FUNCTION_DECL or not c.is_definition():
            continue
        defs.add((c.get_usr(), c.spelling, file, c.location.line, c.storage_class == cindex.StorageClass.STATIC))
        calls_in(c, (file, c.spelling, c.location.line), calls, icalls, stats)
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
    return {"file": entry["file"], "errors": errors[:3], "defs": list(defs), "calls": list(calls), "icalls": list(icalls),
            "binds": list(binds), "read": list(read),
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

    defs, calls, icalls, binds, indirect, failed, with_errors = set(), set(), set(), set(), 0, [], []
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
            icalls.update(map(tuple, r["icalls"]))
            binds.update(map(tuple, r["binds"]))
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
               "defs": sorted(defs), "calls": sorted(calls, key=str), "icalls": sorted(icalls, key=str),
               "binds": sorted(binds, key=str), "files": files},
              open(args.out, "w"))
    print(f"{len(entries)} TUs, {len(failed)} failed, {len(with_errors)} with errors, "
          f"{len(defs)} definitions, {len(calls)} call sites, {indirect} indirect calls "
          f"({len(icalls)} distinct), {len(binds)} bindings")


if __name__ == "__main__":
    main()
