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
with the record and field names), a variable at file scope ("global", with
an empty record and the variable's name), a parameter, a local variable
("var"), or something else. A store into a global has an empty record too.

A call through a local variable is followed one step back, to what the
function assigns to that variable: a member or a global (an `icalls` row of
that kind, as if the call went through it directly) or a function (a store
into a slot named after the function and the variable, and a call through
it).

A call through a parameter goes through the slot `<param USR>`, member the
parameter's position; every call of that function with a function as that
argument is a store into it. `binds` are the places a function is stored in a field: by an
initializer, positional or designated, or by an assignment. Joining the two
on (record, field) gives the calls a field-based, type-aware analysis finds;
it is an upper bound on what runs, not a record of it.

C++ (a translation unit compiled as C++): a function's name is qualified
by the classes it is in, not by namespaces nor template arguments
(`DBImpl::Get`, `Writer::Rep::~Rep`); overloads share it. Methods,
constructors, destructors, operators and templates are functions. A call
row has a ninth column, its form: "call" (the callee's name is written at
the site), "virtual" (a call of a virtual method, which may run any
override), "ctor" (a constructor: `T x(a)`, `new T`, implicit ones too) or
"operator" (an overloaded operator or a conversion), "lambda" (a call of a
lambda through the variable holding it). A call into a
specialization of a template is a call of the template. Also:
  overrides [method usr, name, file, base usr, base name, base file]
  dependent_calls   calls in templates that clang cannot bind until the
                    template is instantiated (a count)

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
import shlex
import sys

from clang import cindex
from clang.cindex import CursorKind, TranslationUnit


class RangeList(ctypes.Structure):
    _fields_ = [("count", ctypes.c_uint), ("ranges", ctypes.POINTER(cindex.SourceRange))]


LIB = cindex.conf.lib
LIB.clang_getAllSkippedRanges.argtypes = [cindex.TranslationUnit]
LIB.clang_getAllSkippedRanges.restype = ctypes.POINTER(RangeList)
LIB.clang_disposeSourceRangeList.argtypes = [ctypes.POINTER(RangeList)]
LIB.clang_getSpecializedCursorTemplate.argtypes = [cindex.Cursor]
LIB.clang_getSpecializedCursorTemplate.restype = cindex.Cursor
LIB.clang_getOverriddenCursors.argtypes = [cindex.Cursor, ctypes.POINTER(ctypes.POINTER(cindex.Cursor)), ctypes.POINTER(ctypes.c_uint)]
LIB.clang_getOverriddenCursors.restype = None
LIB.clang_disposeOverriddenCursors.argtypes = [ctypes.POINTER(cindex.Cursor)]
LIB.clang_disposeOverriddenCursors.restype = None

# The Python bindings may be older than the library: a kind of cursor they
# do not know (C++20's concepts, say) is read as an expression they do not
# expose, and its children are walked.
_from_id = CursorKind.from_id.__func__


def _kind(cls, kind_id):
    try:
        return _from_id(cls, kind_id)
    except ValueError:
        return CursorKind.UNEXPOSED_EXPR


CursorKind.from_id = classmethod(_kind)

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
    # bear writes `arguments`; the kernel's gen_compile_commands.py, `command`.
    words = entry["arguments"] if "arguments" in entry else shlex.split(entry["command"])
    out, args = [], iter(words[1:])
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


def is_global(var):
    return var.semantic_parent is not None and var.semantic_parent.kind == CursorKind.TRANSLATION_UNIT


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
                return ("global" if is_global(d) else "var", "", d.spelling)
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
                f = lhs.referenced if lhs.kind in (CursorKind.MEMBER_REF_EXPR, CursorKind.DECL_REF_EXPR) else None
                # An assignment has the type of its left side; a comparison does not.
                if f is not None and c.type.spelling == kids[0].type.spelling:
                    if f.kind == CursorKind.FIELD_DECL:
                        bind(slot_of(f), kids[1], out)
                    elif f.kind == CursorKind.VAR_DECL and is_global(f):
                        bind(("", f.spelling), kids[1], out)
        stack.extend(c.get_children())


def sources_of(expr):
    """What a function pointer expression may be: (kind, record, name) for a
    member or global it is read from, ("func", cursor) for a function."""
    stack = [expr]
    while stack:
        c = stack.pop()
        if c.kind == CursorKind.MEMBER_REF_EXPR:
            f = c.referenced
            if f is not None and f.kind == CursorKind.FIELD_DECL:
                yield ("field", *slot_of(f))
        elif c.kind == CursorKind.DECL_REF_EXPR:
            d = c.referenced
            if d is not None and d.kind == CursorKind.FUNCTION_DECL:
                yield ("func", d)
            elif d is not None and d.kind == CursorKind.VAR_DECL and is_global(d):
                yield ("global", "", d.spelling)
        elif c.kind == CursorKind.CONDITIONAL_OPERATOR:
            stack.extend(list(c.get_children())[1:])
        elif c.kind in WRAPPERS:
            kids = list(c.get_children())
            if kids:
                stack.append(kids[-1] if c.kind == CursorKind.CSTYLE_CAST_EXPR else kids[0])


def local_sources(fn, var):
    """Everything `fn` assigns to its local variable `var`."""
    stack = list(fn.get_children())
    while stack:
        c = stack.pop()
        kids = list(c.get_children())
        if c.kind == CursorKind.VAR_DECL and c == var:
            for init in kids:
                if init.kind != CursorKind.TYPE_REF:
                    yield from sources_of(init)
        elif c.kind == CursorKind.BINARY_OPERATOR and len(kids) == 2:
            lhs = kids[0]
            while lhs.kind == CursorKind.PAREN_EXPR:
                lhs = next(lhs.get_children())
            if lhs.kind == CursorKind.DECL_REF_EXPR and lhs.referenced == var and c.type.spelling == kids[0].type.spelling:
                yield from sources_of(kids[1])
        stack.extend(kids)


def local_var(call):
    """The local variable an indirect call goes through, if it does."""
    kids = list(call.get_children())
    c = kids[0] if kids else None
    while c is not None and c.kind in WRAPPERS:
        kids = list(c.get_children())
        c = (kids[-1] if c.kind == CursorKind.CSTYLE_CAST_EXPR else kids[0]) if kids else None
    d = c.referenced if c is not None and c.kind == CursorKind.DECL_REF_EXPR else None
    return d if d is not None and d.kind == CursorKind.VAR_DECL and not is_global(d) else None


def calls_in(fn, caller, out, icalls, binds, stats):
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
                for i, arg in enumerate(c.get_arguments()):
                    bind((f"<param {callee.get_usr()}>", str(i)), arg, binds)
            else:
                stats["indirect"] += 1
                where = (site(c.location), c.location.line)
                target = target_of(c)
                if target[0] == "param":
                    names = [a.spelling for a in fn.get_arguments()]
                    if target[2] in names:
                        target = ("param", f"<param {fn.get_usr()}>", str(names.index(target[2])))
                icalls.add((*caller, *target, *where))
                var = local_var(c)
                for source in local_sources(fn, var) if var is not None else ():
                    if source[0] == "func":
                        slot = (f"<local {caller[0]}:{caller[1]}>", var.spelling)
                        d = source[1].get_definition()
                        home = rel(d.location.file.name) if d is not None and d.location.file else None
                        binds.add((*slot, source[1].get_usr(), source[1].spelling, home, *where))
                        icalls.add((*caller, "local", *slot, *where))
                    else:
                        icalls.add((*caller, "local", *source[1:], *where))
        stack.extend(c.get_children())


# --- C++ ---------------------------------------------------------------------
FUNCS = {CursorKind.FUNCTION_DECL, CursorKind.CXX_METHOD, CursorKind.CONSTRUCTOR, CursorKind.DESTRUCTOR,
         CursorKind.CONVERSION_FUNCTION, CursorKind.FUNCTION_TEMPLATE}
RECORDS = {CursorKind.STRUCT_DECL, CursorKind.CLASS_DECL, CursorKind.UNION_DECL, CursorKind.CLASS_TEMPLATE,
           CursorKind.CLASS_TEMPLATE_PARTIAL_SPECIALIZATION}
# Where definitions are, below the translation unit.
SCOPES = RECORDS | {CursorKind.NAMESPACE, CursorKind.LINKAGE_SPEC, CursorKind.UNEXPOSED_DECL}


def is_cpp(entry):
    words = entry["arguments"] if "arguments" in entry else shlex.split(entry["command"])
    return words[0].endswith("++") or os.path.splitext(entry["file"])[1] in (".cc", ".cpp", ".cxx", ".c++", ".C")


def primary(c):
    """The template a specialization was made from (or `c`)."""
    for _ in range(4):
        t = LIB.clang_getSpecializedCursorTemplate(c)
        if t is None or t.kind.is_invalid() or t == c:
            break
        t._tu = c._tu  # (a cursor libclang returns knows its unit only so)
        c = t
    return c


def bare(name):
    """A name without template arguments, as written where it is defined:
    clang spells a template's constructor `List<T>` (an operator keeps its
    `<`: `operator<`)."""
    return name if name.startswith("operator") else name.split("<", 1)[0]


def qualified(c):
    """`Outer::Inner::name`: the classes, not the namespaces."""
    parts, p = [bare(c.spelling)], c.semantic_parent
    while p is not None and p.kind in RECORDS:
        parts.append(bare(p.spelling) or "(anonymous)")
        p = p.semantic_parent
    return "::".join(reversed(parts))


def overridden(c):
    out, n = ctypes.POINTER(cindex.Cursor)(), ctypes.c_uint()
    LIB.clang_getOverriddenCursors(c, ctypes.byref(out), ctypes.byref(n))
    found = [out[i] for i in range(n.value)]
    for b in found:
        b._tu = c._tu
    bases = [(b.get_usr(), qualified(b), site(b.location)) for b in found]
    if n.value:
        LIB.clang_disposeOverriddenCursors(out)
    return bases


def form(callee, call):
    if callee.semantic_parent is not None and callee.semantic_parent.spelling.startswith("(lambda"):
        return "lambda"
    if callee.kind == CursorKind.CONSTRUCTOR:
        return "ctor"
    if callee.kind == CursorKind.CONVERSION_FUNCTION or (callee.spelling.startswith("operator") and not callee.spelling[8:9].isalnum()
                                                          and callee.spelling[8:9] != "_"):
        return "operator"
    if callee.kind == CursorKind.CXX_METHOD and callee.is_virtual_method():
        # `Base::f ()` so written runs that one, not an override.
        kids = list(call.get_children())
        head = kids[0] if kids else None
        return "call" if head is not None and any(t.spelling == "::" for t in head.get_tokens()) else "virtual"
    return "call"


def cpp_calls_in(fn, caller, out, icalls, binds, stats):
    stack = list(fn.get_children())
    while stack:
        c = stack.pop()
        if c.kind == CursorKind.CALL_EXPR:
            callee = c.referenced
            if callee is not None and callee.kind in FUNCS:
                callee = primary(callee)
                loc = c.location
                d = callee.get_definition()
                home = rel(d.location.file.name) if d is not None and d.location.file else None
                out.add((*caller, callee.get_usr(), qualified(callee), home, site(loc), loc.line, form(callee, c)))
                for i, arg in enumerate(c.get_arguments()):
                    bind((f"<param {callee.get_usr()}>", str(i)), arg, binds)
            else:
                target = target_of(c)
                if target == OTHER:
                    # Mostly a call in a template that depends on its
                    # parameters: bound only when instantiated.
                    kids = list(c.get_children())
                    head = kids[0].kind.name if kids else ""
                    # `T (a, b)`, `std::string (p, n)`: an object made, with
                    # no constructor libclang names (a "ctor" all the same).
                    made = not c.spelling and c.type.get_canonical().kind == cindex.TypeKind.RECORD
                    stats["dependent"].add((site(c.location), c.location.line, "construct" if made else head))
                else:
                    stats["indirect"] += 1
                    icalls.add((*caller, *target, site(c.location), c.location.line))
        stack.extend(c.get_children())


def deleted_or_defaulted(c):
    if c.kind not in (CursorKind.CXX_METHOD, CursorKind.CONSTRUCTOR, CursorKind.DESTRUCTOR):
        return False
    deleted = getattr(c, "is_deleted_method", None)
    return c.is_default_method() or (deleted is not None and deleted())


def cpp_walk(root, defs, calls, icalls, binds, overrides, stats):
    for c in root.get_children():
        if c.location.file is None:
            continue
        if c.kind in SCOPES:
            cpp_walk(c, defs, calls, icalls, binds, overrides, stats)
            continue
        file = rel(c.location.file.name)
        if file is None or c.kind not in FUNCS | {CursorKind.VAR_DECL}:
            continue
        binds_in(c, binds)
        if c.kind == CursorKind.VAR_DECL or not c.is_definition() or deleted_or_defaulted(c):
            continue
        name = qualified(c)
        defs.add((c.get_usr(), name, file, c.location.line, c.linkage == cindex.LinkageKind.INTERNAL))
        if c.kind == CursorKind.CXX_METHOD and c.is_virtual_method():
            for base in overridden(c):
                overrides.add((c.get_usr(), name, file, *base))
        cpp_calls_in(c, (file, name, c.location.line), calls, icalls, binds, stats)


def one(entry):
    os.chdir(entry["directory"])
    path = os.path.join(entry["directory"], entry["file"])
    try:
        tu = cindex.Index.create().parse(path, flags(entry), options=TranslationUnit.PARSE_DETAILED_PROCESSING_RECORD)
    except cindex.TranslationUnitLoadError:
        return {"file": entry["file"], "failed": True}
    errors = [d.spelling for d in tu.diagnostics if d.severity >= cindex.Diagnostic.Error]
    defs, calls, icalls, binds, overrides, stats = set(), set(), set(), set(), set(), collections.Counter()
    stats["dependent"] = set()
    cpp = is_cpp(entry)
    if cpp:
        cpp_walk(tu.cursor, defs, calls, icalls, binds, overrides, stats)
    for c in ([] if cpp else tu.cursor.get_children()):
        if c.location.file is None or c.kind not in (CursorKind.FUNCTION_DECL, CursorKind.VAR_DECL):
            continue
        file = rel(c.location.file.name)
        if file is None:
            continue
        binds_in(c, binds)
        if c.kind == CursorKind.VAR_DECL:
            # `handler_t hook = default_handler;` and arrays of functions.
            for init in c.get_children():
                if init.kind not in (CursorKind.TYPE_REF, CursorKind.INIT_LIST_EXPR, CursorKind.INTEGER_LITERAL):
                    bind(("", c.spelling), init, binds)
                elif init.kind == CursorKind.INIT_LIST_EXPR and init.type.get_canonical().kind != cindex.TypeKind.RECORD:
                    for element in init.get_children():
                        if element.kind != CursorKind.INIT_LIST_EXPR:
                            bind(("", c.spelling), element, binds)
        if c.kind != CursorKind.FUNCTION_DECL or not c.is_definition():
            continue
        defs.add((c.get_usr(), c.spelling, file, c.location.line, c.storage_class == cindex.StorageClass.STATIC))
        calls_in(c, (file, c.spelling, c.location.line), calls, icalls, binds, stats)
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
            "binds": list(binds), "overrides": list(overrides), "read": list(read),
            "skipped": skipped, "indirect": stats["indirect"], "dependent": list(stats["dependent"]), "cpp": cpp}


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

    defs, calls, icalls, binds, overrides, indirect, dependent, failed, with_errors = set(), set(), set(), set(), set(), 0, set(), [], []
    cpp_units = 0
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
            overrides.update(map(tuple, r["overrides"]))
            indirect += r["indirect"]
            dependent.update(map(tuple, r["dependent"]))
            cpp_units += r["cpp"]
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
               "binds": sorted(binds, key=str), "files": files,
               **({"cpp_translation_units": cpp_units, "overrides": sorted(overrides, key=str), "dependent_calls": sum(d[2] != "construct" for d in dependent),
                      "dependent_by_head": collections.Counter(d[2] for d in dependent).most_common(),
                      # Where they are: what an indexer says is called there cannot be judged.
                      "dependent_sites": sorted({(d[0], d[1]) for d in dependent if d[0] and d[2] != "construct"})}
                  if cpp_units else {})},
              open(args.out, "w"))
    print(f"{len(entries)} TUs, {len(failed)} failed, {len(with_errors)} with errors, "
          f"{len(defs)} definitions, {len(calls)} call sites, {indirect} indirect calls "
          f"({len(icalls)} distinct), {len(binds)} bindings, {len(overrides)} overrides, {sum(d[2] != 'construct' for d in dependent)} dependent calls")


if __name__ == "__main__":
    main()
