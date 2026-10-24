// Builds the website into out/site: the pages in site/, with the icon,
// screenshots and benchmark charts copied from where the project keeps them,
// the benchmark table's rows taken from the README's, and the version from
// the extension's package.json. Run by
// `wrangler deploy` and `wrangler dev` (wrangler.jsonc) and by CI.
//
//   node site/build.mjs

import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const src = join(root, "site");
const out = join(root, "out", "site");

rmSync(out, { recursive: true, force: true });
cpSync(src, out, { recursive: true, filter: (p) => !p.endsWith("build.mjs") });

const assets = join(out, "assets");
mkdirSync(join(assets, "screenshots"), { recursive: true });
for (const f of ["icon.svg", "icon.png"]) {
  cpSync(join(root, "vscode", "media", f), join(assets, f));
}
const shots = join(root, "vscode", "media", "screenshots");
for (const f of readdirSync(shots).filter((f) => f.endsWith(".png"))) {
  cpSync(join(shots, f), join(assets, "screenshots", f));
}
const charts = ["calls-found.svg", "index-time.svg", "peak-memory.svg"];
for (const f of charts) {
  cpSync(join(root, "docs", "benchmarks", f), join(assets, f));
}

// The README's benchmark table, between <!-- BENCHMARKS:BEGIN --> and END.
// Each tool's cell reads `98.0%<br><sub>0.84 s</sub>`, bold for Norn.
const readme = readFileSync(join(root, "README.md"), "utf8");
const block = readme.split("<!-- BENCHMARKS:BEGIN -->")[1]?.split("<!-- BENCHMARKS:END -->")[0];
if (!block) throw new Error("README.md: no <!-- BENCHMARKS:BEGIN --> ... END block");
const lines = block.split("\n").filter((l) => l.startsWith("|"));
const cells = (l) => l.slice(1, -1).split("|").map((c) => c.trim());
const header = cells(lines[0]);
const rows = lines.slice(2).map(cells);
const columns = ["Project", "Language", "Files", "Norn", "clangd + build database", "clangd", "cscope"];
const at = columns.map((name) => {
  const i = header.indexOf(name);
  if (i < 0) throw new Error(`README.md: the benchmark table has no column "${name}"`);
  return i;
});
if (rows.length === 0) throw new Error("README.md: the benchmark table has no rows");

// A tool's cell as (share of calls found, seconds to index); null where it has none.
function parse(cell) {
  const m = cell.replace(/\*\*/g, "").match(/^(.*?)<br><sub>(.*?)<\/sub>$/);
  if (!m) {
    if (cell === "–") return null;
    throw new Error(`README.md: a benchmark cell does not read "x%<br><sub>t</sub>": ${cell}`);
  }
  const share = parseFloat(m[1]);
  const secs = parseFloat(m[2]);
  return { share: Number.isNaN(share) ? null : share, shareText: m[1], secs, secsText: m[2] };
}
const projects = rows.map((r) => {
  const [project, language, files, ...tools] = at.map((i) => r[i]);
  return { project, language, files, tools: tools.map(parse) };
});

// A cell with a bar under it for the share found, so a column reads at a glance.
function td(v, norn) {
  if (!v) return `<td${norn ? ' class="norn"' : ""}>–</td>`;
  const share = norn ? `<b>${v.shareText}</b>` : v.shareText;
  const bar = v.share == null ? "" : `<span class="bar" style="width:${v.share}%"></span>`;
  return `<td class="cell${norn ? " norn" : ""}">${share}<sub>${v.secsText}</sub>${bar}</td>`;
}

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};
const norn = projects.map((p) => p.tools[0]).filter(Boolean);
const faster = projects.filter((p) => p.tools[0] && p.tools[1]).map((p) => p.tools[1].secs / p.tools[0].secs);
const linux = projects.find((p) => p.project === "Linux")?.tools[0];
const count = (lang) => projects.filter((p) => p.language === lang).length;
const stats = {
  projects: projects.length,
  c: count("C"),
  cpp: count("C++"),
  found: `${median(norn.map((v) => v.share)).toFixed(1)}%`,
  faster: `${median(faster).toFixed(1)}×`,
  linux: linux?.secsText,
};

const text = {
  en: {
    groups: { C: "C", "C++": "C++" },
    stats: [
      [String(stats.projects), `public projects: ${stats.c} in C, ${stats.cpp} in C++`],
      [stats.found, "of the calls clang compiles, found by Norn given nothing (median)"],
      [stats.faster, "faster to index than clangd given each build's own database (median)"],
      ...(stats.linux ? [[stats.linux, "to index the whole Linux kernel"]] : []),
    ],
  },
  zh: {
    groups: { C: "C 專案", "C++": "C++ 專案" },
    stats: [
      [String(stats.projects), `個公開專案：C ${stats.c} 個、C++ ${stats.cpp} 個`],
      [stats.found, "clang 實際編譯的呼叫中，什麼都不給的 Norn 找到的比例（中位數）"],
      [stats.faster, "建立索引的速度，相對於給了建置資料庫的 clangd（中位數）"],
      ...(stats.linux ? [[stats.linux, "為整個 Linux 核心建立索引"]] : []),
    ],
  },
};

function tbody(t) {
  const out = [];
  for (const lang of ["C", "C++"]) {
    const group = projects.filter((p) => p.language === lang);
    if (group.length === 0) continue;
    out.push(`          <tr class="group"><th colspan="6">${t.groups[lang]}</th></tr>`);
    for (const p of group) {
      out.push(`          <tr><td>${p.project}</td><td>${p.files}</td>${p.tools.map((v, k) => td(v, k === 0)).join("")}</tr>`);
    }
  }
  return out.join("\n");
}
const statsHtml = (t) => t.stats.map(([n, label]) => `      <div class="stat"><b>${n}</b><span>${label}</span></div>`).join("\n");

// Each chart's size, from its own <svg>, so the page does not move as it loads.
const sizes = Object.fromEntries(
  charts.map((f) => {
    const svg = readFileSync(join(assets, f), "utf8").slice(0, 400);
    const w = svg.match(/width="(\d+)"/)?.[1], h = svg.match(/height="(\d+)"/)?.[1];
    return [f, w && h ? ` width="${w}" height="${h}"` : ""];
  }),
);

for (const [page, t] of [["index.html", text.en], [join("zh-TW", "index.html"), text.zh]]) {
  const path = join(out, page);
  let html = readFileSync(path, "utf8").replace(/\r\n/g, "\n");
  for (const [name, fill] of [["BENCHMARKS", tbody(t)], ["BENCH-STATS", statsHtml(t)]]) {
    const marked = new RegExp(`(<!-- ${name}:BEGIN -->\\n)[\\s\\S]*?(\\n\\s*<!-- ${name}:END -->)`);
    if (!marked.test(html)) throw new Error(`site/${page}: no <!-- ${name}:BEGIN --> ... END`);
    html = html.replace(marked, `$1${fill}$2`);
  }
  for (const f of charts) {
    html = html.replaceAll(`<img src="/assets/${f}"`, `<img src="/assets/${f}"${sizes[f]}`);
  }
  writeFileSync(path, html);
}

// The version in the pages' structured data, from the extension's.
const { version } = JSON.parse(readFileSync(join(root, "vscode", "package.json"), "utf8"));
for (const page of ["index.html", join("zh-TW", "index.html")]) {
  const path = join(out, page);
  writeFileSync(path, readFileSync(path, "utf8").replaceAll("__NORN_VERSION__", version));
}

console.log(`out/site: version ${version}, ${projects.length} benchmark rows, ${readdirSync(join(assets, "screenshots")).length} screenshots`);
