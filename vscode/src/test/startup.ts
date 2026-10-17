// How long Norn takes to be of use when VS Code opens a folder, and what
// it does after: `node out/test/runTest.js startup <folder>` (see
// runTest.ts), with the server writing what it is asked to NORN_TRACE.
// Waits for the index, then a while longer, to catch work done after it
// (a sync nobody asked for, say).
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function run(): Promise<void> {
  const trace = process.env.NORN_TRACE!;
  // When the extension host was there to run anything, for comparison.
  fs.writeFileSync(trace + ".host", String(Date.now() - Math.round(process.uptime() * 1000)));
  const ext = vscode.extensions.all.find((e) => e.packageJSON.name === "norn-c")!;
  await ext.activate();
  for (let i = 0; i < 3000 && !(fs.existsSync(trace) && fs.readFileSync(trace, "utf8").includes("index ready")); i++) {
    await sleep(100);
  }
  // What one does first: open a source file and ask about it.
  const folder = vscode.workspace.workspaceFolders![0].uri.fsPath;
  const first = process.env.NORN_STARTUP_FILE;
  if (first) {
    await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(path.join(folder, first)));
  }
  await sleep(Number(process.env.NORN_STARTUP_LINGER ?? 20000));
}
