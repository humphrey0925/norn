// The extension as installed from its package (`node out/test/runTest.js
// packaged`, which unpacks ../out/norn.vsix and runs that): every file it
// makes a path to in its own folder while it starts and shows its views is
// there. Unlike package.ts this knows no list: whatever the code reaches
// for is checked, a file added later included.
import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function run(): Promise<void> {
  const ext = vscode.extensions.all.find((e) => e.packageJSON.name === "norn-c")!;
  const own = ext.extensionPath.toLowerCase();
  // Every path joined from a URI, as the views make theirs.
  const reached = new Set<string>();
  const joinPath = vscode.Uri.joinPath;
  vscode.Uri.joinPath = (base: vscode.Uri, ...parts: string[]) => {
    const uri = joinPath(base, ...parts);
    if (uri.scheme === "file" && uri.fsPath.toLowerCase().startsWith(own)) {
      reached.add(uri.fsPath);
    }
    return uri;
  };
  try {
    await ext.activate();
    const folder = vscode.workspace.workspaceFolders![0].uri.fsPath;
    await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(path.join(folder, "bfd", "opncls.c")));
    // Each view is made when it is first shown.
    for (const view of Object.values(ext.packageJSON.contributes.views as Record<string, { id: string }[]>).flat()) {
      await vscode.commands.executeCommand(`${view.id}.focus`);
      await sleep(500);
    }
    await sleep(2000);
  } finally {
    vscode.Uri.joinPath = joinPath;
  }
  const missing = [...reached].filter((p) => !fs.existsSync(p));
  assert.deepStrictEqual(missing.map((p) => path.relative(ext.extensionPath, p)), [], "the extension reaches for files its package does not have");
  for (const css of [...reached].filter((p) => p.endsWith(".css"))) {
    for (const m of fs.readFileSync(css, "utf8").matchAll(/url\(["']?([^"')?#]+)/g)) {
      assert.ok(fs.existsSync(path.join(path.dirname(css), m[1])), `${path.relative(ext.extensionPath, css)} loads ${m[1]}, which is not there`);
    }
  }
  // The views did reach for their files (or this proves nothing).
  assert.ok([...reached].some((p) => p.endsWith("codicon.css")) && [...reached].some((p) => p.endsWith("relation.js")), `views not shown: ${[...reached].join(", ")}`);
  const server = path.join(ext.extensionPath, "bin", "win32-x64", "norn.exe");
  assert.ok(process.platform !== "win32" || fs.existsSync(server), "no built-in server");
  console.log(`packaged: ${reached.size} paths in the extension's folder, all there`);
}
