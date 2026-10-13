// Runs the extension in a real VS Code against a scratch copy of a few
// binutils files: `npm test` (needs ../corpus/c-only and ../target/release/si).
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { runTests } from "@vscode/test-electron";

async function main() {
  const root = path.resolve(__dirname, "../../..");
  const corpus = path.join(root, "corpus", "c-only");
  const work = fs.mkdtempSync(path.join(os.tmpdir(), "si-vscode-"));
  for (const rel of ["bfd/opncls.c", "bfd/bfd-in2.h", "bfd/libbfd.c", "bfd/libbfd.h", "bfd/cache.c", "bfd/bfdio.c", "include/ansidecl.h"]) {
    fs.mkdirSync(path.dirname(path.join(work, rel)), { recursive: true });
    fs.copyFileSync(path.join(corpus, rel), path.join(work, rel));
  }
  fs.mkdirSync(path.join(work, ".vscode"));
  const server = path.join(root, "target", "release", process.platform === "win32" ? "si.exe" : "si");
  fs.writeFileSync(path.join(work, ".vscode", "settings.json"), JSON.stringify({ "si.serverPath": `"${server}"` }));
  try {
    await runTests({
      extensionDevelopmentPath: path.resolve(__dirname, "../.."),
      extensionTestsPath: path.resolve(__dirname, process.argv[2] === "demo" ? "./demo" : "./suite"),
      launchArgs: [work, "--disable-extensions"],
    });
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
