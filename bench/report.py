"""The comparison's results (bench/compare.py) as one page.

    python bench/report.py out/compare/redis.json out/compare/git-c.json ... --startup out/compare/startup.json --out report.html

Every number on the page is read from the JSON files; nothing is typed in.
"""
import argparse
import html
import json
from pathlib import Path

ORDER = ["si", "cbm", "clangd", "cscope", "gtags", "ctags"]
NAMES = {"si": "Norn", "cbm": "codebase-memory-mcp", "clangd": "clangd", "cscope": "cscope", "gtags": "GNU Global", "ctags": "Universal Ctags"}
CORPORA = {"redis": "Redis", "git-c": "git", "c-only": "binutils", "linux": "Linux kernel"}


MODES = [
    ("warm server over a pipe", "常駐伺服器"),
    ("warm MCP server over a pipe", "常駐 MCP 伺服器"),
    ("warm language server over a pipe", "常駐語言伺服器"),
    ("one process per query", "每次查詢啟動一個行程"),
]

# How the comparison was made, for a reader of the page (the JSON carries
# the same in the harness's own words, tool by tool).
METHOD = [
    "平台不同：Norn、codebase-memory-mcp、clangd 在 Windows / NTFS 上執行；Universal Ctags、cscope、GNU Global 在 WSL2 / ext4 上對同一份檔案的複本執行。Linux 的行程啟動和檔案存取較便宜，對後三者有利，尤其是「每次查詢啟動一個行程」的延遲。",
    "執行緒：Ctags、cscope、Global 是單執行緒；Norn、codebase-memory-mcp、clangd 使用全部核心。",
    "「第一次索引」指磁碟上沒有索引；原始檔都已在作業系統的檔案快取中。重複三次取中位數。",
    "記憶體峰值：Windows 上是整個行程樹的常駐記憶體，每 20 毫秒取樣；WSL 上取同樣取樣與核心記錄的單一行程高水位兩者較大者。",
    "查詢延遲：每個語料固定 20 個函式名稱、各重複 3 次。常駐伺服器和每次啟動行程不是同一種成本；Norn 兩種都列。clangd 的「誰呼叫它」不含開檔與解析該檔的時間。",
    "「改一個檔之後」各工具的做法不同：Norn 是在行程內、已知哪個檔變了；codebase-memory-mcp、cscope、GNU Global 的數字含行程啟動與找出變更；clangd 是帶著磁碟上的索引重新啟動。",
    "準確度：定義以（檔案、名稱）比對函式定義；一條呼叫邊是（呼叫者檔案、呼叫者、被呼叫者檔案、被呼叫者）。找到率以 clang 在該工具讀過的檔案裡看到的為分母；正確率以工具回報、且位於某個編譯單元實際編譯到的行上的為分母。",
    "clangd 沒有真正的編譯資料庫：測試用的是合成的 compile_commands.json，沒有 include 路徑和 -D，且以 Windows 主機為目標（對照資料是 Linux 建置）。所以它的準確度是下限，索引時間也可能偏低。它無法列出全部索引內容，定義與呼叫都只抽樣 300 個函式，定義沒有正確率。",
    "cscope 只記錄被呼叫者的名稱，所以每個呼叫會連到所有同名的函式定義；只有恰好一個同名定義時才算「確定」。",
    "GNU Global 的定義不分種類，所以沒有正確率；它有名稱的參考位置，但不歸屬到呼叫者，因此沒有呼叫圖。Universal Ctags 只記錄定義。",
    "輸入不完全相同：Ctags、cscope、Global 拿到的是語料的 *.c 與 *.h 清單；Norn 和 codebase-memory-mcp 自己決定讀哪些檔。Norn 直接讀語料本身，其餘在複本上執行。",
    "codebase-memory-mcp 以 full 模式執行，只使用手動下載的執行檔，沒有執行它的安裝程式。",
]


def mode_zh(mode):
    for en, zh in MODES:
        if mode.startswith(en):
            return zh
    return mode


def esc(s):
    return html.escape(str(s))


def secs(v):
    if v is None:
        return "–"
    if v < 1:
        return f"{v * 1000:.0f} ms"
    if v < 100:
        return f"{v:.2f} s" if v < 10 else f"{v:.1f} s"
    return f"{v:.0f} s" if v < 600 else f"{v / 60:.1f} 分"


def mb(v):
    if v is None:
        return "–"
    v /= 2**20
    return f"{v / 1024:.2f} GB" if v >= 1024 else f"{v:.0f} MB" if v >= 10 else f"{v:.1f} MB"


def ms(v):
    if v is None:
        return "–"
    return f"{v:.2f} ms" if v < 10 else f"{v:.0f} ms"


def pct(v, sampled=False):
    if v is None:
        return "–"
    return f"{v * 100:.2f}%" + ("*" if sampled else "")


def get(d, *path):
    for p in path:
        if not isinstance(d, dict):
            return None
        d = d.get(p)
    return d


def table(head, rows, note=""):
    out = ['<div class="scroll"><table><thead><tr>']
    out += [f"<th>{h}</th>" for h in head]
    out.append("</tr></thead><tbody>")
    for cls, cells in rows:
        out.append(f'<tr class="{cls}">' + "".join(f"<td>{c}</td>" for c in cells) + "</tr>")
    out.append("</tbody></table></div>")
    if note:
        out.append(f'<p class="note">{note}</p>')
    return "".join(out)


def bar(value, most, text):
    """A number with a bar behind it, on a logarithmic scale down from `most`."""
    import math

    if value is None or not most or value <= 0:
        return text
    lo = most / 100000
    w = 0 if value <= lo else max(2, round(100 * math.log(value / lo) / math.log(most / lo)))
    return f'<span class="bar" style="--w:{w}%">{text}</span>'


def corpus_section(d, remarks):
    name = d["corpus"]["name"]
    tools = d["tools"]
    present = [t for t in ORDER if t in tools]
    c = d["corpus"]
    si_counts = get(tools, "si", "counts") or {}
    facts = f'{c["c_files"]:,} 個 .c、{c["h_files"]:,} 個 .h，{c["c_and_h_bytes"] / 2**20:.0f} MB'
    if si_counts.get("lines"):
        facts += f'，{si_counts["lines"] / 1e6:.2f} 百萬行'
    out = [f'<section id="{esc(name)}"><h2>{esc(CORPORA.get(name, name))}</h2><p class="facts">{facts}</p>']

    def row(t, cells):
        if tools[t].get("status") != "ok":
            reason = esc(tools[t].get("reason", "failed"))[:160]
            return ("failed" + (" us" if t == "si" else ""), [NAMES[t]] + cells[1:2] + [f'<span class="why">未完成：{reason}</span>'] + [""] * (len(cells) - 3))
        return ("us" if t == "si" else "", cells)

    # Cost.
    most_t = max((get(tools, t, "index", "wall_s") or 0) for t in present)
    most_m = max((get(tools, t, "index", "peak_rss_bytes") or 0) for t in present)
    rows = []
    for t in present:
        i = get(tools, t, "index") or {}
        inc = get(tools, t, "incremental") or {}
        incr = "需完整重建" if inc.get("supported") is False else secs(inc.get("wall_s"))
        runs = i.get("wall_s_runs") or []
        spread = f' <span class="dim">({min(runs):.2f}–{max(runs):.2f})</span>' if len(runs) > 1 and max(runs) < 100 else ""
        rows.append(row(t, [NAMES[t], bar(i.get("wall_s"), most_t, secs(i.get("wall_s"))) + spread, bar(i.get("peak_rss_bytes"), most_m, mb(i.get("peak_rss_bytes"))), mb(i.get("disk_bytes")), incr]))
    out.append("<h3>建立索引的成本</h3>")
    out.append(table(["工具", "第一次索引", "記憶體峰值", "索引大小", "改一個檔之後"], rows, "第一次索引取多次的中位數，括號內是最小–最大（秒）。長條為對數刻度。"))

    # Queries.
    rows = []
    for t in present:
        q = get(tools, t, "query") or {}
        one = get(q, "one_shot", "find", "p50_ms") or get(q, "one_shot", "p50_ms")
        mode = esc(mode_zh(q.get("mode", "")))
        if t == "si" and one:
            mode += f"；每次查詢啟動一個行程時 {ms(one)}"
        callers = ms(get(q, "callers", "p50_ms"))
        if t == "gtags" and get(q, "callers", "p50_ms"):
            # global -rx answers an easier question: every place the name is written.
            callers += '<br><span class="why">只列出名稱出現處，不分是誰呼叫，與其他工具不是同一題</span>'
        rows.append(row(t, [NAMES[t], ms(get(q, "find", "p50_ms")), callers, f'<span class="dim">{mode}</span>']))
    out.append("<h3>查詢延遲（中位數）</h3>")
    out.append(table(["工具", "找定義", "誰呼叫它", "量測方式"], rows))

    # Accuracy.
    if any(get(tools, t, "accuracy", "definitions") for t in present):
        rows = []
        for t in present:
            a = get(tools, t, "accuracy") or {}
            dfn, calls = a.get("definitions") or {}, a.get("calls") or {}
            sampled = "sampled" in str(dfn.get("note", "")) or t == "clangd"
            if a.get("available") is False:
                # It has calls; they were not scored on this corpus.
                rows.append(row(t, [NAMES[t], '<span class="why">這個語料上沒有評分</span>', "", "", "", "", ""]))
                continue
            ind = a.get("indirect_calls_by_member") or {}
            rows.append(
                row(
                    t,
                    [
                        NAMES[t],
                        pct(dfn.get("recall"), sampled),
                        pct(dfn.get("precision"), sampled),
                        pct(calls.get("recall_any"), sampled) if calls else "無呼叫圖",
                        pct(calls.get("recall_certain"), sampled) if calls else "",
                        pct(calls.get("precision_certain"), sampled) if calls else "",
                        pct(ind.get("recall")) if ind.get("recall") is not None else "–",
                    ],
                )
            )
        truth = get(tools, "si", "accuracy", "calls", "truth_edges")
        out.append("<h3>準確度（對照 clang 實際建置）</h3>")
        out.append(
            table(
                ["工具", "定義找到", "定義正確", "呼叫找到", "找到且確定", "確定者正確", "經函式指標的呼叫"],
                rows,
                (f"對照 {truth:,} 條 clang 看到的呼叫邊。" if truth else "") + " * 為抽樣（clangd 無法列出全部索引內容，取 300 個函式）。「–」表示該工具的輸出無法評這一項。",
            )
        )
    else:
        out.append('<p class="note">這個語料沒有 clang 對照資料，不評準確度。</p>')
    if remarks.get(name):
        out.append(f'<p class="note">{esc(remarks[name])}</p>')
    out.append("</section>")
    return "".join(out)


def startup_section(s):
    if not s:
        return ""
    rows = []
    for r in s["rows"]:
        rows.append(("", [esc(r["what"]), esc(r["before"]), esc(r["now"])]))
    return '<section id="startup"><h2>Norn 的啟動時間</h2>' + table(["到可以查詢為止", "0.17", "0.19"], rows, esc(s.get("note", ""))) + "</section>"


CSS = """
:root{--bg:#f6f5f1;--panel:#fffefb;--fg:#1c1e2b;--dim:#666a7a;--line:#dcd9d0;--accent:#b8791a;--us:#fff6e2;--bar:#e9e4d6;--bad:#a8372c}
@media (prefers-color-scheme: dark){:root:not([data-theme="light"]){--bg:#11142a;--panel:#191d38;--fg:#e9edfa;--dim:#9aa0b8;--line:#2c3157;--accent:#f6be52;--us:#2a2a3d;--bar:#2a3058;--bad:#ff9a8c;color-scheme:dark}}
:root[data-theme="dark"]{--bg:#11142a;--panel:#191d38;--fg:#e9edfa;--dim:#9aa0b8;--line:#2c3157;--accent:#f6be52;--us:#2a2a3d;--bar:#2a3058;--bad:#ff9a8c;color-scheme:dark}
body{background:var(--bg);color:var(--fg);font:15px/1.6 "Noto Sans TC","Segoe UI",system-ui,sans-serif;padding-inline:16px;padding-block:28px 60px}
main{max-width:1040px;margin:0 auto;display:flex;flex-direction:column;gap:30px}
header{display:flex;gap:16px;align-items:center}
header svg{width:56px;height:56px;flex:none}
h1{font-size:28px;margin:0;letter-spacing:.01em;text-wrap:balance}
h2{font-size:21px;margin:0;padding-top:8px;border-top:2px solid var(--fg)}
h3{font-size:15px;margin:14px 0 4px;color:var(--dim);font-weight:600;letter-spacing:.04em}
p{margin:0;max-width:72ch}
.lede{color:var(--dim)}
.facts,.note,.dim{color:var(--dim);font-size:13px}
.note{margin-top:4px}
section{display:flex;flex-direction:column;gap:6px;min-width:0}
.scroll{overflow-x:auto;border:1px solid var(--line);border-radius:8px;background:var(--panel)}
table{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums;font-size:14px}
th,td{padding:7px 12px;text-align:right;white-space:nowrap;border-bottom:1px solid var(--line)}
th:first-child,td:first-child{text-align:left}
td:last-child:has(.dim),td:has(.why){text-align:left;white-space:normal}
th{font-weight:600;font-size:12.5px;color:var(--dim);letter-spacing:.03em}
tbody tr:last-child td{border-bottom:0}
tr.us td{background:var(--us);font-weight:600}
tr.us td:first-child{color:var(--accent)}
.why{color:var(--bad);font-size:13px}
.bar{display:inline-block;min-width:150px;padding:1px 6px;border-radius:3px;background:linear-gradient(to left,var(--bar) var(--w),transparent var(--w))}
ul{margin:0;padding-left:1.2em;display:flex;flex-direction:column;gap:6px;max-width:80ch}
.keys{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:12px}
.key{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:14px;display:flex;flex-direction:column;gap:4px}
.key b{font-size:24px;color:var(--accent);font-variant-numeric:tabular-nums}
.key span{font-size:13px;color:var(--dim)}
nav{display:flex;gap:14px;flex-wrap:wrap;font-size:14px}
a{color:var(--accent)}
"""

MARK = '<svg viewBox="0 0 32 32" aria-hidden="true"><defs><linearGradient id="n" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#222954"/><stop offset="1" stop-color="#0d1126"/></linearGradient></defs><rect width="32" height="32" rx="6.7" fill="url(#n)"/><g fill="none" stroke="#e9edfa" stroke-width="2.3" stroke-linecap="round"><path d="M7 7.5C14.5 7.5 13 16 19.5 16"/><path d="M7 16H19.5"/><path d="M7 24.5C14.5 24.5 13 16 19.5 16"/></g><path d="M19.5 16H25.5" stroke="#f6be52" stroke-width="2.3" stroke-linecap="round"/><g fill="#e9edfa"><circle cx="7" cy="7.5" r="2.3"/><circle cx="7" cy="16" r="2.3"/><circle cx="7" cy="24.5" r="2.3"/></g><circle cx="25.5" cy="16" r="3.1" fill="#f6be52"/></svg>'


def keys(results):
    """A few figures worth reading first, each computed from the results."""
    out = []
    for d in results:
        name = CORPORA.get(d["corpus"]["name"], d["corpus"]["name"])
        tools = d["tools"]
        si = tools.get("si", {})
        t_si = get(si, "index", "wall_s")
        others = {t: get(tools, t, "index", "wall_s") for t in ("cbm", "clangd") if get(tools, t, "status") == "ok"}
        if t_si and others:
            t, v = min(others.items(), key=lambda x: x[1] or 1e9)
            if v:
                out.append((f"{v / t_si:.0f}×", f"{name}：第一次索引比有呼叫圖的最快對手（{NAMES[t]}）快的倍數"))
        r = get(si, "accuracy", "calls", "recall_any")
        if r is not None:
            best = max(((get(tools, t, "accuracy", "calls", "recall_any") or 0), t) for t in tools if t != "si")
            own = "（Norn 使用了語料附的建置條件）" if d["corpus"]["name"] == "git-c" else ""
            out.append((pct(r), f"{name}：Norn 找到的呼叫；其他工具最高 {pct(best[0])}（{NAMES[best[1]]}）{own}"))
    return '<div class="keys">' + "".join(f'<div class="key"><b>{esc(a)}</b><span>{esc(b)}</span></div>' for a, b in out[:8]) + "</div>"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results", nargs="+")
    ap.add_argument("--startup")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    results = [json.loads(Path(p).read_text(encoding="utf-8")) for p in args.results]
    startup = json.loads(Path(args.startup).read_text(encoding="utf-8")) if args.startup else None
    m = results[0]["machine"]
    versions = {}
    for d in results:
        for t, r in d["tools"].items():
            v = r.get("version")
            label = (startup or {}).get("norn_version", "") if t == "si" else ""
            versions.setdefault(t, (v.get("reported") or f'{label} (build {v.get("sha1", "")})').replace("codebase-memory-mcp ", "") if isinstance(v, dict) else str(v).split(",")[0].replace("clangd version ", "").replace("Universal Ctags ", "").replace("cscope: version ", "").replace("gtags (GNU Global) ", ""))
    notes = []
    for d in results:
        for n in d.get("notes", []):
            if n not in notes:
                notes.append(n)
        for r in d["tools"].values():
            for n in r.get("notes", []) or []:
                if n not in notes:
                    notes.append(n)
    nav = "".join(f'<a href="#{esc(d["corpus"]["name"])}">{esc(CORPORA.get(d["corpus"]["name"], d["corpus"]["name"]))}</a>' for d in results)
    page = [
        f"<title>Norn 基準測試</title><style>{CSS}</style><main>",
        f'<header>{MARK}<div><h1>Norn 基準測試</h1><p class="lede">與五個 C 程式碼索引工具，在 {len(results)} 個語料上比成本、查詢與準確度。{esc(results[-1]["date"][:10])}</p></div></header>',
        keys(results),
        f'<nav>{nav}<a href="#startup">啟動時間</a><a href="#method">方法與限制</a></nav>',
    ]
    page += [corpus_section(d, (startup or {}).get("remarks", {})) for d in results]
    page.append(startup_section(startup))
    page.append('<section id="method"><h2>方法與限制</h2><ul>')
    page += [f"<li>{esc(n)}</li>" for n in METHOD]
    page.append("</ul>")
    page.append(
        f'<p class="note">機器：{esc(m["cpu"])}，{m["physical_cores"]} 核 / {m["logical_cores"]} 執行緒，{m["ram_bytes"] / 2**30:.0f} GB；{esc(m["os"].replace("Windows-10-10.0.2", "Windows 11 (10.0.2").replace("-SP0", ")"))}；WSL {esc(get(m, "wsl", "distro") or "")}（{esc(get(m, "wsl", "filesystem") or "")}）。</p>'
    )
    page.append('<p class="note">版本：' + "；".join(f"{esc(NAMES.get(t, t))} {esc(v)}" for t, v in versions.items()) + "。</p>")
    page.append('<p class="note">Source Insight 與 VS Code C/C++ 擴充套件的 IntelliSense 沒有可自動化量測的介面，不在表內。</p></section></main>')
    Path(args.out).write_text("".join(page), encoding="utf-8")
    print(args.out)


if __name__ == "__main__":
    main()
