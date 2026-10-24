"""The comparison's results (bench/compare.py) as charts and tables for the
README: SVG bar charts and a Markdown page. Every number is read from the
JSON files; nothing is typed in.

    python bench/chart.py out/compare/redis.json out/compare/git-c.json ... --out docs/benchmarks
"""
import argparse
import html
import json
import re
from pathlib import Path

ORDER = ["si", "si-cfg", "clangd-db", "clangd", "cbm", "cscope", "gtags", "ctags"]
# Measured on some projects only, and shown on their pages, not in the charts
# or the summary: Norn given the build's configuration, as a user would.
PAGE_ONLY = {"si-cfg"}
NAMES = {"si": "Norn", "si-cfg": "Norn + build configuration", "clangd-db": "clangd + build database", "cbm": "codebase-memory-mcp", "clangd": "clangd", "cscope": "cscope", "gtags": "GNU Global", "ctags": "Universal Ctags"}
CORPORA = {"redis": "Redis", "git-c": "git", "c-only": "binutils", "stm32f4": "STM32F4 HAL", "nrfx": "nrfx", "nrfx-corpus": "nrfx", "linux": "Linux",
           "curl": "curl", "sqlite": "SQLite", "mbedtls": "Mbed TLS", "freertos": "FreeRTOS", "lwip": "lwIP", "u-boot": "U-Boot",
           "fmt": "fmt", "leveldb": "LevelDB", "bitcoin": "Bitcoin Core", "ninja": "Ninja", "googletest": "GoogleTest", "abseil": "Abseil",
           "yaml-cpp": "yaml-cpp", "spdlog": "spdlog", "cppcheck": "Cppcheck"}
# Projects written in C++ (the rest are C), for the charts' groups.
CPP = {"fmt", "LevelDB", "Bitcoin Core", "Ninja", "GoogleTest", "Abseil", "yaml-cpp", "spdlog", "Cppcheck"}
# One colour per tool, readable on a light and a dark page alike.
COLORS = {"si": "#f0a830", "si-cfg": "#c98a1c", "clangd-db": "#2f5f9e", "clangd": "#4c8fd6", "cbm": "#8a6fd1", "cscope": "#4fae7a", "gtags": "#d0644f", "ctags": "#8c96a3"}
TEXT = "#7d8590"


def load(paths):
    """The results by project, in the order first given. Several files of
    one project (a tool measured in another run) are merged: a tool's
    result is taken from the first file that has it finished."""
    runs, by = [], {}
    for p in paths:
        d = json.loads(Path(p).read_text(encoding="utf-8"))
        c = d.get("corpus")
        name = c.get("name") if isinstance(c, dict) else Path(c or p).stem
        name = CORPORA.get(name, name)
        if name in by:
            have = by[name]["tools"]
            for t, r in d.get("tools", {}).items():
                if have.get(t, {}).get("status") != "ok":
                    have[t] = r
            continue
        by[name] = d
        runs.append((name, d))
    return runs


def tools_of(d):
    return [t for t in ORDER if t in d.get("tools", {}) and d["tools"][t].get("status") == "ok"]


def get(d, tool, *path):
    v = d["tools"].get(tool, {})
    for k in path:
        v = v.get(k) if isinstance(v, dict) else None
    return v if isinstance(v, (int, float)) else None


def dot_rows(title, subtitle, groups, tools, value, fmt, log=False, best_high=True):
    """A row per project, C then C++, and on it a dot per tool at its value:
    as many projects as there are fit one width. `value(d, tool)` or None;
    a tool with no value on a project has no dot there, and one with none
    anywhere is left out of the legend."""
    import math
    tools = [t for t in tools if any(value(d, t) is not None for _, d in groups)]
    rows = [("C", None)] + [(c, d) for c, d in groups if c not in CPP] + [("C++", None)] + [(c, d) for c, d in groups if c in CPP]
    rows = [r for i, r in enumerate(rows) if r[1] is not None or any(x[1] is not None for x in rows[i + 1:i + 2])]
    w, left, right, row_h = 760, 120, 70, 22
    # The legend, wrapped to the width: (tool, x, y of its line).
    legend, x, y = [], 16, 66
    for t in tools:
        width = 15 + 7.0 * len(NAMES[t]) + 18
        if x + width > w - 16 and x > 16:
            x, y = 16, y + 20
        legend.append((t, x, y))
        x += width
    top = y + 26
    h = top + row_h * len(rows) + 34
    vals = [v for _, d in groups if d for t in tools if (v := value(d, t)) is not None]
    if log:
        lo = 10 ** math.floor(math.log10(min([v for v in vals if v > 0] or [1])))
        hi = 10 ** math.ceil(math.log10(max(vals or [1])))
        frac = lambda v: (math.log10(max(v, lo)) - math.log10(lo)) / (math.log10(hi) - math.log10(lo))
        ticks = [lo * 10 ** k for k in range(round(math.log10(hi / lo)) + 1)]
    else:
        lo = min(0.5, math.floor(min(vals or [0]) * 10) / 10)
        hi = 1.0
        frac = lambda v: (v - lo) / (hi - lo)
        ticks = [lo + (hi - lo) * k / 5 for k in range(6)]
    x_of = lambda v: left + (w - left - right) * max(0.0, min(1.0, frac(v)))
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="-apple-system,Segoe UI,Helvetica,Arial,sans-serif">',
           f'<text x="16" y="24" font-size="18" font-weight="600" fill="{TEXT}">{html.escape(title)}</text>',
           f'<text x="16" y="44" font-size="12" fill="{TEXT}">{html.escape(subtitle)} {"Higher is better." if best_high else "Lower is better."}{" Log scale." if log else ""}</text>']
    for t, x, y in legend:
        out.append(f'<circle cx="{x + 6}" cy="{y}" r="5" fill="{COLORS[t]}"/>')
        out.append(f'<text x="{x + 15}" y="{y + 4}" font-size="12" fill="{TEXT}"{" font-weight=" + chr(34) + "700" + chr(34) if t == "si" else ""}>{html.escape(NAMES[t])}</text>')
    bottom = top + row_h * len(rows)
    for tk in ticks:
        x = x_of(tk)
        out.append(f'<line x1="{x:.1f}" x2="{x:.1f}" y1="{top - 4}" y2="{bottom}" stroke="{TEXT}" stroke-opacity="0.18"/>')
        out.append(f'<text x="{x:.1f}" y="{bottom + 16}" font-size="10" text-anchor="middle" fill="{TEXT}">{html.escape(short_secs(tk) if log else fmt(tk))}</text>')
    for i, (corpus, d) in enumerate(rows):
        y = top + row_h * i + row_h / 2
        if d is None:
            out.append(f'<text x="16" y="{y + 4:.1f}" font-size="11" font-weight="700" fill="{TEXT}">{html.escape(corpus)}</text>')
            continue
        out.append(f'<text x="{left - 10}" y="{y + 4:.1f}" font-size="12" text-anchor="end" fill="{TEXT}">{html.escape(corpus)}</text>')
        out.append(f'<line x1="{left}" x2="{w - right}" y1="{y:.1f}" y2="{y:.1f}" stroke="{TEXT}" stroke-opacity="0.08"/>')
        # (Norn last, on top; its value at the end of the row.)
        for t in sorted(tools, key=lambda t: t == "si"):
            v = value(d, t)
            if v is None:
                continue
            r = 5.5 if t == "si" else 4.5
            stroke = f' stroke="{TEXT}" stroke-width="1"' if t == "si" else ' fill-opacity="0.85"'
            out.append(f'<circle cx="{x_of(v):.1f}" cy="{y:.1f}" r="{r}" fill="{COLORS[t]}"{stroke}><title>{html.escape(NAMES[t])}: {html.escape(fmt(v))}</title></circle>')
        if (v := value(d, "si")) is not None:
            out.append(f'<text x="{w - right + 8}" y="{y + 4:.1f}" font-size="11" font-weight="700" fill="{TEXT}">{html.escape(fmt(v))}</text>')
    out.append("</svg>")
    return "\n".join(out)


def grouped_bars(title, subtitle, groups, tools, value, fmt, log=False, best_high=True):
    """Columns per tool, grouped by project along the bottom, as model
    evaluations are drawn: a legend of the tools on top, each column's value
    above it. `value(d, tool)` or None; tools with no value anywhere are
    left out, so a chart has room for as many projects as there are."""
    import math
    tools = [t for t in tools if any(value(d, t) is not None for _, d in groups)]
    col, inner, outer = 30, 4, 36
    left, right, top, plot_h, bottom = 46, 16, 100, 240, 46
    group_w = len(tools) * col + (len(tools) - 1) * inner
    w = max(760, left + right + len(groups) * group_w + (len(groups) - 1) * outer + 2 * outer)
    h = top + plot_h + bottom
    vals = [v for _, d in groups for t in tools if (v := value(d, t)) is not None]
    vmax = max(vals or [1]) or 1
    if log:
        lo = 10 ** math.floor(math.log10(min([v for v in vals if v > 0] or [1])))
        hi = 10 ** math.ceil(math.log10(vmax))
        frac = lambda v: (math.log10(max(v, lo)) - math.log10(lo)) / (math.log10(hi) - math.log10(lo))
        ticks = [lo * 10 ** k for k in range(round(math.log10(hi / lo)) + 1)]
    else:
        hi = 1.0 if vmax <= 1 else vmax
        frac = lambda v: v / hi
        ticks = [hi * k / 4 for k in range(5)]
    y_of = lambda f: top + plot_h * (1 - f)
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="-apple-system,Segoe UI,Helvetica,Arial,sans-serif">',
           f'<text x="{left}" y="24" font-size="18" font-weight="600" fill="{TEXT}">{html.escape(title)}</text>',
           f'<text x="{left}" y="44" font-size="12" fill="{TEXT}">{html.escape(subtitle)} {"Higher is better." if best_high else "Lower is better."}{" Log scale." if log else ""}</text>']
    # Legend.
    x = left
    for t in tools:
        out.append(f'<rect x="{x}" y="60" width="12" height="12" rx="2" fill="{COLORS[t]}"/>')
        out.append(f'<text x="{x + 17}" y="70" font-size="12" fill="{TEXT}"{" font-weight=" + chr(34) + "700" + chr(34) if t == "si" else ""}>{html.escape(NAMES[t])}</text>')
        x += 17 + 7.2 * len(NAMES[t]) + 20
    # Grid and its labels.
    for tk in ticks:
        y = y_of(frac(tk))
        out.append(f'<line x1="{left}" x2="{w - right}" y1="{y:.1f}" y2="{y:.1f}" stroke="{TEXT}" stroke-opacity="0.18"/>')
        out.append(f'<text x="{left - 6}" y="{y + 4:.1f}" font-size="10" text-anchor="end" fill="{TEXT}">{html.escape(fmt(tk) if not log else short_secs(tk))}</text>')
    x = left + outer
    for corpus, d in groups:
        for k, t in enumerate(tools):
            v = value(d, t)
            cx = x + k * (col + inner)
            if v is None:
                out.append(f'<text x="{cx + col / 2:.1f}" y="{top + plot_h - 6}" font-size="10" text-anchor="middle" fill="{TEXT}">–</text>')
                continue
            y = y_of(max(0.0, frac(v)))
            bh = max(1.5, top + plot_h - y)
            weight = ' font-weight="700"' if t == "si" else ""
            out.append(f'<rect x="{cx}" y="{top + plot_h - bh:.1f}" width="{col}" height="{bh:.1f}" rx="2" fill="{COLORS[t]}"/>')
            out.append(f'<text x="{cx + col / 2:.1f}" y="{top + plot_h - bh - 5:.1f}" font-size="10" text-anchor="middle" fill="{TEXT}"{weight}>{html.escape(fmt(v))}</text>')
        out.append(f'<text x="{x + group_w / 2:.1f}" y="{top + plot_h + 22}" font-size="13" font-weight="600" text-anchor="middle" fill="{TEXT}">{html.escape(corpus)}</text>')
        x += group_w + outer
    out.append(f'<line x1="{left}" x2="{w - right}" y1="{top + plot_h}" y2="{top + plot_h}" stroke="{TEXT}" stroke-opacity="0.5"/>')
    out.append("</svg>")
    return "\n".join(out)


def version(v):
    """A tool's version as it reports it, without its own name or a copyright."""
    import re
    if isinstance(v, dict):
        v = v.get("reported", "?")
    m = re.search(r"\d+\.\d+(\.\d+)?", str(v or ""))
    return m.group(0) if m else str(v)


def pct(v):
    return "–" if v is None else f"{v:.1%}"


def secs(v):
    return "–" if v is None else (f"{v:.2f} s" if v < 10 else f"{v:.0f} s")


def mb(v):
    return "–" if v is None else f"{v / 2**20:,.0f} MB"


def short_secs(v):
    return f"{v:.2f}" if v < 1 else f"{v:.1f}" if v < 10 else f"{v:.0f}"


def ms(v):
    return "–" if v is None else (f"{v:.2f} ms" if v < 10 else f"{v:,.0f} ms")


def write_summary(readme, out, runs, tools):
    """The charts and one table of calls found and time to index, written
    between a README's <!-- BENCHMARKS:BEGIN --> and <!-- BENCHMARKS:END -->."""
    rel = out.resolve().relative_to(readme.parent.resolve()).as_posix()
    block = [f'<p align="center"><img src="{rel}/calls-found.svg" alt="Calls found" width="760"></p>', "",
             f'<p align="center"><img src="{rel}/index-time.svg" alt="Time to index" width="760"></p>', "",
             f'<p align="center"><img src="{rel}/peak-memory.svg" alt="Peak memory" width="760"></p>', "",
             "| Project | Language | Files | " + " | ".join(NAMES[t] for t in tools) + " |",
             "|---|---|--:|" + "|".join(":-:" for _ in tools) + "|"]
    for corpus, d in runs:
        files = d["tools"].get("si", {}).get("counts", {}).get("files") or (d.get("corpus") or {}).get("c_files", 0) + (d.get("corpus") or {}).get("h_files", 0)
        cells = []
        for t in tools:
            c, w = get(d, t, "accuracy", "calls", "recall_any"), get(d, t, "index", "wall_s")
            cell = "–" if c is None and w is None else f"{pct(c) if c is not None else 'n/a'}<br><sub>{secs(w)}</sub>"
            cells.append(f"**{cell}**" if t == "si" and cell != "–" else cell)
        lang = "C++" if corpus in CPP else "C"
        block.append(f"| {corpus} | {lang} | {f'{files:,}' if files else '–'} | " + " | ".join(cells) + " |")
    zh = readme.name.endswith(".zh-TW.md")
    block += ["", "<sub>每格：找到的呼叫比例（和 clang 對照），下方為建立索引的時間。</sub>" if zh else
              "<sub>Each cell: the share of calls found (checked against clang), and below it the time to index.</sub>"]
    low = [c for c, d in runs if (r := get(d, "clangd", "accuracy", "calls", "recall_any")) is not None and r < 0.5]
    if "clangd" in tools and low:
        block += ["", f"<sub>clangd 在 {'、'.join(low)}：只給它沒有 include 路徑、沒有 <code>-D</code> 的條件（沒有 compilation database "
                  "的專案能給的就這些），需要 build 的標頭檔路徑或定義才能解析的原始碼，它大多解析不了。有真正 build 的 "
                  "database 時它會好很多，但這裡量的不是那種情況。</sub>" if zh else
                  f"<sub>clangd on {', '.join(low)}: given no include paths and no <code>-D</code>, as a project without a "
                  "compilation database would, most of the code that needs the build's include paths or definitions does not parse "
                  "for it. With a real build's database it does far better; that is not what is measured here.</sub>"]
    text = readme.read_text(encoding="utf-8")
    begin, end = "<!-- BENCHMARKS:BEGIN -->", "<!-- BENCHMARKS:END -->"
    i, j = text.index(begin) + len(begin), text.index(end)
    readme.write_text(text[:i] + "\n" + "\n".join(block) + "\n" + text[j:], encoding="utf-8")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results", nargs="+")
    ap.add_argument("--out", required=True)
    ap.add_argument("--readme", action="append", default=[], help="a README whose <!-- BENCHMARKS:BEGIN/END --> block is written with the summary")
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    runs = load(a.results)
    tools = [t for t in ORDER if t not in PAGE_ONLY and any(t in tools_of(d) for _, d in runs)]

    calls = lambda d, t: get(d, t, "accuracy", "calls", "recall_any")
    (out / "calls-found.svg").write_text(dot_rows(
        "Calls found (%)", "Of the calls in clang's own view of a real build, the share each tool reports.", runs,
        tools, calls, lambda v: f"{v * 100:.1f}"), encoding="utf-8")
    (out / "index-time.svg").write_text(dot_rows(
        "Time to index (seconds)", "A cold index of the whole project, wall-clock.", runs, tools,
        lambda d, t: get(d, t, "index", "wall_s"), short_secs, log=True, best_high=False), encoding="utf-8")
    peak = lambda d, t: (v / 2**20 if (v := get(d, t, "index", "peak_rss_bytes")) is not None else None)
    (out / "peak-memory.svg").write_text(dot_rows(
        "Peak memory while indexing (MB)", "The most memory each tool's indexing held at once.", runs, tools,
        peak, lambda v: f"{v:,.0f}", log=True, best_high=False), encoding="utf-8")

    m = runs[0][1].get("machine", {})
    # (Where Norn ran: the Windows machine, or Linux, as on EC2.)
    on_windows = any(str(d["tools"].get("si", {}).get("platform", "")).startswith("Windows") for _, d in runs)
    lines = ["# Benchmarks", "",
             "Norn against other ways of indexing C and C++, on public projects. Every number here is produced by",
             "`bench/compare.py` and turned into this page by `bench/chart.py`; nothing is typed in.", "",
             "![Calls found](calls-found.svg)", "", "![Time to index](index-time.svg)", "", "![Peak memory](peak-memory.svg)", "",
             "## How it is measured", "",
             "- **Ground truth**: clang's own view of each project, compiled with its real build",
             "  (`bench/gt_extract.py`): every function definition, and every call with the function it",
             "  reaches. A tool's *calls found* is the share of those calls it reports with the right target;",
             "  *precision* is the share of the calls it reports as certain that are right.",
             "- **Index**: an index of the whole project from nothing (the files already in the OS cache),",
             "  wall-clock, with peak memory and the size on disk.",
             "- **Update**: the time to bring the index up to date after one file is edited.",
             "- **Queries**: median latency of finding a symbol by name and of listing a function's callers,",
             "  each tool queried the way it is meant to be (a warm server where it has one).",
             "- **clangd** is given a compilation database with no include paths and no `-D`: what a project",
             "  without a working `compile_commands.json` gives it. With a real build's database it would be",
             "  slower to index and more accurate.",
             "- **clangd + build database** is clangd at its best: given each project's own `compile_commands.json`",
             *(["  (the one its ground truth was made from), on the tree that build was made in. It runs in WSL 2",
                "  (Linux, clangd 20) because those builds are Linux builds. It is the reference for what a",
                "  compiler-based index finds when it is told exactly how every file is compiled."] if on_windows else
               ["  (the one its ground truth was made from), on the tree that build was made in. It is the",
                "  reference for what a compiler-based index finds when it is told exactly how every file is",
                "  compiled."]),
             "- clangd's accuracy is measured on a sample of the functions clang saw (callers and callees of each",
             "  through its call hierarchy); the other tools' on every call.",
             "- ctags records definitions only, and GNU Global references to a name rather than calls from a",
             "  function, so neither has a figure for calls.",
             "- **Norn** is given nothing: no configuration, no include paths, no `-D`. Where a project's",
             "  `#if`s leave many calls uncertain without one (FreeRTOS, spdlog, Linux), it is measured again",
             "  given the build's configuration, as a user would give it (*Norn + build configuration*, on",
             "  those projects' pages only).",
             "- Norn marks a call it cannot be sure of (under an `#if` the configuration does not settle) as",
             "  uncertain. *Calls found* counts every call a tool reports; *of them certain* counts only the",
             "  ones it reports as certain, and *precision* is measured on those. The other tools mark none.",
             *(["- Norn, clangd and codebase-memory-mcp run on Windows (NTFS); ctags, cscope and GNU Global, which",
                "  are Unix tools, run in WSL 2 on a copy of each project on its ext4 file system."] if on_windows else
               ["- Every tool runs on the same Linux machine, on an ext4 file system."]),
             "- Each tool indexes each project three times; the median is reported.",
             "- To measure it all again from nothing, on EC2 or any Ubuntu 24.04: [bench/ec2](../../bench/ec2/README.md).", "",
             f"Machine: {m.get('cpu', '?')}, {m.get('physical_cores', '?')} cores, "
             f"{(m.get('ram_bytes') or 0) / 2**30:.0f} GB RAM, {m.get('os', '?')}. "
             f"Measured {runs[0][1].get('date', '?')[:10]}.", ""]
    for corpus, d in runs:
        ts = tools_of(d)
        if not ts:
            continue
        counts = d["tools"].get("si", {}).get("counts", {})
        lines += [f"## {corpus}", ""]
        if counts:
            lines += [f"{counts.get('files', 0):,} files, {counts.get('lines', 0):,} lines.", ""]
        lines += ["| Tool | Index | Peak memory | On disk | Update after an edit | Find | Callers | Definitions found | Calls found | of them certain | Call precision |",
                  "|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|"]
        for t in ts:
            q = lambda k: get(d, t, "query", k, "p50_ms")
            name = f"**{NAMES[t]}**" if t in ("si", "si-cfg") else NAMES[t]
            lines.append(f"| {name} | {secs(get(d, t, 'index', 'wall_s'))} | {mb(get(d, t, 'index', 'peak_rss_bytes'))} | "
                         f"{mb(get(d, t, 'index', 'disk_bytes'))} | {secs(get(d, t, 'incremental', 'wall_s'))} | {ms(q('find'))} | {ms(q('callers'))} | "
                         f"{pct(get(d, t, 'accuracy', 'definitions', 'recall'))} | {pct(calls(d, t))} | {pct(get(d, t, 'accuracy', 'calls', 'recall_certain'))} | "
                         f"{pct(get(d, t, 'accuracy', 'calls', 'precision_certain'))} |")
        failed = [t for t in ORDER if t in d.get("tools", {}) and d["tools"][t].get("status") not in ("ok", None) and "skipped" not in d["tools"][t]]
        for t in failed:
            # (A program's path as this machine has it, as its name.)
            reason = re.sub(r"(?:[A-Za-z]:)?[\\/]\S*[\\/]([^\\/\s]+)", r"\1", str(d["tools"][t].get("reason", "failed")))
            lines.append(f"\n{NAMES[t]}: did not finish ({reason}).")
        if (r := get(d, "clangd", "accuracy", "calls", "recall_any")) is not None and r < 0.5:
            lines += ["", "clangd is given no include paths and no `-D`, as a project without a compilation database would: "
                      "most of the code that needs the build's include paths or definitions does not parse for it. "
                      "With a real build's database it does far better; that is not what is measured here."]
        if (given := d["tools"].get("si-cfg", {}).get("given")) and "si-cfg" in ts:
            lines += ["", f"Norn + build configuration: Norn given {given}. Everywhere else Norn is given nothing."]
        versions = [f"{NAMES[t]} {version(d['tools'][t].get('version'))}" for t in ts if t not in ("si", "si-cfg")]
        if versions:
            lines += ["", "Versions: " + "; ".join(versions) + "."]
        lines.append("")
    (out / "README.md").write_text("\n".join(lines), encoding="utf-8")
    if a.readme:
        for r in a.readme:
            write_summary(Path(r), out, runs, tools)
    print(f"{out}: {len(runs)} projects, {len(tools)} tools")


if __name__ == "__main__":
    main()
