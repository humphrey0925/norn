// The extension for one platform, with the server built for it inside:
//   node release/package.js <vscode target> <server binary> [out dir]
// e.g. `node release/package.js linux-arm64 dist/norn-linux-arm64/norn`.
// Writes <out dir>/norn-<version>-<target>.vsix (default out/) and checks
// that what the extension needs once installed is in it. VS Code installs
// the package made for the platform it runs on; each has its own server.
"use strict";
const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");

/** VS Code's names of platforms, and Rust's of what to build for them. */
const RUST = {
  "win32-x64": "x86_64-pc-windows-msvc",
  "win32-arm64": "aarch64-pc-windows-msvc",
  "linux-x64": "x86_64-unknown-linux-musl",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "alpine-x64": "x86_64-unknown-linux-musl",
  "alpine-arm64": "aarch64-unknown-linux-musl",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
};

const [target, server, outDir = "out"] = process.argv.slice(2);
if (!RUST[target] || !server) {
  console.error(`usage: node release/package.js <${Object.keys(RUST).join("|")}> <server binary> [out dir]`);
  process.exit(2);
}
const root = path.resolve(__dirname, "..");
const ext = path.join(root, "vscode");
const version = JSON.parse(fs.readFileSync(path.join(ext, "package.json"), "utf8")).version;
const run = (cmd, args, cwd = root, env = {}) =>
  execFileSync(cmd, args, { cwd, stdio: "inherit", shell: process.platform === "win32", env: { ...process.env, ...env } });

// The server, under the name of the platform it runs on.
const exe = target.startsWith("win32") ? "norn.exe" : "norn";
fs.rmSync(path.join(ext, "bin"), { recursive: true, force: true });
fs.mkdirSync(path.join(ext, "bin", target), { recursive: true });
fs.copyFileSync(path.resolve(server), path.join(ext, "bin", target, exe));
fs.chmodSync(path.join(ext, "bin", target, exe), 0o755);

// The change log, the license and the notices go in too (the notices of
// the crates linked for this platform).
for (const f of ["CHANGELOG.md", "LICENSE", "NOTICE"]) {
  fs.copyFileSync(path.join(root, f), path.join(ext, f));
}
run("node", [path.join(root, "release", "notices.js"), path.join(ext, "THIRD_PARTY_NOTICES.txt")], root, { NORN_TARGET: RUST[target] });

const out = path.resolve(root, outDir, `norn-${version}-${target}.vsix`);
fs.mkdirSync(path.dirname(out), { recursive: true });
run("npx", ["vsce", "package", "--target", target, "-o", out], ext);
run("node", [path.join(ext, "out", "test", "package.js"), out]);
console.log(out);
