"""Benchmark driver: si (this repo) vs codebase-memory-mcp on one corpus.

    python bench/run.py [--corpus corpus/c-only] [--skip-cbm] [--quick]

Writes bench/results/latest.json. Peak memory is the peak of the summed RSS of
the measured process tree, sampled every 20 ms; codebase-memory-mcp spawns a
detached daemon and workers, so for it every process with that image name is
summed.
"""

import argparse
import json
import math
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import psutil

import check_defs
import check_truth

ROOT = Path(__file__).resolve().parent.parent
SI = ROOT / "target" / "release" / ("si.exe" if os.name == "nt" else "si")
CBM = ROOT / "tools" / "cbm" / ("codebase-memory-mcp.exe" if os.name == "nt" else "codebase-memory-mcp")
# codebase-memory-mcp only accepts a cache directory owned by the current
# account, so the benchmark cache lives under the home directory.
CBM_CACHE = Path(os.environ.get("CBM_BENCH_CACHE", Path.home() / ".cache" / "cbm-bench"))
CBM_ENV = {**os.environ, "CBM_CACHE_DIR": str(CBM_CACHE)}
OUT = ROOT / "out" / "bench"


def tree_rss(proc, image_name):
    procs = []
    try:
        procs = [proc] + proc.children(recursive=True)
    except psutil.Error:
        pass
    if image_name:
        seen = {p.pid for p in procs}
        procs += [p for p in psutil.process_iter(["name"]) if p.info["name"] == image_name and p.pid not in seen]
    total = 0
    for p in procs:
        try:
            total += p.memory_info().rss
        except psutil.Error:
            pass
    return total


TIMEOUT_S = 900


def measure(cmd, env=None, image_name=None, cwd=None):
    """Run cmd to completion; return (wall seconds, peak tree RSS bytes, stdout)."""
    with tempfile.TemporaryFile() as out:
        t0 = time.perf_counter()
        # stdin must be closed: `cbm cli <tool>` without arguments reads them from stdin.
        child = subprocess.Popen([str(c) for c in cmd], stdin=subprocess.DEVNULL, stdout=out,
                                 stderr=subprocess.DEVNULL, env=env, cwd=cwd)
        proc = psutil.Process(child.pid)
        peak = 0
        while child.poll() is None:
            peak = max(peak, tree_rss(proc, image_name))
            if time.perf_counter() - t0 > TIMEOUT_S:
                child.kill()
                raise RuntimeError(f"{cmd[0]} still running after {TIMEOUT_S}s")
            time.sleep(0.02)
        wall = time.perf_counter() - t0
        if child.returncode != 0:
            raise RuntimeError(f"{cmd[0]} exited with {child.returncode}")
        out.seek(0)
        return wall, peak, out.read().decode("utf-8", "replace")


def si(*args, threads=0):
    wall, peak, out = measure([SI, "--threads", threads, *args])
    return wall, peak, json.loads(out)


def slope(xs, ys):
    """Least-squares slope of log(y) against log(x): the empirical complexity exponent."""
    lx, ly = [math.log(x) for x in xs], [math.log(y) for y in ys]
    mx, my = statistics.mean(lx), statistics.mean(ly)
    return sum((a - mx) * (b - my) for a, b in zip(lx, ly)) / sum((a - mx) ** 2 for a in lx)


def pct(values, p):
    s = sorted(values)
    return s[round((len(s) - 1) * p)]


def latency(seconds):
    return {"n": len(seconds), "p50_ms": pct(seconds, 0.5) * 1e3, "p99_ms": pct(seconds, 0.99) * 1e3,
            "mean_ms": statistics.mean(seconds) * 1e3}


# ---------------------------------------------------------------- si

def bench_si_stores(corpus, reps):
    """Full index, one store per process so peak RSS is attributable."""
    res = {}
    for store in ["none", "csr", "csr-packed", "sqlite-naive", "sqlite-tuned"]:
        runs = [si("index", corpus, "--out", OUT / "si", "--stores", store) for _ in range(reps)]
        wall, peak, js = sorted(runs, key=lambda r: r[0])[len(runs) // 2]
        res[store] = {"wall_s": wall, "peak_rss_bytes": peak, "report": js}
        print(f"  si index store={store:13s} wall {wall:6.2f}s  peak {peak >> 20:5d} MB", flush=True)
    return res


def bench_si_scaling(corpus, strides):
    """Single-threaded cost against input size: the complexity measurement."""
    rows = []
    for stride in strides:
        _, peak, js = si("index", corpus, "--out", OUT / "si", "--stores", "csr", "--stride", stride, threads=1)
        rows.append({
            "stride": stride, "files": js["files"], "bytes": js["bytes"], "refs": js["refs"]["total"],
            "symbols": js["symbols"], "edges": js["edges"],
            "extract_ms": js["ms"]["extract"], "link_ms": js["ms"]["link"],
            "peak_rss_bytes": peak, "csr_bytes": js["stores"]["csr"]["bytes"],
            "graph_heap_bytes": js["mem"]["graph_heap_bytes"],
        })
        print(f"  si scaling stride={stride:2d} files {js['files']:5d}  extract {js['ms']['extract']:8.0f} ms"
              f"  link {js['ms']['link']:6.1f} ms", flush=True)
    col = lambda k: [r[k] for r in rows]
    fits = {
        "extract_ms_vs_bytes": slope(col("bytes"), col("extract_ms")),
        "link_ms_vs_refs": slope(col("refs"), col("link_ms")),
        "link_ms_vs_edges": slope(col("edges"), col("link_ms")),
        "edges_vs_refs": slope(col("refs"), col("edges")),
        "csr_bytes_vs_edges": slope(col("edges"), col("csr_bytes")),
        "peak_rss_vs_bytes": slope(col("bytes"), col("peak_rss_bytes")),
    }
    return {"rows": rows, "loglog_slopes": fits}


def bench_si_threads(corpus, counts):
    rows = []
    for n in counts:
        runs = [si("index", corpus, "--out", OUT / "si", "--stores", "none", threads=n) for _ in range(2)]
        wall, peak, js = min(runs, key=lambda r: r[2]["ms"]["extract"])
        rows.append({"threads": n, "extract_ms": js["ms"]["extract"], "link_ms": js["ms"]["link"],
                     "slowest_file_ms": js["ms"]["slowest_file"], "peak_rss_bytes": peak})
        print(f"  si threads={n:2d} extract {js['ms']['extract']:8.0f} ms  peak {peak >> 20:5d} MB", flush=True)
    return rows


class SiServer:
    def __init__(self, index_dir):
        self.p = subprocess.Popen([str(SI), "serve", str(index_dir)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  text=True, encoding="utf-8")

    def ask(self, op, name, depth=3):
        self.p.stdin.write(json.dumps({"op": op, "name": name, "depth": depth}) + "\n")
        self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())

    def close(self):
        self.p.stdin.close()
        self.p.wait()


# ---------------------------------------------------------------- codebase-memory-mcp

def cbm_project(corpus):
    return str(Path(corpus).resolve()).replace(":", "").replace("\\", "-").replace("/", "-")


def cbm_index(corpus, mode):
    wall, peak, out = measure([CBM, "cli", "--quiet", "--json", "index_repository", "--repo-path",
                               Path(corpus).resolve().as_posix(), "--mode", mode], env=CBM_ENV, image_name=CBM.name)
    info = json.loads(out[out.index("{"):])["structuredContent"]
    db = CBM_CACHE / f"{info['project']}.db"
    return {"wall_s": wall, "peak_rss_bytes": peak, "db_bytes": db.stat().st_size, "nodes": info["nodes"],
            "edges": info["edges"], "parse_partial": info.get("parse_partial_count"),
            "parse_unusable": info.get("parse_unusable_count"), "project": info["project"]}


def cbm_startup_s():
    """Fixed cost of any one-shot `cli` command (it starts a temporary daemon)."""
    return min(measure([CBM, "cli", "--quiet", "list_projects"], env=CBM_ENV)[0] for _ in range(3))


def cbm_logged_durations(count):
    """Server-side `duration_ms` of the last `count` tool calls, from cbm's own daemon log."""
    log = (CBM_CACHE / "logs" / "cbm-daemon.log").read_text(encoding="utf-8", errors="replace")
    ms = [int(l.rsplit("duration_ms=", 1)[1]) for l in log.splitlines() if "method=tools/call" in l and "duration_ms=" in l]
    return [m / 1e3 for m in ms[-count:]]


def cbm_reset(corpus):
    for f in CBM_CACHE.glob(cbm_project(corpus) + ".db*"):
        f.unlink()


def bench_cbm_index(corpus, modes):
    res = {}
    for mode in modes:
        cbm_reset(corpus)
        res[mode] = cbm_index(corpus, mode)
        r = res[mode]
        print(f"  cbm index mode={mode:9s} wall {r['wall_s']:6.2f}s  peak {r['peak_rss_bytes'] >> 20:5d} MB"
              f"  db {r['db_bytes'] >> 20} MB  nodes {r['nodes']} edges {r['edges']}", flush=True)
    return res


def touch_and_reindex(corpus, mode, rel="bfd/elf.c"):
    """Re-index after a one-line edit to a single file (restored afterwards)."""
    path = Path(corpus) / rel
    original = path.read_bytes()
    try:
        path.write_bytes(original + b"\n/* bench edit */\n")
        return cbm_index(corpus, mode)
    finally:
        path.write_bytes(original)


class McpClient:
    """Minimal MCP stdio client: newline-delimited JSON-RPC."""

    def __init__(self):
        self.p = subprocess.Popen([str(CBM)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  env=CBM_ENV, cwd=tempfile.gettempdir(), text=True, encoding="utf-8")
        self.id = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "si-bench", "version": "0"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.p.stdin.flush()

    def rpc(self, method, params):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("codebase-memory-mcp closed its stdout")
            msg = json.loads(line)
            if msg.get("id") == self.id:
                return msg

    def call(self, tool, args):
        msg = self.rpc("tools/call", {"name": tool, "arguments": args})
        result = msg.get("result", {})
        if "structuredContent" in result:
            return result["structuredContent"]
        text = result.get("content", [{}])[0].get("text", "{}")
        try:
            return json.loads(text)
        except ValueError:
            return {"raw": text}

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.kill()


def cbm_callers(resp):
    names = set()
    for group in resp.get("callers", {}).get("groups", []):
        names.update(row[0] for row in group["rows"] if row[0] != "__file__")
    return names


def bench_roundtrip(index_dir, project, n, with_cbm):
    """Same sampled functions, same four questions, each tool over a stdio pipe."""
    _, _, names = si("sample", index_dir, "--n", n)
    names = list(dict.fromkeys(names))
    res = {"functions": len(names)}

    srv = SiServer(index_dir)
    t = {k: [] for k in ("find", "callers", "callees", "tree3")}
    si_callers, unique_def = {}, {}
    for name in names:
        for op in t:
            t0 = time.perf_counter()
            r = srv.ask("tree" if op == "tree3" else op, name)
            t[op].append(time.perf_counter() - t0)
            if op == "find":
                unique_def[name] = sum(x["kind"] == "function" and not x["decl"] for x in r["results"]) == 1
            if op == "callers":
                fns = [x for x in r["results"] if x["kind"] == "function"]
                si_callers[name] = ({x["name"] for x in fns if x["certain"]}, {x["name"] for x in fns})
    srv.close()
    res["si"] = {k: latency(v) for k, v in t.items()}

    if with_cbm:
        mcp = McpClient()
        base = {"project": project, "format": "json"}
        trace = {**base, "limit": 1000, "max_output_tokens": 200000}
        queries = {
            "find": lambda nm: mcp.call("search_graph", {**base, "name_pattern": f"^{nm}$"}),
            "callers": lambda nm: mcp.call("trace_path", {**trace, "function_name": nm, "direction": "inbound", "depth": 1}),
            "callees": lambda nm: mcp.call("trace_path", {**trace, "function_name": nm, "direction": "outbound", "depth": 1}),
            "tree3": lambda nm: mcp.call("trace_path", {**trace, "function_name": nm, "direction": "outbound", "depth": 3}),
        }
        t = {k: [] for k in queries}
        # Caller sets are compared by name, and only for functions with one
        # definition, so both tools are answering about the same symbol.
        agree = {"certain": [0, 0, 0], "all": [0, 0, 0]}  # both, only_si, only_cbm
        compared = 0
        disagreements = []
        for name in names:
            for op, q in queries.items():
                t0 = time.perf_counter()
                r = q(name)
                t[op].append(time.perf_counter() - t0)
                if op == "callers" and unique_def[name]:
                    compared += 1
                    theirs = cbm_callers(r)
                    for key, ours in zip(agree, si_callers[name]):
                        agree[key][0] += len(ours & theirs)
                        agree[key][1] += len(ours - theirs)
                        agree[key][2] += len(theirs - ours)
                    ours = si_callers[name][1]
                    if ours != theirs:
                        disagreements.append({"function": name, "only_si": sorted(ours - theirs),
                                              "only_cbm": sorted(theirs - ours)})
        mcp.close()
        res["cbm"] = {k: latency(v) for k, v in t.items()}
        res["cbm_server_side"] = latency(cbm_logged_durations(len(names) * len(queries)))
        res["caller_disagreements"] = disagreements
        res["caller_agreement"] = {"functions_compared": compared,
                                   **{k: dict(zip(("both", "only_si", "only_cbm"), v)) for k, v in agree.items()}}
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default=str(ROOT / "corpus" / "c-only"))
    ap.add_argument("--skip-cbm", action="store_true")
    ap.add_argument("--quick", action="store_true", help="one repetition, fewer sizes")
    args = ap.parse_args()
    corpus = args.corpus
    OUT.mkdir(parents=True, exist_ok=True)
    CBM_CACHE.mkdir(parents=True, exist_ok=True)
    reps = 1 if args.quick else 3

    cpu = platform.processor()
    res = {
        "date": time.strftime("%Y-%m-%d %H:%M:%S"),
        "machine": {"cpu": cpu, "physical_cores": psutil.cpu_count(logical=False),
                    "logical_cores": psutil.cpu_count(), "ram_bytes": psutil.virtual_memory().total,
                    "os": platform.platform()},
        "corpus": corpus,
    }

    print("si: full index per store", flush=True)
    res["si_stores"] = bench_si_stores(corpus, reps)
    print("si: scaling (1 thread)", flush=True)
    res["si_scaling"] = bench_si_scaling(corpus, [8, 4, 2, 1] if args.quick else [16, 8, 4, 2, 1])
    print("si: threads", flush=True)
    res["si_threads"] = bench_si_threads(corpus, [1, 28] if args.quick else [1, 2, 4, 8, 14, 20, 28])

    print("si: in-process queries and incremental", flush=True)
    index_dir = OUT / "si"
    si("index", corpus, "--out", index_dir, "--stores", "csr,csr-packed,sqlite-naive,sqlite-tuned")
    res["si_query"] = si("bench-query", index_dir, "--n", 2000)[2]
    expected, found, missing = check_defs.recall(corpus, index_dir)
    res["si_accuracy"] = {"function_defs_expected": expected, "function_defs_found": found,
                          "function_def_recall": found / expected, "files_with_missing_defs": len(missing)}
    print(f"  function definitions: {found}/{expected} = {found / expected:.2%} (regex estimate)", flush=True)
    truth = ROOT / "out" / "gt" / "truth.json"  # from bench/gt_extract.py, if the corpus was built
    if truth.exists():
        res["si_truth"] = check_truth.score(truth, index_dir, corpus)[0]
        d, c = res["si_truth"]["definitions"], res["si_truth"]["calls"]
        print(f"  against clang: definitions recall {d['recall']:.2%} precision {d['precision']:.2%}; "
              f"calls recall {c['recall_any']:.2%} ({c['recall_certain']:.2%} certain) "
              f"precision {c['precision_certain']:.2%}", flush=True)
    wall, peak, js = si("bench-incr", corpus, "--n", 30)
    res["si_incremental"] = js

    project = None
    if not args.skip_cbm:
        res["cbm_startup_s"] = cbm_startup_s()
        print(f"cbm: one-shot cli startup {res['cbm_startup_s']:.2f}s; full index per mode", flush=True)
        res["cbm_index"] = bench_cbm_index(corpus, ["fast", "moderate", "full"])
        project = res["cbm_index"]["full"]["project"]
        print("cbm: re-index after a one-file edit", flush=True)
        res["cbm_incremental"] = {"after_edit": touch_and_reindex(corpus, "full"), "no_change": cbm_index(corpus, "full")}
        for k, r in res["cbm_incremental"].items():
            print(f"  cbm reindex {k:10s} wall {r['wall_s']:6.2f}s  peak {r['peak_rss_bytes'] >> 20} MB", flush=True)

    print("round-trip queries over stdio", flush=True)
    res["roundtrip"] = bench_roundtrip(index_dir, project, 100 if args.quick else 300, not args.skip_cbm)

    out = ROOT / "bench" / "results" / "latest.json"
    out.write_text(json.dumps(res, indent=1), encoding="utf-8")
    print(f"wrote {out}")


if __name__ == "__main__":
    sys.exit(main())
