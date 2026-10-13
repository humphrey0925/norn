// Not a test: arranges the views on a real file and holds the window open
// so a screenshot can be taken. `node out/test/runTest.js demo`.
import * as path from "path";
import * as vscode from "vscode";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function run(): Promise<void> {
  const folder = vscode.workspace.workspaceFolders![0].uri.fsPath;
  const doc = await vscode.workspace.openTextDocument(path.join(folder, "bfd", "bfdio.c"));
  const editor = await vscode.window.showTextDocument(doc);
  await vscode.extensions.all.find((e) => e.packageJSON.name === "si-code-index")!.activate();
  for (let i = 0; i < 60; i++) {
    const s = await vscode.commands.executeCommand<vscode.SymbolInformation[]>("vscode.executeWorkspaceSymbolProvider", "bfd_read");
    if (s?.length) break;
    await sleep(200);
  }
  await vscode.commands.executeCommand("workbench.action.closeAuxiliaryBar");
  await vscode.commands.executeCommand("si.search.focus");
  await vscode.commands.executeCommand("si.relation.focus");
  await vscode.commands.executeCommand("si.context.focus");
  const at = doc.positionAt(doc.getText().indexOf(process.env.SI_DEMO_NEEDLE ?? "iovec->bread (abfd, ptr") + Number(process.env.SI_DEMO_OFFSET ?? 8));
  await vscode.window.showTextDocument(doc, { selection: new vscode.Range(at, at) });
  editor.revealRange(new vscode.Range(at, at), vscode.TextEditorRevealType.InCenter);
  await sleep(1500);
  if (process.env.SI_DEMO_SCOPE) {
    await vscode.commands.executeCommand("si.scope.clearAll");
    await vscode.commands.executeCommand("workbench.actions.treeView.si.fileSymbols.collapseAll");
    await vscode.commands.executeCommand("si.scope.focus");
  }
  if (process.env.SI_DEMO_CONDITIONS) {
    const fs = await import("fs");
    fs.writeFileSync(path.join(folder, ".cproject"), `<?xml version="1.0"?><cproject>
      <configuration name="Debug"><folderInfo><toolChain><tool><option valueType="definedSymbols"><listOptionValue value="DEBUG"/><listOptionValue value="BOARD_REV=3"/><listOptionValue value="USE_RTOS"/><listOptionValue value="HAVE_MMAP=1"/></option>
      <option valueType="undefDefinedSymbols"><listOptionValue value="NDEBUG"/></option></tool></toolChain></folderInfo></configuration>
      <configuration name="Release"><folderInfo><toolChain><tool><option valueType="definedSymbols"><listOptionValue value="NDEBUG"/><listOptionValue value="BOARD_REV=3"/></option></tool></toolChain></folderInfo></configuration></cproject>`);
    await vscode.commands.executeCommand("si.sync");
    await vscode.commands.executeCommand("si.conditions.switch", ".cproject#Release");
    await vscode.commands.executeCommand("si.conditions.switch", ".cproject#Debug");
    await vscode.commands.executeCommand("si.conditions.define", "ARCH_SIZE=64");
    await vscode.commands.executeCommand("si.conditions.undefine", "_WIN32");
    await vscode.commands.executeCommand("si.conditions.assume", "CONFIG_");
    await vscode.commands.executeCommand("workbench.actions.treeView.si.fileSymbols.collapseAll");
    await vscode.commands.executeCommand("si.conditions.focus");
    if (process.env.SI_DEMO_CONDITIONS === "pick") {
      void vscode.commands.executeCommand("si.conditions.switch");
    }
  }
  if (process.env.SI_DEMO_SEARCH) {
    // Nothing to type into a webview from here; the list is shown by the
    // check script's own query. Left for manual runs.
  }
  if (process.env.SI_DEMO_VIEW === "graph") {
    await vscode.commands.executeCommand("si.relation.graph");
  }
  if (process.env.SI_DEMO_MODE) {
    await vscode.commands.executeCommand("si.relation." + process.env.SI_DEMO_MODE);
  }
  await sleep(Number(process.env.SI_DEMO_HOLD_MS ?? 12000));
}
