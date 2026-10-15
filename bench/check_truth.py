#!/usr/bin/env python3
"""Score an si index against clang's ground truth (bench/gt_extract.py).

The truth covers one build configuration, so:
  recall     is over everything clang saw in corpus files;
  precision  is over what si reports in files clang read, on lines that some
             translation unit actually compiled.
"""
import argparse
import bisect
import collections
import io
import json
import sqlite3
import subprocess
from pathlib import Path

# Generated at configure time from a file the corpus does have, with the same
# definitions; clang reads the generated copy.
GENERATED = {"<build>/bfd/bfd.h": "bfd/bfd-in2.h", "<build>/bfd/bfd_stdint.h": "bfd/bfd_stdint.h",
             "<build>/bfd/elf32-target.h": "bfd/elfxx-target.h", "<build>/bfd/elf64-target.h": "bfd/elfxx-target.h"}

SI = Path(__file__).resolve().parent.parent / "target" / "release" / "norn.exe"


def si_lines(cmd, index):
    """The rows `si <cmd>` prints, one at a time: the kernel's calls are
    tens of millions, more than fit in memory all at once as Python lists."""
    p = subprocess.Popen([str(SI), cmd, str(index)], stdout=subprocess.PIPE)
    for line in io.TextIOWrapper(p.stdout, encoding="utf-8", errors="replace", newline="\n"):
        yield line.rstrip("\n").split("\t")
    if p.wait():
        raise subprocess.CalledProcessError(p.returncode, [str(SI), cmd, str(index)])


def si_index(index, callers=None):
    """(files, function definitions, call rows) of an si index. `callers`:
    keep only the calls made from these files (those clang compiled; no
    other call can be scored)."""
    files, defs = set(), collections.defaultdict(list)
    for file, line, kind, decl, name in si_lines("symbols", index):
        files.add(file)
        if kind == "function" and decl == "def":
            defs[file, name].append(int(line))
    calls, indirect = [], []
    for sf, sl, cn, cf, _cl, en, ef, _el, how, decl, via, path in si_lines("calls", index):
        if callers is not None and cf not in callers:
            continue
        if path == "indirect":
            indirect.append((sf, int(sl), cn, cf, en, ef, via, decl, how))
        else:
            calls.append((sf, int(sl), cn, cf, en, ef, how, decl))
    return files, defs, calls, indirect


def cbm_index(db_path, project=None):
    """The same from a codebase-memory-mcp database, opened read-only."""
    db = sqlite3.connect(f"file:{Path(db_path).as_posix()}?mode=ro", uri=True)
    if project is None:
        project = db.execute("select project from file_hashes group by 1 order by count(*) desc").fetchone()[0]
    files = {r[0] for r in db.execute("select rel_path from file_hashes where project = ?", (project,))}
    defs = collections.defaultdict(list)
    for file, name, line in db.execute(
            "select file_path, name, start_line from nodes where project = ? and label = 'Function'", (project,)):
        defs[file, name].append(line)
    rows = db.execute("""select s.file_path, json_extract(e.properties, '$.line'), s.name, s.file_path, t.name, t.file_path
                         from edges e join nodes s on s.id = e.source_id join nodes t on t.id = e.target_id
                         where e.project = ? and e.type = 'CALLS' and s.label = 'Function' and t.label = 'Function'""",
                      (project,))
    calls = [(sf, line or 0, cn, cf, en, ef, "certain", "def") for sf, line, cn, cf, en, ef in rows]
    return files, defs, calls, []


def short_names(t):
    """The truth with every function named without its classes, for a tool
    that does not record them (`DBImpl::Get` as `Get`)."""
    last = lambda n: n.rsplit("::", 1)[-1]
    t["defs"] = [[u, last(n), *rest] for u, n, *rest in t["defs"]]
    t["calls"] = [[cf, last(cn), cl, u, last(en), *rest] for cf, cn, cl, u, en, *rest in t["calls"]]
    t["icalls"] = [[cf, last(cn), *rest] for cf, cn, *rest in t.get("icalls", [])]
    t["binds"] = [[r, m, u, last(n), *rest] for r, m, u, n, *rest in t.get("binds", [])]
    if "overrides" in t:
        t["overrides"] = [[u, last(n), f, bu, last(bn), bf] for u, n, f, bu, bn, bf in t["overrides"]]
    return t


class Truth:
    def __init__(self, path, short=False):
        t = json.load(open(path, encoding="utf-8"))
        if short:
            t = short_names(t)
        self.raw = t
        self.files = {GENERATED.get(f, f): v for f, v in t["files"].items()}
        self.defs = [(name, GENERATED.get(file, file), line) for _, name, file, line, _ in t["defs"]]
        self.homes = collections.defaultdict(set)  # usr -> files defining it
        for usr, _, file, _, _ in t["defs"]:
            self.homes[usr].add(GENERATED.get(file, file))
        self.calls = t["calls"]
        self._starts = {f: [r[0] for r in v["inactive"]] for f, v in self.files.items()}
        # C++: calls in templates that clang binds only once instantiated.
        self.dependent = {(GENERATED.get(f, f), line) for f, line in t.get("dependent_sites", [])}

    def active(self, file, line):
        """Did any translation unit compile this line?"""
        info = self.files.get(file)
        if info is None:
            return False
        i = bisect.bisect_right(self._starts[file], line) - 1
        return i < 0 or line > info["inactive"][i][1]


def score(truth_path, index, corpus, cbm=False, loaded=None, callers=None, short=False):
    """`loaded`: (files, defs, call rows, indirect rows) of some other tool, in
    the shape si_index returns; it is then scored as codebase-memory-mcp is (one
    kind of call edge, no stores). `callers`: score calls only from these
    (file, function) pairs, for a tool whose calls were sampled."""
    t = Truth(truth_path, short)
    corpus = Path(corpus)
    # files, (file, name) -> lines, call rows
    if loaded is not None:
        cbm = True
        si_files, si_defs, call_rows, indirect_rows = loaded
    else:
        # Only calls from a file clang compiled are scored.
        scored = {GENERATED.get(r[0], r[0]) for r in t.calls} | {GENERATED.get(r[0], r[0]) for r in t.raw.get("icalls", [])}
        si_files, si_defs, call_rows, indirect_rows = cbm_index(index) if cbm else si_index(index, scored)
    if callers is not None:
        call_rows = [r for r in call_rows if (r[3], r[2]) in callers]

    # --- definitions -------------------------------------------------------
    text = {}

    def written(file, line, name):
        """Is the name spelled on that line (not produced by a macro)?"""
        if file not in text:
            text[file] = (corpus / file).read_bytes().decode("latin-1").split("\n")  # not splitlines: ^L is not a line break
        return line <= len(text[file]) and name.rsplit("::", 1)[-1] in text[file][line - 1]

    want = {(file, name, line) for name, file, line in t.defs if file in si_files}
    outside = len({d for d in t.defs if d[1] not in si_files})
    hit, generated, missed = 0, [], []
    for file, name, line in want:
        if (file, name) in si_defs:
            hit += 1
        elif not written(file, line, name):
            generated.append((file, line, name))
        else:
            missed.append((file, line, name))
    truth_names = {(file, name) for file, name, _ in want}
    extra, checked = [], 0
    for (file, name), lines in si_defs.items():
        for line in lines:
            if t.active(file, line):
                checked += 1
                if (file, name) not in truth_names:
                    extra.append((file, line, name))
    defs = {"truth": len(want), "truth_outside_corpus": outside, "found": hit, "recall": hit / len(want),
            "missed_written": len(missed), "missed_macro_generated": len(generated),
            "si_in_compiled_lines": checked, "si_not_in_truth": len(extra), "precision": 1 - len(extra) / max(1, checked)}

    # --- calls -------------------------------------------------------------
    want_calls = set()       # (caller file, caller name, callee file, callee name)
    names_called = collections.defaultdict(set)  # (caller file, caller name) -> callee names, defined or not
    per_program = set()      # (caller file, caller name, callee name): which definition depends on the link
    external = 0
    forms = collections.Counter()
    by_form = collections.defaultdict(set)  # form -> edges, for the forms not in the score
    unscored = set()  # (caller file, caller name, callee name) of those forms
    for cf, cn, _cl, usr, callee_name, home, _site_file, _site_line, *form in t.calls:
        cf = GENERATED.get(cf, cf)
        if cf not in si_files or (callers is not None and (cf, cn) not in callers):
            continue
        form = form[0] if form else "call"
        forms[form] += 1
        if form not in ("call", "virtual"):
            homes = {GENERATED.get(home, home)} if home else t.homes.get(usr, set())
            if len(homes) == 1 and homes <= si_files:
                by_form[form].add((cf, cn, *homes, callee_name))
            # (A call of a constructor or operator found is no error, only
            # not scored: it is no call written by the callee's name.)
            unscored.add((cf, cn, callee_name))
            continue
        names_called[cf, cn].add(callee_name)
        homes = {GENERATED.get(home, home)} if home else t.homes.get(usr, set())
        if len(homes) > 1:
            per_program.add((cf, cn, callee_name))
        elif not homes or not homes <= si_files:
            external += 1
        else:
            want_calls.add((cf, cn, *homes, callee_name))
    want_calls = {k for k in want_calls if (k[0], k[1], k[3]) not in per_program}

    certain, any_, to_decl = set(), set(), set()
    judged = []  # certain si edges on compiled lines from functions clang knows
    for site_file, site_line, cn, cf, en, ef, how, decl in call_rows:
        key = (cf, cn, ef, en)
        if decl == "decl":
            to_decl.add(key)
            continue
        any_.add(key)
        if how == "certain":
            certain.add(key)
            if (cf, cn) in names_called and (cf, cn, en) not in per_program and (cf, cn, en) not in unscored \
                    and (site_file, int(site_line)) not in t.dependent and t.active(site_file, int(site_line)):
                judged.append(key)
    judged_keys = set(judged)
    right = judged_keys & want_calls
    # The caller does call that name, but clang bound it elsewhere: to another
    # definition in the corpus, or to nothing the build defines (libc).
    elsewhere = {(k[0], k[1], k[3]) for k in want_calls}
    wrong_file = {k for k in judged_keys - want_calls if (k[0], k[1], k[3]) in elsewhere}
    undefined = {k for k in judged_keys - want_calls - wrong_file if k[3] in names_called[k[0], k[1]]}
    phantom = judged_keys - want_calls - wrong_file - undefined
    missed_calls = want_calls - any_
    # A virtual call may run any override of the method named: the edges to
    # the overrides defined in the corpus (transitively), which a reader
    # following the call wants listed too.
    overriders = collections.defaultdict(set)  # base usr -> (file, name) of overrides
    for usr, name, file, base_usr, _bn, _bf in t.raw.get("overrides", []):
        overriders[base_usr].add((usr, GENERATED.get(file, file), name))
    want_virtual = set()
    for cf, cn, _cl, usr, _callee_name, _home, _sf, _sl, *form in t.calls:
        cf = GENERATED.get(cf, cf)
        if not form or form[0] != "virtual" or cf not in si_files:
            continue
        seen, todo = set(), [usr]
        while todo:
            for u, f, n in overriders.get(todo.pop(), ()):
                if u not in seen and f in si_files:
                    seen.add(u)
                    todo.append(u)
                    want_virtual.add((cf, cn, f, n))
    other_forms = {f: {"truth_edges": len(e), "found_any": len(e & any_), "recall_any": len(e & any_) / max(1, len(e))}
                   for f, e in sorted(by_form.items())}
    if want_virtual:
        other_forms["virtual_overrides"] = {"truth_edges": len(want_virtual), "found_any": len(want_virtual & any_),
                                            "recall_any": len(want_virtual & any_) / max(1, len(want_virtual))}
    calls = {"truth_edges": len(want_calls), "truth_call_sites_to_undefined": external,
             "truth_edges_decided_by_link": len(per_program),
             "found_certain": len(want_calls & certain), "found_any": len(want_calls & any_),
             "recall_certain": len(want_calls & certain) / max(1, len(want_calls)),
             "recall_any": len(want_calls & any_) / max(1, len(want_calls)),
             "si_certain_judged": len(judged_keys), "right": len(right),
             "wrong_definition_chosen": len(wrong_file), "callee_undefined_in_build": len(undefined),
             "no_such_call": len(phantom),
             "precision_certain": len(right) / max(1, len(judged_keys) - len(undefined)),
             "si_edges_to_prototypes": len(to_decl),
             **({"truth_call_sites_by_form": dict(forms), "other_forms": other_forms,
                 "dependent_calls_in_templates": t.raw.get("dependent_calls", 0)} if "cpp_translation_units" in t.raw else {})}
    # --- calls through struct members -----------------------------------------
    # What clang's types say: a call through member M of struct S may reach
    # any function stored in S.M anywhere. An upper bound, not a trace.
    stored = collections.defaultdict(set)  # (record, member) -> (callee file, callee name)
    for record, member, usr, fn_name, home, _sf, _sl in t.raw.get("binds", []):
        homes = {GENERATED.get(home, home)} if home else t.homes.get(usr, set())
        if len(homes) == 1 and homes <= si_files:
            stored[record, member].add((*homes, fn_name))
    want_ind, callers_ind, sites = set(), set(), collections.Counter()
    for cf, cn, _cl, how, record, member, _sf, _sl in t.raw.get("icalls", []):
        cf = GENERATED.get(cf, cf)
        if cf not in si_files:
            continue
        slot = how in ("field", "global", "local", "param")
        sites[how if not slot or stored.get((record, member)) else how + ", nothing stored"] += 1
        if slot:
            callers_ind.add((cf, cn))
            want_ind.update((cf, cn, ef, en) for ef, en in stored.get((record, member), ()))
    # Where si says a function is stored. A store in a file clang did not
    # compile cannot be judged, nor can the calls that only it explains.
    compiled_stores, stores = None, {}
    binds_file = None if cbm else Path(index) / "binds.tsv"
    if binds_file and binds_file.exists():
        compiled_stores, pairs, right_pairs = set(), set(), set()
        truth_pairs = {(member, fn_name) for _r, member, _u, fn_name, _h, _sf, _sl in t.raw.get("binds", [])}
        for line in binds_file.read_text(encoding="utf-8").splitlines():
            site_file, site_line, member, fn_name, fn_file, what = line.split("\t")
            if t.active(site_file, int(site_line)) and (fn_file, fn_name) in truth_names:
                compiled_stores.add((member, fn_file, fn_name))
                pairs.add((member, fn_name))
        right_pairs = pairs & truth_pairs
        stores = {"si_stores_in_compiled_lines": len(pairs), "right": len(right_pairs),
                  "precision": len(right_pairs) / max(1, len(pairs)),
                  "truth_stores": len(truth_pairs), "recall": len(right_pairs) / max(1, len(truth_pairs))}
    got_ind, judged_ind, judged_sure = set(), set(), set()
    if cbm:
        # It has one kind of call edge; any of them may be one of these.
        got_ind = {(cf, cn, ef, en) for _sf, _sl, cn, cf, en, ef, _how, _decl in call_rows}
    for site_file, site_line, cn, cf, en, ef, via, decl, how in indirect_rows:
        if decl == "decl":
            continue
        got_ind.add((cf, cn, ef, en))
        # Only a callee clang compiled can be told right from wrong.
        if (cf, cn) in callers_ind and (ef, en) in truth_names and t.active(site_file, site_line):
            if compiled_stores is None or via == "*" or (via, ef, en) in compiled_stores:
                judged_ind.add((cf, cn, ef, en))
                if how == "certain":
                    judged_sure.add((cf, cn, ef, en))
    indirect = {"truth_call_sites": dict(sites), "truth_edges": len(want_ind), "found": len(want_ind & got_ind),
                "recall": len(want_ind & got_ind) / max(1, len(want_ind)),
                "si_edges": len(got_ind), "si_judged": len(judged_ind), "right": len(judged_ind & want_ind),
                "precision": len(judged_ind & want_ind) / max(1, len(judged_ind)),
                # Those si is sure of: one member of that name, one function.
                "si_judged_certain": len(judged_sure), "right_certain": len(judged_sure & want_ind),
                "precision_certain": len(judged_sure & want_ind) / max(1, len(judged_sure)), "stores": stores}

    detail = {"missed_indirect": sorted(want_ind - got_ind), "wrong_indirect": sorted(judged_ind - want_ind),
              "missed_defs": sorted(missed), "generated_defs": sorted(generated), "extra_defs": sorted(extra),
              "missed_calls": sorted(missed_calls), "wrong_file": sorted(wrong_file), "undefined": sorted(undefined), "phantom": sorted(phantom),
              "names_called": names_called, "want_calls": want_calls, "any": any_, "certain": certain}
    return {"translation_units": t.raw["translation_units"], "indirect_calls": t.raw["indirect_calls"],
            "definitions": defs, "calls": calls, "indirect_calls_by_member": indirect}, detail


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("truth")
    ap.add_argument("index")
    ap.add_argument("corpus")
    ap.add_argument("--cbm", action="store_true", help="INDEX is a codebase-memory-mcp database")
    ap.add_argument("--show", type=int, default=0, help="print this many examples of each kind of error")
    args = ap.parse_args()
    res, detail = score(args.truth, args.index, args.corpus, args.cbm)
    print(json.dumps(res, indent=2))
    if args.show:
        for k in ("missed_defs", "extra_defs", "generated_defs", "missed_calls", "wrong_file", "undefined", "phantom",
                  "missed_indirect", "wrong_indirect"):
            by_file = collections.Counter(x[0] for x in detail[k])
            print(f"\n{k}: {len(detail[k])}; top files {by_file.most_common(6)}")
            step = max(1, len(detail[k]) // args.show)
            for x in detail[k][::step][:args.show]:
                print("   ", *x)


if __name__ == "__main__":
    main()
