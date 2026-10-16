// The server as released: `cargo build --release`, with the paths of the
// machine it is built on left out of the binary.
//   node release/build.js [rust target] [--zig]
// Rust writes where sources are into a binary (for panic messages): the
// crates under the builder's home (`C:\Users\<name>\.cargo\registry\...`),
// the standard library under `.rustup`, the checkout. Each is written as a
// name of no one's machine instead: /cargo, /rustup, /norn.
"use strict";
const { execFileSync } = require("child_process");
const os = require("os");
const path = require("path");

const args = process.argv.slice(2);
const zig = args.includes("--zig");
const target = args.find((a) => !a.startsWith("--"));
const root = path.resolve(__dirname, "..");
const home = os.homedir();
const prefixes = [
  [process.env.CARGO_HOME || path.join(home, ".cargo"), "/cargo"],
  [process.env.RUSTUP_HOME || path.join(home, ".rustup"), "/rustup"],
  [root, "/norn"],
];
const remap = prefixes.flatMap(([from, to]) => [`--remap-path-prefix=${from}=${to}`]);
// (`/PDBALTPATH`: the debug file named without its folder; MSVC only.)
const msvc = (target ?? (process.platform === "win32" ? "msvc" : "")).includes("msvc");
// (Passed one by one, not split at spaces: a home folder may have one.)
const flags = [...(process.env.RUSTFLAGS ?? "").split(" "), ...remap, ...(msvc ? ["-Clink-arg=/PDBALTPATH:%_PDB%"] : [])].filter((f) => f).join("\x1f");
// The C the crates compile (tree-sitter's) names its files too.
const cflags = msvc ? "" : prefixes.map(([from, to]) => `-ffile-prefix-map=${from}=${to}`).join(" ");
const cargo = ["build", "--release", "--locked", ...(target ? ["--target", target] : [])];
if (zig) {
  cargo[0] = "zigbuild";
}
execFileSync("cargo", cargo, {
  cwd: root,
  stdio: "inherit",
  env: { ...process.env, RUSTFLAGS: "", CARGO_ENCODED_RUSTFLAGS: flags, ...(cflags ? { CFLAGS: `${process.env.CFLAGS ?? ""} ${cflags}`.trim() } : {}) },
});
