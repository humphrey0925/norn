#!/usr/bin/env python3
"""si against other C indexers on one corpus: cost, queries, accuracy.

    python bench/compare.py --corpus redis --tools si,cbm,clangd,ctags,cscope,gtags \
        --out out/compare/redis.json

Tools run one after another. si, codebase-memory-mcp (cbm) and clangd run on
Windows; universal-ctags, cscope and GNU global run inside WSL on a copy of the
corpus on its ext4 filesystem. cbm and clangd, which are also measured after an
edit, work on a second copy next to the output (never on the corpus itself);
si reads the corpus in place. A tool that fails or runs out of time is
recorded as failed, with the reason and whatever it had measured.

Accuracy is bench/check_truth.py's, against clang's own view of a real build
(bench/gt_extract.py), for the corpora that have one.
"""
import argparse
import collections
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time
import traceback
from pathlib import Path

import psutil

import check_truth
import compare_clangd
import run

ROOT = Path(__file__).resolve().parent.parent
TRUTH = {"c-only": "truth.json", "git-c": "truth-git.json", "redis": "truth-redis-src.json", "linux": "truth-linux.json",
         "leveldb": "truth-leveldb.json", "fmt": "truth-fmt.json", "bitcoin": "truth-bitcoin.json",
         "stm32f4": "truth-stm32f4.json", "nrfx": "truth-nrfx.json",
         "nrfx-corpus": "truth-nrfx.json"}
WSL_TOOLS = ("ctags", "cscope", "gtags")
ALL_TOOLS = ("si", "cbm", "clangd", "clangd-db", *WSL_TOOLS)
SKIP_DIRS = (".git", ".si", ".norn")
# Where the Windows tools run, as the results say it.
HOST = "Windows, NTFS" if os.name == "nt" else "Linux (WSL2), ext4"
EDIT = b"\n/* bench edit */\n"


# C and C++ sources and headers.
SOURCE_SUFFIXES = (".c", ".h", ".cc", ".cpp", ".cxx", ".c++", ".hh", ".hpp", ".hxx", ".h++", ".ipp", ".inl", ".tcc")
UNIT_SUFFIXES = (".c", ".cc", ".cpp", ".cxx", ".c++")


def main_checkout():
    """The repository this is a worktree of (binaries and corpora live there)."""
    git = ROOT / ".git"
    if git.is_file():
        gitdir = Path(git.read_text(encoding="utf-8").split(":", 1)[1].strip())
        return gitdir.parents[2]
    return ROOT


def locate(rel):
    for base in (ROOT, main_checkout()):
        if (base / rel).exists():
            return base / rel
    return ROOT / rel


def wsl_path(path):
    p = Path(path).resolve()
    return f"/mnt/{p.drive[0].lower()}{p.as_posix()[2:]}"


def dir_bytes(path):
    return sum(f.stat().st_size for f in Path(path).rglob("*") if f.is_file())


def sha(text):
    return hashlib.sha1(text.encode()).hexdigest()


def first_line(cmd):
    try:
        r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL)
        return ((r.stdout + r.stderr).strip().splitlines() or ["?"])[0]
    except (OSError, subprocess.SubprocessError) as e:
        return f"? ({e})"


def exe_id(path):
    st = Path(path).stat()
    return {"path": str(path), "bytes": st.st_size, "modified": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(st.st_mtime)),
            "sha1": hashlib.sha1(Path(path).read_bytes()).hexdigest()[:12]}


class Edited:
    """One file of a copy with a comment appended, put back on exit."""

    def __init__(self, path, appended=EDIT):
        self.path, self.appended = Path(path), appended

    def __enter__(self):
        self.original = self.path.read_bytes()
        self.path.write_bytes(self.original + self.appended)

    def __exit__(self, *exc):
        self.path.write_bytes(self.original)


class Ctx:
    """What every tool needs to know about this run."""

    def __init__(self, args):
        self.args = args
        self.corpus = Path(args.corpus)
        if not self.corpus.is_dir():
            self.corpus = locate(Path("corpus") / args.corpus)
        self.corpus = self.corpus.resolve()
        self.name = self.corpus.name
        self.work = (Path(args.work) if args.work else Path(args.out).resolve().parent / "work" / self.name).resolve()
        if self.corpus in self.work.parents or self.work == self.corpus:
            raise SystemExit("the work directory must not be inside the corpus")
        self.work.mkdir(parents=True, exist_ok=True)
        self.copy = self.work / "src"
        truth = Path(args.truth) if args.truth else locate(Path("out") / "gt" / TRUTH.get(self.name, "-"))
        self.truth = truth if truth.is_file() else None
        self.files = []
        for d, dirs, fs in os.walk(self.corpus):
            dirs[:] = sorted(x for x in dirs if x not in SKIP_DIRS)
            self.files += [(Path(d) / f).relative_to(self.corpus).as_posix() for f in fs if f.endswith(SOURCE_SUFFIXES)]
        self.files.sort()
        self.c_files = [f for f in self.files if f.endswith(UNIT_SUFFIXES)]
        # The file every tool sees edited: the .c file of median size.
        by_size = sorted(self.c_files, key=lambda f: ((self.corpus / f).stat().st_size, f))
        self.edit_file = args.edit_file or by_size[len(by_size) // 2]
        self.names = None
        self.deadline = 0
        self._copied = False

    def left(self):
        left = self.deadline - time.perf_counter()
        if left <= 0:
            raise TimeoutError(f"timed out after {self.args.timeout:.0f}s")
        return left

    def ensure_copy(self):
        """Mirror the corpus (without .git and .norn) for the tools that are
        measured after an edit. Not timed."""
        if self._copied:
            return
        if os.name != "nt":
            # (On Linux, to compare there what runs on both.)
            self.copy.mkdir(parents=True, exist_ok=True)
            excludes = [x for d in SKIP_DIRS for x in ("--exclude", d)]
            subprocess.run(["rsync", "-a", "--delete", *excludes, f"{self.corpus}/", f"{self.copy}/"], check=True)
            self._copied = True
            return
        r = subprocess.run(["robocopy", str(self.corpus), str(self.copy), "/MIR", "/XD", *SKIP_DIRS, "/MT:16",
                            "/NFL", "/NDL", "/NJH", "/NJS", "/NP", "/R:1", "/W:1"], capture_output=True, text=True)
        if r.returncode >= 8:
            raise RuntimeError(f"robocopy failed ({r.returncode}): {r.stdout[-300:]}")
        self._copied = True


def query_names(ctx, si_index_dir):
    """A fixed list of functions to ask every tool about: each defined once,
    in a .c file, with its name on the line of the definition."""
    n = ctx.args.names
    if ctx.truth:
        t = json.loads(ctx.truth.read_text(encoding="utf-8"))
        count = collections.Counter(d[1] for d in t["defs"])
        called = {c[4] for c in t["calls"]}
        text = {}
        out = []
        for _usr, name, file, line, *_ in sorted(t["defs"], key=lambda d: sha(d[1])):
            if count[name] != 1 or name not in called or not file.endswith(UNIT_SUFFIXES) or not (ctx.corpus / file).is_file():
                continue
            if file not in text:
                text[file] = (ctx.corpus / file).read_bytes().decode("latin-1").split("\n")
            if line <= len(text[file]) and name.rsplit("::", 1)[-1] in text[file][line - 1]:
                out.append({"name": name, "file": file, "line": line})
            if len(out) == n:
                break
        return out, f"the {n} functions of lowest SHA-1(name) among those clang saw defined once, in a .c file, and called"
    # No ground truth: from si's own index.
    sampled = json.loads(subprocess.run([str(run.SI), "sample", str(si_index_dir), "--n", str(n * 20), "--seed", "1"],
                                        capture_output=True, check=True).stdout)
    srv = run.SiServer(si_index_dir)
    out = []
    for name in dict.fromkeys(sampled):
        defs = [x for x in srv.ask("find", name)["results"] if x["kind"] == "function" and not x["decl"]]
        if len(defs) == 1 and defs[0]["file"].endswith(".c"):
            out.append({"name": name, "file": defs[0]["file"], "line": defs[0]["line"]})
        if len(out) == n:
            break
    srv.close()
    return out, f"{n} functions from `si sample --seed 1` that si finds defined once, in a .c file (no ground truth here)"


def median_run(runs):
    """Of repeated cold indexes, the one of median wall time, with all walls."""
    mid = sorted(runs, key=lambda r: r["wall_s"])[len(runs) // 2]
    return {**mid, "wall_s_runs": [r["wall_s"] for r in runs]}


def accuracy(ctx, score, notes=()):
    out = {"definitions": score["definitions"], "calls": score["calls"]}
    if notes:
        out["notes"] = list(notes)
    return out


NO_TRUTH = {"available": False, "note": "no clang ground truth for this corpus"}


# ---------------------------------------------------------------- si

def tool_si(ctx, res):
    si, out = run.SI, ctx.work / "si"
    res["version"] = exe_id(si)  # it has no --version: the binary is identified by its hash
    res["platform"] = f"{HOST}, corpus in place"
    cmd = [si, "index", ctx.corpus, "--out", out, "--stores", "csr"]
    res["command"] = " ".join(str(c) for c in cmd)
    runs = []
    for _ in range(ctx.args.index_reps):
        shutil.rmtree(out, ignore_errors=True)
        run.TIMEOUT_S = ctx.left()
        wall, peak, text = run.measure(cmd)
        js = json.loads(text)
        runs.append({"wall_s": wall, "peak_rss_bytes": peak, "disk_bytes": dir_bytes(out),
                     "self_reported_total_ms": js["ms"]["total"], "report": js})
    res["index"] = median_run(runs)
    res["counts"] = {k: res["index"]["report"][k] for k in ("files", "lines", "symbols", "edges")}

    run.TIMEOUT_S = ctx.left()
    js = json.loads(run.measure([si, "bench-incr", ctx.corpus, "--n", ctx.args.incr_n])[2])
    res["incremental"] = {
        "supported": True, "wall_s": js["total"]["p50_ns"] / 1e9, "n": js["total"]["n"],
        "reextract_one_file_p50_ms": js["reextract_one_file"]["p50_ns"] / 1e6,
        "relink_p50_ms": js["full_relink"]["p50_ns"] / 1e6, "report": js,
        "note": f"`si bench-incr --n {ctx.args.incr_n}`: median over {js['total']['n']} files it picks itself, of "
                "re-extracting that file and relinking, timed inside a process that already holds the index; "
                "no process start, no change detection, nothing written to disk. Not the same file as the other tools edit."}

    names = [n["name"] for n in ctx.names]
    srv = run.SiServer(out)
    warm = {"find": [], "callers": []}
    answered = collections.Counter()
    srv.ask("find", names[0])
    for rep in range(ctx.args.reps):
        for name in names:
            for op in warm:
                t0 = time.perf_counter()
                r = srv.ask(op, name)
                warm[op].append(time.perf_counter() - t0)
                answered[op] += rep == 0 and bool(r["results"])
    srv.close()
    cold = {"find": [], "callers": []}
    for rep in range(ctx.args.reps):
        for name in names:
            for op in cold:
                t0 = time.perf_counter()
                subprocess.run([str(si), "query", str(out), op, name], capture_output=True, timeout=ctx.left())
                cold[op].append(time.perf_counter() - t0)
    res["query"] = {
        "mode": "warm server over a pipe (`si serve`)",
        "find": {**run.latency(warm["find"]), "answered": answered["find"], "names": len(names)},
        "callers": {**run.latency(warm["callers"]), "answered": answered["callers"], "names": len(names)},
        "one_shot": {"mode": "one process per query (`si query`): process start and loading the index included",
                     "find": run.latency(cold["find"]), "callers": run.latency(cold["callers"])}}

    if not ctx.truth:
        res["accuracy"] = NO_TRUTH
        return
    # binds.tsv (where functions are stored in members) is only for the scorer.
    scored = ctx.work / "si-scored"
    shutil.rmtree(scored, ignore_errors=True)
    run.TIMEOUT_S = ctx.left()
    run.measure([si, "index", ctx.corpus, "--out", scored, "--stores", "csr", "--dump-binds"])
    score = check_truth.score(ctx.truth, scored, ctx.corpus)[0]
    res["accuracy"] = {**accuracy(ctx, score), "indirect_calls_by_member": score["indirect_calls_by_member"]}


# ---------------------------------------------------------------- codebase-memory-mcp

def tool_cbm(ctx, res):
    res["version"] = {"reported": first_line([run.CBM, "--version"]), **exe_id(run.CBM)}
    res["platform"] = f"{HOST}, copy of the corpus without .git and .norn"
    ctx.ensure_copy()
    mode = ctx.args.cbm_mode
    res["command"] = f"codebase-memory-mcp cli --quiet --json index_repository --repo-path <copy> --mode {mode}"
    run.CBM_CACHE.mkdir(parents=True, exist_ok=True)
    runs = []
    for _ in range(ctx.args.index_reps):
        run.cbm_reset(ctx.copy)
        run.TIMEOUT_S = ctx.left()
        r = run.cbm_index(ctx.copy, mode)
        r["disk_bytes"] = sum(f.stat().st_size for f in run.CBM_CACHE.glob(r["project"] + ".db*"))
        runs.append(r)
    res["index"] = {**median_run(runs), "note": "peak memory sums every process named codebase-memory-mcp.exe: "
                    "the cli starts a detached daemon and workers"}
    project = res["index"]["project"]
    res["counts"] = {"nodes": res["index"]["nodes"], "edges": res["index"]["edges"]}

    run.TIMEOUT_S = ctx.left()
    same = run.cbm_index(ctx.copy, mode)
    with Edited(ctx.copy / ctx.edit_file):
        run.TIMEOUT_S = ctx.left()
        after = run.cbm_index(ctx.copy, mode)
    run.TIMEOUT_S = ctx.left()
    run.cbm_index(ctx.copy, mode)  # back in step with the restored file
    res["incremental"] = {"supported": True, "wall_s": after["wall_s"], "peak_rss_bytes": after["peak_rss_bytes"],
                          "edited": ctx.edit_file, "no_change_wall_s": same["wall_s"],
                          "one_shot_startup_s": run.cbm_startup_s(),
                          "note": "the same index command again after one file changed; it finds the change itself. "
                                  "Includes starting the cli and its daemon (one_shot_startup_s)."}

    mcp = run.McpClient()
    try:
        base = {"project": project, "format": "json"}
        trace = {**base, "limit": 1000, "max_output_tokens": 200000, "depth": 1, "direction": "inbound"}
        ops = {"find": lambda nm: mcp.call("search_graph", {**base, "name_pattern": f"^{nm}$"}),
               "callers": lambda nm: run.cbm_callers(mcp.call("trace_path", {**trace, "function_name": nm}))}
        t = {k: [] for k in ops}
        answered = collections.Counter()
        ops["find"](ctx.names[0]["name"])
        for rep in range(ctx.args.reps):
            for n in ctx.names:
                for op, q in ops.items():
                    ctx.left()
                    t0 = time.perf_counter()
                    r = q(n["name"])
                    t[op].append(time.perf_counter() - t0)
                    if rep == 0:
                        answered[op] += bool(r) if op == "callers" else n["name"] in json.dumps(r)
    finally:
        mcp.close()
    res["query"] = {"mode": "warm MCP server over a pipe (search_graph, trace_path inbound depth 1)",
                    **{op: {**run.latency(v), "answered": answered[op], "names": len(ctx.names)} for op, v in t.items()}}

    if not ctx.truth:
        res["accuracy"] = NO_TRUTH
        return
    # (Its functions are named without their classes, as cscope's are.)
    score = check_truth.score(ctx.truth, run.CBM_CACHE / f"{project}.db", ctx.corpus, cbm=True, short=True)[0]
    res["accuracy"] = {**accuracy(ctx, score), "indirect_calls_by_member": score["indirect_calls_by_member"]}


# ---------------------------------------------------------------- clangd

def tool_clangd(ctx, res, real_db=None):
    """clangd's background index. With `real_db` (a build's own
    compile_commands.json, --clangd-db), on the tree that build was made
    in, in place: what clangd does at its best. Without, on a copy, with a
    database of nothing but the files: what a project without one gives it."""
    exe = ctx.args.clangd
    res["version"] = first_line([exe, "--version"])
    res["platform"] = f"{HOST}, " + ("the built tree in place" if real_db else "copy of the corpus without .git and .norn")
    if not real_db:
        ctx.ensure_copy()
    root = ctx.corpus if real_db else ctx.copy
    db = ctx.work / "clangd"
    index_dir = db / ".cache" / "clangd" / "index"
    log = ctx.work / "clangd.log"
    flags = ctx.args.clangd_flags.split()
    res["command"] = (f"clangd --background-index --background-index-priority=normal --compile-commands-dir=<dir outside "
                      f"the sources>; database: " + (f"the build's own, {real_db}" if real_db else
                                                     f"`clang {' '.join(flags)} -c <file>` for each of {len(ctx.c_files)} .c files"))
    res["notes"] = [
        "The compilation database is the build's own (the one the ground truth was made from): every file's include "
        "paths, definitions and target, on the tree that build was made in.",
        "Timing ends when $/progress for backgroundIndexProgress ends and stays ended for "
        f"{compare_clangd.QUIET_S:.0f}s; it starts at process start and includes opening one file to trigger it."] if real_db else [
        "The compilation database is synthetic: no include paths and no -D. Headers a file includes by a path "
        "relative to itself are found, others are not, and every #if is decided without the project's configuration. "
        "A real build's database would make clangd both slower (more headers per file) and more accurate.",
        "Headers are indexed only through the .c files that include them.",
        "With no target given clangd compiles for this Windows host and reads its system headers (so <unistd.h> is "
        "missing and errno is a call to _errno), while the ground truth is a Linux build.",
        "Timing ends when $/progress for backgroundIndexProgress ends and stays ended for "
        f"{compare_clangd.QUIET_S:.0f}s; it starts at process start and includes opening one file to trigger it."]
    deadline = lambda: time.perf_counter() + ctx.left()

    runs = []
    client = None
    for _ in range(ctx.args.index_reps):
        if client:
            client.stop()
        shutil.rmtree(db, ignore_errors=True)
        if real_db:
            db.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(real_db, db / "compile_commands.json")
        else:
            compare_clangd.write_database(db, root, ctx.c_files, flags)
        client, wall = compare_clangd.index(exe, db, root, log, deadline(), ctx.c_files[0])
        if wall is None:
            client.stop()
            raise RuntimeError("clangd never reported background indexing")
        runs.append({"wall_s": wall, "peak_rss_bytes": client.peak, "disk_bytes": dir_bytes(index_dir),
                     "index_files": sum(1 for _ in index_dir.glob("*.idx"))})
    try:
        res["index"] = {**median_run(runs), "index_dir": str(index_dir),
                        "note": "peak memory sampled every 20 ms until indexing ended"}
        stray = [p for p in (root / ".cache", root / "compile_commands.json", ctx.corpus / ".cache") if p.exists()]
        res["index"]["wrote_into_sources"] = [str(p) for p in stray]

        find, callers, a_find, a_callers = compare_clangd.time_queries(client, ctx.names, ctx.args.reps)
        res["query"] = {
            "mode": "warm language server over a pipe",
            "find": {**run.latency(find), "answered": a_find, "names": len(ctx.names), "note": "workspace/symbol"},
            "callers": {**run.latency(callers), "answered": a_callers, "names": len(ctx.names),
                        "note": "prepareCallHierarchy + callHierarchy/incomingCalls at the definition, in a file "
                                "already open and parsed (opening it is not timed)"}}

        if ctx.truth:
            res["accuracy"] = clangd_accuracy(ctx, client, deadline())
        else:
            res["accuracy"] = NO_TRUTH
    finally:
        client.stop()

    # Incremental: what a restart costs with the index on disk, unchanged and
    # after one file was edited (clangd re-indexes what is stale as it loads).
    client, same = compare_clangd.index(exe, db, root, log, deadline(), ctx.c_files[0])
    client.stop()
    walls, peak, rewritten = [], 0, []
    for i in range(3):
        # A different edit each time: the index on disk now knows the last one.
        with Edited(root / ctx.edit_file, EDIT * (i + 1)):
            edited_at = time.time()
            client, after = compare_clangd.index(exe, db, root, log, deadline(), ctx.c_files[0])
            peak = max(peak, client.peak)
            client.stop()
        walls.append(after)
        rewritten.append(sum(f.stat().st_mtime >= edited_at for f in index_dir.glob("*.idx")))
    client, _ = compare_clangd.index(exe, db, root, log, deadline(), ctx.c_files[0])  # back in step with the file
    client.stop()
    quiet = f"no indexing reported within {compare_clangd.NO_WORK_S:.0f}s"
    done = [w for w in walls if w is not None]
    res["incremental"] = {
        "supported": bool(done), "wall_s": statistics.median(done) if done else None, "wall_s_runs": walls,
        "peak_rss_bytes": peak, "edited": ctx.edit_file,
        "no_change_wall_s": same if same is not None else quiet, "index_files_rewritten": rewritten,
        "note": "restart of clangd with its index on disk after one .c file changed, from process start to the end of "
                "background indexing: loading every shard, checking each file for staleness, re-indexing the stale one. "
                "Median of three edits. An editor that keeps clangd running pays less: only the re-index on save. "
                f"A run listed as null: {quiet}."}


def tool_clangd_db(ctx, res):
    if not ctx.args.clangd_db:
        raise RuntimeError("clangd-db needs --clangd-db <the build's compile_commands.json>")
    tool_clangd(ctx, res, real_db=ctx.args.clangd_db)


def clangd_accuracy(ctx, client, deadline):
    # (Its call hierarchy names a method without its class, as cscope does.)
    t = check_truth.Truth(ctx.truth, short=True)
    n = ctx.args.sample_fraction
    text = {}

    def written(file, line, name):
        if file not in text:
            text[file] = (ctx.corpus / file).read_bytes().decode("latin-1").split("\n")
        return line <= len(text[file]) and name in text[file][line - 1]

    pool = [(file, name, line) for name, file, line in t.defs
            if file.endswith(UNIT_SUFFIXES) and (ctx.corpus / file).is_file() and written(file, line, name)]
    functions = compare_clangd.sample(pool, n)
    t0 = time.perf_counter()
    rows, unresolved, found_defs = compare_clangd.call_edges(client, functions, deadline)
    callers = {(f, name) for f, name, _ in functions}
    # A callee outside the sources (a system header of this machine) is no
    # edge of the project's graph; the other tools cannot report one either.
    files = set(ctx.files)
    outside = sum(r[5] not in files for r in rows)
    rows = [r for r in rows if r[5] in files]
    (ctx.work / "clangd-edges.json").write_text(json.dumps({"functions": functions, "rows": rows, "unresolved": unresolved}),
                                                encoding="utf-8")
    score, detail = check_truth.score(ctx.truth, None, ctx.corpus, loaded=(files, {}, rows, []), callers=callers, short=True)
    # clangd keeps one symbol for all external functions of one name (every
    # program's cmd_main, say) and answers with the calls of all of them.
    times_defined = collections.Counter(name for name, _file, _line in t.defs)
    merged = sum(times_defined[k[1]] > 1 for k in detail["phantom"])
    how = (f"sampled: {len(functions)} functions, {n:.0%} of the {len(pool)} that clang saw, drawn by lowest SHA-1(file, name), "
           "among those "
           "defined in a source file (not a header) with the name written on the line")
    return {
        "definitions": {"truth": len(functions), "found": len(found_defs), "recall": len(found_defs) / max(1, len(functions)),
                        "precision": None,
                        "note": how + "; found = workspace/symbol returns a function of that name in that file. "
                                      "Precision not measured: clangd has no listing of everything it indexed."},
        "calls": {**score["calls"], "sampled_callers": len(functions), "callers_clangd_did_not_find": len(unresolved),
                  "edges_to_files_outside_the_sources_dropped": outside,
                  "no_such_call_from_callers_defined_more_than_once": merged,
                  "note": how + "; edges are callHierarchy/outgoingCalls of each, all counted as certain. Recall and "
                                "precision are over edges from these callers only. outgoingCalls also lists functions "
                                "whose address is taken without a call, which count against precision here, and for a "
                                "function name that several programs define it lists the calls of all of them "
                                "(no_such_call_from_callers_defined_more_than_once).",
                  "seconds": time.perf_counter() - t0}}


# ---------------------------------------------------------------- ctags, cscope, global (WSL)

def wsl(ctx, *cmd, timeout=None):
    return subprocess.run(["wsl", "-d", ctx.args.wsl_distro, "--cd", "~", "--", *cmd], capture_output=True, text=True,
                          encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL, timeout=timeout)


def wsl_sync(ctx):
    """Copy the corpus onto WSL's own filesystem (not timed)."""
    if getattr(ctx, "_synced", False):
        return
    dst = f"{ctx.args.wsl_dir}/{ctx.name}"
    wsl(ctx, "mkdir", "-p", dst)
    r = wsl(ctx, "rsync", "-a", "--delete", *[x for d in SKIP_DIRS for x in ("--exclude", d)],
            wsl_path(ctx.corpus) + "/", dst + "/")
    if r.returncode != 0:
        raise RuntimeError(f"rsync into WSL failed: {r.stderr[-300:]}")
    ctx._synced = True


def wsl_loaded(dump):
    """A WSL tool's dump in the shape check_truth scores."""
    defs = collections.defaultdict(list)
    homes = collections.defaultdict(set)
    for file, name, line in dump["defs"]:
        defs[file, name].append(line)
        homes[name].add(file)
    rows = []
    for file, line, caller, callee in dump["calls"] or ():
        for home in homes.get(callee, ()):
            rows.append((file, line, caller, file, callee, home, "certain" if len(homes[callee]) == 1 else "possible", "def"))
    return set(dump["files"]), defs, rows, []


def tool_wsl(tool):
    def go(ctx, res):
        res["platform"] = "WSL2 (Linux), ext4, copy of the corpus without .git and .norn"
        wsl_sync(ctx)
        names = ctx.work / "names.json"
        names.write_text(json.dumps(ctx.names), encoding="utf-8")
        out, dump = ctx.work / f"{tool}.json", ctx.work / f"{tool}-dump.json"
        for f in (out, dump):
            f.unlink(missing_ok=True)
        left = ctx.left()
        try:
            r = wsl(ctx, "python3", wsl_path(ROOT / "bench" / "compare_wsl.py"), "--tool", tool,
                    "--src", f"{ctx.args.wsl_dir}/{ctx.name}", "--work", f"{ctx.args.wsl_dir}/idx/{ctx.name}/{tool}",
                    "--names", wsl_path(names), "--edit-file", ctx.edit_file, "--out", wsl_path(out),
                    "--dump", wsl_path(dump), "--reps", str(ctx.args.reps), "--index-reps", str(ctx.args.index_reps),
                    "--timeout", str(left), *([] if ctx.truth else ["--no-dump"]), timeout=left + 120)
            stderr = r.stderr
        except subprocess.TimeoutExpired:
            wsl(ctx, "pkill", "-f", "compare_wsl.py")
            stderr = "killed"
        if out.exists():
            res.update(json.loads(out.read_text(encoding="utf-8")))
        if not out.exists() or res["status"] != "ok":
            raise RuntimeError(res.pop("reason", None) or f"compare_wsl.py left no result: {stderr[-400:]}")

        if not ctx.truth:
            res["accuracy"] = NO_TRUTH
            return
        d = json.loads(dump.read_text(encoding="utf-8"))
        # cscope and global do not say which class a function is in: they
        # are scored on unqualified names (`Get` for `DBImpl::Get`).
        short = tool in ("cscope", "gtags")
        score = check_truth.score(ctx.truth, None, ctx.corpus, loaded=wsl_loaded(d), short=short)[0]
        if short and score["calls"].get("truth_call_sites_by_form"):
            score["note"] = "scored on unqualified names: the tool does not record which class a function is in"
        acc = accuracy(ctx, score)
        if not d["typed"]:
            acc["definitions"].update({"precision": None, "si_in_compiled_lines": None, "si_not_in_truth": None})
            acc["definitions"]["note"] = ("global does not say what kind of symbol a definition is: recall counts a "
                                          "function as found if anything of that name is defined in that file; "
                                          "precision of 'function definitions' cannot be stated")
        if d["calls"] is None:
            acc["calls"] = None
            acc["calls_note"] = {"ctags": "n/a: ctags records definitions only",
                                 "gtags": "n/a: global records references to a name (global -rx), not which function "
                                          "they are in"}[tool]
        else:
            acc["calls"]["note"] = ("cscope records the name a function calls, not which definition: each call is "
                                    "taken as an edge to every function definition of that name in its database, "
                                    "'certain' (and judged for precision) when there is exactly one, as check_truth "
                                    "treats si's ambiguous edges. Complete, not sampled.")
        res["accuracy"] = acc
    return go


TOOLS = {"si": tool_si, "cbm": tool_cbm, "clangd": tool_clangd, "clangd-db": tool_clangd_db, **{t: tool_wsl(t) for t in WSL_TOOLS}}

NOTES = [
    "si, cbm and clangd ran on Windows/NTFS; ctags, cscope and global ran in WSL2 on ext4, on a copy of the same files. "
    "Process start and file access are cheaper on Linux, which favours the WSL tools, most of all in one-process-per-query latency.",
    "ctags, cscope and global were given exactly the corpus's *.c and *.h files; si and cbm choose what to read themselves "
    "(si also reads other C-bearing files such as .def; cbm reads every language it knows).",
    "ctags, cscope and global are single-threaded; si, cbm and clangd use all cores.",
    "'Cold' index means no index on disk; the source files are in the operating system's file cache for every tool.",
    "Peak memory: Windows tools, summed RSS of the process tree sampled every 20 ms; WSL tools, the larger of the same "
    "sampling over /proc and the kernel's high-water mark of the largest single process.",
    "Query latency is over each of the fixed names repeated --reps times, so p99 of a few dozen samples is close to the maximum. "
    "'answered' is how many names got a non-empty answer, not whether it was right.",
    "Query modes differ by tool (see each tool's query.mode): a warm server and a process per query are not the same cost. "
    "si reports both.",
    "Incremental modes differ by tool (see each incremental.note): si's figure is in-process and is told which file changed; "
    "cbm, cscope and `gtags -i` include process start and finding the change; clangd's is a restart.",
    "Accuracy is check_truth.py's for every tool: definitions are function definitions matched by (file, name); a call edge is "
    "(caller file, caller name, callee file, callee name); recall is over what clang saw in files the tool read; precision "
    "is over what the tool reports on lines some translation unit compiled, for callers clang knows.",
]


def machine(ctx, wsl_used):
    m = {"cpu": platform.processor(), "physical_cores": psutil.cpu_count(logical=False),
         "logical_cores": psutil.cpu_count(), "ram_bytes": psutil.virtual_memory().total, "os": platform.platform(),
         "python": platform.python_version(), "cpu_percent_before_start": psutil.cpu_percent(interval=1.0)}
    if wsl_used:
        r = wsl(ctx, "sh", "-c", "uname -r; nproc; grep MemTotal /proc/meminfo; grep ^PRETTY_NAME= /etc/os-release; "
                                 "df --output=fstype . | tail -1", timeout=120)
        lines = r.stdout.strip().splitlines()
        if len(lines) >= 5:
            m["wsl"] = {"kernel": lines[0], "cores": int(lines[1]), "ram_bytes": int(lines[2].split()[1]) * 1024,
                        "distro": lines[3].split("=", 1)[1].strip('"'), "filesystem": lines[4].strip()}
        else:
            m["wsl"] = {"error": (r.stdout + r.stderr)[-300:]}
    return m


def summary(res):
    def f(v, spec, scale=1.0):
        return format(v * scale, spec) if isinstance(v, (int, float)) else "-".rjust(int(spec.split(".")[0]))

    print(f"\n{'tool':8s} {'index s':>8s} {'peak MB':>8s} {'disk MB':>8s} {'incr s':>8s} {'find ms':>8s} {'callers':>8s}"
          f" {'def R':>7s} {'def P':>7s} {'call R':>7s} {'certain':>7s} {'call P':>7s}")
    for name, t in res["tools"].items():
        if "skipped" in t:
            continue
        ix, inc, q, acc = t.get("index", {}), t.get("incremental", {}), t.get("query", {}), t.get("accuracy", {})
        d, c = acc.get("definitions") or {}, acc.get("calls") or {}
        print(f"{name:8s} {f(ix.get('wall_s'), '8.2f')} {f(ix.get('peak_rss_bytes'), '8.0f', 2**-20)}"
              f" {f(ix.get('disk_bytes'), '8.1f', 2**-20)} {f(inc.get('wall_s'), '8.3f')}"
              f" {f((q.get('find') or {}).get('p50_ms'), '8.2f')} {f((q.get('callers') or {}).get('p50_ms'), '8.2f')}"
              f" {f(d.get('recall'), '7.2%')} {f(d.get('precision'), '7.2%')}"
              f" {f(c.get('recall_any'), '7.2%')} {f(c.get('recall_certain'), '7.2%')} {f(c.get('precision_certain'), '7.2%')}"
              + (f"  FAILED: {t['reason']}" if t["status"] != "ok" else ""))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--corpus", required=True, help="a directory, or the name of one under corpus/")
    ap.add_argument("--tools", default=",".join(ALL_TOOLS), help="comma-separated, of: " + ", ".join(ALL_TOOLS))
    ap.add_argument("--out", required=True, help="the JSON to write")
    ap.add_argument("--timeout", type=float, default=3600, help="seconds allowed to each tool (default 3600)")
    ap.add_argument("--work", help="scratch directory (default: work/<corpus> next to --out); never inside the corpus")
    ap.add_argument("--truth", help="clang ground truth (default: out/gt/, by corpus name)")
    ap.add_argument("--names", type=int, default=20, help="functions asked about in the query timings")
    ap.add_argument("--reps", type=int, default=3, help="times each query is repeated")
    ap.add_argument("--index-reps", type=int, default=1, help="cold indexes per tool; the median is reported")
    ap.add_argument("--incr-n", type=int, default=4, help="files `si bench-incr` re-extracts")
    ap.add_argument("--edit-file", help="file edited for the incremental runs (default: the .c file of median size)")
    exe = ".exe" if os.name == "nt" else ""
    ap.add_argument("--si", default=str(locate(Path(f"target/release/norn{exe}"))))
    ap.add_argument("--cbm", default=str(locate(Path(f"tools/cbm/codebase-memory-mcp{exe}"))))
    ap.add_argument("--cbm-mode", default="full", choices=["fast", "moderate", "full"])
    ap.add_argument("--clangd-db", help="clangd-db: the build's own compile_commands.json; the corpus must be the tree it was built in")
    ap.add_argument("--clangd", default=shutil.which("clangd") or "C:/Program Files/LLVM/bin/clangd.exe")
    ap.add_argument("--clangd-flags", default="", help="extra compiler flags for every entry of the synthetic database")
    ap.add_argument("--sample-fraction", type=float, default=0.10,
                    help="what part of the functions clang saw is drawn (at random, seeded) for a tool whose calls are "
                         "asked one function at a time (clangd, Source Insight)")
    ap.add_argument("--wsl-distro", default="Ubuntu-24.04")
    ap.add_argument("--wsl-dir", default="si-bench", help="directory for the WSL copy, relative to its home")
    args = ap.parse_args()
    tools = [t.strip() for t in args.tools.split(",") if t.strip()]
    unknown = [t for t in tools if t not in TOOLS]
    if unknown:
        ap.error(f"unknown tools: {unknown}")

    run.SI = check_truth.SI = Path(args.si)
    run.CBM = Path(args.cbm)
    ctx = Ctx(args)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)

    res = {"date": time.strftime("%Y-%m-%d %H:%M:%S"), "corpus": {"name": ctx.name, "path": str(ctx.corpus),
                                                                   "c_files": len(ctx.c_files), "h_files": len(ctx.files) - len(ctx.c_files),
                                                                   "c_and_h_bytes": sum((ctx.corpus / f).stat().st_size for f in ctx.files)},
           "truth": str(ctx.truth) if ctx.truth else None, "edit_file": ctx.edit_file,
           "arguments": {k: v for k, v in vars(args).items()},
           "machine": machine(ctx, any(t in WSL_TOOLS for t in tools)), "notes": NOTES, "tools": {}}

    # The query workload needs an si index when there is no ground truth.
    name_index = ctx.work / "si-names"
    if not ctx.truth:
        subprocess.run([args.si, "index", str(ctx.corpus), "--out", str(name_index), "--stores", "csr"],
                       capture_output=True, check=True)
    ctx.names, how = query_names(ctx, name_index)
    res["query_names"] = {"how": how, "names": ctx.names}

    for tool in ALL_TOOLS:
        if tool not in tools:
            res["tools"][tool] = {"status": "skipped", "skipped": True}
            continue
        print(f"== {tool}", flush=True)
        t = res["tools"][tool] = {"status": "ok"}
        t0 = time.perf_counter()
        ctx.deadline = t0 + args.timeout
        try:
            TOOLS[tool](ctx, t)
        except Exception as e:
            traceback.print_exc()
            t["status"] = "failed"
            t["reason"] = f"{type(e).__name__}: {e}"
        t["harness_wall_s"] = time.perf_counter() - t0
        out.write_text(json.dumps(res, indent=1), encoding="utf-8")
    summary(res)
    print(f"wrote {out}")
    return 0 if all(t["status"] != "failed" for t in res["tools"].values()) else 1


if __name__ == "__main__":
    sys.exit(main())
