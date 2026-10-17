// Runs the extension in a real VS Code against a scratch copy of a few
// binutils files: `npm test` (needs ../corpus/c-only and ../target/release/si).
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { downloadAndUnzipVSCode, runTests } from "@vscode/test-electron";
import { extract } from "./zip";

/** The VS Code to test in. On macOS its program was `Electron` and is
 *  `Code` from 1.142, which @vscode/test-electron 2 does not know of. */
async function vscode(): Promise<string> {
  const exe = await downloadAndUnzipVSCode();
  const code = path.join(path.dirname(exe), "Code");
  return process.platform === "darwin" && !fs.existsSync(exe) && fs.existsSync(code) ? code : exe;
}

/** `startup <folder>`: VS Code opening a folder of one's own, as it is
 *  (its `.norn` included), Norn's server from this build set in a user
 *  settings folder of the test's own, not in the folder. */
async function startup(folder: string) {
  const root = path.resolve(__dirname, "../../..");
  const user = fs.mkdtempSync(path.join(os.tmpdir(), "norn-startup-"));
  fs.mkdirSync(path.join(user, "User"), { recursive: true });
  const server = path.join(root, "target", "release", process.platform === "win32" ? "norn.exe" : "norn");
  fs.writeFileSync(path.join(user, "User", "settings.json"), JSON.stringify({ "norn.serverPath": server, "workbench.startupEditor": "none" }));
  const t = Date.now();
  console.log(`launched at ${t} ms since 1970`);
  try {
    await runTests({
      vscodeExecutablePath: await vscode(),
      extensionDevelopmentPath: path.resolve(__dirname, "../.."),
      extensionTestsPath: path.resolve(__dirname, "./startup"),
      // NORN_STARTUP_CPP: with the C/C++ extension too (see `cpp` below).
      launchArgs: [folder, "--user-data-dir", user, ...(process.env.NORN_STARTUP_CPP ? ["--extensions-dir", path.resolve(__dirname, "../../.vscode-test/extensions-cpp")] : ["--disable-extensions"])],
    });
  } finally {
    console.log(`VS Code ran ${((Date.now() - t) / 1000).toFixed(1)} s`);
    fs.rmSync(user, { recursive: true, force: true });
  }
}

async function main() {
  if (process.argv[2] === "startup") {
    return startup(path.resolve(process.argv[3]));
  }
  const root = path.resolve(__dirname, "../../..");
  // `packaged [file.vsix]`: the extension as its package installs it, its
  // own server included (see packaged.ts).
  const packaged = process.argv[2] === "packaged";
  let extension = path.resolve(__dirname, "../..");
  if (packaged) {
    const unpacked = fs.mkdtempSync(path.join(os.tmpdir(), "norn-vsix-"));
    extract(path.resolve(process.argv[3] ?? path.join(root, "out", "norn.vsix")), unpacked);
    extension = path.join(unpacked, "extension");
  }
  const corpus = path.join(root, "corpus", "c-only");
  // For pictures (`demo`), a folder with a name fit to be seen in the
  // window's title, and the whole of two directories in it.
  const demo = process.argv[2] === "demo";
  const made = fs.mkdtempSync(path.join(os.tmpdir(), "si-vscode-"));
  const work = demo ? path.join(made, "norn-demo") : made;
  fs.mkdirSync(work, { recursive: true });
  if (demo) {
    for (const dir of ["bfd", "include"]) {
      fs.cpSync(path.join(corpus, dir), path.join(work, dir), { recursive: true });
    }
  }
  for (const rel of demo ? [] : ["bfd/opncls.c", "bfd/bfd-in2.h", "bfd/libbfd.c", "bfd/libbfd.h", "bfd/cache.c", "bfd/bfdio.c", "include/ansidecl.h"]) {
    fs.mkdirSync(path.dirname(path.join(work, rel)), { recursive: true });
    fs.copyFileSync(path.join(corpus, rel), path.join(work, rel));
  }
  // And a little C++.
  if (!demo) {
    fs.mkdirSync(path.join(work, "cpp"));
    for (const [name, text] of Object.entries(CPP)) {
      fs.writeFileSync(path.join(work, "cpp", name), text);
    }
  }
  fs.mkdirSync(path.join(work, ".vscode"));
  // A scope on file: the first-time question is for people, not for a test.
  fs.mkdirSync(path.join(work, ".norn"));
  fs.writeFileSync(path.join(work, ".norn", "config.json"), JSON.stringify({ include: [], exclude: [] }));
  const server = path.join(root, "target", "release", process.platform === "win32" ? "norn.exe" : "norn");
  // `cpp`: with the C/C++ extension (installed into the test's own
  // extensions folder: `node out/test/runTest.js cpp`), Norn its provider.
  const cpp = process.argv[2] === "cpp";
  const withCpp = cpp ? { "C_Cpp.default.configurationProvider": "humphrey0925.norn-c" } : {};
  fs.writeFileSync(path.join(work, ".vscode", "settings.json"), JSON.stringify({ ...withCpp, ...(process.env.SI_TEST_BUNDLED || packaged ? {} : { "norn.serverPath": `"${server}"` }), ...(process.env.SI_DEMO_THEME ? { "workbench.colorTheme": process.env.SI_DEMO_THEME } : {}) }));
  try {
    await runTests({
      vscodeExecutablePath: await vscode(),
      extensionDevelopmentPath: extension,
      extensionTestsPath: path.resolve(__dirname, process.argv[2] === "demo" ? "./demo" : cpp ? "./cpp" : packaged ? "./packaged" : "./suite"),
      launchArgs: cpp ? [work, "--extensions-dir", path.resolve(__dirname, "../../.vscode-test/extensions-cpp")] : [work, "--disable-extensions"],
    });
  } finally {
    fs.rmSync(made, { recursive: true, force: true });
    if (packaged) {
      fs.rmSync(path.dirname(extension), { recursive: true, force: true });
    }
  }
}

/** A class, a virtual method defined apart, a call through a reference. */
const CPP: Record<string, string> = {
  "shapes.h": `#pragma once
namespace geo {
class Shape {
 public:
  virtual ~Shape() = default;
  virtual double Area() const = 0;
  double Twice() const { return 2 * Area(); }
};
class Circle : public Shape {
 public:
  explicit Circle(double r) : r_(r) {}
  double Area() const override;
 private:
  double r_;
};
}  // namespace geo
`,
  "shapes.cc": `#include "shapes.h"
namespace geo {
double Circle::Area() const { return 3.14 * r_ * r_; }
}  // namespace geo
`,
  "main.cc": `#include "shapes.h"
double Total(const geo::Shape& s) { return s.Area() + s.Twice(); }
`,
};

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
