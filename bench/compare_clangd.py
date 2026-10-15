"""clangd for bench/compare.py: background index over LSP, queries, call edges.

clangd is given a synthetic compilation database (one `clang -c <file>` per
.c file, no flags unless asked) in a directory outside the sources, and writes
its index next to that database, in `.cache/clangd/index`.
"""
import hashlib
import math
import json
import os
import queue
import subprocess
import threading
import time
import urllib.parse
from pathlib import Path

import psutil

PROGRESS = "backgroundIndexProgress"
QUIET_S = 3.0      # no new indexing this long after the last one ended: done
NO_WORK_S = 20.0   # nothing started this long after the trigger: nothing to do


def tree_rss(proc):
    total = 0
    try:
        procs = [proc] + proc.children(recursive=True)
    except psutil.Error:
        return 0
    for p in procs:
        try:
            total += p.memory_info().rss
        except psutil.Error:
            pass
    return total


def dir_bytes(path):
    return sum(f.stat().st_size for f in Path(path).rglob("*") if f.is_file())


def write_database(db_dir, root, c_files, flags):
    db_dir.mkdir(parents=True, exist_ok=True)
    root = Path(root).resolve()
    entries = [{"directory": root.as_posix(), "file": (root / f).as_posix(),
                "arguments": ["clang", *flags, "-c", (root / f).as_posix()]} for f in c_files]
    (db_dir / "compile_commands.json").write_text(json.dumps(entries, indent=0), encoding="utf-8")


class Clangd:
    def __init__(self, exe, db_dir, root, log):
        self.root = Path(root).resolve()
        self.args = [str(exe), "--background-index", "--background-index-priority=normal",
                     f"--compile-commands-dir={Path(db_dir).as_posix()}", "--log=error", "--pch-storage=memory"]
        self.t0 = time.perf_counter()
        self.log = open(log, "ab")
        self.p = subprocess.Popen(self.args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
        self.ps = psutil.Process(self.p.pid)
        self.q = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()
        self.id = 0
        self.peak = 0
        self.began = 0          # background indexing rounds started
        self.active = False
        self.last_end = None    # perf_counter of the last round's end
        self.progress = ""
        self.request("initialize", {
            "processId": os.getpid(), "rootUri": self.root.as_uri(),
            "capabilities": {"window": {"workDoneProgress": True},
                             "textDocument": {"callHierarchy": {}, "documentSymbol": {"hierarchicalDocumentSymbolSupport": True}}}})
        self.notify("initialized", {})

    def _read(self):
        out = self.p.stdout
        try:
            while True:
                length = 0
                while (line := out.readline().strip()):
                    if line.lower().startswith(b"content-length:"):
                        length = int(line.split(b":")[1])
                if not length:
                    break
                self.q.put(json.loads(out.read(length)))
        except (OSError, ValueError):
            pass
        self.q.put(None)

    def send(self, msg):
        body = json.dumps(msg).encode()
        self.p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.p.stdin.flush()

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def pump(self, timeout):
        """Next message from clangd, or None after `timeout`. Answers its
        requests and follows background-index progress on the way."""
        try:
            msg = self.q.get(timeout=timeout)
        except queue.Empty:
            return {}
        if msg is None:
            raise RuntimeError(f"clangd exited ({self.p.poll()})")
        if "method" in msg and "id" in msg:  # window/workDoneProgress/create and the like
            self.send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
        elif msg.get("method") == "$/progress" and msg["params"].get("token") == PROGRESS:
            value = msg["params"]["value"]
            if value["kind"] == "begin":
                self.began += 1
                self.active = True
            elif value["kind"] == "end":
                self.active = False
                self.last_end = time.perf_counter()
            else:
                self.progress = value.get("message", "")
        return msg

    def request(self, method, params, timeout=600):
        self.id += 1
        want = self.id
        self.send({"jsonrpc": "2.0", "id": want, "method": method, "params": params})
        end = time.perf_counter() + timeout
        while True:
            msg = self.pump(0.5)
            if msg.get("id") == want and "method" not in msg:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg["result"]
            if time.perf_counter() > end:
                raise TimeoutError(f"clangd did not answer {method} in {timeout}s")

    def wait_indexed(self, deadline):
        """Until background indexing has ended and stayed ended. Returns seconds
        from process start to the end of the last round (None if none ran)."""
        waiting_since = time.perf_counter()
        while True:
            self.pump(0.02)
            self.peak = max(self.peak, tree_rss(self.ps))
            now = time.perf_counter()
            if self.began and not self.active and now - self.last_end > QUIET_S:
                return self.last_end - self.t0
            if not self.began and now - waiting_since > NO_WORK_S:
                return None
            if now > deadline:
                raise TimeoutError(f"clangd still indexing at the timeout ({self.progress})")

    # -- documents

    def uri(self, rel):
        return (self.root / rel).as_uri()

    def rel(self, uri):
        path = urllib.parse.unquote(urllib.parse.urlparse(uri).path).lstrip("/")
        try:
            rel = os.path.relpath(path, self.root)
        except ValueError:
            return path
        return path if rel.startswith("..") else rel.replace("\\", "/")

    def open(self, rel):
        text = (self.root / rel).read_bytes().decode("latin-1")
        self.notify("textDocument/didOpen", {"textDocument": {"uri": self.uri(rel), "languageId": "c", "version": 1, "text": text}})
        return text.split("\n")

    def close_doc(self, rel):
        self.notify("textDocument/didClose", {"textDocument": {"uri": self.uri(rel)}})

    def prepare(self, rel, lines, name, line):
        """The call hierarchy item for function `name` defined on `line`."""
        col = lines[line - 1].find(name) if line <= len(lines) else -1
        if col < 0:
            return None
        items = self.request("textDocument/prepareCallHierarchy", {
            "textDocument": {"uri": self.uri(rel)}, "position": {"line": line - 1, "character": col}}) or []
        return next((i for i in items if i["name"] == name), None)

    def stop(self):
        try:
            self.request("shutdown", None, timeout=30)
            self.notify("exit", None)
            self.p.wait(timeout=30)
        except Exception:
            self.p.kill()
        self.log.close()


def sample(functions, fraction):
    """A fixed pseudo-random part of (file, name, line), `fraction` of them:
    those of lowest SHA-1(file, name), which orders them as a random draw
    with a fixed seed would, and the same on every machine."""
    key = lambda f: hashlib.sha1(f"{f[0]}\0{f[1]}".encode()).hexdigest()
    pool = sorted(sorted(set(functions)), key=key)
    return pool[:max(1, math.ceil(len(pool) * fraction))] if pool else []


def index(exe, db_dir, root, log, deadline, first_file):
    """Cold or warm start up to the end of background indexing."""
    c = Clangd(exe, db_dir, root, log)
    try:
        # The database is only looked at (and its files queued) once a file needs it.
        c.open(first_file)
        wall = c.wait_indexed(deadline)
        c.close_doc(first_file)
    except BaseException:
        c.stop()
        raise
    return c, wall


def call_edges(c, functions, deadline):
    """outgoingCalls of each (file, name, line): rows in the shape
    check_truth scores, the functions clangd could not find at their
    definition, and which of them workspace/symbol finds there."""
    rows, unresolved, found_defs = [], [], set()
    by_file = {}
    for file, name, line in functions:
        by_file.setdefault(file, []).append((name, line))
    for file, fns in sorted(by_file.items()):
        if time.perf_counter() > deadline:
            raise TimeoutError("timed out asking clangd for outgoing calls")
        lines = c.open(file)
        for name, line in fns:
            hits = c.request("workspace/symbol", {"query": name}) or []
            if any(h["name"] == name and h["kind"] == 12 and c.rel(h["location"]["uri"]) == file for h in hits):
                found_defs.add((file, name))
            item = c.prepare(file, lines, name, line)
            if item is None:
                unresolved.append((file, name))
                continue
            for call in c.request("callHierarchy/outgoingCalls", {"item": item}) or []:
                to = call["to"]
                site = call["fromRanges"][0]["start"]["line"] + 1 if call.get("fromRanges") else line
                rows.append((file, site, name, file, to["name"], c.rel(to["uri"]), "certain", "def"))
        c.close_doc(file)
    return rows, unresolved, found_defs


def time_queries(c, names, reps):
    """find = workspace/symbol; callers = prepareCallHierarchy + incomingCalls
    at the definition, in a file that is already open and parsed."""
    lines = {}
    for n in names:
        if n["file"] not in lines:
            lines[n["file"]] = c.open(n["file"])
            c.request("textDocument/documentSymbol", {"textDocument": {"uri": c.uri(n["file"])}})  # parsed now
    find, callers, answered_find, answered_callers = [], [], 0, 0
    for rep in range(reps):
        for n in names:
            t0 = time.perf_counter()
            hits = c.request("workspace/symbol", {"query": n["name"]}) or []
            find.append(time.perf_counter() - t0)
            t0 = time.perf_counter()
            item = c.prepare(n["file"], lines[n["file"]], n["name"], n["line"])
            calls = c.request("callHierarchy/incomingCalls", {"item": item}) if item else []
            callers.append(time.perf_counter() - t0)
            if rep == 0:
                answered_find += any(h["name"] == n["name"] for h in hits)
                answered_callers += bool(calls)
    for file in lines:
        c.close_doc(file)
    return find, callers, answered_find, answered_callers
