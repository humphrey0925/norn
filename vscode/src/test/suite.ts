// What the extension has to do inside the editor, checked there.
import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function until<T>(what: string, f: () => Promise<T | undefined | false>): Promise<T> {
  for (let i = 0; i < 100; i++) {
    const v = await f();
    if (v) {
      return v;
    }
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${what}`);
}

function find(doc: vscode.TextDocument, needle: string): vscode.Position {
  const i = doc.getText().indexOf(needle);
  assert.ok(i >= 0, needle);
  return doc.positionAt(i + 1);
}

export async function run(): Promise<void> {
  const folder = vscode.workspace.workspaceFolders![0].uri.fsPath;
  const doc = await vscode.workspace.openTextDocument(path.join(folder, "bfd", "bfdio.c"));
  const editor = await vscode.window.showTextDocument(doc);
  const ext = vscode.extensions.all.find((e) => e.packageJSON.name === "si-code-index")!;
  await ext.activate();

  // The server is up and has indexed the folder.
  const symbols = await until("workspace symbols", async () => {
    const s = await vscode.commands.executeCommand<vscode.SymbolInformation[]>("vscode.executeWorkspaceSymbolProvider", "bfd_cache_init");
    return s?.length ? s : undefined;
  });
  assert.ok(symbols.some((s) => s.name === "bfd_cache_init"));

  // Go to definition across files.
  const call = find(doc, "bfd_set_error (bfd_error_invalid_operation)");
  const defs = await vscode.commands.executeCommand<vscode.Location[]>("vscode.executeDefinitionProvider", doc.uri, call);
  assert.ok(defs.length >= 1 && defs[0].uri.fsPath.endsWith("bfd-in2.h"), JSON.stringify(defs));

  // Call hierarchy through a function pointer: bfd_read calls iovec->bread.
  const read = find(doc, "bfd_read (void *ptr");
  const items = await vscode.commands.executeCommand<vscode.CallHierarchyItem[]>("vscode.prepareCallHierarchy", doc.uri, read);
  assert.strictEqual(items[0].name, "bfd_read");
  const out = await vscode.commands.executeCommand<vscode.CallHierarchyOutgoingCall[]>("vscode.provideOutgoingCalls", items[0]);
  const names = out.map((c) => c.to.name);
  assert.ok(names.includes("cache_bread"), names.join(","));

  // CodeLens over function definitions, with resolved counts.
  const lenses = await vscode.commands.executeCommand<vscode.CodeLens[]>("vscode.executeCodeLensProvider", doc.uri, 400);
  const resolved = lenses.filter((l) => l.command);
  assert.ok(resolved.length > 5, `only ${resolved.length} of ${lenses.length} lenses resolved`);
  assert.ok(resolved.some((l) => /callers/.test(l.command!.title)));
  assert.ok(resolved.some((l) => /through function pointers/.test(l.command!.title)), resolved.map((l) => l.command!.title).slice(0, 8).join(" | "));

  // The views and their commands exist and run.
  editor.selection = new vscode.Selection(read, read);
  for (const view of ["si.relation.focus", "si.context.focus", "si.search.focus", "si.fileSymbols.focus"]) {
    await vscode.commands.executeCommand(view);
  }
  for (const command of ["si.relation.graph", "si.relation.tree", "si.relation.callers", "si.relation.references", "si.relation.assignments", "si.relation.callees", "si.relation.lock", "si.relation.unlock", "si.context.lock", "si.context.unlock"]) {
    await vscode.commands.executeCommand(command);
  }

  // A locked Relation stays where it is when the cursor moves, and moves
  // when asked to.
  type Target = { position: { line: number } } | undefined;
  const shownAt = async () => (await vscode.commands.executeCommand<Target>("si.relation.target"))?.position.line;
  await vscode.commands.executeCommand("si.relation.sync");
  const before = await shownAt();
  assert.strictEqual(before, read.line);
  await vscode.commands.executeCommand("si.relation.lock");
  const elsewhere = new vscode.Position(read.line + 40, 2);
  editor.selection = new vscode.Selection(elsewhere, elsewhere);
  await new Promise((r) => setTimeout(r, 500));
  assert.strictEqual(await shownAt(), before, "locked: the cursor moved, the view did not");
  await vscode.commands.executeCommand("si.relation.sync");
  assert.strictEqual(await shownAt(), elsewhere.line, "asked: the locked view shows the cursor's place");
  await vscode.commands.executeCommand("si.context.sync");
  await vscode.commands.executeCommand("si.relation.unlock");
  editor.selection = new vscode.Selection(read, read);

  // Split: a result opened "beside" lands in another group, and one opened
  // normally stays in the group of the editor the views follow.
  await vscode.window.showTextDocument(doc, vscode.ViewColumn.One);
  const target = { uri: vscode.Uri.file(path.join(folder, "bfd", "cache.c")).toString(), line: 373, character: 0 };
  await vscode.commands.executeCommand("si.open", target, true);
  assert.strictEqual(vscode.window.activeTextEditor!.viewColumn, vscode.ViewColumn.Two);
  assert.ok(vscode.window.activeTextEditor!.document.fileName.endsWith("cache.c"));
  assert.strictEqual(vscode.window.activeTextEditor!.selection.active.line, 373);
  await sleep(400); // the views now follow the editor in group two
  await vscode.commands.executeCommand("si.open", { uri: doc.uri.toString(), line: 10, character: 0 }, false);
  assert.strictEqual(vscode.window.activeTextEditor!.viewColumn, vscode.ViewColumn.Two, "opens where the focus was");

  // The index scope: a folder excluded is gone from the index, the choice
  // is written into the workspace, and taking it back brings it back.
  const has = async (name: string) => {
    const s = await vscode.commands.executeCommand<vscode.SymbolInformation[]>("vscode.executeWorkspaceSymbolProvider", name);
    return !!s?.some((x) => x.name === name);
  };
  assert.ok(await has("ATTRIBUTE_UNUSED"));
  await vscode.commands.executeCommand("si.scope.exclude", vscode.Uri.file(path.join(folder, "include")));
  assert.ok(!(await has("ATTRIBUTE_UNUSED")), "include/ left the index");
  const config = JSON.parse(Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.file(path.join(folder, ".si", "config.json")))).toString());
  assert.deepStrictEqual(config, { include: [], exclude: ["include"] });
  await vscode.commands.executeCommand("si.scope.only", vscode.Uri.file(path.join(folder, "bfd")));
  assert.ok((await has("bfd_cache_init")) && !(await has("ATTRIBUTE_UNUSED")));
  await vscode.commands.executeCommand("si.scope.all");
  assert.ok(await has("ATTRIBUTE_UNUSED"), "the whole workspace again");
  await vscode.commands.executeCommand("si.scope.focus");
  await vscode.commands.executeCommand("si.sync");

  // Checking is only a draft: clear everything, and nothing changes until
  // Apply; then nothing is indexed; pick one folder and apply again.
  await vscode.commands.executeCommand("si.scope.clearAll");
  assert.strictEqual(await vscode.commands.executeCommand("si.scope.pending"), true);
  assert.ok(await has("bfd_cache_init"), "not applied yet");
  await vscode.commands.executeCommand("si.scope.apply");
  assert.ok(!(await has("bfd_cache_init")) && !(await has("ATTRIBUTE_UNUSED")), "nothing indexed");
  await vscode.commands.executeCommand("si.scope.include", vscode.Uri.file(path.join(folder, "bfd")));
  assert.ok((await has("bfd_cache_init")) && !(await has("ATTRIBUTE_UNUSED")));
  const picked = JSON.parse(Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.file(path.join(folder, ".si", "config.json")))).toString());
  assert.deepStrictEqual(picked, { include: ["bfd"], exclude: ["."] });
  await vscode.commands.executeCommand("si.scope.selectAll");
  await vscode.commands.executeCommand("si.scope.discard");
  assert.strictEqual(await vscode.commands.executeCommand("si.scope.pending"), false);
  await vscode.commands.executeCommand("si.scope.selectAll");
  await vscode.commands.executeCommand("si.scope.apply");
  assert.ok(await has("ATTRIBUTE_UNUSED"));

  // Compile conditions: bfdio.c defines FD_CLOEXEC under `#ifndef
  // FD_CLOEXEC`. Told the build defines it, that branch is not indexed.
  assert.ok(await has("FD_CLOEXEC"));
  type Conditions = { defines: Record<string, string>; conditionFiles: string[]; state: { active: string | null; configurations: Record<string, unknown>; available: { name: string; source: string }[]; effective: { name: string; value: string | null; source: string }[] } };
  const conditionsNow = () => vscode.commands.executeCommand<Conditions>("si.conditions.get");
  await vscode.commands.executeCommand("si.conditions.define", "FD_CLOEXEC=1");
  assert.ok(!(await has("FD_CLOEXEC")) && (await has("bfd_read")), "the skipped branch left the index");
  let conditions = await conditionsNow();
  assert.deepStrictEqual(conditions.defines, { FD_CLOEXEC: "1" });
  assert.deepStrictEqual([conditions.state.active, conditions.state.effective], ["Default", [{ name: "FD_CLOEXEC", value: "1", source: "" }]]);
  // Another set, in force when made: nothing is said there, both branches are back.
  await vscode.commands.executeCommand("si.conditions.switch", "Release");
  assert.ok(await has("FD_CLOEXEC"));
  await vscode.commands.executeCommand("si.conditions.switch", "Default");
  assert.ok(!(await has("FD_CLOEXEC")));
  await vscode.commands.executeCommand("si.conditions.remove", { type: "macro", name: "FD_CLOEXEC", value: "1", source: "" });
  assert.ok(await has("FD_CLOEXEC"));
  // A set that follows the build's own description: a DS-5 project.
  fs.writeFileSync(path.join(folder, ".cproject"), `<?xml version="1.0"?><cproject><configuration name="Debug"><folderInfo><toolChain><tool>
    <option valueType="definedSymbols"><listOptionValue value="FD_CLOEXEC=1"/><listOptionValue value="BOARD_EVK"/></option></tool></toolChain></folderInfo></configuration></cproject>`);
  await vscode.commands.executeCommand("si.sync");
  conditions = await conditionsNow();
  assert.deepStrictEqual(conditions.state.available, [{ name: "Debug", source: ".cproject#Debug" }]);
  await vscode.commands.executeCommand("si.conditions.switch", ".cproject#Debug");
  conditions = await conditionsNow();
  assert.deepStrictEqual([conditions.state.active, conditions.conditionFiles], ["Debug", [".cproject#Debug"]]);
  assert.deepStrictEqual(conditions.state.effective.map((e) => [e.name, e.value, e.source]), [["BOARD_EVK", "1", ".cproject#Debug"], ["FD_CLOEXEC", "1", ".cproject#Debug"]]);
  assert.ok(!(await has("FD_CLOEXEC")));
  for (const name of ["Debug", "Release"]) {
    await vscode.commands.executeCommand("si.conditions.delete", name);
  }
  fs.rmSync(path.join(folder, ".cproject"));
  assert.ok(await has("FD_CLOEXEC"));
  await vscode.commands.executeCommand("si.conditions.focus");

  // The index is kept in the workspace, and git is told to leave it alone.
  assert.ok((await vscode.workspace.fs.stat(vscode.Uri.file(path.join(folder, ".si", "cache", "manifest")))).size > 0);
  const ignore = Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.file(path.join(folder, ".si", ".gitignore")))).toString();
  assert.ok(ignore.includes("/cache/"), ignore);

  // Views dragged elsewhere can be put back with one command.
  const commands = await vscode.commands.getCommands(true);
  for (const view of ["si.search", "si.fileSymbols", "si.scope", "si.relation", "si.context"]) {
    assert.ok(commands.includes(`${view}.resetViewLocation`), `${view} can be reset`);
  }
  await vscode.commands.executeCommand("si.resetLayout");

  // Saving an edit re-indexes it.
  const edit = new vscode.WorkspaceEdit();
  edit.insert(doc.uri, new vscode.Position(doc.lineCount, 0), "\nint si_probe (void) { return 0; }\n");
  await vscode.workspace.applyEdit(edit);
  await doc.save();
  await until("the saved function", async () => {
    const s = await vscode.commands.executeCommand<vscode.SymbolInformation[]>("vscode.executeWorkspaceSymbolProvider", "si_probe");
    return s?.some((x) => x.name === "si_probe");
  });
}
