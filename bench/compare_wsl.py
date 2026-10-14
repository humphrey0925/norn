#!/usr/bin/env python3
"""The Linux half of bench/compare.py: universal-ctags, cscope and GNU global.

Runs inside WSL on a copy of the corpus that lives on ext4, standard library
only. compare.py starts it; it can also be run by hand:

    python3 compare_wsl.py --tool cscope --src ~/si-bench/redis \
        --work ~/si-bench/idx/redis/cscope --names names.json \
        --edit-file src/geo.c --out cscope.json --dump cscope-dump.json

All three tools are given the same explicit list of every *.c and *.h under
--src and write their index into --work, never into the sources. The result is
rewritten after each phase, so that a run that is killed leaves what it had.
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

PAGE = os.sysconf("SC_PAGE_SIZE")
EDIT = b"\n/* bench edit */\n"


class Deadline:
    def __init__(self, seconds):
        self.end = time.monotonic() + seconds

    def left(self):
        left = self.end - time.monotonic()
        if left <= 0:
            raise TimeoutError("timed out")
        return left


def tree_rss(root):
    """Summed resident memory of `root` and every descendant, from /proc."""
    parent, rss = {}, {}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            with open(f"/proc/{pid}/stat") as f:
                stat = f.read()
            with open(f"/proc/{pid}/statm") as f:
                rss[int(pid)] = int(f.read().split()[1]) * PAGE
            parent[int(pid)] = int(stat.rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            pass
    total = 0
    for pid in rss:
        p = pid
        while p in parent and p != root:
            p = parent[p]
        if p == root:
            total += rss[pid]
    return total


def measure(cmd, cwd, deadline, env=None):
    """Run to completion: wall seconds, peak summed RSS of the tree (sampled
    every 20 ms by a thread), and the kernel's high-water mark of the largest
    process."""
    left = deadline.left()
    t0 = time.perf_counter()
    errors = tempfile.TemporaryFile()  # not a pipe: nobody reads it while the tool runs
    child = subprocess.Popen(cmd, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=errors)
    peak, done, timed_out = [0], threading.Event(), []

    def sample():
        while not done.is_set():
            peak[0] = max(peak[0], tree_rss(child.pid))
            if time.perf_counter() - t0 > left:
                timed_out.append(1)
                child.kill()
                return
            done.wait(0.02)

    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    _, status, usage = os.wait4(child.pid, 0)
    wall = time.perf_counter() - t0
    done.set()
    sampler.join()
    child.returncode = status
    errors.seek(0)
    err = errors.read().decode("utf-8", "replace")
    if timed_out:
        raise TimeoutError(f"{cmd[0]} timed out")
    if status != 0:
        raise RuntimeError(f"{' '.join(cmd[:3])} exited with status {status}: {err[-400:]}")
    single = usage.ru_maxrss * 1024
    return {"wall_s": wall, "peak_rss_bytes": max(peak[0], single), "largest_process_rss_bytes": single}


def cold_index(a, cmd, deadline, products):
    """`cmd` with no index present, --index-reps times: the run of median wall
    time, the walls of all, and the bytes of `products` left in --work."""
    runs = []
    for _ in range(a.index_reps):
        for f in os.listdir(a.work):
            if f != os.path.basename(a.list):
                os.remove(os.path.join(a.work, f))
        runs.append(measure(cmd, a.src, deadline))
    mid = sorted(runs, key=lambda r: r["wall_s"])[len(runs) // 2]
    return {**mid, "wall_s_runs": [r["wall_s"] for r in runs], "disk_bytes": dir_bytes(a.work, products)}


def pct(values, p):
    s = sorted(values)
    return s[round((len(s) - 1) * p)]


def latency(seconds):
    return {"n": len(seconds), "p50_ms": pct(seconds, 0.5) * 1e3, "p99_ms": pct(seconds, 0.99) * 1e3,
            "mean_ms": statistics.mean(seconds) * 1e3}


def time_queries(names, reps, make_cmd, cwd, deadline, env=None):
    """One process per query, as these tools are used. Returns latency and the
    number of output lines for each name (from the first repetition)."""
    times, lines = [], {}
    subprocess.run(make_cmd(names[0]["name"]), cwd=cwd, env=env, capture_output=True, timeout=deadline.left())  # warm
    for rep in range(reps):
        for n in names:
            t0 = time.perf_counter()
            r = subprocess.run(make_cmd(n["name"]), cwd=cwd, env=env, capture_output=True, timeout=deadline.left())
            times.append(time.perf_counter() - t0)
            if rep == 0:
                lines[n["name"]] = len(r.stdout.splitlines())
    return {**latency(times), "answered": sum(v > 0 for v in lines.values()), "names": len(names)}


def dir_bytes(path, names=None):
    return sum(os.path.getsize(os.path.join(path, f)) for f in os.listdir(path) if names is None or f in names)


def version(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    return (r.stdout + r.stderr).strip().splitlines()[0]


class Edited:
    """One file of the copy with a comment appended, put back on exit."""

    def __init__(self, path, index):
        self.path, self.index = path, index

    def __enter__(self):
        # These tools compare modification times in whole seconds: an edit in
        # the second the index was written would not be seen as newer.
        while int(time.time()) <= int(os.stat(self.index).st_mtime):
            time.sleep(0.05)
        self.stat = os.stat(self.path)
        with open(self.path, "rb") as f:
            self.original = f.read()
        with open(self.path, "wb") as f:
            f.write(self.original + EDIT)

    def __exit__(self, *exc):
        with open(self.path, "wb") as f:
            f.write(self.original)
        os.utime(self.path, ns=(self.stat.st_atime_ns, self.stat.st_mtime_ns))


# ---------------------------------------------------------------- ctags

def ctags(a, res, save, deadline):
    res["version"] = version(["ctags", "--version"])
    tags = os.path.join(a.work, "tags")
    cmd = ["ctags", "--languages=C", "--langmap=C:.c.h", "--fields=+n", "-L", a.list, "-f", tags]
    res["command"] = " ".join(cmd)
    res["index"] = cold_index(a, cmd, deadline, {"tags"})
    save()
    res["incremental"] = {"supported": False, "note": "ctags has no incremental mode: a change means a full rebuild "
                          "(index.wall_s). `ctags -a` appends without removing a file's old tags."}
    find = time_queries(a.names_list, a.reps, lambda n: ["readtags", "-t", tags, "-e", "-n", n], a.src, deadline)
    res["query"] = {"mode": "one process per query (readtags, binary search in the sorted tags file)", "find": find,
                    "callers": None, "callers_note": "ctags records no references"}
    save()
    defs, kinds = [], {}
    with open(tags, "rb") as f:
        for raw in f:
            if raw.startswith(b"!_TAG_"):
                continue
            line = raw.decode("latin-1").rstrip("\n")
            name, file, rest = line.split("\t", 2)
            fields = rest.rsplit(';"\t', 1)[1].split("\t")
            kinds[fields[0]] = kinds.get(fields[0], 0) + 1
            if fields[0] == "f":
                lineno = next(int(x[5:]) for x in fields if x.startswith("line:"))
                defs.append([file, name, lineno])
    res["counts"] = {"tags_by_kind": kinds}
    return {"defs": defs, "typed": True, "calls": None}


# ---------------------------------------------------------------- cscope

def parse_cscope(path):
    """Function definitions and calls from an uncompressed (-c) cscope
    database. A source line is `<number> <text>` followed by alternating symbol
    and text lines up to an empty line; a symbol line that starts with a tab
    carries a mark: @ file, $ function definition, } its end, ` call,
    # macro definition, ) its end."""
    defs, calls = [], []
    file, fn, in_macro, lineno, fresh = None, None, False, 0, False
    with open(path, "rb") as f:
        f.readline()  # header
        for raw in f:
            line = raw.decode("latin-1").rstrip("\n")
            if not line:
                fresh = True
                continue
            if line[0] == "\t":
                fresh = False
                mark, name = line[1], line[2:]
                if mark == "@":
                    if not name:
                        break  # the trailer follows
                    file, fn, in_macro = name, None, False
                elif mark == "$":
                    fn = name
                    defs.append([file, name, lineno])
                elif mark == "}":
                    fn = None
                elif mark == "#":
                    in_macro = True
                elif mark == ")":
                    in_macro = False
                elif mark == "`" and fn and not in_macro:
                    calls.append([file, lineno, fn, name])
            elif fresh:
                fresh = False
                head = line.split(" ", 1)[0]
                if head.isdigit():
                    lineno = int(head)
    return defs, calls


def cscope(a, res, save, deadline):
    res["version"] = version(["cscope", "-V"])
    db = os.path.join(a.work, "cscope.out")
    cmd = ["cscope", "-b", "-q", "-k", "-i", a.list, "-f", db]
    res["command"] = " ".join(cmd)
    res["index"] = cold_index(a, cmd, deadline, {"cscope.out", "cscope.out.in", "cscope.out.po"})
    save()

    base = ["cscope", "-d", "-q", "-k", "-f", db]
    res["query"] = {"mode": "one process per query (cscope -d -L, inverted index from -q)",
                    "find": time_queries(a.names_list, a.reps, lambda n: [*base, "-L", "-1", n], a.src, deadline),
                    "callers": time_queries(a.names_list, a.reps, lambda n: [*base, "-L", "-3", n], a.src, deadline)}
    save()

    # The same sources once more without compression or inverted index, only
    # to read every definition and call out of it in one pass (not timed).
    dump = os.path.join(a.work, "dump.out")
    subprocess.run(["cscope", "-b", "-c", "-k", "-i", a.list, "-f", dump], cwd=a.src, check=True,
                   stdin=subprocess.DEVNULL, timeout=deadline.left())
    defs, calls = parse_cscope(dump)
    os.remove(dump)
    # Does that agree with what `cscope -L -2` (functions called by) answers?
    callees = {}
    for file, _line, fn, callee in calls:
        callees.setdefault(fn, set()).add(callee)
    names = sorted({d[1] for d in defs})
    step = max(1, len(names) // 200)
    same = asked = 0
    differing = []
    for fn in names[::step][:200]:
        out = subprocess.run([*base, "-L", "-2", fn], cwd=a.src, capture_output=True, timeout=deadline.left()).stdout
        got = {l.split(" ")[1] for l in out.decode("latin-1").splitlines() if l.count(" ") >= 2}
        asked += 1
        if got == callees.get(fn, set()):
            same += 1
        elif len(differing) < 10:
            differing.append({"function": fn, "only_query": sorted(got - callees.get(fn, set()))[:5],
                              "only_dump": sorted(callees.get(fn, set()) - got)[:5]})
    res["counts"] = {"function_definitions": len(defs), "call_sites": len(calls)}
    res["dump_check"] = {"functions_asked": asked, "same_callee_set_as_cscope_L2": same, "examples_differing": differing,
                         "note": "calls are read from an uncompressed rebuild of the database; this compares that "
                                 "with `cscope -dL -2 <fn>` for every k-th function name"}
    save()

    before = os.stat(db).st_mtime_ns
    with Edited(os.path.join(a.src, a.edit_file), db):
        m = measure(cmd, a.src, deadline)
    res["incremental"] = {"supported": True, **m, "edited": a.edit_file, "index_rewritten": os.stat(db).st_mtime_ns != before,
                          "note": "the same build command with the old database present: cscope copies the entries of "
                                  "unchanged files and reparses the edited one, then rebuilds the inverted index (-q) whole"}
    return {"defs": defs, "typed": True, "calls": calls}


# ---------------------------------------------------------------- GNU global

def gtags(a, res, save, deadline):
    res["version"] = version(["gtags", "--version"])
    env = {**os.environ, "GTAGSROOT": a.src, "GTAGSDBPATH": a.work}
    cmd = ["gtags", "-f", a.list, a.work]
    res["command"] = " ".join(cmd)
    res["index"] = cold_index(a, cmd, deadline, {"GTAGS", "GRTAGS", "GPATH"})
    save()

    res["query"] = {"mode": "one process per query (global)",
                    "find": time_queries(a.names_list, a.reps, lambda n: ["global", "-x", n], a.src, deadline, env),
                    "callers": time_queries(a.names_list, a.reps, lambda n: ["global", "-rx", n], a.src, deadline, env),
                    "callers_note": "global -rx lists references to the name; they are not attributed to a calling "
                                    "function, so this is 'where is it used', not 'who calls it'"}
    save()

    out = subprocess.run(["global", "-x", ".*"], cwd=a.src, env=env, capture_output=True, check=True,
                         timeout=deadline.left()).stdout
    defs = []
    for line in out.decode("latin-1").splitlines():
        parts = line.split(None, 3)
        if len(parts) >= 3 and parts[1].isdigit():
            defs.append([parts[2], parts[0], int(parts[1])])
    res["counts"] = {"definitions_any_kind": len(defs)}
    save()

    db = os.path.join(a.work, "GTAGS")
    stamps = [os.stat(db).st_mtime_ns]
    with Edited(os.path.join(a.src, a.edit_file), db):
        scan = measure(["gtags", "-i", "-f", a.list, a.work], a.src, deadline)
    stamps.append(os.stat(db).st_mtime_ns)
    with Edited(os.path.join(a.src, a.edit_file), db):
        single = measure(["global", "--single-update", a.edit_file], a.src, deadline, env)
    stamps.append(os.stat(db).st_mtime_ns)
    single["index_rewritten"] = stamps[2] != stamps[1]
    res["incremental"] = {"supported": True, **scan, "edited": a.edit_file, "index_rewritten": stamps[1] != stamps[0],
                          "note": "`gtags -i -f <list>`: finds the changed file itself by comparing times",
                          "told_which_file": {**single, "note": "`global --single-update <file>`: told which file changed"}}
    return {"defs": defs, "typed": False, "calls": None}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tool", required=True, choices=["ctags", "cscope", "gtags"])
    ap.add_argument("--src", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--names", required=True)
    ap.add_argument("--edit-file", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--dump", required=True)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--index-reps", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=3600)
    a = ap.parse_args()
    a.src = os.path.abspath(os.path.expanduser(a.src))
    a.work = os.path.abspath(os.path.expanduser(a.work))
    with open(a.names) as f:
        a.names_list = json.load(f)
    deadline = Deadline(a.timeout)
    shutil.rmtree(a.work, ignore_errors=True)
    os.makedirs(a.work)

    files = []
    for d, dirs, fs in os.walk(a.src):
        dirs[:] = sorted(x for x in dirs if x not in (".git", ".si"))
        files += [os.path.relpath(os.path.join(d, f), a.src) for f in fs if f.endswith((".c", ".h"))]
    files.sort()
    a.list = os.path.join(a.work, "files.txt")
    with open(a.list, "w", encoding="latin-1") as f:
        f.write("".join(x + "\n" for x in files))

    res = {"status": "ok", "files": len(files)}

    def save():
        with open(a.out, "w") as f:
            json.dump(res, f, indent=1)

    try:
        dump = {"ctags": ctags, "cscope": cscope, "gtags": gtags}[a.tool](a, res, save, deadline)
        dump["files"] = files
        with open(a.dump, "w") as f:
            json.dump(dump, f)
    except (Exception, subprocess.TimeoutExpired) as e:
        res["status"] = "failed"
        res["reason"] = f"{type(e).__name__}: {e}"
    save()
    return 0 if res["status"] == "ok" else 1


if __name__ == "__main__":
    sys.exit(main())
