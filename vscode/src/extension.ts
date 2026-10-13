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

type Mode = "callees" | "callers" | "references";

let client: LanguageClient | undefined;
/** The text editor that last had focus: what the views are about. */
let current: vscode.TextEditor | undefined;

const ICONS: Record<string, string> = {
  function: "symbol-function", macro: "symbol-constant", struct: "symbol-struct", union: "symbol-struct",
  enum: "symbol-enum", enum_const: "symbol-enum-member", typedef: "symbol-class", global: "symbol-variable",
  field: "symbol-field", file: "symbol-file",
};
const KIND_NAMES: Record<string, string> = {
  function: "函式", macro: "巨集", struct: "結構", union: "聯合", enum: "列舉", enum_const: "列舉常數",
  typedef: "型別別名", global: "全域變數", field: "結構成員",
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
    w.options = { enableScripts: true, localResourceRoots: [this.media] };
    const file = (name: string) => w.asWebviewUri(vscode.Uri.joinPath(this.media, name));
    w.html = `<!doctype html><html><head><meta charset="utf-8">
      <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${w.cspSource} 'unsafe-inline'; script-src ${w.cspSource};">
      <link rel="stylesheet" href="${file("relation.css")}"></head><body>
      <div id="bar" role="tablist">
        <button role="tab" data-mode="callees">被呼叫者</button>
        <button role="tab" data-mode="callers">呼叫者</button>
        <button role="tab" data-mode="references">參照</button>
        <span id="views"><button data-view="tree">樹狀</button><button data-view="graph">圖形</button></span>
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
    const r = await ask<{ root: Entry | null; entries: Entry[] }>("si/relation", { ...at, direction: this.mode });
    if (at !== this.at) {
      return; // the cursor has moved on
    }
    const what = this.mode === "callers" ? "呼叫者" : "被呼叫者";
    const message = r?.root ? `${r.root.name} 的${what}：${r.entries.length}` : "游標不在函式上";
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
    return { groups, message: `參照：${found.length} 處，${byFile.size} 個檔案` };
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
      [14, "巨集", "symbol-constant"], [12, "函式", "symbol-function"], [23, "結構", "symbol-struct"], [10, "列舉", "symbol-enum"],
      [22, "列舉常數", "symbol-enum-member"], [5, "型別別名", "symbol-class"], [13, "全域變數", "symbol-variable"], [8, "結構成員", "symbol-field"],
    ];
    this.groups = [];
    for (const [kind, title] of order) {
      const kids = symbols
        .filter((s) => s.kind === kind)
        .map((s) => new Node(s.name, undefined, { uri, line: s.location.range.start.line, character: s.location.range.start.character }, undefined, String(s.location.range.start.line + 1)));
      if (kids.length) {
        this.groups.push(new Node(title, undefined, undefined, kids, String(kids.length)));
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
    if (node.target) {
      item.command = { command: "si.open", title: "開啟", arguments: [node.target, false] };
    }
    return item;
  }

  getChildren(node?: Node): Node[] {
    return node ? node.children ?? [] : this.groups;
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
  .row:hover, .row:focus { background: var(--vscode-list-hoverBackground); outline: none; }
  .name { font-family: var(--vscode-editor-font-family); overflow: hidden; text-overflow: ellipsis; }
  .name b { color: var(--vscode-list-highlightForeground); font-weight: 600; }
  .dim { color: var(--vscode-descriptionForeground); font-size: 0.9em; margin-left: auto; overflow: hidden; text-overflow: ellipsis; }
  .kind { color: var(--vscode-descriptionForeground); font-size: 0.85em; flex: none; width: 4.5em; }
  .head { padding: 4px 8px; border-bottom: 1px solid var(--vscode-panel-border); cursor: pointer; display: flex; gap: 10px; flex-wrap: wrap; }
  .head strong { font-family: var(--vscode-editor-font-family); }
  pre { margin: 0; padding: 4px 0; font-family: var(--vscode-editor-font-family); font-size: var(--vscode-editor-font-size); line-height: 1.5; }
  pre div { padding: 0 8px; white-space: pre; }
  pre span.n { display: inline-block; width: 4.5em; padding-right: 1em; text-align: right; color: var(--vscode-editorLineNumber-foreground); }
  pre div.at { background: var(--vscode-editor-lineHighlightBackground, var(--vscode-list-hoverBackground)); }
  .empty { padding: 8px; color: var(--vscode-descriptionForeground); }
`;

function page(body: string, script: string): string {
  const nonce = Math.random().toString(36).slice(2);
  return `<!doctype html><html><head><meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
    <style>${STYLE}</style></head><body>${body}<script nonce="${nonce}">const vscode = acquireVsCodeApi();${script}</script></body></html>`;
}

/** Fuzzy project symbol search: fragments in any order, initials. */
class Search implements vscode.WebviewViewProvider {
  resolveWebviewView(view: vscode.WebviewView) {
    view.webview.options = { enableScripts: true };
    view.webview.html = page(
      `<div class="wrap"><input id="q" placeholder="片段或縮寫，例如 init cache、bci" spellcheck="false"></div><div id="list"></div>`,
      `
      const q = document.getElementById("q"), list = document.getElementById("list");
      let timer, results = [];
      const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
      // Mark what matched: each word as a substring, else its letters in order.
      function mark(name, words) {
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
      q.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => vscode.postMessage({ query: q.value }), 60); });
      q.addEventListener("keydown", (e) => { if (e.key === "Enter" && results[0]) vscode.postMessage({ open: results[0], beside: e.ctrlKey || e.metaKey }); });
      list.addEventListener("click", (e) => {
        const row = e.target.closest(".row");
        if (row) vscode.postMessage({ open: results[+row.dataset.i], beside: e.ctrlKey || e.metaKey });
      });
      window.addEventListener("message", (e) => {
        results = e.data.results;
        const words = e.data.query.toLowerCase().split(/\\s+/).filter(Boolean);
        list.innerHTML = results.length
          ? results.map((r, i) => '<div class="row" tabindex="0" data-i="' + i + '"><span class="kind">' + esc(r.kindName) + '</span><span class="name">' + mark(r.name, words) + '</span><span class="dim">' + esc(r.file) + ":" + r.line + "</span></div>").join("")
          : (words.length ? '<div class="empty">沒有符合的符號</div>' : "");
      });
      q.focus();`,
    );
    view.webview.onDidReceiveMessage(async (m: { query?: string; open?: Entry; beside?: boolean }) => {
      if (m.open) {
        await open(m.open.uri, m.open.range.start.line, m.open.range.start.character, !!m.beside);
      } else if (m.query !== undefined) {
        const query = m.query;
        const found = query.trim() ? (await ask<Entry[]>("si/search", { query, limit: 100 })) ?? [] : [];
        void view.webview.postMessage({ query, results: found.map((e) => ({ ...e, kindName: KIND_NAMES[e.kind] ?? e.kind })) });
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
    view.webview.options = { enableScripts: true };
    view.webview.html = page(
      `<div id="root"><div class="empty">把游標停在一個名稱上，這裡會顯示它的定義。</div></div>`,
      `
      const root = document.getElementById("root");
      let shown;
      const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
      root.addEventListener("click", (e) => { if (shown) vscode.postMessage({ open: shown, beside: e.ctrlKey || e.metaKey }); });
      window.addEventListener("message", (e) => {
        const d = e.data;
        shown = d.entry;
        if (!d.entry) { root.innerHTML = '<div class="empty">' + esc(d.message) + "</div>"; return; }
        const more = d.entry.candidates > 1 ? '<span class="dim">共 ' + d.entry.candidates + " 個候選</span>" : "";
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
      void this.view.webview.postMessage({ entry: null, message: "這裡沒有索引認得的名稱。" });
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
    const parts = [`${callers?.entries.length ?? 0} 個呼叫者`, `${callees?.entries.length ?? 0} 個被呼叫者`];
    if (indirect) {
      parts.push(`其中 ${indirect} 個經函式指標`);
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
  const aliases = vscode.workspace.getConfiguration("si").get<string[]>("includeAliases", []);
  return { command: serverPath(), args: ["lsp", ...aliases.flatMap((a) => ["--include-alias", a])] };
}

export async function activate(context: vscode.ExtensionContext) {
  const relation = new Relation(vscode.Uri.joinPath(context.extensionUri, "media"));
  const fileSymbols = new FileSymbols();
  const contextView = new Context();
  const lenses = new Lenses();
  const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 10);
  status.text = "$(sync~spin) SI 建立索引中";
  status.show();

  const selector: vscode.DocumentSelector = [{ scheme: "file", pattern: "**/*.{c,h}" }];

  async function refreshStatus() {
    const s = await ask<{ files: number; symbols: number }>("si/status", {});
    status.text = s ? `$(symbol-namespace) SI ${s.files.toLocaleString()} 檔 · ${s.symbols.toLocaleString()} 符號` : "$(warning) SI 未啟動";
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

  async function start() {
    const { command, args } = serverCommand();
    // A path that is not there is the usual reason for not starting: say so
    // plainly instead of leaving a spawn error in the log.
    if (/[\/]/.test(command) && !fs.existsSync(command)) {
      status.text = "$(warning) SI 未啟動";
      const pick = await vscode.window.showErrorMessage(`找不到 si 執行檔：${command}`, "開啟設定");
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
    try {
      await client.start();
    } catch (e) {
      status.text = "$(warning) SI 未啟動";
      void vscode.window.showErrorMessage(`無法啟動 si（${command}）。請在設定 si.serverPath 指定 si 執行檔的路徑。${e instanceof Error ? e.message : ""}`);
      return;
    }
    await refreshStatus();
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
    vscode.window.registerWebviewViewProvider("si.search", new Search(), { webviewOptions: { retainContextWhenHidden: true } }),
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
      if (isSource(doc)) {
        // The server re-indexes on the save notification; ask after it has.
        await refreshStatus();
        fileSymbols.refresh();
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
      status.text = "$(sync~spin) SI 建立索引中";
      if (client?.isRunning()) {
        await client.stop();
      }
      await start();
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      // A corrected path should take effect without reloading the window.
      if (e.affectsConfiguration("si.serverPath") || e.affectsConfiguration("si.includeAliases")) {
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
