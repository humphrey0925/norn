//! `si lsp`: the index as a Language Server, over stdin/stdout.
//!
//! Go to definition, find references, document and workspace symbols, and
//! call hierarchy (through macros and function pointers), kept up to date as
//! files are saved. An editor extension is then only what starts this.
//!
//! The index is by line: a position is turned into the identifier under it,
//! and a result into the place that identifier is written on its line.

use serde_json::{json, Value};
use si_core::graph::Graph;
use si_core::model::{flag, kind, refkind};
use si_core::session::Session;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Results sent for one request at most.
const LIMIT: usize = 2000;

fn read_message(input: &mut impl BufRead) -> Option<Value> {
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message(out: &mut impl Write, msg: &Value) {
    let body = msg.to_string();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

/// `file:///e%3A/dir/a.c` to a path, and back.
fn uri_to_path(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (raw[i], raw.get(i + 1).copied().and_then(hex), raw.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                bytes.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    let mut path = String::from_utf8_lossy(&bytes).into_owned();
    // `/e:/dir` is `e:/dir` on Windows.
    if path.as_bytes().get(2) == Some(&b':') && path.starts_with('/') {
        path.remove(0);
    }
    PathBuf::from(path)
}

fn path_to_uri(path: &Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !p.starts_with('/') {
        out.push('/');
    }
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn symbol_kind(k: u8) -> u32 {
    match k {
        kind::FILE => 1,
        kind::FUNCTION => 12,
        kind::MACRO => 14,
        kind::STRUCT | kind::UNION => 23,
        kind::ENUM => 10,
        kind::ENUM_CONST => 22,
        kind::TYPEDEF => 5,
        kind::GLOBAL => 13,
        kind::FIELD => 8,
        _ => 13,
    }
}

struct Server {
    session: Session,
    /// At start: files read back from the last run's save, files parsed.
    opened: (usize, usize),
    root: PathBuf,
    /// Text of the documents the editor has open, by path.
    open: HashMap<PathBuf, String>,
    /// Lines of files read to place results, until the next update.
    lines: HashMap<String, Vec<String>>,
}

impl Server {
    fn g(&self) -> &Graph {
        &self.session.graph
    }

    /// The path as the index names it, if it is under the root.
    fn relative(&self, path: &Path) -> Option<String> {
        let (p, root) = (path.to_string_lossy().replace('\\', "/"), self.root.to_string_lossy().replace('\\', "/"));
        let root = root.trim_end_matches('/');
        let under = p.len() > root.len() && p[..root.len()].eq_ignore_ascii_case(root) && p.as_bytes()[root.len()] == b'/';
        under.then(|| p[root.len() + 1..].to_string())
    }

    fn line_text(&mut self, file: &str, line: u32) -> String {
        let path = self.root.join(file);
        if let Some(text) = self.open.get(&path) {
            return text.lines().nth(line as usize - 1).unwrap_or("").to_string();
        }
        let lines = self.lines.entry(file.to_string()).or_insert_with(|| {
            String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()).lines().map(str::to_string).collect()
        });
        lines.get(line as usize - 1).cloned().unwrap_or_default()
    }

    /// (file, 1-based line, identifier) at a `textDocument` position.
    fn at(&mut self, params: &Value) -> Option<(String, u32, String)> {
        let path = uri_to_path(params["textDocument"]["uri"].as_str()?);
        let file = self.relative(&path)?;
        let line = params["position"]["line"].as_u64()? as u32 + 1;
        let character = params["position"]["character"].as_u64()? as usize;
        let text = self.line_text(&file, line);
        let b = text.as_bytes();
        let at = character.min(b.len());
        let start = (0..at).rev().take_while(|&i| is_ident(b[i])).last().unwrap_or(at);
        let end = (at..b.len()).take_while(|&i| is_ident(b[i])).last().map_or(at, |i| i + 1);
        (end > start).then(|| (file, line, text[start..end].to_string()))
    }

    /// Where `name` is written on a line, as an LSP location.
    fn location(&mut self, file: &str, line: u32, name: &str) -> Value {
        let text = self.line_text(file, line);
        let b = text.as_bytes();
        let col = text
            .match_indices(name)
            .map(|(i, _)| i)
            .find(|&i| (i == 0 || !is_ident(b[i - 1])) && b.get(i + name.len()).is_none_or(|&c| !is_ident(c)));
        let (start, end) = col.map_or((0, 0), |c| (c, c + name.len()));
        let l = line.saturating_sub(1);
        json!({ "uri": path_to_uri(&self.root.join(file)),
                "range": { "start": { "line": l, "character": start }, "end": { "line": l, "character": end } } })
    }

    fn sym_location(&mut self, sym: u32) -> Value {
        let (file, line, name) = {
            let g = self.g();
            let s = &g.syms[sym as usize];
            (g.file_path(s.file).to_string(), s.line, g.sym_name(sym).to_string())
        };
        self.location(&file, line, &name)
    }

    /// What the name under the cursor refers to; definitions first.
    fn targets(&mut self, params: &Value) -> Vec<u32> {
        let Some((file, line, name)) = self.at(params) else { return Vec::new() };
        let g = self.g();
        let mut found = si_core::definition_at(g, &file, line, &name);
        found.sort_by_key(|&s| (g.syms[s as usize].flags & flag::DECL != 0, s));
        found
    }

    fn definition(&mut self, params: &Value) -> Value {
        let mut found = self.targets(params);
        // The definition, when there is one; else the declarations.
        if found.iter().any(|&s| self.g().syms[s as usize].flags & flag::DECL == 0) {
            found.retain(|&s| self.g().syms[s as usize].flags & flag::DECL == 0);
        }
        json!(found.into_iter().take(LIMIT).map(|s| self.sym_location(s)).collect::<Vec<_>>())
    }

    fn references(&mut self, params: &Value) -> Value {
        let targets = self.targets(params);
        let mut places: Vec<(String, u32, String)> = Vec::new();
        for &t in &targets {
            let g = self.g();
            let name = g.sym_name(t).to_string();
            if params["context"]["includeDeclaration"].as_bool().unwrap_or(true) {
                let s = &g.syms[t as usize];
                places.push((g.file_path(s.file).to_string(), s.line, name.clone()));
            }
            // A stored function's edge carries the store's line but not its
            // file, so those are left to the call hierarchy.
            for e in g.refs_to(t).iter().filter(|e| e.kind() != refkind::BIND) {
                places.push((g.file_path(g.syms[e.other as usize].file).to_string(), e.line(), name.clone()));
            }
        }
        places.sort();
        places.dedup();
        json!(places.into_iter().take(LIMIT).map(|(f, l, n)| self.location(&f, l, &n)).collect::<Vec<_>>())
    }

    fn symbol_info(&mut self, sym: u32) -> Value {
        let (name, k, container) = {
            let g = self.g();
            let s = &g.syms[sym as usize];
            (g.sym_name(sym).to_string(), s.kind, g.file_path(s.file).to_string())
        };
        json!({ "name": name, "kind": symbol_kind(k), "location": self.sym_location(sym), "containerName": container })
    }

    fn document_symbols(&mut self, params: &Value) -> Value {
        let path = uri_to_path(params["textDocument"]["uri"].as_str().unwrap_or(""));
        let Some(file) = self.relative(&path) else { return json!([]) };
        let g = self.g();
        let Some(f) = (0..g.files.len() as u32).find(|&f| g.file_path(f) == file) else { return json!([]) };
        let syms: Vec<u32> = g.file_syms(f).skip(1).collect();
        json!(syms.into_iter().map(|s| self.symbol_info(s)).collect::<Vec<_>>())
    }

    fn workspace_symbols(&mut self, params: &Value) -> Value {
        let query = params["query"].as_str().unwrap_or("");
        // Fragments in any order, initials, letters in order.
        let syms: Vec<u32> = si_core::search::search(self.g(), query, 200).into_iter().map(|h| h.1).collect();
        json!(syms.into_iter().map(|s| self.symbol_info(s)).collect::<Vec<_>>())
    }

    fn hierarchy_item(&mut self, sym: u32) -> Value {
        let loc = self.sym_location(sym);
        let g = self.g();
        let s = &g.syms[sym as usize];
        json!({ "name": g.sym_name(sym), "kind": symbol_kind(s.kind), "detail": g.file_path(s.file),
                "uri": loc["uri"], "range": loc["range"], "selectionRange": loc["range"],
                "data": { "file": g.file_path(s.file), "line": s.line, "name": g.sym_name(sym) } })
    }

    /// The symbol a call hierarchy item stands for, found again after updates.
    fn item_symbol(&self, item: &Value) -> Option<u32> {
        let g = self.g();
        let (file, line, name) = (item["data"]["file"].as_str()?, item["data"]["line"].as_u64()?, item["data"]["name"].as_str()?);
        let all = g.find(name);
        let here = |&&s: &&u32| g.file_path(g.syms[s as usize].file) == file;
        all.iter().find(|s| here(s) && g.syms[**s as usize].line as u64 == line).or(all.iter().find(here)).copied()
    }

    fn prepare_hierarchy(&mut self, params: &Value) -> Value {
        let callable: Vec<u32> = self
            .targets(params)
            .into_iter()
            .filter(|&s| matches!(self.g().syms[s as usize].kind, kind::FUNCTION | kind::MACRO))
            .collect();
        let defs: Vec<u32> = callable.iter().copied().filter(|&s| self.g().syms[s as usize].flags & flag::DECL == 0).collect();
        let pick = if defs.is_empty() { callable } else { defs };
        json!(pick.into_iter().take(16).map(|s| self.hierarchy_item(s)).collect::<Vec<_>>())
    }

    /// Callers (`incoming`) or callees of an item, each with the lines the
    /// calls are on in the calling function's file.
    fn calls(&mut self, params: &Value, incoming: bool) -> Value {
        let Some(sym) = self.item_symbol(&params["item"]) else { return json!([]) };
        let g = self.g();
        let edges = if incoming { si_core::callers_through_macros(g, sym) } else { si_core::callees_through_slots(g, sym) };
        // other end -> lines of the calls
        let mut by_other: Vec<(u32, Vec<u32>)> = Vec::new();
        for c in edges {
            if !matches!(g.syms[c.caller as usize].kind, kind::FUNCTION | kind::MACRO) {
                continue;
            }
            match by_other.iter_mut().find(|x| x.0 == c.caller) {
                Some(x) => x.1.push(c.line),
                None => by_other.push((c.caller, vec![c.line])),
            }
        }
        by_other.truncate(LIMIT);
        let mut out = Vec::new();
        for (other, mut lines) in by_other {
            lines.sort_unstable();
            lines.dedup();
            let ranges: Vec<Value> = lines
                .iter()
                .map(|&l| json!({ "start": { "line": l.saturating_sub(1), "character": 0 }, "end": { "line": l.saturating_sub(1), "character": 0 } }))
                .collect();
            let item = self.hierarchy_item(other);
            out.push(if incoming { json!({ "from": item, "fromRanges": ranges }) } else { json!({ "to": item, "fromRanges": ranges }) });
        }
        json!(out)
    }

    /// `si/search`: fuzzy symbol search with scores, for the search view
    /// (an editor filters `workspace/symbol` results again by its own rules).
    fn search(&mut self, params: &Value) -> Value {
        let limit = params["limit"].as_u64().unwrap_or(100) as usize;
        let hits = si_core::search::search(self.g(), params["query"].as_str().unwrap_or(""), limit);
        json!(hits.into_iter().map(|(score, s)| { let mut v = self.entry(s); v["score"] = json!(score); v }).collect::<Vec<_>>())
    }

    /// A symbol for the extension's own views.
    fn entry(&mut self, sym: u32) -> Value {
        let loc = self.sym_location(sym);
        let g = self.g();
        let s = &g.syms[sym as usize];
        json!({ "name": g.sym_name(sym), "kind": kind::name(s.kind), "file": g.file_path(s.file), "line": s.line,
                "endLine": s.end_line, "declaration": s.flags & flag::DECL != 0, "uri": loc["uri"], "range": loc["range"],
                "data": { "file": g.file_path(s.file), "line": s.line, "name": g.sym_name(sym) } })
    }

    /// `si/context`: what the name under the cursor is defined as.
    fn context(&mut self, params: &Value) -> Value {
        let mut found = self.targets(params);
        if found.iter().any(|&s| self.g().syms[s as usize].flags & flag::DECL == 0) {
            found.retain(|&s| self.g().syms[s as usize].flags & flag::DECL == 0);
        }
        let all = found.len();
        match found.first() {
            Some(&s) => {
                let mut v = self.entry(s);
                v["candidates"] = json!(all);
                v
            }
            None => Value::Null,
        }
    }

    /// `si/relation`: the callers or callees of the function under the
    /// cursor (or the function the cursor is in, or `item`), each saying how
    /// the call comes about: by name, through a macro, through a member or
    /// variable holding the function, through a pointer; certain or not.
    fn relation(&mut self, params: &Value) -> Value {
        let incoming = params["direction"].as_str() != Some("callees");
        let root = if params["item"].is_object() {
            self.item_symbol(&json!({ "data": params["item"] }))
        } else {
            let callable = |s: &Server, x: u32| matches!(s.g().syms[x as usize].kind, kind::FUNCTION | kind::MACRO);
            let mut under: Vec<u32> = self.targets(params).into_iter().filter(|&x| callable(self, x)).collect();
            under.sort_by_key(|&x| self.g().syms[x as usize].flags & flag::DECL != 0);
            under.first().copied().or_else(|| {
                // Not on a function's name: the function the cursor is in.
                let file = self.relative(&uri_to_path(params["textDocument"]["uri"].as_str()?))?;
                let line = params["position"]["line"].as_u64()? as u32 + 1;
                let g = self.g();
                let f = (0..g.files.len() as u32).find(|&f| g.file_path(f) == file)?;
                g.symbol_at(f, line).filter(|&x| g.syms[x as usize].kind == kind::FUNCTION)
            })
        };
        let Some(root) = root else { return json!({ "root": null, "entries": [] }) };
        let g = self.g();
        let edges = if incoming { si_core::callers_through_macros(g, root) } else { si_core::callees_through_slots(g, root) };
        // (other end, macro, member or variable, pointer) -> (lines, certain)
        let mut groups: Vec<((u32, Option<u32>, Option<u32>, bool), Vec<u32>, bool)> = Vec::new();
        for c in edges {
            if !matches!(g.syms[c.caller as usize].kind, kind::FUNCTION | kind::MACRO | kind::GLOBAL) {
                continue;
            }
            let key = (c.caller, c.via.filter(|v| Some(*v) != c.slot), c.slot, c.pointer);
            match groups.iter_mut().find(|x| x.0 == key) {
                Some(x) => {
                    x.1.push(c.line);
                    x.2 &= !c.ambiguous;
                }
                None => groups.push((key, vec![c.line], !c.ambiguous)),
            }
        }
        groups.truncate(LIMIT);
        let mut entries = Vec::new();
        for ((other, via, slot, pointer), mut lines, certain) in groups {
            lines.sort_unstable();
            lines.dedup();
            let names = { let g = self.g(); (via.map(|m| g.sym_name(m).to_string()), slot.map(|m| g.sym_name(m).to_string())) };
            let mut v = self.entry(other);
            v["lines"] = json!(lines);
            v["viaMacro"] = json!(names.0);
            v["through"] = json!(names.1);
            v["pointer"] = json!(pointer);
            v["certain"] = json!(certain);
            entries.push(v);
        }
        json!({ "root": self.entry(root), "entries": entries })
    }

    fn update(&mut self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            self.session.update(&paths);
            self.lines.clear();
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Option<Value> {
        Some(match method {
            "textDocument/definition" | "textDocument/declaration" => self.definition(params),
            "textDocument/references" => self.references(params),
            "textDocument/documentSymbol" => self.document_symbols(params),
            "workspace/symbol" => self.workspace_symbols(params),
            "textDocument/prepareCallHierarchy" => self.prepare_hierarchy(params),
            "callHierarchy/incomingCalls" => self.calls(params, true),
            "callHierarchy/outgoingCalls" => self.calls(params, false),
            "si/search" => self.search(params),
            "si/context" => self.context(params),
            "si/relation" => self.relation(params),
            "si/status" => json!({ "files": self.session.files(), "symbols": self.g().syms.len(), "edges": self.g().out.len(),
                                   "loaded": self.opened.0, "parsed": self.opened.1,
                                   "cache": self.session.cache_dir().map(|d| d.to_string_lossy().into_owned()) }),
            "shutdown" => Value::Null,
            _ => return None,
        })
    }

    fn notification(&mut self, method: &str, params: &Value) {
        let doc = || params["textDocument"]["uri"].as_str().map(uri_to_path);
        match method {
            "textDocument/didOpen" => {
                if let (Some(path), Some(text)) = (doc(), params["textDocument"]["text"].as_str()) {
                    self.open.insert(path, text.to_string());
                }
            }
            "textDocument/didChange" => {
                // Full sync: the last change is the whole text. The index
                // itself follows what is saved.
                if let (Some(path), Some(text)) = (doc(), params["contentChanges"].as_array().and_then(|c| c.last()).and_then(|c| c["text"].as_str())) {
                    self.open.insert(path, text.to_string());
                }
            }
            "textDocument/didClose" => {
                if let Some(path) = doc() {
                    self.open.remove(&path);
                }
            }
            "textDocument/didSave" => self.update(doc().into_iter().collect()),
            "workspace/didChangeWatchedFiles" => {
                let paths = params["changes"].as_array().into_iter().flatten().filter_map(|c| c["uri"].as_str()).map(uri_to_path).collect();
                self.update(paths);
            }
            _ => {}
        }
    }
}

pub fn run(root: Option<&Path>, open: &dyn Fn(&Path) -> (Session, si_core::session::Update)) {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut server: Option<Server> = None;
    while let Some(msg) = read_message(&mut input) {
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let (id, params) = (msg.get("id").cloned(), &msg["params"]);
        if method == "initialize" {
            let from_client = params["rootUri"].as_str().map(uri_to_path).or(params["rootPath"].as_str().map(PathBuf::from));
            let dir = root.map(Path::to_path_buf).or(from_client).unwrap_or_else(|| PathBuf::from("."));
            let (session, opened) = open(&dir);
            let opened = (opened.loaded, opened.extracted);
            server = Some(Server { session, opened, root: dir, open: HashMap::new(), lines: HashMap::new() });
            let capabilities = json!({
                "textDocumentSync": { "openClose": true, "change": 1, "save": true },
                "definitionProvider": true, "declarationProvider": true, "referencesProvider": true,
                "documentSymbolProvider": true, "workspaceSymbolProvider": true, "callHierarchyProvider": true,
            });
            let result = json!({ "capabilities": capabilities, "serverInfo": { "name": "si", "version": env!("CARGO_PKG_VERSION") } });
            write_message(&mut out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            continue;
        }
        if method == "exit" {
            break;
        }
        match (&mut server, id) {
            (Some(s), Some(id)) => {
                let reply = match s.request(&method, params) {
                    Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    None => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{method} is not supported") } }),
                };
                write_message(&mut out, &reply);
            }
            (Some(s), None) => s.notification(&method, params),
            (None, Some(id)) => {
                let error = json!({ "code": -32002, "message": "not initialized" });
                write_message(&mut out, &json!({ "jsonrpc": "2.0", "id": id, "error": error }));
            }
            (None, None) => {}
        }
    }
}
