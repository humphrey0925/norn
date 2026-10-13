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
        while True:
            length = 0
            while (line := self.p.stdout.readline().strip()):
                if line.lower().startswith(b"content-length:"):
                    length = int(line.split(b":")[1])
            reply = json.loads(self.p.stdout.read(length))
            # Progress while indexing comes between a request and its reply.
            if "id" in reply:
                break
            self.notes = getattr(self, "notes", []) + [reply]
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
        # The whole project's symbols, in name order whatever the case, a
        # stretch at a time.
        first = c.request("si/symbols", {"offset": 0, "limit": 50})
        names = [x["name"] for x in first["items"]]
        assert first["total"] > 3000 and len(names) == 50 and names == sorted(names, key=lambda n: (n.lower(), n)), names[:8]
        later = c.request("si/symbols", {"offset": first["total"] - 5, "limit": 50})
        assert len(later["items"]) == 5 and later["items"][0]["name"].lower() >= names[-1].lower()
        spot = c.request("si/place", {k: first["items"][10][k] for k in ("file", "line", "name")})
        assert spot["range"]["start"]["line"] == first["items"][10]["line"] - 1, spot
        grouped = c.request("si/symbols", {"offset": 0, "limit": 2000, "order": "kind"})["items"]
        kinds = [x["kind"] for x in grouped]
        runs = [k for i, k in enumerate(kinds) if i == 0 or kinds[i - 1] != k]
        assert runs[:2] == ["function", "macro"] and len(runs) == len(set(runs)), runs
        fn_names = [x["name"] for x in grouped if x["kind"] == "function"]
        assert fn_names == sorted(fn_names, key=lambda n: (n.lower(), n))
        print(f"all symbols: {first['total']} in name order, fetched by the stretch")

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
        # Who assigns the member `bread`? And where is cache_bread stored?
        who = c.request("si/assignments", c.at("bfd/bfdio.c", "bread (abfd, ptr"))
        stored = {(r["function"]["name"], Path(r["file"]).name) for r in who["rows"]}
        assert who["by"] == "member" and ("cache_bread", "cache.c") in stored and ("memory_bread", "bfdio.c") in stored, stored
        where = c.request("si/assignments", c.at("bfd/cache.c", "cache_bread (struct bfd *abfd"))
        assert where["by"] == "function" and [r["member"]["name"] for r in where["rows"]] == ["bread"], where
        assert where["rows"][0]["inside"] == "cache_iovec", where["rows"][0]
        # "Called by" of what is not a function: who uses it. A variable...
        rel = c.request("si/relation", {**c.at("bfd/cache.c", "bfd_last_cache", 1), "direction": "callers"})
        users = {e["name"] for e in rel["entries"]}
        assert rel["root"]["kind"] == "global" and {"insert", "snip", "bfd_cache_lookup_worker"} <= users, (rel["root"], users)
        assert all(e["lines"] for e in rel["entries"])
        # ...a struct member (here one that is called through)...
        rel = c.request("si/relation", {**c.at("bfd/bfdio.c", "bread (abfd, ptr"), "direction": "callers"})
        assert rel["root"]["kind"] == "field" and "bfd_read" in {e["name"] for e in rel["entries"]}, rel["root"]
        # ...which "calls" the functions stored in it...
        rel = c.request("si/relation", {**c.at("bfd/bfdio.c", "bread (abfd, ptr"), "direction": "callees"})
        assert rel["root"]["kind"] == "field" and "cache_bread" in {e["name"] for e in rel["entries"]}, rel
        # ...and a type.
        rel = c.request("si/relation", {**c.at("bfd/cache.c", "bfd_iovec cache_iovec"), "direction": "callers"})
        assert rel["root"]["name"] == "bfd_iovec" and rel["entries"], rel["root"]
        print("called by: variables, members and types list who uses them")
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
        # The project's scope: leave `include/` out, and it is gone; the
        # setting is written into the tree; a sync sees files appear.
        r = c.request("si/setScope", {"include": [], "exclude": ["include"]})
        assert r["removed"] == 1 and (work / ".si" / "config.json").is_file(), r
        assert not c.request("workspace/symbol", {"query": "ATTRIBUTE_UNUSED"})
        (work / "bfd" / "extra.c").write_text("int si_extra (void) { return 0; }\n")
        r = c.request("si/sync", {})
        assert r["added"] == 1 and r["exclude"] == ["include"], r
        r = c.request("si/setScope", {"include": [], "exclude": []})
        assert r["added"] == 1 and c.request("workspace/symbol", {"query": "ATTRIBUTE_UNUSED"}), r
        print("scope: folders leave and return, saved in .si/config.json; sync picks up new files")
        # Compile conditions: bfdio.c has `#ifndef FD_CLOEXEC` / `#define
        # FD_CLOEXEC 1`; told the build defines it, that branch is skipped.
        has = lambda name: any(x["name"] == name for x in c.request("workspace/symbol", {"query": name}))
        assert has("FD_CLOEXEC")
        r = c.request("si/setConditions", {"defines": {"FD_CLOEXEC": "1"}})
        assert r["conditions"]["known"] == 1 and r["parsed"] >= 7, r
        assert not has("FD_CLOEXEC") and has("bfd_read")
        saved = json.loads((work / ".si" / "config.json").read_text())
        assert saved["defines"] == {"FD_CLOEXEC": "1"} and "exclude" in saved, saved
        c.request("si/setConditions", {})
        assert has("FD_CLOEXEC")
        print("conditions: a branch the build does not compile leaves the index, and comes back")
        c.close()
    print("ok")


if __name__ == "__main__":
    main()
