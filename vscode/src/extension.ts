// SI for VS Code: fuzzy project symbol search, file symbols, a Context view
// that follows the cursor and a Relation view of callers and callees. All of
// it asks the `si lsp` server; nothing is indexed here.
//
// With the editor split, the views follow the editor that last had focus,
// results open in that editor's group (or beside it, on request), and each
// view can be locked so it stops following.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

/** A symbol as the server's `si/*` requests describe it. */
interface Entry {
  name: string;
  kind: string;
  file: string;
  line: number;
  endLine: number;
  declaration: boolean;
  uri: string;
  range: { start: { line: number; character: number }; end: { line: number; character: number } };
  data: { file: string; line: number; name: string };
  score?: number;
  candidates?: number;
  // `si/relation` only:
  lines?: number[];
  viaMacro?: string | null;
  through?: string | null;
  pointer?: boolean;
  certain?: boolean;
}

type Mode = "callees" | "callers" | "references" | "assignments";

let client: LanguageClient | undefined;
/** The extension's own folder: where the icon font is. */
let home: vscode.Uri;

/** The symbol icons VS Code itself uses, for the views drawn here. */
function codicons(w: vscode.Webview): { roots: vscode.Uri[]; link: string; csp: string } {
  const dist = vscode.Uri.joinPath(home, "node_modules", "@vscode", "codicons", "dist");
  return {
    roots: [dist],
    link: `<link rel="stylesheet" href="${w.asWebviewUri(vscode.Uri.joinPath(dist, "codicon.css"))}">`,
    csp: `font-src ${w.cspSource}; style-src ${w.cspSource} 'unsafe-inline';`,
  };
}
/** The text editor that last had focus: what the views are about. */
let current: vscode.TextEditor | undefined;

const ICONS: Record<string, string> = {
  function: "symbol-function", macro: "symbol-constant", struct: "symbol-struct", union: "symbol-struct",
  enum: "symbol-enum", enum_const: "symbol-enum-member", typedef: "symbol-class", global: "symbol-variable",
  field: "symbol-field", file: "symbol-file",
};
const KIND_NAMES: Record<string, string> = {
  function: "function", macro: "macro", struct: "struct", union: "union", enum: "enum", enum_const: "enumerator",
  typedef: "typedef", global: "variable", field: "member",
};

function isSource(doc: vscode.TextDocument): boolean {
  return doc.uri.scheme === "file" && /\.[ch]$/i.test(doc.fileName);
}

function positionParams(editor: vscode.TextEditor) {
  const p = editor.selection.active;
  return { textDocument: { uri: editor.document.uri.toString() }, position: { line: p.line, character: p.character } };
}

async function ask<T>(method: string, params: unknown): Promise<T | undefined> {
  if (!client || !client.isRunning()) {
    return undefined;
  }
  try {
    return await client.sendRequest<T>(method, params);
  } catch {
    return undefined;
  }
}

/** Show a place: in the group of the editor the views follow, or beside it. */
async function open(uri: string, line: number, character = 0, beside = false) {
  const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(uri));
  const at = new vscode.Position(Math.max(0, line), character);
  const viewColumn = beside ? vscode.ViewColumn.Beside : current?.viewColumn ?? vscode.ViewColumn.Active;
  await vscode.window.showTextDocument(doc, { viewColumn, selection: new vscode.Range(at, at), preview: true });
}

// ---------------------------------------------------------------- Relation

/** A row of a tree view. */
class Node {
  constructor(
    readonly label: string,
    readonly entry?: Entry,
    readonly target?: { uri: string; line: number; character: number },
    readonly children?: Node[],
    readonly detail?: string,
    readonly icon?: string,
  ) {}
}

type At = ReturnType<typeof positionParams>;

/** Callers, callees or references of the function at the cursor, as a tree
 *  or a graph (media/relation.js draws both). */
class Relation implements vscode.WebviewViewProvider {
  mode: Mode = "callees";
  locked = false;
  private view: vscode.WebviewView | undefined;
  private at: At | undefined;

  constructor(private readonly media: vscode.Uri) {}

  resolveWebviewView(view: vscode.WebviewView) {
    this.view = view;
    const w = view.webview;
    const icons = codicons(w);
    w.options = { enableScripts: true, localResourceRoots: [this.media, ...icons.roots] };
    const file = (name: string) => w.asWebviewUri(vscode.Uri.joinPath(this.media, name));
    w.html = `<!doctype html><html><head><meta charset="utf-8">
      <meta http-equiv="Content-Security-Policy" content="default-src 'none'; ${icons.csp} script-src ${w.cspSource};">
      ${icons.link}<link rel="stylesheet" href="${file("relation.css")}"></head><body>
      <div id="bar" role="tablist">
        <button role="tab" data-mode="callees">Calls</button>
        <button role="tab" data-mode="callers">Called by</button>
        <button role="tab" data-mode="references">References</button>
        <button role="tab" data-mode="assignments" title="Where a function pointer is assigned, or where a function is stored">Assignments</button>
        <span id="views"><button data-view="tree">Tree</button><button data-view="graph">Graph</button></span>
      </div>
      <div id="message"></div><div id="root"></div>
      <script src="${file("relation.js")}"></script></body></html>`;
    w.onDidReceiveMessage(async (m: { ready?: boolean; open?: { uri: string; line: number; character: number }; beside?: boolean; mode?: Mode; expand?: Entry["data"]; id?: number }) => {
      if (m.ready) {
        await this.load(this.at ?? (current ? positionParams(current) : undefined));
      } else if (m.open) {
        await open(m.open.uri, m.open.line, m.open.character, !!m.beside);
      } else if (m.mode) {
        this.setMode(m.mode);
      } else if (m.expand) {
        const r = await ask<{ entries: Entry[] }>("si/relation", { item: m.expand, direction: this.mode });
        void w.postMessage({ type: "children", id: m.id, entries: r?.entries ?? [] });
      }
    });
  }

  setMode(mode: Mode) {
    this.mode = mode;
    void this.load(this.at);
  }

  setView(view: "tree" | "graph") {
    void this.view?.webview.postMessage({ type: "view", view });
  }

  /** Follow the cursor, unless locked. */
  follow(editor: vscode.TextEditor) {
    if (!this.locked) {
      void this.load(positionParams(editor));
    }
  }

  private async load(at: At | undefined) {
    this.at = at;
    const w = this.view?.webview;
    if (!w || !at) {
      return;
    }
    if (this.mode === "references") {
      void w.postMessage({ type: "refs", ...(await this.references(at)) });
      return;
    }
    if (this.mode === "assignments") {
      const r = await ask<{ target: Entry | null; by: string; rows: unknown[] }>("si/assignments", at);
      const message = !r?.target
        ? ""
        : r.by === "member"
          ? `${r.rows.length} assignments to ${r.target.name}`
          : `${r.target.name} is stored in ${r.rows.length} places`;
      void w.postMessage({ type: "assignments", target: r?.target ?? null, by: r?.by ?? "member", rows: r?.rows ?? [], message });
      return;
    }
    const r = await ask<{ root: Entry | null; entries: Entry[] }>("si/relation", { ...at, direction: this.mode });
    if (at !== this.at) {
      return; // the cursor has moved on
    }
    const what = this.mode === "callers" ? "called by" : "calls";
    const message = r?.root ? `${r.root.name} ${what} ${r.entries.length}` : "The cursor is not on a function";
    void w.postMessage({ type: "root", mode: this.mode, root: r?.root ?? null, entries: r?.entries ?? [], message });
  }

  private async references(at: At) {
    const uri = vscode.Uri.parse(at.textDocument.uri);
    const pos = new vscode.Position(at.position.line, at.position.character);
    const found = (await vscode.commands.executeCommand<vscode.Location[]>("vscode.executeReferenceProvider", uri, pos)) ?? [];
    const byFile = new Map<string, vscode.Location[]>();
    for (const loc of found) {
      const k = loc.uri.toString();
      byFile.set(k, [...(byFile.get(k) ?? []), loc]);
    }
    const groups = [];
    for (const [file, locs] of byFile) {
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(file));
      const items = locs.map((l) => ({
        line: l.range.start.line,
        character: l.range.start.character,
        text: l.range.start.line < doc.lineCount ? doc.lineAt(l.range.start.line).text.trim() : "",
      }));
      groups.push({ file: vscode.workspace.asRelativePath(doc.uri), uri: file, items });
    }
    return { groups, message: `${found.length} references in ${byFile.size} files` };
  }
}

// ---------------------------------------------------------------- File symbols

class FileSymbols implements vscode.TreeDataProvider<Node> {
  private readonly changed = new vscode.EventEmitter<Node | undefined>();
  readonly onDidChangeTreeData = this.changed.event;
  private groups: Node[] = [];
  private shown = "";

  async show(editor: vscode.TextEditor) {
    const uri = editor.document.uri.toString();
    if (uri === this.shown) {
      return;
    }
    this.shown = uri;
    const symbols = (await ask<{ name: string; kind: number; location: { range: Entry["range"] } }[]>("textDocument/documentSymbol", { textDocument: { uri } })) ?? [];
    const order: [number, string, string][] = [
      [14, "Macros", "symbol-constant"], [12, "Functions", "symbol-function"], [23, "Structs", "symbol-struct"], [10, "Enums", "symbol-enum"],
      [22, "Enumerators", "symbol-enum-member"], [5, "Typedefs", "symbol-class"], [13, "Variables", "symbol-variable"], [8, "Members", "symbol-field"],
    ];
    this.groups = [];
    for (const [kind, title, icon] of order) {
      const kids = symbols
        .filter((s) => s.kind === kind)
        .map((s) => new Node(s.name, undefined, { uri, line: s.location.range.start.line, character: s.location.range.start.character }, undefined, String(s.location.range.start.line + 1), icon));
      if (kids.length) {
        this.groups.push(new Node(title, undefined, undefined, kids, String(kids.length), icon));
      }
    }
    this.changed.fire(undefined);
  }

  refresh() {
    this.shown = "";
    if (current) {
      void this.show(current);
    }
  }

  getTreeItem(node: Node): vscode.TreeItem {
    const item = new vscode.TreeItem(node.label, node.children ? vscode.TreeItemCollapsibleState.Expanded : vscode.TreeItemCollapsibleState.None);
    item.description = node.detail;
    if (node.icon) {
      item.iconPath = new vscode.ThemeIcon(node.icon);
    }
    if (node.target) {
      item.command = { command: "si.open", title: "Open", arguments: [node.target, false] };
    }
    return item;
  }

  getChildren(node?: Node): Node[] {
    return node ? node.children ?? [] : this.groups;
  }
}

// ---------------------------------------------------------------- Index scope

interface ScopeState {
  include: string[];
  exclude: string[];
  config: string;
}

interface Synced extends ScopeState {
  files: number;
  symbols: number;
  added: number;
  removed: number;
  changed: number;
  parsed: number;
  ms: number;
  error?: string;
}

const NEVER = [".si", ".git", ".svn", ".hg"];

function under(p: string, rule: string): boolean {
  return rule === "" || p === rule || p.startsWith(rule + "/");
}

/** Is the folder part of the project? The rule on its longest enclosing
 *  folder decides (the server applies the same rule to files). */
function inScope(scope: ScopeState, rel: string): boolean {
  const longest = (rules: string[]) => Math.max(-1, ...rules.filter((r) => under(rel, r)).map((r) => r.length));
  const inc = longest(scope.include), exc = longest(scope.exclude);
  return inc < 0 && exc < 0 ? scope.include.length === 0 : inc > exc;
}

/** The folders of the workspace, each with a checkbox: checked folders are
 *  indexed. What is chosen is kept in `.si/config.json` in the workspace. */
class ScopeTree implements vscode.TreeDataProvider<string> {
  private readonly changed = new vscode.EventEmitter<string | undefined>();
  readonly onDidChangeTreeData = this.changed.event;
  scope: ScopeState = { include: [], exclude: [], config: "" };
  /** Checkboxes changed but not applied yet: nothing is indexed anew until
   *  Apply, so a selection can be put together first. */
  draft: { include: string[]; exclude: string[] } | undefined;

  constructor(
    private readonly afterSync: (r: Synced, what: string) => void,
    private readonly busy: <T>(viewId: string, what: string, work: () => Promise<T>) => Promise<T>,
    private readonly onDraft: (pending: boolean) => void,
  ) {}

  /** What the checkboxes show: the draft, or what is in force. */
  private shown(): ScopeState {
    return this.draft ? { ...this.draft, config: this.scope.config } : this.scope;
  }

  private setDraft(draft: { include: string[]; exclude: string[] } | undefined) {
    this.draft = draft;
    this.onDraft(!!draft);
    this.changed.fire(undefined);
  }

  private root(): vscode.Uri | undefined {
    return vscode.workspace.workspaceFolders?.[0]?.uri;
  }

  async load() {
    const s = await ask<ScopeState>("si/scope", {});
    if (s) {
      this.scope = s;
      this.changed.fire(undefined);
    }
  }

  /** Folder path relative to the workspace, `/` between parts. */
  relative(uri: vscode.Uri): string | undefined {
    const root = this.root();
    if (!root) {
      return undefined;
    }
    const rel = path.relative(root.fsPath, uri.fsPath).split(path.sep).join("/");
    return rel.startsWith("..") ? undefined : rel;
  }

  async getChildren(rel?: string): Promise<string[]> {
    const root = this.root();
    if (!root) {
      return [];
    }
    try {
      const entries = await vscode.workspace.fs.readDirectory(rel ? vscode.Uri.joinPath(root, rel) : root);
      return entries
        .filter(([name, type]) => (type & vscode.FileType.Directory) !== 0 && !NEVER.includes(name))
        .map(([name]) => (rel ? `${rel}/${name}` : name))
        .sort();
    } catch {
      return [];
    }
  }

  getTreeItem(rel: string): vscode.TreeItem {
    const item = new vscode.TreeItem(path.posix.basename(rel), vscode.TreeItemCollapsibleState.Collapsed);
    item.id = rel;
    item.iconPath = vscode.ThemeIcon.Folder;
    const shown = this.shown();
    item.checkboxState = inScope(shown, rel) ? vscode.TreeItemCheckboxState.Checked : vscode.TreeItemCheckboxState.Unchecked;
    // Say where a rule is written; the folders under it follow it.
    item.description = shown.include.includes(rel) ? "included" : shown.exclude.includes(rel) ? "excluded" : undefined;
    item.contextValue = "folder";
    return item;
  }

  /** Check or uncheck `rel` in the draft, dropping the rules under it that
   *  this overrides. Nothing is indexed until `applyDraft`. */
  toggle(rel: string, wanted: boolean) {
    const now = this.shown();
    const keep = (rules: string[]) => rules.filter((r) => !under(r, rel));
    const next = { include: keep(now.include), exclude: keep(now.exclude) };
    // Folders are being picked one by one (some are listed, or everything
    // was cleared): unchecking the last of them leaves nothing, not all.
    const picking = now.include.length > 0 || now.exclude.includes("");
    if (picking && next.include.length === 0 && !next.exclude.includes("")) {
      next.exclude.push("");
    }
    if (inScope({ ...next, config: "" }, rel) !== wanted) {
      (wanted ? next.include : next.exclude).push(rel);
    }
    this.setDraft(next);
  }

  /** Every folder, or none: the starting points for a selection. */
  selectAll() {
    this.setDraft({ include: [], exclude: [] });
  }

  clearAll() {
    this.setDraft({ include: [], exclude: [""] });
  }

  discard() {
    this.setDraft(undefined);
  }

  async applyDraft() {
    if (this.draft) {
      await this.apply(this.draft, "Applied the scope");
    }
  }

  /** Put `rel` in or out right away (from a menu, where that is the whole request). */
  async set(rel: string, wanted: boolean) {
    this.toggle(rel, wanted);
    await this.applyDraft();
  }

  async apply(next: { include: string[]; exclude: string[] }, what: string) {
    const r = await this.busy("si.scope", "indexing", () => ask<Synced>("si/setScope", { include: next.include, exclude: next.exclude }));
    if (!r || r.error) {
      void vscode.window.showErrorMessage(r?.error ?? "SI is not running.");
      return;
    }
    this.scope = { include: r.include, exclude: r.exclude, config: r.config };
    this.setDraft(undefined);
    this.afterSync(r, what);
  }
}

// ---------------------------------------------------------------- Compile conditions

interface ConditionSettings {
  defines: Record<string, string>;
  undefines: string[];
  assumeUndefined: string[];
  conditionFiles: string[];
  known?: number;
}

/** One row of the Conditions view: a group, or a setting in it. */
interface ConditionRow {
  group: "defines" | "undefines" | "conditionFiles" | "assumeUndefined";
  key?: string;
}

/** What the build defines, so that `#if` branches it does not compile are
 *  left out of the index. Kept in `.si/config.json` with the scope. */
class ConditionsTree implements vscode.TreeDataProvider<ConditionRow> {
  private readonly changed = new vscode.EventEmitter<ConditionRow | undefined>();
  readonly onDidChangeTreeData = this.changed.event;
  settings: ConditionSettings = { defines: {}, undefines: [], assumeUndefined: [], conditionFiles: [] };

  constructor(
    private readonly afterSync: (r: Synced, what: string) => void,
    private readonly busy: <T>(viewId: string, what: string, work: () => Promise<T>) => Promise<T>,
  ) {}

  async load() {
    const s = await ask<ConditionSettings>("si/conditions", {});
    if (s) {
      this.settings = s;
      this.changed.fire(undefined);
    }
  }

  getChildren(row?: ConditionRow): ConditionRow[] {
    const s = this.settings;
    if (!row) {
      return [{ group: "defines" }, { group: "undefines" }, { group: "conditionFiles" }, { group: "assumeUndefined" }];
    }
    if (row.key !== undefined) {
      return [];
    }
    const keys = row.group === "defines" ? Object.keys(s.defines).sort() : [...s[row.group]].sort();
    return keys.map((key) => ({ group: row.group, key }));
  }

  getTreeItem(row: ConditionRow): vscode.TreeItem {
    const s = this.settings;
    if (row.key === undefined) {
      const titles = {
        defines: ["Defined", "Macros the build defines", "symbol-constant"],
        undefines: ["Not defined", "Macros the build does not define", "circle-slash"],
        conditionFiles: ["From files", "Files read for more: a header of #define lines, or a Kconfig .config", "file"],
        assumeUndefined: ["Assume not defined", "Prefixes (or * for everything) whose names are not defined unless listed or defined in the sources", "question"],
      } as const;
      const [label, tip, icon] = titles[row.group];
      const count = row.group === "defines" ? Object.keys(s.defines).length : s[row.group].length;
      const item = new vscode.TreeItem(label, count ? vscode.TreeItemCollapsibleState.Expanded : vscode.TreeItemCollapsibleState.None);
      item.description = count ? String(count) : "none";
      item.tooltip = tip;
      item.iconPath = new vscode.ThemeIcon(icon);
      item.contextValue = `group.${row.group}`;
      return item;
    }
    const item = new vscode.TreeItem(row.key === "*" ? "* (every other name)" : row.key);
    if (row.group === "defines") {
      item.description = s.defines[row.key] === "" ? "defined" : `= ${s.defines[row.key]}`;
    }
    item.contextValue = "condition";
    return item;
  }

  async apply(next: ConditionSettings, what: string) {
    const r = await this.busy("si.conditions", "indexing", () => ask<Synced & { conditions?: ConditionSettings }>("si/setConditions", {
      defines: next.defines, undefines: next.undefines, assumeUndefined: next.assumeUndefined, conditionFiles: next.conditionFiles,
    }));
    if (!r || r.error) {
      void vscode.window.showErrorMessage(r?.error ?? "SI is not running.");
      return;
    }
    this.settings = r.conditions ?? next;
    this.changed.fire(undefined);
    this.afterSync(r, what);
  }

  private copy(): ConditionSettings {
    const s = this.settings;
    return { defines: { ...s.defines }, undefines: [...s.undefines], assumeUndefined: [...s.assumeUndefined], conditionFiles: [...s.conditionFiles] };
  }

  /** `NAME` or `NAME=value` is defined; a name is one thing or the other. */
  async define(text: string) {
    const [name, ...rest] = text.split("=");
    const next = this.copy();
    next.defines[name.trim()] = rest.join("=").trim();
    next.undefines = next.undefines.filter((u) => u !== name.trim());
    await this.apply(next, `Defined ${name.trim()}`);
  }

  async undefine(name: string) {
    const next = this.copy();
    delete next.defines[name];
    if (!next.undefines.includes(name)) {
      next.undefines.push(name);
    }
    await this.apply(next, `${name} is not defined`);
  }

  async add(group: "conditionFiles" | "assumeUndefined", value: string) {
    const next = this.copy();
    if (!next[group].includes(value)) {
      next[group].push(value);
    }
    await this.apply(next, group === "conditionFiles" ? `Reading conditions from ${value}` : `Assuming ${value} is not defined`);
  }

  async remove(row: ConditionRow) {
    if (row.key === undefined) {
      return;
    }
    const next = this.copy();
    if (row.group === "defines") {
      delete next.defines[row.key];
    } else {
      next[row.group] = next[row.group].filter((x) => x !== row.key);
    }
    await this.apply(next, `Removed ${row.key}`);
  }
}

// ---------------------------------------------------------------- webviews

const STYLE = `
  body { padding: 0; color: var(--vscode-foreground); font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); }
  input { width: 100%; box-sizing: border-box; margin: 6px 0; padding: 4px 6px; color: var(--vscode-input-foreground);
          background: var(--vscode-input-background); border: 1px solid var(--vscode-input-border, transparent); outline-offset: -1px; }
  input:focus { outline: 1px solid var(--vscode-focusBorder); }
  .wrap { padding: 0 8px; }
  .row { display: flex; gap: 8px; padding: 2px 8px; cursor: pointer; white-space: nowrap; align-items: baseline; }
  html, body { height: 100%; }
  body { display: flex; flex-direction: column; }
  #bar { display: flex; align-items: center; gap: 8px; padding: 0 0 4px; }
  #count { color: var(--vscode-descriptionForeground); font-size: 0.85em; }
  #order { margin-left: auto; display: flex; border: 1px solid var(--vscode-panel-border); border-radius: 3px; overflow: hidden; }
  #order button { font: inherit; font-size: 0.85em; padding: 0 8px; border: 0; background: none; color: var(--vscode-descriptionForeground); cursor: pointer; }
  #order button[aria-pressed="true"] { background: var(--vscode-list-activeSelectionBackground); color: var(--vscode-list-activeSelectionForeground); }
  #order button:focus-visible { outline: 1px solid var(--vscode-focusBorder); outline-offset: -1px; }
  #scroll { flex: 1; overflow-y: auto; position: relative; min-height: 0; }
  #scroll #list .row { position: absolute; left: 0; right: 0; height: 22px; box-sizing: border-box; padding: 0 8px; align-items: center; }
  #space { width: 1px; }
  .row:hover, .row:focus { background: var(--vscode-list-hoverBackground); outline: none; }
  .name { font-family: var(--vscode-editor-font-family); overflow: hidden; text-overflow: ellipsis; }
  .name b { color: var(--vscode-list-highlightForeground); font-weight: 600; }
  .dim { color: var(--vscode-descriptionForeground); font-size: 0.9em; margin-left: auto; overflow: hidden; text-overflow: ellipsis; }
  .codicon { flex: none; align-self: center; font-size: 14px !important; }
  .k-function { color: var(--vscode-symbolIcon-functionForeground, var(--vscode-charts-purple)); }
  .k-macro { color: var(--vscode-symbolIcon-constantForeground, var(--vscode-charts-orange)); }
  .k-field { color: var(--vscode-symbolIcon-fieldForeground, var(--vscode-charts-blue)); }
  .k-global { color: var(--vscode-symbolIcon-variableForeground, var(--vscode-charts-blue)); }
  .k-struct, .k-union, .k-enum, .k-typedef { color: var(--vscode-symbolIcon-structForeground, var(--vscode-charts-yellow)); }
  .k-enum_const { color: var(--vscode-symbolIcon-enumeratorMemberForeground, var(--vscode-charts-blue)); }
  .head { padding: 4px 8px; border-bottom: 1px solid var(--vscode-panel-border); cursor: pointer; display: flex; gap: 10px; flex-wrap: wrap; }
  .head strong { font-family: var(--vscode-editor-font-family); }
  pre { margin: 0; padding: 4px 0; font-family: var(--vscode-editor-font-family); font-size: var(--vscode-editor-font-size); line-height: 1.5; }
  pre div { padding: 0 8px; white-space: pre; }
  pre span.n { display: inline-block; width: 4.5em; padding-right: 1em; text-align: right; color: var(--vscode-editorLineNumber-foreground); }
  pre div.at { background: var(--vscode-editor-lineHighlightBackground, var(--vscode-list-hoverBackground)); }
  .empty { padding: 8px; color: var(--vscode-descriptionForeground); }
`;

function page(w: vscode.Webview, body: string, script: string): string {
  const nonce = Math.random().toString(36).slice(2);
  const icons = codicons(w);
  w.options = { enableScripts: true, localResourceRoots: icons.roots };
  return `<!doctype html><html><head><meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; ${icons.csp} script-src 'nonce-${nonce}';">
    ${icons.link}<style>${STYLE}</style></head><body>${body}<script nonce="${nonce}">const vscode = acquireVsCodeApi();${script}</script></body></html>`;
}

/** Fuzzy project symbol search: fragments in any order, initials. */
class Search implements vscode.WebviewViewProvider {
  private view: vscode.WebviewView | undefined;

  /** The index changed: what is listed may have too. */
  refresh() {
    void this.view?.webview.postMessage({ type: "reset" });
  }

  resolveWebviewView(view: vscode.WebviewView) {
    this.view = view;
    view.webview.html = page(
      view.webview,
      `<div class="wrap"><input id="q" placeholder="Fragments or initials: init cache, bci" spellcheck="false">
       <div id="bar"><span id="count"></span><span id="order" role="group" aria-label="Sort"><button data-order="name" title="Sort by name">Name</button><button data-order="kind" title="Functions together, then macros, types, variables and members; each by name">Kind</button></span></div></div>
       <div id="scroll"><div id="space"></div><div id="list"></div></div>`,
      `
      const q = document.getElementById("q"), list = document.getElementById("list"), space = document.getElementById("space");
      const scroll = document.getElementById("scroll"), count = document.getElementById("count");
      const ROW = 22, PAGE = 200;
      // With nothing typed: every symbol of the project in name order,
      // fetched a page at a time as it scrolls into view. With something
      // typed: the matches, best first.
      let timer, query = "", results = [], total = 0, pages = new Map(), asked = new Set(), epoch = 0;
      let order = (vscode.getState() || {}).order || "name";
      const RANK = { function: 0, macro: 1, struct: 2, union: 3, enum: 4, enum_const: 5, typedef: 6, global: 7, field: 8 };
      // Matches come best first; by kind, the best of each kind first.
      const arranged = (found) => (order === "kind" ? found.map((r, i) => [r, i]).sort((a, b) => RANK[a[0].kind] - RANK[b[0].kind] || a[1] - b[1]).map((x) => x[0]) : found);
      let found = [];
      const esc = (s) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
      // Mark what matched: each word as a substring, else its letters in order.
      function mark(name, words) {
        if (!words.length) return esc(name);
        const on = new Array(name.length).fill(false), lower = name.toLowerCase();
        for (const w of words) {
          const i = lower.indexOf(w);
          if (i >= 0) { for (let k = 0; k < w.length; k++) on[i + k] = true; continue; }
          let k = 0;
          for (let j = 0; j < lower.length && k < w.length; j++) if (lower[j] === w[k]) { on[j] = true; k++; }
        }
        let out = "";
        for (let i = 0; i < name.length; i++) out += on[i] ? "<b>" + esc(name[i]) + "</b>" : esc(name[i]);
        return out;
      }
      const words = () => query.toLowerCase().split(/\\s+/).filter(Boolean);
      const item = (i) => (query ? results[i] : pages.get(Math.floor(i / PAGE))?.[i % PAGE]);
      function row(r, i, w) {
        if (!r) return '<div class="row" style="top:' + i * ROW + 'px"><span class="dim">…</span></div>';
        return '<div class="row" tabindex="0" data-i="' + i + '" style="top:' + i * ROW + 'px" title="' + esc(r.kindName + " " + r.name + "\\n" + r.file + ":" + r.line) + '"><span class="codicon codicon-' + r.icon + " k-" + r.kind + '"></span><span class="name">' + mark(r.name, w) + '</span><span class="dim">' + esc(r.file.split("/").pop()) + ":" + r.line + "</span></div>";
      }
      function render() {
        const n = query ? results.length : total;
        space.style.height = n * ROW + "px";
        count.textContent = query ? (n ? n + " matches" : "No matching symbols") : (n ? n.toLocaleString() + " symbols" : "");
        document.querySelectorAll("[data-order]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.order === order)));
        const first = Math.max(0, Math.floor(scroll.scrollTop / ROW) - 10);
        const last = Math.min(n, Math.ceil((scroll.scrollTop + scroll.clientHeight) / ROW) + 10);
        const w = words();
        let html = "";
        for (let i = first; i < last; i++) html += row(item(i), i, w);
        list.innerHTML = html;
        if (!query) {
          for (let p = Math.floor(first / PAGE); p <= Math.floor(Math.max(first, last - 1) / PAGE); p++) {
            if (!pages.has(p) && !asked.has(p)) { asked.add(p); vscode.postMessage({ page: p, size: PAGE, epoch, order }); }
          }
        }
      }
      function reset() { epoch++; pages = new Map(); asked = new Set(); render(); }
      document.getElementById("order").addEventListener("click", (e) => {
        const b = e.target.closest("button");
        if (!b || b.dataset.order === order) return;
        order = b.dataset.order;
        vscode.setState({ order });
        scroll.scrollTop = 0;
        results = arranged(found);
        reset();
      });
      scroll.addEventListener("scroll", render);
      new ResizeObserver(render).observe(scroll);
      q.addEventListener("input", () => {
        clearTimeout(timer);
        timer = setTimeout(() => {
          query = q.value.trim();
          scroll.scrollTop = 0;
          if (query) vscode.postMessage({ query }); else { found = []; results = []; render(); }
        }, 60);
      });
      q.addEventListener("keydown", (e) => { const r = item(0); if (e.key === "Enter" && r) vscode.postMessage({ open: r, beside: e.ctrlKey || e.metaKey }); });
      list.addEventListener("click", (e) => {
        const el = e.target.closest("[data-i]");
        const r = el && item(+el.dataset.i);
        if (r) vscode.postMessage({ open: r, beside: e.ctrlKey || e.metaKey });
      });
      window.addEventListener("message", (e) => {
        const d = e.data;
        if (d.type === "reset") return reset();
        if (d.type === "page") {
          if (d.epoch !== epoch) return;
          total = d.total;
          pages.set(d.page, d.items);
        } else if (d.query === query) {
          found = d.results;
          results = arranged(found);
        }
        render();
      });
      reset();
      q.focus();`,
    );
    const decorate = <T extends { kind: string }>(e: T) => ({ ...e, kindName: KIND_NAMES[e.kind] ?? e.kind, icon: ICONS[e.kind] ?? "symbol-misc" });
    view.webview.onDidReceiveMessage(async (m: { query?: string; page?: number; size?: number; epoch?: number; order?: string; open?: { file: string; line: number; name: string }; beside?: boolean }) => {
      if (m.open) {
        // A listing carries no columns: ask where the name is written.
        const at = await ask<{ uri: string; range: Entry["range"] }>("si/place", { file: m.open.file, line: m.open.line, name: m.open.name });
        if (at) {
          await open(at.uri, at.range.start.line, at.range.start.character, !!m.beside);
        }
      } else if (m.page !== undefined) {
        const size = m.size ?? 200;
        const r = await ask<{ total: number; items: { name: string; kind: string; file: string; line: number }[] }>("si/symbols", { offset: m.page * size, limit: size, order: m.order ?? "name" });
        void view.webview.postMessage({ type: "page", page: m.page, epoch: m.epoch, total: r?.total ?? 0, items: (r?.items ?? []).map(decorate) });
      } else if (m.query !== undefined) {
        const found = (await ask<Entry[]>("si/search", { query: m.query, limit: 200 })) ?? [];
        void view.webview.postMessage({ type: "results", query: m.query, results: found.map(decorate) });
      }
    });
  }
}

/** The definition of whatever the cursor is on, without going there. */
class Context implements vscode.WebviewViewProvider {
  private view: vscode.WebviewView | undefined;
  locked = false;
  private last = "";

  resolveWebviewView(view: vscode.WebviewView) {
    this.view = view;
    view.webview.html = page(
      view.webview,
      `<div id="root"><div class="empty">Put the cursor on a name to see its definition here.</div></div>`,
      `
      const root = document.getElementById("root");
      let shown;
      const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
      root.addEventListener("click", (e) => { if (shown) vscode.postMessage({ open: shown, beside: e.ctrlKey || e.metaKey }); });
      window.addEventListener("message", (e) => {
        const d = e.data;
        shown = d.entry;
        if (!d.entry) { root.innerHTML = '<div class="empty">' + esc(d.message) + "</div>"; return; }
        const more = d.entry.candidates > 1 ? '<span class="dim">' + d.entry.candidates + " candidates</span>" : "";
        root.innerHTML = '<div class="head"><strong>' + esc(d.entry.name) + "</strong><span>" + esc(d.kindName) + "</span><span>" + esc(d.entry.file) + ":" + d.entry.line + "</span>" + more + "</div><pre>"
          + d.lines.map((t, i) => '<div class="' + (d.first + i === d.entry.line ? "at" : "") + '"><span class="n">' + (d.first + i) + "</span>" + esc(t) + "</div>").join("") + "</pre>";
      });`,
    );
    view.webview.onDidReceiveMessage(async (m: { open: Entry; beside?: boolean }) => {
      await open(m.open.uri, m.open.range.start.line, m.open.range.start.character, !!m.beside);
    });
    if (current) {
      void this.follow(current);
    }
  }

  async follow(editor: vscode.TextEditor) {
    if (this.locked || !this.view?.visible) {
      return;
    }
    const at = positionParams(editor);
    const key = JSON.stringify(at);
    if (key === this.last) {
      return;
    }
    this.last = key;
    const entry = await ask<Entry | null>("si/context", at);
    if (key !== this.last) {
      return; // the cursor has moved on
    }
    if (!entry) {
      void this.view.webview.postMessage({ entry: null, message: "Nothing the index knows here." });
      return;
    }
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(entry.uri));
    const first = Math.max(1, entry.line - 2);
    const last = Math.min(doc.lineCount, Math.max(entry.endLine, entry.line) + 1, first + 40);
    const lines: string[] = [];
    for (let l = first; l <= last; l++) {
      lines.push(doc.lineAt(l - 1).text);
    }
    void this.view.webview.postMessage({ entry, kindName: KIND_NAMES[entry.kind] ?? entry.kind, first, lines });
  }
}

// ---------------------------------------------------------------- CodeLens

class Lenses implements vscode.CodeLensProvider {
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.changed.event;

  refresh() {
    this.changed.fire();
  }

  async provideCodeLenses(doc: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    if (!vscode.workspace.getConfiguration("si").get<boolean>("codeLens.enabled", true) || !isSource(doc)) {
      return [];
    }
    const symbols = (await ask<{ name: string; kind: number; location: { range: Entry["range"] } }[]>("textDocument/documentSymbol", { textDocument: { uri: doc.uri.toString() } })) ?? [];
    // Definitions only: a function symbol whose line opens a body further down.
    return symbols
      .filter((s) => s.kind === 12)
      .map((s) => {
        const r = s.location.range;
        const lens = new vscode.CodeLens(new vscode.Range(r.start.line, r.start.character, r.end.line, r.end.character));
        (lens as vscode.CodeLens & { si: unknown }).si = { textDocument: { uri: doc.uri.toString() }, position: r.start };
        return lens;
      });
  }

  async resolveCodeLens(lens: vscode.CodeLens): Promise<vscode.CodeLens> {
    const at = (lens as vscode.CodeLens & { si: ReturnType<typeof positionParams> }).si;
    const [callers, callees] = await Promise.all([
      ask<{ entries: Entry[] }>("si/relation", { ...at, direction: "callers" }),
      ask<{ entries: Entry[] }>("si/relation", { ...at, direction: "callees" }),
    ]);
    const indirect = (callees?.entries ?? []).filter((e) => e.through || e.pointer).length;
    const parts = [`${callers?.entries.length ?? 0} callers`, `${callees?.entries.length ?? 0} calls`];
    if (indirect) {
      parts.push(`${indirect} through function pointers`);
    }
    lens.command = { title: parts.join(" · "), command: "si.showRelationAt", arguments: [at] };
    return lens;
  }
}

// ---------------------------------------------------------------- activation

/** The setting as a path: quotes around it (as copied from Explorer or a
 *  shell), `~` and `${workspaceFolder}` are all things people write. */
function serverPath(): string {
  let p = vscode.workspace.getConfiguration("si").get<string>("serverPath", "si").trim();
  p = p.replace(/^(["'])(.*)\1$/, "$2").trim();
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (folder) {
    p = p.replace(/\$\{workspaceFolder\}/g, folder);
  }
  if (p === "~" || p.startsWith("~/") || p.startsWith("~\\")) {
    p = path.join(os.homedir(), p.slice(1));
  }
  return p || "si";
}

function serverCommand(): { command: string; args: string[] } {
  const config = vscode.workspace.getConfiguration("si");
  const aliases = config.get<string[]>("includeAliases", []);
  const args = ["lsp", ...aliases.flatMap((a) => ["--include-alias", a])];
  // The index is kept between sessions unless told otherwise.
  if (!config.get<boolean>("cache.enabled", true)) {
    args.push("--no-cache");
  } else if (config.get<string>("cache.directory", "").trim()) {
    args.push("--cache-dir", config.get<string>("cache.directory", "").trim());
  }
  return { command: serverPath(), args };
}

export async function activate(context: vscode.ExtensionContext) {
  home = context.extensionUri;
  const relation = new Relation(vscode.Uri.joinPath(context.extensionUri, "media"));
  const fileSymbols = new FileSymbols();
  const search = new Search();
  const contextView = new Context();
  const lenses = new Lenses();
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 10);
  status.text = "$(sync~spin) SI indexing";
  status.command = "si.sync";
  status.show();

  const selector: vscode.DocumentSelector = [{ scheme: "file", pattern: "**/*.{c,h}" }];

  async function refreshStatus() {
    const s = await ask<{ files: number; symbols: number; loaded: number; parsed: number; cache: string | null }>("si/status", {});
    status.text = s ? `$(symbol-namespace) SI ${s.files.toLocaleString()} files · ${s.symbols.toLocaleString()} symbols` : "$(warning) SI not running";
    status.tooltip = s
      ? `Click to sync the index with the disk.\nAt start: ${s.loaded.toLocaleString()} files read from the saved index, ${s.parsed.toLocaleString()} parsed
` + (s.cache ? `Index kept in ${s.cache}` : "Index not kept (si.cache.enabled is off)")
      : undefined;
  }

  /** What the server says while it indexes: files parsed so far, then linking. */
  function progress(p: { phase: "parsing" | "linking" | "done"; done: number; total: number }) {
    if (p.phase === "done") {
      void refreshStatus();
    } else if (p.phase === "linking") {
      status.text = "$(sync~spin) SI linking";
    } else {
      status.text = `$(sync~spin) SI parsing ${p.done.toLocaleString()} / ${p.total.toLocaleString()} files`;
    }
  }
  /** Run something that indexes, with the views it comes from showing that they are busy. */
  async function busy<T>(viewId: string, what: string, work: () => Promise<T>): Promise<T> {
    status.text = `$(sync~spin) SI ${what}`;
    return vscode.window.withProgress({ location: { viewId } }, work);
  }

  function afterSync(r: Synced, what: string) {
    fileSymbols.refresh();
    search.refresh();
    lenses.refresh();
    void refreshStatus();
    if (current) {
      follow(current, true);
    }
    vscode.window.setStatusBarMessage(
      `SI: ${what}: ${r.added} added, ${r.removed} removed, ${r.changed} changed, ${r.parsed} parsed (${Math.round(r.ms)} ms)`,
      6000,
    );
  }
  let scopeView: vscode.TreeView<string> | undefined;
  const scopeTree = new ScopeTree(afterSync, busy, (pending) => {
    void vscode.commands.executeCommand("setContext", "si.scopePending", pending);
    if (scopeView) {
      scopeView.message = pending ? "Not applied yet. Press Apply (the check mark) to index this selection." : undefined;
    }
  });
  scopeView = vscode.window.createTreeView("si.scope", { treeDataProvider: scopeTree, manageCheckboxStateManually: true });
  scopeView.onDidChangeCheckboxState((e) => {
    // Checking only prepares; Apply indexes.
    for (const [rel, state] of e.items) {
      scopeTree.toggle(rel, state === vscode.TreeItemCheckboxState.Checked);
    }
  });
  /** A folder from the scope tree (its path) or from the Explorer (a URI). */
  const folderOf = (arg: unknown): string | undefined => (typeof arg === "string" ? arg : arg instanceof vscode.Uri ? scopeTree.relative(arg) : undefined);
  const conditions = new ConditionsTree(afterSync, busy);
  const validName = (v: string) => (/^[A-Za-z_][A-Za-z0-9_]*(\s*=.*)?$/.test(v.trim()) ? undefined : "A macro name, optionally followed by =value");
  async function sync() {
    const r = await busy("si.scope", "syncing", () => ask<Synced>("si/sync", {}));
    if (r) {
      scopeTree.scope = { include: r.include, exclude: r.exclude, config: r.config };
      await conditions.load();
      afterSync(r, "Synced with disk");
    } else {
      await refreshStatus();
    }
  }

  let timer: NodeJS.Timeout | undefined;
  function follow(editor: vscode.TextEditor | undefined, now = false) {
    // Only a C source editor changes what the views are about: clicking into
    // a terminal or a settings page leaves them as they are.
    if (!editor || !isSource(editor.document)) {
      return;
    }
    current = editor;
    clearTimeout(timer);
    timer = setTimeout(() => {
      void fileSymbols.show(editor);
      void contextView.follow(editor);
      relation.follow(editor);
    }, now ? 0 : 150);
  }

  /** A folder opened for the first time has no scope yet. Indexing all of
   *  a very large tree is not something to do unasked: offer to choose the
   *  folders first. Returns false if the user backed out. */
  async function firstTime(): Promise<boolean> {
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!root || fs.existsSync(path.join(root, ".si", "config.json"))) {
      return true;
    }
    const all = "Index Everything", choose = "Choose Folders First";
    const pick = await vscode.window.showInformationMessage(
      "SI has not indexed this folder before.",
      { modal: true, detail: "Index every C file in it, or choose which folders to index first? The choice is kept in .si/config.json and can be changed any time in the Index Scope view." },
      all,
      choose,
    );
    if (!pick) {
      return false;
    }
    fs.mkdirSync(path.join(root, ".si"), { recursive: true });
    fs.writeFileSync(path.join(root, ".si", "config.json"), JSON.stringify({ include: [], exclude: pick === choose ? ["."] : [] }, null, 2) + "\n");
    if (pick === choose) {
      void vscode.commands.executeCommand("si.scope.focus");
    }
    return true;
  }

  async function start(rebuild = false) {
    if (!(await firstTime())) {
      status.text = "$(circle-slash) SI not started";
      status.tooltip = "Click to choose what to index.";
      status.command = "si.restart";
      return;
    }
    status.command = "si.sync";
    const { command, args } = serverCommand();
    if (rebuild && !args.includes("--no-cache")) {
      args.push("--rebuild");
    }
    // A path that is not there is the usual reason for not starting: say so
    // plainly instead of leaving a spawn error in the log.
    if (/[\/]/.test(command) && !fs.existsSync(command)) {
      status.text = "$(warning) SI not running";
      const pick = await vscode.window.showErrorMessage(`si executable not found: ${command}`, "Open Settings");
      if (pick) {
        void vscode.commands.executeCommand("workbench.action.openSettings", "si.serverPath");
      }
      return;
    }
    const serverOptions: ServerOptions = { command, args, options: { cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath } };
    const clientOptions: LanguageClientOptions = {
      documentSelector: selector as LanguageClientOptions["documentSelector"],
      synchronize: { fileEvents: vscode.workspace.createFileSystemWatcher("**/*.{c,h}") },
    };
    client = new LanguageClient("si", "SI", serverOptions, clientOptions);
    client.onNotification("si/progress", progress);
    try {
      await client.start();
    } catch (e) {
      status.text = "$(warning) SI not running";
      void vscode.window.showErrorMessage(`Could not start si (${command}). Set si.serverPath to the si executable. ${e instanceof Error ? e.message : ""}`);
      return;
    }
    await refreshStatus();
    await scopeTree.load();
    await conditions.load();
    follow(vscode.window.activeTextEditor, true);
  }

  const lock = (key: string, view: { locked: boolean }, on: boolean) => {
    view.locked = on;
    void vscode.commands.executeCommand("setContext", key, on);
    if (!on) {
      follow(current, true);
    }
  };

  context.subscriptions.push(
    status,
    vscode.window.registerWebviewViewProvider("si.relation", relation, { webviewOptions: { retainContextWhenHidden: true } }),
    vscode.window.registerTreeDataProvider("si.fileSymbols", fileSymbols),
    vscode.window.registerWebviewViewProvider("si.search", search, { webviewOptions: { retainContextWhenHidden: true } }),
    vscode.window.registerWebviewViewProvider("si.context", contextView),
    vscode.languages.registerCodeLensProvider(selector, lenses),
    vscode.window.onDidChangeActiveTextEditor((e) => follow(e, true)),
    vscode.window.onDidChangeTextEditorSelection((e) => {
      // The same file may be open in two groups; only the focused one counts.
      if (e.textEditor === vscode.window.activeTextEditor) {
        follow(e.textEditor);
      }
    }),
    vscode.workspace.onDidSaveTextDocument(async (doc) => {
      // The scope edited by hand takes effect when its file is saved.
      if (scopeTree.scope.config && doc.uri.fsPath.toLowerCase() === scopeTree.scope.config.toLowerCase()) {
        await sync();
        return;
      }
      if (isSource(doc)) {
        // The server re-indexes on the save notification; ask after it has.
        await refreshStatus();
        fileSymbols.refresh();
        search.refresh();
        lenses.refresh();
      }
    }),
    vscode.commands.registerCommand("si.open", (t: { uri: string; line: number; character: number }, beside: boolean) => open(t.uri, t.line, t.character, beside)),
    vscode.commands.registerCommand("si.openToSide", (n: Node) => n?.target && open(n.target.uri, n.target.line, n.target.character, true)),
    vscode.commands.registerCommand("si.relation.tree", () => relation.setView("tree")),
    vscode.commands.registerCommand("si.relation.graph", () => relation.setView("graph")),
    vscode.commands.registerCommand("si.relation.callers", () => relation.setMode("callers")),
    vscode.commands.registerCommand("si.relation.callees", () => relation.setMode("callees")),
    vscode.commands.registerCommand("si.relation.references", () => relation.setMode("references")),
    vscode.commands.registerCommand("si.relation.assignments", () => relation.setMode("assignments")),
    vscode.commands.registerCommand("si.relation.lock", () => lock("si.relationLocked", relation, true)),
    vscode.commands.registerCommand("si.relation.unlock", () => lock("si.relationLocked", relation, false)),
    vscode.commands.registerCommand("si.context.lock", () => lock("si.contextLocked", contextView, true)),
    vscode.commands.registerCommand("si.context.unlock", () => lock("si.contextLocked", contextView, false)),
    vscode.commands.registerCommand("si.showRelationAt", async (at: ReturnType<typeof positionParams>) => {
      await vscode.commands.executeCommand("si.relation.focus");
      const uri = vscode.Uri.parse(at.textDocument.uri);
      const editor = vscode.window.visibleTextEditors.find((e) => e.document.uri.toString() === uri.toString());
      if (editor) {
        const p = new vscode.Position(at.position.line, at.position.character);
        editor.selection = new vscode.Selection(p, p);
        relation.locked = false;
        void vscode.commands.executeCommand("setContext", "si.relationLocked", false);
        follow(editor, true);
      }
    }),
    vscode.commands.registerCommand("si.restart", async () => {
      status.text = "$(sync~spin) SI indexing";
      if (client?.isRunning()) {
        await client.stop();
      }
      await start();
    }),
    scopeView,
    vscode.window.registerTreeDataProvider("si.conditions", conditions),
    vscode.commands.registerCommand("si.conditions.define", async (arg?: unknown) => {
      const text = typeof arg === "string" ? arg : await vscode.window.showInputBox({ prompt: "Macro the build defines", placeHolder: "CONFIG_SMP  or  ARCH_SIZE=64", validateInput: validName });
      return text ? conditions.define(text.trim()) : undefined;
    }),
    vscode.commands.registerCommand("si.conditions.undefine", async (arg?: unknown) => {
      const text = typeof arg === "string" ? arg : await vscode.window.showInputBox({ prompt: "Macro the build does not define", placeHolder: "_WIN32", validateInput: (v) => (/^[A-Za-z_][A-Za-z0-9_]*$/.test(v.trim()) ? undefined : "A macro name") });
      return text ? conditions.undefine(text.trim()) : undefined;
    }),
    vscode.commands.registerCommand("si.conditions.addFile", async (arg?: unknown) => {
      const root = vscode.workspace.workspaceFolders?.[0]?.uri;
      let file = typeof arg === "string" ? arg : undefined;
      if (!file && root) {
        const picked = await vscode.window.showOpenDialog({ defaultUri: root, canSelectMany: false, openLabel: "Read Conditions from This File", title: "A header of #define lines, or a Kconfig .config" });
        // Inside the workspace a relative path keeps the setting portable.
        file = picked?.[0] ? scopeTree.relative(picked[0]) ?? picked[0].fsPath : undefined;
      }
      return file ? conditions.add("conditionFiles", file) : undefined;
    }),
    vscode.commands.registerCommand("si.conditions.assume", async (arg?: unknown) => {
      const text = typeof arg === "string" ? arg : await vscode.window.showInputBox({
        prompt: "Names with this prefix are not defined unless listed or defined in the sources",
        placeHolder: "CONFIG_   (or * for every name the build does not define)",
      });
      return text ? conditions.add("assumeUndefined", text.trim()) : undefined;
    }),
    vscode.commands.registerCommand("si.conditions.remove", (row: ConditionRow) => conditions.remove(row)),
    vscode.commands.registerCommand("si.conditions.get", () => conditions.settings),
    vscode.commands.registerCommand("si.sync", sync),
    vscode.commands.registerCommand("si.scope.apply", () => scopeTree.applyDraft()),
    vscode.commands.registerCommand("si.scope.discard", () => scopeTree.discard()),
    vscode.commands.registerCommand("si.scope.selectAll", () => scopeTree.selectAll()),
    vscode.commands.registerCommand("si.scope.clearAll", () => scopeTree.clearAll()),
    vscode.commands.registerCommand("si.resetLayout", async () => {
      // Put every SI view back where it started, whatever was dragged where.
      for (const view of ["si.search", "si.fileSymbols", "si.scope", "si.relation", "si.context"]) {
        try {
          await vscode.commands.executeCommand(`${view}.resetViewLocation`);
        } catch {
          // Already at home.
        }
      }
      await vscode.commands.executeCommand("si.relation.focus");
    }),
    vscode.commands.registerCommand("si.scope.include", (arg: unknown) => {
      const rel = folderOf(arg);
      return rel !== undefined && rel !== "" ? scopeTree.set(rel, true) : undefined;
    }),
    vscode.commands.registerCommand("si.scope.exclude", (arg: unknown) => {
      const rel = folderOf(arg);
      return rel !== undefined && rel !== "" ? scopeTree.set(rel, false) : undefined;
    }),
    vscode.commands.registerCommand("si.scope.only", (arg: unknown) => {
      const rel = folderOf(arg);
      return rel ? scopeTree.apply({ include: [rel], exclude: [] }, `Indexing only ${rel}`) : undefined;
    }),
    vscode.commands.registerCommand("si.scope.all", () => scopeTree.apply({ include: [], exclude: [] }, "Indexing the whole workspace")),
    vscode.commands.registerCommand("si.scope.pending", () => !!scopeTree.draft),
    vscode.commands.registerCommand("si.scope.openConfig", async () => {
      if (!scopeTree.scope.config || !fs.existsSync(scopeTree.scope.config)) {
        // Nothing chosen yet: write the setting out so there is a file to edit.
        await scopeTree.apply({ include: scopeTree.scope.include, exclude: scopeTree.scope.exclude }, "Wrote the scope");
      }
      if (scopeTree.scope.config) {
        await vscode.window.showTextDocument(vscode.Uri.file(scopeTree.scope.config));
      }
    }),
    vscode.commands.registerCommand("si.rebuild", async () => {
      status.text = "$(sync~spin) SI rebuilding";
      if (client?.isRunning()) {
        await client.stop();
      }
      await start(true);
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      // A corrected path should take effect without reloading the window.
      if (e.affectsConfiguration("si.serverPath") || e.affectsConfiguration("si.includeAliases") || e.affectsConfiguration("si.cache")) {
        void vscode.commands.executeCommand("si.restart");
      }
    }),
    { dispose: () => void client?.stop() },
  );
  await start();
}

export async function deactivate() {
  await client?.stop();
}
