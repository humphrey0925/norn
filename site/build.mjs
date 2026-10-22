// Builds the website into out/site: the pages in site/, with the icon,
// screenshots and benchmark charts copied from where the project keeps them,
// and the benchmark table's rows taken from the README's. Run by
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
for (const f of ["calls-found.svg", "index-time.svg"]) {
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
const columns = ["Project", "Files", "Norn", "clangd + build database", "clangd", "cscope"];
const at = columns.map((name) => {
  const i = header.indexOf(name);
  if (i < 0) throw new Error(`README.md: the benchmark table has no column "${name}"`);
  return i;
});
if (rows.length === 0) throw new Error("README.md: the benchmark table has no rows");

function result(cell, norn) {
  const m = cell.replace(/\*\*/g, "").match(/^(.*?)<br><sub>(.*?)<\/sub>$/);
  if (!m) throw new Error(`README.md: a benchmark cell does not read "x%<br><sub>t</sub>": ${cell}`);
  const share = norn ? `<b>${m[1]}</b>` : m[1];
  return `<td${norn ? ' class="norn"' : ""}>${share}<sub>${m[2]}</sub></td>`;
}
const tbody = rows
  .map((r) => {
    const [project, files, ...tools] = at.map((i) => r[i]);
    const tds = tools.map((c, k) => result(c, k === 0)).join("");
    return `          <tr><td>${project}</td><td>${files}</td>${tds}</tr>`;
  })
  .join("\n");

for (const page of ["index.html", join("zh-TW", "index.html")]) {
  const path = join(out, page);
  const html = readFileSync(path, "utf8");
  const marked = /(<!-- BENCHMARKS:BEGIN -->\n)[\s\S]*?(\n\s*<!-- BENCHMARKS:END -->)/;
  if (!marked.test(html)) throw new Error(`site/${page}: no <!-- BENCHMARKS:BEGIN --> ... END in the table`);
  writeFileSync(path, html.replace(marked, `$1${tbody}$2`));
}

console.log(`out/site: ${rows.length} benchmark rows, ${readdirSync(join(assets, "screenshots")).length} screenshots`);
