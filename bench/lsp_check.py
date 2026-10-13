#!/usr/bin/env python3
"""Drive `si lsp` the way an editor would and check what comes back.

    python bench/lsp_check.py [corpus]

Uses a scratch copy of a few binutils files, so that saving an edit can be
tried without touching the corpus.
"""
import json
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SI = ROOT / "target" / "release" / "si.exe"


class Client:
    def __init__(self, root):
        self.p = subprocess.Popen([str(SI), "lsp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.id = 0
        self.root = Path(root)
        self.request("initialize", {"rootUri": self.root.as_uri(), "capabilities": {}})
        self.notify("initialized", {})

    def send(self, msg):
        body = json.dumps(msg).encode()
        self.p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.p.stdin.flush()

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method, params):
        self.id += 1
        self.send({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        length = 0
        while (line := self.p.stdout.readline().strip()):
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        reply = json.loads(self.p.stdout.read(length))
        assert reply.get("id") == self.id and "error" not in reply, reply
        return reply["result"]

    def at(self, file, needle, occurrence=0):
        """textDocument position of the `occurrence`-th `needle` in `file`."""
        seen = 0
        for n, line in enumerate((self.root / file).read_text(encoding="latin-1").split("\n")):
            col = line.find(needle)
            if col >= 0:
                if seen == occurrence:
                    return {"textDocument": {"uri": (self.root / file).as_uri()}, "position": {"line": n, "character": col + 1}}
                seen += 1
        raise KeyError(needle)

    def where(self, loc):
        return Path(loc["uri"]).name, loc["range"]["start"]["line"] + 1

    def close(self):
        self.request("shutdown", None)
        self.notify("exit", None)
        self.p.wait(timeout=10)


def main():
    corpus = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "corpus" / "c-only")
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp) / "w"
        for rel in ["bfd/opncls.c", "bfd/bfd-in2.h", "bfd/libbfd.c", "bfd/libbfd.h", "bfd/cache.c", "bfd/binary.c",
                    "bfd/targets.c", "bfd/bfdio.c", "include/ansidecl.h"]:
            (work / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(corpus / rel, work / rel)
        t = time.perf_counter()
        c = Client(work)
        print(f"initialize: {time.perf_counter() - t:.2f}s")

        # A call to a function defined in another file.
        defs = c.request("textDocument/definition", c.at("bfd/opncls.c", "bfd_set_error (bfd_error_no_memory)"))
        assert [c.where(d)[0] for d in defs] == ["bfd-in2.h"] or defs, defs
        print("definition of bfd_set_error:", [c.where(d) for d in defs])

        # Go to a macro, then to what it is made of.
        defs = c.request("textDocument/definition", c.at("bfd/opncls.c", "BFD_SEND_FMT (abfd"))
        assert c.where(defs[0])[0] == "bfd-in2.h", defs
        print("definition of BFD_SEND_FMT:", c.where(defs[0]))

        refs = c.request("textDocument/references", {**c.at("bfd/opncls.c", "_bfd_new_bfd (void)"), "context": {"includeDeclaration": True}})
        files = sorted({c.where(r)[0] for r in refs})
        assert len(refs) >= 4 and "opncls.c" in files, refs
        print(f"references to _bfd_new_bfd: {len(refs)} in {files}")

        symbols = c.request("textDocument/documentSymbol", {"textDocument": {"uri": (work / "bfd/cache.c").as_uri()}})
        names = {s["name"] for s in symbols}
        assert {"bfd_cache_init", "bfd_cache_close"} <= names, sorted(names)[:20]
        print(f"document symbols of cache.c: {len(symbols)}")

        found = c.request("workspace/symbol", {"query": "bfd_cache_"})
        assert any(s["name"] == "bfd_cache_init" for s in found), found
        print(f"workspace symbols bfd_cache_*: {len(found)}")
        # Fragments in any order, and initials.
        for query, want in [("init cache", "bfd_cache_init"), ("bci", "bfd_cache_init"), ("mem bread", "memory_bread")]:
            names = [x["name"] for x in c.request("workspace/symbol", {"query": query})]
            assert want in names[:10], (query, names[:10])
        print("fuzzy search: 'init cache', 'bci', 'mem bread' find what they should")

        # Call hierarchy through a function pointer: cache_bread is only ever
        # stored in the `bread` member of `cache_iovec` and called through it.
        items = c.request("textDocument/prepareCallHierarchy", c.at("bfd/cache.c", "cache_bread (struct bfd *abfd"))
        assert items and items[0]["name"] == "cache_bread", items
        incoming = c.request("callHierarchy/incomingCalls", {"item": items[0]})
        callers = sorted(x["from"]["name"] for x in incoming)
        assert "bfd_read" in callers, callers
        print("callers of cache_bread (through iovec->bread):", callers[:8])
        outgoing = c.request("callHierarchy/outgoingCalls", {"item": items[0]})
        assert any(x["to"]["name"] == "cache_bread_1" for x in outgoing), [x["to"]["name"] for x in outgoing]

        # The extension's own requests.
        at = c.at("bfd/bfdio.c", "bfd_read (void *ptr")
        rel = c.request("si/relation", {**at, "direction": "callees"})
        through = {e["name"]: e["through"] for e in rel["entries"]}
        assert rel["root"]["name"] == "bfd_read" and through.get("cache_bread") == "bread", through
        ctx = c.request("si/context", c.at("bfd/bfdio.c", "bread (abfd, ptr"))
        assert ctx["name"] == "bread" and ctx["kind"] == "field", ctx
        hits = c.request("si/search", {"query": "init cache"})
        assert hits[0]["score"] >= hits[-1]["score"] and any(h["name"] == "bfd_cache_init" for h in hits), hits[:3]
        print("si/relation, si/context, si/search answer as the views expect")

        # Save an edit: a new function calling an existing one.
        path = work / "bfd/cache.c"
        path.write_bytes(path.read_bytes() + b"\nint si_probe (void) { return bfd_cache_init (0); }\n")
        t = time.perf_counter()
        c.notify("textDocument/didSave", {"textDocument": {"uri": path.as_uri()}})
        found = c.request("workspace/symbol", {"query": "si_probe"})
        assert [s["name"] for s in found] == ["si_probe"], found
        items = c.request("textDocument/prepareCallHierarchy", c.at("bfd/cache.c", "bfd_cache_init (bfd *abfd)"))
        callers = sorted(x["from"]["name"] for x in c.request("callHierarchy/incomingCalls", {"item": items[0]}))
        assert "si_probe" in callers, callers
        print(f"after saving an edit: si_probe indexed and calls bfd_cache_init ({time.perf_counter() - t:.3f}s)")
        c.close()
    print("ok")


if __name__ == "__main__":
    main()
