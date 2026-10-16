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
  let editor = await vscode.window.showTextDocument(doc);
  const ext = vscode.extensions.all.find((e) => e.packageJSON.name === "norn")!;
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

  // Norn's own go-to commands (the editor menu, F12): the index's answer alone.
  editor.selection = new vscode.Selection(call, call);
  await vscode.commands.executeCommand("norn.goToDefinition");
  await until("Norn: Go to Definition", async () => vscode.window.activeTextEditor?.document.uri.fsPath.endsWith("bfd-in2.h"));
  editor = await vscode.window.showTextDocument(doc);
  // On the definition itself: its prototype, a header's.
  const named = find(doc, "bfd_read (void *ptr");
  editor.selection = new vscode.Selection(named, named);
  await vscode.commands.executeCommand("norn.goToDefinition");
  await until("Norn: Go to Definition, from the definition", async () => vscode.window.activeTextEditor?.document.uri.fsPath.endsWith(".h"));
  editor = await vscode.window.showTextDocument(doc);
  // Of a member that holds functions, the functions stored in it; of what
  // `abfd->iovec` is, its struct.
  const bread = find(doc, "bread (abfd, ptr, size)");
  const stored = await vscode.commands.executeCommand<vscode.Location[]>("vscode.executeImplementationProvider", doc.uri, bread);
  // (GNU style: the return type on a line of its own, the name on the next.)
  const lineOf = async (l: vscode.Location) => {
    const d = await vscode.workspace.openTextDocument(l.uri);
    return d.getText(new vscode.Range(l.range.start.line, 0, Math.min(l.range.start.line + 1, d.lineCount - 1), 1000));
  };
  const storedLines = await Promise.all(stored.map(lineOf));
  assert.ok(storedLines.some((t) => t.includes("cache_bread")), storedLines.join(" | "));
  const iovec = find(doc, "iovec->bread (abfd, ptr, size)");
  const typed = await vscode.commands.executeCommand<vscode.Location[]>("vscode.executeTypeDefinitionProvider", doc.uri, iovec);
  assert.ok(typed.length >= 1 && (await lineOf(typed[0])).includes("struct bfd_iovec"), JSON.stringify(typed));

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

  // With no path set, the server is the one inside the extension.
  if (process.env.SI_TEST_BUNDLED) {
    const used = await vscode.commands.executeCommand<string>("norn.serverInUse");
    assert.ok(/[\\/]bin[\\/][a-z0-9]+-[a-z0-9]+[\\/]norn(\.exe)?$/.test(used), used);
  }

  // The views and their commands exist and run.
  editor.selection = new vscode.Selection(read, read);
  for (const view of ["norn.relation.focus", "norn.context.focus", "norn.search.focus", "norn.fileSymbols.focus"]) {
    await vscode.commands.executeCommand(view);
  }
  for (const command of ["norn.relation.graph", "norn.relation.tree", "norn.relation.callers", "norn.relation.references", "norn.relation.assignments", "norn.relation.callees", "norn.relation.lock", "norn.relation.unlock", "norn.context.lock", "norn.context.unlock"]) {
    await vscode.commands.executeCommand(command);
  }

  // A locked Relation stays where it is when the cursor moves, and moves
  // when asked to.
  type Target = { position: { line: number } } | undefined;
  const shownAt = async () => (await vscode.commands.executeCommand<Target>("norn.relation.target"))?.position.line;
  await vscode.commands.executeCommand("norn.relation.sync");
  const before = await shownAt();
  assert.strictEqual(before, read.line);
  await vscode.commands.executeCommand("norn.relation.lock");
  const elsewhere = new vscode.Position(read.line + 40, 2);
  editor.selection = new vscode.Selection(elsewhere, elsewhere);
  await new Promise((r) => setTimeout(r, 500));
  assert.strictEqual(await shownAt(), before, "locked: the cursor moved, the view did not");
  await vscode.commands.executeCommand("norn.relation.sync");
  assert.strictEqual(await shownAt(), elsewhere.line, "asked: the locked view shows the cursor's place");
  await vscode.commands.executeCommand("norn.context.sync");
  await vscode.commands.executeCommand("norn.relation.unlock");
  // Context shows the file of the definition, whole, at its line.
  const setError = doc.positionAt(doc.getText().indexOf("bfd_set_error (bfd_error_invalid_operation)") + 3);
  editor.selection = new vscode.Selection(setError, setError);
  await vscode.commands.executeCommand("norn.context.sync");
  let shown: { uri: string; line: number; lines: number } | undefined;
  for (let i = 0; i < 40 && !shown?.uri.endsWith("bfd-in2.h"); i++) {
    await new Promise((r) => setTimeout(r, 100));
    shown = await vscode.commands.executeCommand("norn.context.shown");
  }
  const header = await vscode.workspace.openTextDocument(vscode.Uri.parse(shown!.uri));
  assert.ok(shown!.uri.endsWith("bfd-in2.h") && shown!.lines === header.lineCount && header.lineAt(shown!.line - 1).text.includes("bfd_set_error"), JSON.stringify(shown));
  editor.selection = new vscode.Selection(read, read);

  // References in Relation: from the index, each with its line's text; a
  // click on one shows it in Context, and the editor stays where it is.
  type Refs = { groups: { file: string; uri: string; items: { line: number; character: number; text: string }[] }[]; message: string };
  const refs = await vscode.commands.executeCommand<Refs>("norn.relation.referencesAt", { textDocument: { uri: doc.uri.toString() }, position: setError });
  const away = refs.groups.flatMap((g) => g.items.map((it) => ({ ...it, uri: g.uri }))).find((it) => it.uri !== doc.uri.toString());
  assert.ok(away && away.text.includes("bfd_set_error"), JSON.stringify(refs).slice(0, 400));
  const editorBefore = vscode.window.activeTextEditor!.document.uri.toString();
  await vscode.commands.executeCommand("norn.relation.showReference", away);
  const atRef = await vscode.commands.executeCommand<{ uri: string; line: number }>("norn.context.shown");
  assert.ok(atRef.uri === away.uri && atRef.line === away.line + 1, JSON.stringify({ atRef, away }));
  assert.strictEqual(vscode.window.activeTextEditor!.document.uri.toString(), editorBefore, "Context showed it; the editor did not go there");

  // Parameters and local variables: under their function in File Symbols,
  // and with a definition, a context and a relation of their own.
  type Listed = { label: string; children?: Listed[] };
  let listed: Listed[] = [];
  for (let i = 0; i < 40 && !listed.length; i++) {
    await new Promise((r) => setTimeout(r, 100));
    listed = (await vscode.commands.executeCommand<Listed[]>("norn.fileSymbols.tree")) ?? [];
  }
  const readFn = listed.find((g) => g.label === "Functions")?.children?.find((f) => f.label === "bfd_read");
  const inside = (readFn?.children ?? []).map((c) => c.label);
  assert.ok(inside.includes("ptr") && inside.includes("nread"), JSON.stringify(inside));
  const useOf = doc.positionAt(doc.getText().indexOf("nread = abfd->iovec") + 2);
  const declared = await vscode.commands.executeCommand<vscode.Location[]>("vscode.executeDefinitionProvider", doc.uri, useOf);
  assert.ok(declared.length === 1 && declared[0].range.start.line < useOf.line && doc.lineAt(declared[0].range.start.line).text.includes("nread"), "a local's definition is its declaration");
  editor.selection = new vscode.Selection(useOf, useOf);
  await vscode.commands.executeCommand("norn.context.sync");
  let local: { uri: string; line: number } | undefined;
  for (let i = 0; i < 40 && local?.line !== declared[0].range.start.line + 1; i++) {
    await new Promise((r) => setTimeout(r, 100));
    local = await vscode.commands.executeCommand("norn.context.shown");
  }
  assert.strictEqual(local?.line, declared[0].range.start.line + 1, "Context shows the local's declaration");
  editor.selection = new vscode.Selection(read, read);

  // Split: a result opened "beside" lands in another group, and one opened
  // normally stays in the group of the editor the views follow.
  await vscode.window.showTextDocument(doc, vscode.ViewColumn.One);
  const target = { uri: vscode.Uri.file(path.join(folder, "bfd", "cache.c")).toString(), line: 373, character: 0 };
  await vscode.commands.executeCommand("norn.open", target, true);
  assert.strictEqual(vscode.window.activeTextEditor!.viewColumn, vscode.ViewColumn.Two);
  assert.ok(vscode.window.activeTextEditor!.document.fileName.endsWith("cache.c"));
  assert.strictEqual(vscode.window.activeTextEditor!.selection.active.line, 373);
  await sleep(400); // the views now follow the editor in group two
  await vscode.commands.executeCommand("norn.open", { uri: doc.uri.toString(), line: 10, character: 0 }, false);
  assert.strictEqual(vscode.window.activeTextEditor!.viewColumn, vscode.ViewColumn.Two, "opens where the focus was");

  // The index scope: a folder excluded is gone from the index, the choice
  // is written into the workspace, and taking it back brings it back.
  const has = async (name: string) => {
    const s = await vscode.commands.executeCommand<vscode.SymbolInformation[]>("vscode.executeWorkspaceSymbolProvider", name);
    return !!s?.some((x) => x.name === name);
  };
  assert.ok(await has("ATTRIBUTE_UNUSED"));
  await vscode.commands.executeCommand("norn.scope.exclude", vscode.Uri.file(path.join(folder, "include")));
  assert.ok(!(await has("ATTRIBUTE_UNUSED")), "include/ left the index");
  const config = JSON.parse(Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.file(path.join(folder, ".norn", "config.json")))).toString());
  assert.deepStrictEqual(config, { include: [], exclude: ["include"] });
  await vscode.commands.executeCommand("norn.scope.only", vscode.Uri.file(path.join(folder, "bfd")));
  assert.ok((await has("bfd_cache_init")) && !(await has("ATTRIBUTE_UNUSED")));
  await vscode.commands.executeCommand("norn.scope.all");
  assert.ok(await has("ATTRIBUTE_UNUSED"), "the whole workspace again");
  await vscode.commands.executeCommand("norn.scope.focus");
  await vscode.commands.executeCommand("norn.sync");

  // Checking is only a draft: clear everything, and nothing changes until
  // Apply; then nothing is indexed; pick one folder and apply again.
  await vscode.commands.executeCommand("norn.scope.clearAll");
  assert.strictEqual(await vscode.commands.executeCommand("norn.scope.pending"), true);
  assert.ok(await has("bfd_cache_init"), "not applied yet");
  await vscode.commands.executeCommand("norn.scope.apply");
  assert.ok(!(await has("bfd_cache_init")) && !(await has("ATTRIBUTE_UNUSED")), "nothing indexed");
  await vscode.commands.executeCommand("norn.scope.include", vscode.Uri.file(path.join(folder, "bfd")));
  assert.ok((await has("bfd_cache_init")) && !(await has("ATTRIBUTE_UNUSED")));
  const picked = JSON.parse(Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.file(path.join(folder, ".norn", "config.json")))).toString());
  assert.deepStrictEqual(picked, { include: ["bfd"], exclude: ["."] });
  await vscode.commands.executeCommand("norn.scope.selectAll");
  await vscode.commands.executeCommand("norn.scope.discard");
  assert.strictEqual(await vscode.commands.executeCommand("norn.scope.pending"), false);
  await vscode.commands.executeCommand("norn.scope.selectAll");
  await vscode.commands.executeCommand("norn.scope.apply");
  assert.ok(await has("ATTRIBUTE_UNUSED"));

  // Compile conditions: bfdio.c defines FD_CLOEXEC under `#ifndef
  // FD_CLOEXEC`. Told the build defines it, that branch is not indexed.
  assert.ok(await has("FD_CLOEXEC"));
  type Conditions = { defines: Record<string, string>; conditionFiles: string[]; state: { active: string | null; configurations: Record<string, unknown>; available: { name: string; source: string }[]; effective: { name: string; value: string | null; source: string }[] } };
  const conditionsNow = () => vscode.commands.executeCommand<Conditions>("norn.conditions.get");
  await vscode.commands.executeCommand("norn.conditions.define", "FD_CLOEXEC=1");
  assert.ok(!(await has("FD_CLOEXEC")) && (await has("bfd_read")), "the skipped branch left the index");
  let conditions = await conditionsNow();
  assert.deepStrictEqual(conditions.defines, { FD_CLOEXEC: "1" });
  assert.deepStrictEqual([conditions.state.active, conditions.state.effective], ["Default", [{ name: "FD_CLOEXEC", value: "1", source: "" }]]);
  // Another set, in force when made: nothing is said there, both branches are back.
  await vscode.commands.executeCommand("norn.conditions.switch", "Release");
  assert.ok(await has("FD_CLOEXEC"));
  await vscode.commands.executeCommand("norn.conditions.switch", "Default");
  assert.ok(!(await has("FD_CLOEXEC")));
  await vscode.commands.executeCommand("norn.conditions.remove", { type: "macro", name: "FD_CLOEXEC", value: "1", source: "" });
  assert.ok(await has("FD_CLOEXEC"));
  // A set that follows the build's own description: a DS-5 project.
  fs.writeFileSync(path.join(folder, ".cproject"), `<?xml version="1.0"?><cproject><configuration name="Debug"><folderInfo><toolChain><tool>
    <option valueType="definedSymbols"><listOptionValue value="FD_CLOEXEC=1"/><listOptionValue value="BOARD_EVK"/></option>
    <option valueType="includePath"><listOptionValue value="../include"/></option></tool></toolChain></folderInfo></configuration></cproject>`);
  await vscode.commands.executeCommand("norn.sync");
  conditions = await conditionsNow();
  assert.deepStrictEqual(conditions.state.available, [{ name: "Debug", source: ".cproject#Debug" }]);
  await vscode.commands.executeCommand("norn.conditions.switch", ".cproject#Debug");
  conditions = await conditionsNow();
  assert.deepStrictEqual([conditions.state.active, conditions.conditionFiles], ["Debug", [".cproject#Debug"]]);
  assert.deepStrictEqual(conditions.state.effective.map((e) => [e.name, e.value, e.source]), [["BOARD_EVK", "1", ".cproject#Debug"], ["FD_CLOEXEC", "1", ".cproject#Debug"]]);
  assert.ok(!(await has("FD_CLOEXEC")));
  // Passed on to the C/C++ extension: a configuration of its own in a file
  // that keeps what else is in it, comments included.
  const cpp = path.join(folder, ".vscode", "c_cpp_properties.json");
  fs.writeFileSync(cpp, `{
    // theirs
    "configurations": [ { "name": "Win32", "defines": ["_DEBUG"], "compilerPath": "cl.exe" } ],
    "version": 4
}`);
  await vscode.commands.executeCommand("norn.conditions.toCpp");
  let written = fs.readFileSync(cpp, "utf8");
  assert.ok(written.includes("// theirs") && written.includes('"compilerPath": "cl.exe"'), written);
  let ours = (eval("(" + written + ")") as { configurations: { name: string; defines: string[]; includePath: string[] }[] }).configurations;
  assert.deepStrictEqual(ours.map((c) => c.name), ["Win32", "Norn"]);
  assert.deepStrictEqual([ours[0].defines, ours[1].defines, ours[1].includePath], [["_DEBUG"], ["BOARD_EVK=1", "FD_CLOEXEC=1"], ["${workspaceFolder}/**", "${workspaceFolder}/include"]]);
  // In step from then on, when that is asked for; and not offered back as a build to follow.
  await vscode.workspace.getConfiguration("norn").update("conditions.updateCppProperties", true, vscode.ConfigurationTarget.Workspace);
  await vscode.commands.executeCommand("norn.conditions.define", "EXTRA=2");
  written = fs.readFileSync(cpp, "utf8");
  ours = (eval("(" + written + ")") as typeof ours extends infer T ? { configurations: T } : never).configurations;
  assert.deepStrictEqual(ours[1].defines, ["BOARD_EVK=1", "EXTRA=2", "FD_CLOEXEC=1"]);
  assert.deepStrictEqual((await conditionsNow()).state.available.map((a) => a.source), [".cproject#Debug", ".vscode/c_cpp_properties.json#Win32"]);
  await vscode.workspace.getConfiguration("norn").update("conditions.updateCppProperties", undefined, vscode.ConfigurationTarget.Workspace);
  fs.rmSync(cpp);
  for (const name of ["Debug", "Release"]) {
    await vscode.commands.executeCommand("norn.conditions.delete", name);
  }
  fs.rmSync(path.join(folder, ".cproject"));
  assert.ok(await has("FD_CLOEXEC"));
  // What the sources define themselves decides branches too: a switch in
  // a header the file includes.
  fs.writeFileSync(path.join(folder, "bfd", "sw.h"), "#ifndef SW_H\n#define SW_H\n#define USE_PROBE 0\n#endif\n");
  fs.writeFileSync(path.join(folder, "bfd", "sw.c"), '#include "sw.h"\n#if USE_PROBE\nint probe_on (void) { return 1; }\n#else\nint probe_off (void) { return 0; }\n#endif\n');
  await vscode.commands.executeCommand("norn.sync");
  assert.ok((await has("probe_off")) && !(await has("probe_on")), "the header's switch decides the branch");
  const sourced = (await conditionsNow()).state as unknown as { fromSources: { name: string; value: string; files: number }[] };
  assert.ok(sourced.fromSources.some((e) => e.name === "USE_PROBE" && e.value === "0" && e.files === 2), JSON.stringify(sourced.fromSources.slice(0, 5)));
  await vscode.commands.executeCommand("norn.conditions.sourcesOff");
  assert.ok((await has("probe_off")) && (await has("probe_on")), "told not to, both branches");
  await vscode.commands.executeCommand("norn.conditions.sourcesOn");
  assert.ok(!(await has("probe_on")));
  await vscode.commands.executeCommand("norn.conditions.delete", "Default");
  fs.rmSync(path.join(folder, "bfd", "sw.h"));
  fs.rmSync(path.join(folder, "bfd", "sw.c"));
  await vscode.commands.executeCommand("norn.sync");
  await vscode.commands.executeCommand("norn.conditions.focus");

  // The index is kept in the workspace, and git is told to leave it alone.
  assert.ok((await vscode.workspace.fs.stat(vscode.Uri.file(path.join(folder, ".norn", "cache", "manifest")))).size > 0);
  const ignore = Buffer.from(await vscode.workspace.fs.readFile(vscode.Uri.file(path.join(folder, ".norn", ".gitignore")))).toString();
  assert.ok(ignore.includes("/cache/"), ignore);

  // Views dragged elsewhere can be put back with one command.
  const commands = await vscode.commands.getCommands(true);
  for (const view of ["norn.search", "norn.fileSymbols", "norn.scope", "norn.relation", "norn.context"]) {
    assert.ok(commands.includes(`${view}.resetViewLocation`), `${view} can be reset`);
  }
  await vscode.commands.executeCommand("norn.resetLayout");

  // C++: a method is named with its class, and a call of one on an object
  // leads to it.
  await until("the C++ method", async () => {
    const s = await vscode.commands.executeCommand<vscode.SymbolInformation[]>("vscode.executeWorkspaceSymbolProvider", "Area");
    return s?.some((x) => x.name === "Circle::Area");
  });
  const ccDoc = await vscode.workspace.openTextDocument(path.join(folder, "cpp", "main.cc"));
  await vscode.window.showTextDocument(ccDoc);
  const twice = await vscode.commands.executeCommand<vscode.Location[]>("vscode.executeDefinitionProvider", ccDoc.uri, find(ccDoc, "Twice()"));
  assert.ok(twice.length >= 1 && twice[0].uri.fsPath.endsWith("shapes.h") && twice[0].range.start.line === 6, JSON.stringify(twice));
  editor = await vscode.window.showTextDocument(doc);

  // A macro shows the number it comes to, the macros it names read too.
  const bfdh = await vscode.workspace.openTextDocument(path.join(folder, "bfd", "bfd-in2.h"));
  const hovers = await vscode.commands.executeCommand<vscode.Hover[]>("vscode.executeHoverProvider", bfdh.uri, find(bfdh, "BSF_EXPORT "));
  const hoverText = hovers.flatMap((h) => h.contents.map((c) => (typeof c === "string" ? c : c.value))).join("\n");
  assert.ok(/#define BSF_EXPORT BSF_GLOBAL/.test(hoverText) && /= 2\b/.test(hoverText), hoverText);

  // The names the index has complete the word being typed.
  const typing = doc.getText().indexOf("bfd_set_error (bfd_error_invalid_operation)");
  const completions = await vscode.commands.executeCommand<vscode.CompletionList>("vscode.executeCompletionItemProvider", doc.uri, doc.positionAt(typing + "bfd_set_err".length));
  assert.ok(completions.items.some((i) => (typeof i.label === "string" ? i.label : i.label.label) === "bfd_set_error"), completions.items.map((i) => i.label).slice(0, 20).join(" "));

  // A function's hover is how it is declared; a call being written, its
  // parameters; after `->`, the members of what it points to.
  const called = find(doc, "bfd_set_error (bfd_error_invalid_operation)");
  const fnHovers = await vscode.commands.executeCommand<vscode.Hover[]>("vscode.executeHoverProvider", doc.uri, called);
  const fnText = fnHovers.flatMap((h) => h.contents.map((c) => (typeof c === "string" ? c : c.value))).join("\n");
  assert.ok(/void bfd_set_error \(bfd_error_type error_tag\)/.test(fnText), fnText);
  const help = await vscode.commands.executeCommand<vscode.SignatureHelp>("vscode.executeSignatureHelpProvider", doc.uri, doc.positionAt(typing + "bfd_set_error (".length), "(");
  assert.ok(help?.signatures.some((s) => /bfd_set_error \(bfd_error_type error_tag\)/.test(s.label)) && help.activeParameter === 0, JSON.stringify(help));
  const arrow = doc.getText().indexOf("abfd->iovec == NULL") + "abfd->".length;
  const members = await vscode.commands.executeCommand<vscode.CompletionList>("vscode.executeCompletionItemProvider", doc.uri, doc.positionAt(arrow), ">");
  const memberNames = members.items.map((i) => (typeof i.label === "string" ? i.label : i.label.label));
  assert.ok(memberNames.includes("iovec") && memberNames.includes("filename") && !memberNames.includes("bfd_set_error"), memberNames.slice(0, 20).join(" "));

  // An #include leads to the file it means, and is a link to it.
  const included = doc.positionAt(doc.getText().indexOf('#include "libbfd.h"') + 12);
  const includedFile = await vscode.commands.executeCommand<vscode.Location[]>("vscode.executeDefinitionProvider", doc.uri, included);
  assert.ok(includedFile.length >= 1 && includedFile[0].uri.fsPath.endsWith("libbfd.h"), JSON.stringify(includedFile));
  const links = await vscode.commands.executeCommand<vscode.DocumentLink[]>("vscode.executeLinkProvider", doc.uri);
  assert.ok(links.some((l) => l.target?.fsPath.endsWith("libbfd.h")), JSON.stringify(links.map((l) => l.target?.fsPath)));

  // The name under the cursor, where else in the file it means the same.
  const marks = await vscode.commands.executeCommand<vscode.DocumentHighlight[]>("vscode.executeDocumentHighlights", doc.uri, find(doc, "bfd_set_error (bfd_error_invalid_operation)"));
  assert.ok(marks.length >= 2 && marks.every((m) => doc.getText(m.range) === "bfd_set_error"), JSON.stringify(marks.map((m) => m.range.start.line)));

  // A source file's header, and back.
  const libbfd = await vscode.workspace.openTextDocument(path.join(folder, "bfd", "libbfd.c"));
  await vscode.window.showTextDocument(libbfd);
  await vscode.commands.executeCommand("norn.switchHeaderSource");
  await until("the header of libbfd.c", async () => vscode.window.activeTextEditor?.document.uri.fsPath.endsWith("libbfd.h"));
  editor = await vscode.window.showTextDocument(doc);

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
