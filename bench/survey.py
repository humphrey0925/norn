"""Index other people's trees and look for trouble: a crash, a hang, names
that cannot be names, time and memory out of proportion.

    python bench/survey.py corpus/redis corpus/curl ...

For each tree: `si index` (a cold index with every core), then what the
index holds, then a language server over it (start from nothing, start
again from the saved index, one edit).
"""
import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import psutil

ROOT = Path(__file__).resolve().parent.parent
SI = ROOT / "target" / "release" / ("norn.exe" if sys.platform == "win32" else "norn")
NAME = re.compile(r"^[A-Za-z_$][A-Za-z0-9_$]*$")


def peak(cmd, stdin=None, timeout=3600):
    """Run, returning (stdout, seconds, peak resident bytes, exit code)."""
    t = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, stdin=stdin)
    ps, high = psutil.Process(p.pid), 0
    while p.poll() is None:
        try:
            high = max(high, ps.memory_info().rss)
        except psutil.Error:
            break
        if time.perf_counter() - t > timeout:
            p.kill()
            return b"", time.perf_counter() - t, high, "timeout"
        time.sleep(0.02)
    out = p.stdout.read()
    return out, time.perf_counter() - t, high, p.wait()


class Server:
    def __init__(self, root, *args):
        self.p = subprocess.Popen([str(SI), "lsp", *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.id, self.root, self.high = 0, Path(root).resolve(), 0
        self.request("initialize", {"rootUri": self.root.as_uri(), "capabilities": {}})
        # The server answers before the index is there: wait for it.
        while self.request("si/status", {}).get("loading"):
            time.sleep(0.02)

    def send(self, msg):
        body = json.dumps(msg).encode()
        self.p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.p.stdin.flush()

    def request(self, method, params):
        self.id += 1
        self.send({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        while True:
            length = 0
            while line := self.p.stdout.readline().strip():
                if line.lower().startswith(b"content-length:"):
                    length = int(line.split(b":")[1])
            reply = json.loads(self.p.stdout.read(length))
            if "id" in reply:
                assert "error" not in reply, reply
                return reply["result"]

    def rss(self):
        return psutil.Process(self.p.pid).memory_info().rss

    def close(self):
        self.request("shutdown", None)
        self.send({"jsonrpc": "2.0", "method": "exit", "params": None})
        self.p.wait(timeout=30)


def survey(tree):
    tree = Path(tree)
    row = {"tree": tree.name}
    with tempfile.TemporaryDirectory() as tmp:
        out, secs, high, code = peak([str(SI), "index", str(tree), "--out", str(Path(tmp) / "idx"), "--stores", "csr"])
        row.update(index_s=round(secs, 2), index_peak_mb=round(high / 2**20), exit=code)
        if code != 0:
            return row
        d = json.loads(out)
        row.update(files=d["files"], mlines=round(d["lines"] / 1e6, 2), symbols=d["symbols"], edges=d["edges"],
                   parse_error_files=d["files_with_parse_errors"], slowest=d["slowest_file"], slowest_ms=round(d["ms"]["slowest_file"]),
                   prescan_ms=round(d["ms"]["prescan"]), extract_ms=round(d["ms"]["extract"]), link_ms=round(d["ms"]["link"]))
        syms = subprocess.run([str(SI), "symbols", str(Path(tmp) / "idx")], capture_output=True).stdout.decode("utf-8", "replace").splitlines()
        odd = [s for s in syms if not NAME.match(s.split("\t")[4]) and s.split("\t")[2] != "file"]
        row.update(odd_names=len(odd), odd_examples=[o.split("\t")[0] + ":" + o.split("\t")[1] + " " + repr(o.split("\t")[4]) for o in odd[:5]])
        # The language server: from nothing, from the save, and one edit.
        cache = str(Path(tmp) / "cache")
        t = time.perf_counter()
        s = Server(tree, "--cache-dir", cache)
        row.update(lsp_cold_s=round(time.perf_counter() - t, 2), lsp_rss_mb=round(s.rss() / 2**20))
        cond = s.request("si/conditions", {})
        row.update(from_sources=cond["fromSourcesTotal"])
        s.close()
        t = time.perf_counter()
        s = Server(tree, "--cache-dir", cache)
        row.update(lsp_warm_s=round(time.perf_counter() - t, 2))
        listing = s.request("si/symbols", {"offset": 0, "limit": 50, "order": "kind"})
        functions = [x for x in listing["items"] if x["kind"] == "function"]
        t = time.perf_counter()
        hits = s.request("si/search", {"query": "init", "limit": 50})
        row.update(search_ms=round((time.perf_counter() - t) * 1e3, 1), search_hits=len(hits))
        if functions:
            f = functions[len(functions) // 2]
            at = s.request("si/place", {"file": f["file"], "line": f["line"], "name": f["name"]})
            if at:
                where = {"textDocument": {"uri": at["uri"]}, "position": at["range"]["start"]}
                t = time.perf_counter()
                for direction in ("callers", "callees"):
                    s.request("si/relation", {**where, "direction": direction})
                s.request("textDocument/documentSymbol", {"textDocument": {"uri": at["uri"]}})
                row.update(relation_ms=round((time.perf_counter() - t) * 1e3, 1))
                # An edit that changes nothing, as a save: how long until the index answers again.
                t = time.perf_counter()
                s.send({"jsonrpc": "2.0", "method": "textDocument/didSave", "params": {"textDocument": {"uri": at["uri"]}}})
                s.request("si/status", {})
                row.update(save_ms=round((time.perf_counter() - t) * 1e3, 1))
        s.close()
    return row


def main():
    rows = []
    for tree in sys.argv[1:]:
        try:
            rows.append(survey(tree))
        except Exception as e:  # a tree that breaks the tool is the finding
            rows.append({"tree": Path(tree).name, "failed": repr(e)})
        print(json.dumps(rows[-1]), flush=True)
    out = ROOT / "out" / "survey.json"
    out.parent.mkdir(exist_ok=True)
    out.write_text(json.dumps(rows, indent=1))


main()
