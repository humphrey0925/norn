// With the C/C++ extension installed and SI its configuration provider:
// what SI's conditions say reaches IntelliSense, and again when they
// change, whether through SI's commands or by hand in `.si/config.json`.
// `node out/test/runTest.js cpp` (see runTest.ts).
import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function until<T>(what: string, f: () => T | undefined | false): Promise<T> {
  for (let i = 0; i < 300; i++) {
    const v = f();
    if (v) {
      return v;
    }
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${what}`);
}

export async function run(): Promise<void> {
  const folder = vscode.workspace.workspaceFolders![0].uri.fsPath;
  assert.ok(vscode.extensions.getExtension("ms-vscode.cpptools"), "the C/C++ extension is not installed in the test's extensions folder");
  const doc = await vscode.workspace.openTextDocument(path.join(folder, "bfd", "bfdio.c"));
  await vscode.window.showTextDocument(doc);
  const ext = vscode.extensions.all.find((e) => e.packageJSON.name === "si-code-index")!;
  await ext.activate();
  // The same module the extension host loaded: its probe is the extension's.
  // eslint-disable-next-line @typescript-eslint/no-var-requires
  const { cppProbe } = require("../extension") as { cppProbe: { asked: number; defines: string[]; includePath: string[] } };

  // A set of conditions in force, made through SI's own command.
  await vscode.commands.executeCommand("si.conditions.define", "ZZ_FIRST=1");
  await until("the C/C++ extension to ask SI for the file's configuration", () => cppProbe.defines.includes("ZZ_FIRST=1"));
  assert.ok(cppProbe.includePath.some((p) => p.replace(/\\/g, "/").endsWith("/include")), `header folders are given: ${cppProbe.includePath}`);

  // Changed through SI: asked again, and given the new value.
  let asked = cppProbe.asked;
  await vscode.commands.executeCommand("si.conditions.define", "ZZ_SECOND=2");
  await until("the changed conditions to reach the C/C++ extension", () => cppProbe.asked > asked && cppProbe.defines.includes("ZZ_SECOND=2"));

  // Changed by hand in the file: taken up without a sync being asked for.
  asked = cppProbe.asked;
  const file = path.join(folder, ".si", "config.json");
  const config = JSON.parse(fs.readFileSync(file, "utf8"));
  const active = config.activeConfiguration as string;
  config.configurations[active].defines.ZZ_BY_HAND = "3";
  fs.writeFileSync(file, JSON.stringify(config, null, 2));
  await until("a hand edit of .si/config.json to reach the C/C++ extension", () => cppProbe.asked > asked && cppProbe.defines.includes("ZZ_BY_HAND=3"));
}
