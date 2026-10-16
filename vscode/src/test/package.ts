// What the extension needs when installed is in the package: `node
// out/test/package.js [file.vsix]` (default ../out/norn.vsix), no VS Code
// needed. The package is made by rules of its own (.vscodeignore, the
// bundling), and a file left out of it is missing only once installed:
// tests run from this folder have everything. (0.19.0 to 0.19.3 left out
// the icon font, and the views showed no icons.)
import * as assert from "assert";
import * as path from "path";
import * as fs from "fs";
import { builtinModules } from "module";
import { entries, read } from "./zip";

export function check(vsix: string): string[] {
  const bytes = fs.readFileSync(vsix);
  const all = entries(bytes);
  const names = new Set(all.map((e) => e.name));
  const text = (name: string) => read(bytes, all.find((e) => e.name === name)!).toString("utf8");
  const has = (rel: string) => names.has("extension/" + path.posix.normalize(rel.replace(/^\.\//, "")));
  const missing: string[] = [];
  const need = (rel: string, why: string) => {
    if (!has(rel)) {
      missing.push(`${rel} (${why})`);
    }
  };

  assert.ok(names.has("extension/package.json"), "no package.json in the package");
  const manifest = JSON.parse(text("extension/package.json"));
  need(manifest.main, "main");
  need(manifest.icon, "icon");
  // Every icon given as a file anywhere in what the extension contributes.
  const icons = (v: unknown, where: string): void => {
    if (Array.isArray(v)) {
      v.forEach((x, i) => icons(x, `${where}[${i}]`));
    } else if (v && typeof v === "object") {
      for (const [k, x] of Object.entries(v)) {
        if (k === "icon" && typeof x === "string" && !x.startsWith("$(")) {
          need(x, `${where}.icon`);
        } else if (k === "icon" && x && typeof x === "object") {
          Object.values(x).forEach((p) => typeof p === "string" && need(p, `${where}.icon`));
        } else {
          icons(x, `${where}.${k}`);
        }
      }
    }
  };
  icons(manifest.contributes, "contributes");

  // The server it starts: one, for the platform the package is for.
  const servers = all.map((e) => e.name).filter((n) => /^extension\/bin\/(win32|linux|alpine|darwin)-(x64|arm64)\/norn(\.exe)?$/.test(n));
  if (servers.length !== 1) {
    missing.push(`bin/<platform>/norn (the built-in server: ${servers.length} found)`);
  }
  // What others made that it includes, with their licenses.
  need("THIRD_PARTY_NOTICES.txt", "the third-party notices");

  // The icon font the views link, and what its stylesheet loads in turn.
  const css = "node_modules/@vscode/codicons/dist/codicon.css";
  need(css, "the views' icons");
  if (has(css)) {
    for (const m of text("extension/" + css).matchAll(/url\(["']?([^"')?#]+)/g)) {
      need(path.posix.join(path.posix.dirname(css), m[1]), "loaded by codicon.css");
    }
  }

  // The bundle requires nothing that is not there at run time: vscode
  // itself and Node's own modules. (Anything else was to be bundled.)
  const main = "extension/" + path.posix.normalize(manifest.main.replace(/^\.\//, ""));
  const builtin = new Set([...builtinModules, ...builtinModules.map((m) => "node:" + m), "vscode"]);
  for (const m of text(main).matchAll(/\brequire\(\s*["']([^"']+)["']\s*\)/g)) {
    if (!builtin.has(m[1]) && !m[1].startsWith(".")) {
      missing.push(`${m[1]} (required by ${manifest.main}, not bundled)`);
    }
  }
  return missing;
}

if (require.main === module) {
  const vsix = path.resolve(process.argv[2] ?? path.join(__dirname, "../../../out/norn.vsix"));
  const missing = check(vsix);
  if (missing.length) {
    console.error(`${vsix} lacks what the extension needs once installed:\n  ` + missing.join("\n  "));
    process.exit(1);
  }
  console.log(`${path.basename(vsix)}: everything the extension needs is in it`);
}
