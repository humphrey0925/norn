"""How long the language server takes to be of use, from nothing and from
its save.

    python bench/startup.py corpus/c-only [--exe path] [--runs 3] [--cache dir]

For each start: the time until `initialize` is answered, and the time
until the first answers that hold something (`si/status` with symbols, a
search with hits). With `SI_START_PROFILE` set in the environment, the
server's own account of where the time went is printed too.
"""
import argparse
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import psutil

ROOT = Path(__file__).resolve().parent.parent


class Server:
    def __init__(self, exe, root, cache):
        self.t0 = time.perf_counter()
        self.p = subprocess.Popen([exe, "lsp", "--cache-dir", cache], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.id, self.root = 0, Path(root).resolve()

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
            if reply.get("id") == self.id:
                return reply.get("result")

    def since(self):
        return time.perf_counter() - self.t0

    def close(self):
        rss = psutil.Process(self.p.pid).memory_info().rss
        self.request("shutdown", None)
        self.send({"jsonrpc": "2.0", "method": "exit", "params": None})
        self.p.stdin.close()
        err = self.p.stderr.read().decode("utf-8", "replace")
        self.p.wait(timeout=60)
        return rss, err


def start(exe, tree, cache, query):
    s = Server(exe, tree, cache)
    s.request("initialize", {"rootUri": s.root.as_uri(), "capabilities": {}})
    row = {"initialized_s": round(s.since(), 3)}
    while True:
        status = s.request("si/status", {})
        if status and status.get("symbols"):
            break
        time.sleep(0.02)
    hits = s.request("si/search", {"query": query, "limit": 20})
    row.update(usable_s=round(s.since(), 3), symbols=status["symbols"], edges=status["edges"], hits=len(hits or []))
    # Everything the full index gives (relations of a local, an edit): when
    # the server says nothing is still being loaded.
    while status.get("loading"):
        time.sleep(0.05)
        status = s.request("si/status", {})
    row.update(complete_s=round(s.since(), 3))
    rss, err = s.close()
    row.update(rss_mb=rss >> 20)
    return row, err


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tree")
    ap.add_argument("--exe", default=str(ROOT / "target" / "release" / "norn.exe"))
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--cache", help="keep the save here between invocations (else a temporary one, so the first start is from nothing)")
    ap.add_argument("--query", default="init")
    args = ap.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        cache = args.cache or str(Path(tmp) / "cache")
        for run in range(args.runs):
            row, err = start(args.exe, args.tree, cache, args.query)
            print(json.dumps({"run": run, **row}), flush=True)
            for line in err.splitlines():
                if line.startswith("start "):
                    print("   ", line, flush=True)


if __name__ == "__main__":
    sys.exit(main())
