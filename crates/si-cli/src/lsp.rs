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

mod more;

/// Results sent for one request at most.
const LIMIT: usize = 2000;

/// With `NORN_TRACE` set to a file, what the server was asked and when
/// (milliseconds since it started), and what each change of the index
/// cost: to see what an editor does with it at start.
fn trace(line: impl FnOnce() -> String) {
    use std::sync::{Mutex, OnceLock};
    static FILE: OnceLock<Option<(std::time::Instant, Mutex<std::fs::File>)>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os("NORN_TRACE")?;
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some((std::time::Instant::now(), Mutex::new(file)))
    });
    if let Some((start, file)) = file {
        let _ = writeln!(file.lock().unwrap(), "{:>8.0} {}", start.elapsed().as_secs_f64() * 1000.0, line());
    }
}

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
    // In one write: the progress thread writes to the same stream.
    let body = msg.to_string();
    let _ = out.write_all(format!("Content-Length: {}\r\n\r\n{}", body.len(), body).as_bytes());
    let _ = out.flush();
}

/// A parameter or local variable found under the cursor.
/// A line with its comments taken out (`//` to the end, `/* */` on it).
fn uncommented(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    loop {
        let (a, b) = (rest.find("//"), rest.find("/*"));
        match (a, b) {
            (Some(i), b) if b.is_none_or(|b| i < b) => {
                out.push_str(&rest[..i]);
                return out;
            }
            (_, Some(i)) => {
                out.push_str(&rest[..i]);
                match rest[i + 2..].find("*/") {
                    Some(e) => rest = &rest[i + 2 + e + 2..],
                    None => return out,
                }
            }
            _ => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

/// The words of a comment, its markers taken out.
fn comment_words(lines: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    for l in lines {
        let mut t = l.trim();
        for p in ["/**<", "/*!<", "///<", "//!<", "/**", "/*!", "/*", "///", "//!", "//"] {
            if let Some(r) = t.strip_prefix(p) {
                t = r;
                break;
            }
        }
        let t = t.trim_end().trim_end_matches("*/").trim();
        let t = t.strip_prefix('*').map_or(t, str::trim);
        if !t.is_empty() || out.last().is_some_and(|l| !l.is_empty()) {
            out.push(t.to_string());
        }
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out.join("  \n")
}

/// The type a declaration gives `name`: the last word before it that is
/// no qualifier (`static const struct foo *p` gives `foo`).
fn type_word(decl: &str, name: &str) -> Option<String> {
    const NOT: &[&str] = &[
        "static", "extern", "const", "volatile", "register", "inline", "struct", "union", "enum", "unsigned", "signed", "restrict", "typedef", "__IO",
        "__I", "__O", "__IM", "__OM", "__IOM", "__restrict", "__inline", "__volatile__", "__const",
    ];
    // (What is between braces is the members of a struct written there.)
    let mut flat = String::new();
    let mut depth = 0;
    for c in decl.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ if depth == 0 => flat.push(c),
            _ => {}
        }
    }
    let decl = flat.as_str();
    let b = decl.as_bytes();
    let at = decl.match_indices(name).map(|m| m.0).find(|&i| (i == 0 || !is_ident(b[i - 1])) && b.get(i + name.len()).is_none_or(|&c| !is_ident(c)))?;
    let words: Vec<&str> = decl[..at].split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty() && !w.as_bytes()[0].is_ascii_digit()).collect();
    words.iter().rev().find(|w| !NOT.contains(w)).map(|w| w.to_string())
}

const C_KEYWORDS: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto", "if", "inline",
    "int", "long", "register", "restrict", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union", "unsigned", "void",
    "volatile", "while", "_Bool", "_Static_assert", "_Alignof", "_Alignas", "_Noreturn", "_Atomic", "_Thread_local", "_Generic", "bool", "true", "false",
    "NULL",
];

struct LocalHit {
    file: String,
    name: String,
    /// The line of its declaration.
    line: u32,
    param: bool,
    type_name: Option<String>,
    function: String,
    function_line: u32,
    /// The lines it is used on, in order.
    uses: Vec<u32>,
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
        kind::CLASS => 5,
        kind::NAMESPACE => 3,
        _ => 13,
    }
}

/// How a symbol is shown: a C++ member with its classes (`DBImpl::Get`).
/// (Looked up again by its own name, which `data` carries.)
fn display_name(g: &Graph, sym: u32) -> String {
    if g.owner_of(sym).is_none() {
        return g.sym_name(sym).to_string();
    }
    si_core::graph::Qualifier::new(g).name(g, sym).into_owned()
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
    /// Every symbol worth listing, by name without regard to case; made
    /// when first asked for and dropped when the index changes.
    listing: [Option<Vec<u32>>; 2],
    /// Per file of the index, whether no compilation reads it under these
    /// conditions (a header only `#if`s they switch off include): what is
    /// in it is not listed, searched or related; made when first asked for.
    hidden: Option<std::rc::Rc<Vec<bool>>>,
    /// The editor has the C/C++ extension too (`initializationOptions`):
    /// a hover says only what its does not.
    alongside: bool,
    /// The index is still being read or made: the session answers with
    /// what was saved (or nothing), and what would change it waits.
    loading: bool,
    waiting: Vec<Value>,
}

/// What changes the index, and so has to wait for there to be one.
fn changes(method: &str) -> bool {
    matches!(method, "si/setConditions" | "si/setScope" | "si/sync" | "textDocument/didSave" | "workspace/didChangeWatchedFiles")
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

    /// Where `name` is written on a line, as an LSP location: on the
    /// line, or a line or two below it (GNU style: `static int` on a line
    /// of its own, the name on the next); else the line itself.
    fn location(&mut self, file: &str, line: u32, name: &str) -> Value {
        let mut found = None;
        for l in line..line + 3 {
            let text = self.line_text(file, l);
            let b = text.as_bytes();
            let col = text
                .match_indices(name)
                .map(|(i, _)| i)
                .find(|&i| (i == 0 || !is_ident(b[i - 1])) && b.get(i + name.len()).is_none_or(|&c| !is_ident(c)));
            if let Some(c) = col {
                found = Some((l, c));
                break;
            }
        }
        let (line, (start, end)) = found.map_or((line, (0, 0)), |(l, c)| (l, (c, c + name.len())));
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

    /// Per file of the index, whether no compilation reads it.
    fn hidden(&mut self) -> std::rc::Rc<Vec<bool>> {
        if let Some(h) = &self.hidden {
            return h.clone();
        }
        let readings = si_core::readings_of(self.session.conditions());
        let unread: std::collections::HashSet<&str> = readings.unread.iter().map(String::as_str).collect();
        let g = &self.session.graph;
        let h = std::rc::Rc::new((0..g.files.len() as u32).map(|f| unread.contains(g.file_path(f))).collect::<Vec<bool>>());
        self.hidden = Some(h.clone());
        h
    }

    /// Whether symbol `sym` is in a file some compilation reads.
    fn shown(&mut self, sym: u32) -> bool {
        let file = self.session.graph.syms[sym as usize].file as usize;
        !self.hidden().get(file).copied().unwrap_or(false)
    }

    /// What the name under the cursor refers to; definitions first, and
    /// those in a file no compilation reads only if there is nothing else.
    fn targets(&mut self, params: &Value) -> Vec<u32> {
        let Some((file, line, name)) = self.at(params) else { return Vec::new() };
        let hidden = self.hidden();
        let g = self.g();
        let mut found = si_core::definition_at(g, &file, line, &name);
        found.sort_by_key(|&s| (g.syms[s as usize].flags & flag::DECL != 0, s));
        if found.iter().any(|&s| !hidden[g.syms[s as usize].file as usize]) {
            found.retain(|&s| !hidden[g.syms[s as usize].file as usize]);
        }
        found
    }

    /// The parameter or local variable the name under the cursor is, when
    /// it is one: locals are no part of the graph, they are looked up in
    /// the facts of the file, in the function the cursor is in.
    fn local_at(&mut self, params: &Value) -> Option<LocalHit> {
        let (file, line, name) = self.at(params)?;
        let f = self.session.facts_of(&file)?;
        let mut best: Option<(usize, &si_core::model::Local)> = None;
        for (i, l) in f.locals.iter().enumerate().filter(|(_, l)| f.name(l.name as usize) == name) {
            let Some(d) = f.defs.get(l.scope as usize) else { continue };
            if line < d.line || line > d.end_line.max(d.line) {
                continue;
            }
            // The declaration last made at or above the cursor (an inner
            // block may declare the name again); else the function's first.
            let better = match best {
                None => true,
                Some((_, b)) => l.line <= line && (b.line > line || l.line >= b.line),
            };
            if better {
                best = Some((i, l));
            }
        }
        let (i, l) = best?;
        let d = &f.defs[l.scope as usize];
        let mut uses: Vec<u32> = f.local_uses.iter().filter(|u| u.0 == i as u32).map(|u| u.1).collect();
        uses.dedup();
        Some(LocalHit {
            file,
            name,
            line: l.line,
            param: l.param,
            type_name: (l.type_name != si_core::model::NONE).then(|| f.name(l.type_name as usize).to_string()),
            function: f.name(d.name as usize).to_string(),
            function_line: d.line,
            uses,
        })
    }

    /// A local as the views take a symbol.
    fn local_entry(&mut self, h: &LocalHit) -> Value {
        let loc = self.location(&h.file, h.line, &h.name);
        json!({ "name": h.name, "kind": if h.param { "parameter" } else { "local" }, "file": h.file, "line": h.line, "endLine": h.line,
                "declaration": false, "uri": loc["uri"], "range": loc["range"], "function": h.function, "type": h.type_name,
                "data": { "file": h.file, "line": h.line, "name": h.name } })
    }

    /// `si/locals`: the parameters and local variables of each function of
    /// a file, for listing under the functions.
    fn locals(&mut self, params: &Value) -> Value {
        let path = uri_to_path(params["textDocument"]["uri"].as_str().unwrap_or(""));
        let Some(file) = self.relative(&path) else { return json!([]) };
        let Some(f) = self.session.facts_of(&file) else { return json!([]) };
        let rows: Vec<(String, u32, String, u32, bool, Option<String>)> = f
            .locals
            .iter()
            .filter_map(|l| {
                let d = f.defs.get(l.scope as usize)?;
                let ty = (l.type_name != si_core::model::NONE).then(|| f.name(l.type_name as usize).to_string());
                Some((f.name(d.name as usize).to_string(), d.line, f.name(l.name as usize).to_string(), l.line, l.param, ty))
            })
            .collect();
        let mut out = Vec::new();
        for (function, function_line, name, line, param, ty) in rows {
            let loc = self.location(&file, line, &name);
            out.push(json!({ "function": function, "functionLine": function_line, "name": name, "line": line, "kind": if param { "parameter" } else { "local" },
                             "type": ty, "uri": loc["uri"], "range": loc["range"] }));
        }
        json!(out)
    }

    /// `textDocument/hover` on a macro or an enumerator: what it is
    /// defined as, and the number it comes to (an `#if`'s arithmetic over
    /// the definitions the index has).
    fn hover(&mut self, params: &Value) -> Value {
        // A macro with arguments, written with them here: `BUF_Q(3)`.
        let asked = self.at(params);
        let here_file = asked.as_ref().map(|a| a.0.clone());
        let used = self.at(params).and_then(|(file, line, name)| {
            let text = self.line_text(&file, line);
            let b = text.as_bytes();
            let mut from = 0;
            while let Some(i) = text[from..].find(&name).map(|i| from + i) {
                from = i + name.len();
                if i > 0 && is_ident(b[i - 1]) || b.get(from).is_some_and(|&c| is_ident(c)) {
                    continue;
                }
                let open = from + text[from..].len() - text[from..].trim_start().len();
                if b.get(open) != Some(&b'(') {
                    continue;
                }
                let mut depth = 0;
                for (j, &c) in b.iter().enumerate().skip(open) {
                    depth += (c == b'(') as i32 - (c == b')') as i32;
                    if depth == 0 {
                        return Some(format!("{name}{}", &text[open..=j]));
                    }
                }
            }
            None
        });
        if !self.alongside {
            if let Some(h) = self.local_at(params) {
                return self.local_hover(&h);
            }
        }
        let mut found = self.targets(params);
        let g = &self.session.graph;
        if !self.alongside && !found.is_empty() && !found.iter().any(|&s| matches!(g.syms[s as usize].kind, kind::MACRO | kind::ENUM_CONST)) {
            return self.declaration_hover(found);
        }
        let g = &self.session.graph;
        found.retain(|&s| matches!(g.syms[s as usize].kind, kind::MACRO | kind::ENUM_CONST) && g.syms[s as usize].flags & flag::DECL == 0);
        if found.is_empty() {
            return Value::Null;
        }
        // On a definition: that one.
        if let Some((file, line, _)) = self.at(params) {
            let g = &self.session.graph;
            let here: Vec<u32> = found.iter().copied().filter(|&s| g.syms[s as usize].line == line && g.file_path(g.syms[s as usize].file) == file).collect();
            if !here.is_empty() {
                found = here;
            }
        }
        let g = &self.session.graph;
        // Certainly compiled first.
        found.sort_by_key(|&s| (g.syms[s as usize].flags & flag::CONDITIONAL != 0, s));
        found.truncate(6);
        let (root, open) = (self.root.clone(), &self.open);
        let mut read_files: HashMap<String, std::sync::Arc<Vec<u8>>> = HashMap::new();
        let mut read = |p: &str| -> Option<std::sync::Arc<Vec<u8>>> {
            if let Some(b) = read_files.get(p) {
                return Some(b.clone());
            }
            let path = root.join(p);
            let bytes = std::sync::Arc::new(match open.get(&path) {
                Some(text) => text.as_bytes().to_vec(),
                None => std::fs::read(&path).ok()?,
            });
            read_files.insert(p.to_string(), bytes.clone());
            Some(bytes)
        };
        let conditions = self.session.conditions();
        let readings = si_core::readings_of(conditions);
        let mut values = si_core::value::Values::new(g, conditions, &mut read).without(&readings.unread);
        if let Some((file, line, _)) = &asked {
            values = values.asked_at(file, *line).read_with(&readings, file);
        }
        let shown = |n: i64, unsigned: bool| {
            let dec = if unsigned { (n as u64).to_string() } else { n.to_string() };
            if (0..10).contains(&n) { dec } else { format!("{dec} /* 0x{:X} */", n as u64) }
        };
        // Beside the C/C++ extension, whose hover has the definition and
        // its expansion: only what it does not, the value.
        let alongside = self.alongside;
        let several = found.len() > 1;
        let mut parts = Vec::new();
        for s in found {
            let sym = &g.syms[s as usize];
            let at = format!("{}:{}", g.file_path(sym.file), sym.line);
            let name = g.sym_name(s).to_string();
            let maybe = if sym.flags & flag::CONDITIONAL != 0 { " · only where an #if not known holds" } else { "" };
            if sym.kind == kind::ENUM_CONST {
                if alongside {
                    continue;
                }
                let v = values.enumerator(s).map_or(String::new(), |n| format!(" = {}", shown(n, false)));
                parts.push(format!("```c\n{name}{v}\n```\n\n{at}{maybe}"));
                continue;
            }
            let Some(d) = values.define(s) else { continue };
            let head = match &d.params {
                Some(p) => format!("#define {}({}) {}", d.name, p.join(", "), d.body),
                None => format!("#define {} {}", d.name, d.body),
            };
            // The number it comes to; else what of it is numbers worked
            // out, the rest as written (a variable, a member); else why not.
            let squeeze = |t: &str| t.chars().filter(|c| !c.is_whitespace() && *c != '(' && *c != ')').collect::<String>();
            // (Names defined more than one way, left as written.)
            let several_ways = |values: &si_core::value::Values| -> String {
                let names: Vec<String> = values.missing.iter().filter(|m| m.1.contains("definitions")).take(3).map(|(n, w)| format!("`{n}`: {w}")).collect();
                if names.is_empty() { String::new() } else { format!("Not expanded: {}", names.join("; ")) }
            };
            values.missing.clear();
            // (What it is written as, what it comes to; a note.)
            let mut value: Option<(String, String)> = None;
            let mut note = String::new();
            if d.params.is_none() {
                if let Some(n) = values.of_macro(s) {
                    value = Some((name.clone(), shown(n.number, n.unsigned)));
                } else {
                    match values.fold_or_why(&d.body, sym.file) {
                        Ok(f) if squeeze(&f) != squeeze(&d.body) => {
                            value = Some((name.clone(), f));
                            note = several_ways(&values);
                        }
                        Ok(_) => {}
                        Err(why) => note = format!("No value: {why}"),
                    }
                    if value.is_none() && note.is_empty() && !values.missing.is_empty() {
                        let why: Vec<String> = values.missing.iter().take(3).map(|(n, w)| format!("`{n}`: {w}")).collect();
                        note = format!("No value: {}", why.join("; "));
                    }
                }
            }
            if let (Some(_), Some(here)) = (&d.params, &used) {
                let at = here_file.as_deref().and_then(|f| (0..g.files.len() as u32).find(|&i| g.file_path(i) == f)).unwrap_or(sym.file);
                values.missing.clear();
                match values.of(here, at) {
                    Some(n) => value = Some((here.clone(), shown(n.number, n.unsigned))),
                    None => match values.fold_or_why(here, at) {
                        Ok(f) => {
                            value = Some((here.clone(), f));
                            note = several_ways(&values);
                        }
                        Err(why) => note = format!("`{here}`: no value: {why}"),
                    },
                }
            }
            let code = value.map(|(what, v)| format!("```c\n{what} = {v}{}\n```", if several && alongside { format!(" // {at}") } else { String::new() }));
            let note = if note.is_empty() { String::new() } else { format!("\n\n{note}") };
            if alongside {
                if code.is_some() || !note.is_empty() {
                    parts.push(format!("{}{note}", code.unwrap_or_default()));
                }
            } else {
                let code = code.map_or(String::new(), |c| format!("\n{c}"));
                parts.push(format!("```c\n{head}\n```{code}{note}\n\n{at}{maybe}"));
            }
        }
        if parts.is_empty() {
            return Value::Null;
        }
        let sep = if several && !alongside { "\n\n---\n\n" } else { "\n" };
        json!({ "contents": { "kind": "markdown", "value": parts.join(sep) } })
    }

    /// The declaration of a symbol as written (a function's up to its
    /// body, a struct's whole as far as 20 lines), and its comment: the one
    /// just above it, else the one after it on its line.
    fn written(&mut self, sym: u32) -> (String, String) {
        let (file, line, end, k, name) = {
            let g = self.g();
            let s = &g.syms[sym as usize];
            (g.file_path(s.file).to_string(), s.line, s.end_line.max(s.line), s.kind, g.sym_name(sym).to_string())
        };
        let whole = matches!(k, kind::STRUCT | kind::UNION | kind::ENUM | kind::TYPEDEF);
        let most = if whole { 20 } else { 8 };
        let mut code: Vec<String> = Vec::new();
        let (mut depth, mut braces) = (0i32, 0i32);
        let mut cut = false;
        for l in line..=end.min(line + most - 1) {
            let raw = self.line_text(&file, l);
            let text = if whole { raw.trim_end().to_string() } else { uncommented(&raw).trim_end().to_string() };
            let mut kept = String::new();
            for c in text.chars() {
                // (A function up to its body; a variable or member to its
                // `;`, braces and all.)
                if !whole && k != kind::MACRO && depth == 0 && braces == 0 && (c == ';' || c == '{' && k == kind::FUNCTION) {
                    cut = c == '{';
                    break;
                }
                depth += (c == '(') as i32 - (c == ')') as i32;
                braces += (c == '{') as i32 - (c == '}') as i32;
                kept.push(c);
            }
            let ended = kept.len() < text.len();
            if !kept.trim().is_empty() {
                code.push(kept.trim_end().to_string());
            }
            if ended {
                break;
            }
        }
        if whole && end >= line + most {
            code.push("    …".into());
        }
        // (Indented as written, less what all of it is indented by.)
        let indent = code.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
        let mut text = code.iter().map(|l| l.get(indent..).unwrap_or(l.trim_start()).to_string()).collect::<Vec<_>>().join("\n");
        if text.trim().is_empty() {
            text = name.clone();
        }
        if cut {
            text.push_str(" { … }");
        } else if !whole && k != kind::MACRO && k != kind::ENUM_CONST && !text.ends_with(';') {
            text.push(';');
        }
        // The comment above.
        let mut doc: Vec<String> = Vec::new();
        let mut l = line;
        let above = if l > 1 { self.line_text(&file, l - 1) } else { String::new() };
        if above.trim_end().ends_with("*/") {
            while l > 1 {
                l -= 1;
                let t = self.line_text(&file, l);
                doc.push(t.clone());
                if let Some(i) = t.find("/*") {
                    // (The comment after another declaration is its.)
                    if !t[..i].trim().is_empty() {
                        doc.clear();
                    }
                    break;
                }
                if doc.len() > 40 {
                    break;
                }
            }
            doc.reverse();
        } else {
            while l > 1 && doc.len() < 40 {
                let t = self.line_text(&file, l - 1);
                if !t.trim_start().starts_with("//") {
                    break;
                }
                doc.push(t);
                l -= 1;
            }
            doc.reverse();
        }
        // Else after it on its line.
        if doc.is_empty() {
            let t = self.line_text(&file, line);
            if let Some(i) = t.find("//").or_else(|| t.find("/*")).filter(|&i| !t[..i].trim().is_empty()) {
                doc.push(t[i..].to_string());
            }
        }
        (text, comment_words(&doc))
    }

    /// The comment a prototype of a function has (a header's, where the
    /// definition has none of its own).
    fn prototype_doc(&mut self, sym: u32) -> String {
        let protos: Vec<u32> = {
            let g = self.g();
            let k = g.syms[sym as usize].kind;
            g.find(g.sym_name(sym)).iter().copied().filter(|&s| g.syms[s as usize].kind == k && g.syms[s as usize].flags & flag::DECL != 0).take(8).collect()
        };
        for s in protos {
            let (_, doc) = self.written(s);
            if !doc.is_empty() {
                return doc;
            }
        }
        String::new()
    }

    /// A hover for what is no macro: its declaration and its comment.
    fn declaration_hover(&mut self, found: Vec<u32>) -> Value {
        let defs: Vec<u32> = {
            let g = self.g();
            let d: Vec<u32> = found.iter().copied().filter(|&s| g.syms[s as usize].flags & flag::DECL == 0).collect();
            if d.is_empty() { found.clone() } else { d }
        };
        let spare = found.first().map_or(String::new(), |&s| self.prototype_doc(s));
        let mut parts: Vec<String> = Vec::new();
        let mut doc_seen = String::new();
        for &s in defs.iter().take(3) {
            let (code, mut doc) = self.written(s);
            if doc.is_empty() {
                doc = spare.clone();
            }
            let g = self.g();
            let sym = &g.syms[s as usize];
            let maybe = if sym.flags & flag::CONDITIONAL != 0 { " · only where an #if not known holds" } else { "" };
            let at = format!("{}:{}", g.file_path(sym.file), sym.line);
            let shown = if doc.is_empty() || doc == doc_seen { String::new() } else { format!("\n\n{doc}") };
            doc_seen = doc;
            parts.push(format!("```c\n{code}\n```{shown}\n\n{at}{maybe}"));
        }
        if parts.is_empty() {
            return Value::Null;
        }
        json!({ "contents": { "kind": "markdown", "value": parts.join("\n\n---\n\n") } })
    }

    /// A hover for a parameter or local variable: how it is declared.
    fn local_hover(&mut self, h: &LocalHit) -> Value {
        let text = uncommented(&self.line_text(&h.file, h.line));
        let b = text.as_bytes();
        let at = text.match_indices(h.name.as_str()).map(|m| m.0).find(|&i| (i == 0 || !is_ident(b[i - 1])) && b.get(i + h.name.len()).is_none_or(|&c| !is_ident(c)));
        let decl = match at {
            // A parameter: between the commas round it.
            Some(i) if h.param => {
                let from = text[..i].rfind(['(', ',']).map_or(0, |p| p + 1);
                let to = text[i..].find([',', ')']).map_or(text.len(), |p| i + p);
                text[from..to].trim().to_string()
            }
            _ => {
                let t = text.trim();
                t.split_once(';').map_or(t, |x| x.0).trim().to_string() + ";"
            }
        };
        let what = if h.param { "parameter" } else { "local" };
        json!({ "contents": { "kind": "markdown", "value": format!("```c\n{decl}\n```\n\n{what} of `{}` · {}:{}", h.function, h.file, h.line) } })
    }

    /// The members of a type by name: a struct's or union's fields; a
    /// typedef's, of what it names.
    fn members(&mut self, type_name: &str, depth: u32) -> Vec<u32> {
        if depth > 4 {
            return Vec::new();
        }
        let hidden = self.hidden();
        let named: Vec<u32> = {
            let g = self.g();
            g.find(type_name)
                .iter()
                .copied()
                .filter(|&s| {
                    let y = &g.syms[s as usize];
                    matches!(y.kind, kind::STRUCT | kind::UNION | kind::TYPEDEF | kind::CLASS) && y.flags & flag::DECL == 0 && !hidden[y.file as usize]
                })
                .collect()
        };
        let mut out = Vec::new();
        for s in named {
            let (file, line, end, k) = {
                let y = &self.g().syms[s as usize];
                (y.file, y.line, y.end_line.max(y.line), y.kind)
            };
            let fields: Vec<u32> = {
                let g = self.g();
                g.file_syms(file)
                    .filter(|&f| {
                        let y = &g.syms[f as usize];
                        y.kind == kind::FIELD && y.line >= line && y.line <= end
                    })
                    .collect()
            };
            let fields = self.direct(file, line, end, fields);
            if !fields.is_empty() {
                out.extend(fields);
            } else if k == kind::TYPEDEF {
                // `typedef struct tag name;`: the tag's.
                let (text, _) = self.written(s);
                let words: Vec<&str> = text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
                let name = self.g().sym_name(s).to_string();
                let tag = words.windows(2).find(|w| matches!(w[0], "struct" | "union")).map(|w| w[1].to_string()).or_else(|| type_word(&text, &name));
                if let Some(t) = tag.filter(|t| *t != name) {
                    out.extend(self.members(&t, depth + 1));
                }
            }
            if !out.is_empty() {
                break;
            }
        }
        out
    }

    /// Of the fields in lines `line..=end` (a struct written there), its
    /// own: not those of a struct written inside it with a name of its own
    /// (`struct pos { int x; } at;` has `at`, not `x`); those of one
    /// without one (`union { int a; };`), which are its own.
    fn direct(&mut self, file: u32, line: u32, end: u32, fields: Vec<u32>) -> Vec<u32> {
        let path = self.g().file_path(file).to_string();
        let lines: Vec<String> = (line..=end).map(|l| uncommented(&self.line_text(&path, l))).collect();
        // Per brace pair, whether it is one without a name: `};`.
        let all: Vec<char> = lines.join("\n").chars().collect();
        let mut opens = Vec::new();
        let mut open_of = vec![usize::MAX; all.len()];
        let mut plain = vec![false; all.len()];
        for (i, &c) in all.iter().enumerate() {
            if c == '{' {
                opens.push(i);
            } else if c == '}' {
                if let Some(o) = opens.pop() {
                    open_of[i] = o;
                    plain[o] = !opens.is_empty() && all[i + 1..].iter().find(|c| !c.is_whitespace()) == Some(&';');
                }
            }
        }
        // The depth, of braces that count, of each name on each line.
        let mut at: Vec<Vec<(String, i32)>> = vec![Vec::new(); lines.len()];
        let (mut row, mut depth, mut word) = (0usize, 0i32, String::new());
        let mut stack: Vec<bool> = Vec::new();
        for (i, &c) in all.iter().enumerate() {
            if c.is_ascii_alphanumeric() || c == '_' {
                word.push(c);
                continue;
            }
            if !word.is_empty() {
                at[row].push((std::mem::take(&mut word), depth));
            }
            match c {
                '\n' => row += 1,
                '{' => {
                    let counts = !plain[i];
                    depth += counts as i32;
                    stack.push(counts);
                }
                '}' => depth -= stack.pop().unwrap_or(false) as i32,
                _ => {}
            }
        }
        if !word.is_empty() {
            at[row].push((word, depth));
        }
        let g = self.g();
        fields
            .into_iter()
            .filter(|&f| {
                let y = &g.syms[f as usize];
                let name = g.sym_name(f);
                at.get((y.line - line) as usize).is_some_and(|ws| ws.iter().any(|(w, d)| w == name && *d == 1))
            })
            .collect()
    }

    /// The type of a name as the cursor's function sees it: a local's or
    /// parameter's, else a variable's of the index.
    fn type_of_name(&mut self, file: &str, line: u32, name: &str) -> Option<String> {
        let local = self.session.facts_of(file).and_then(|f| {
            let mut best: Option<&si_core::model::Local> = None;
            for l in f.locals.iter().filter(|l| f.name(l.name as usize) == name) {
                let Some(d) = f.defs.get(l.scope as usize) else { continue };
                if line >= d.line && line <= d.end_line.max(d.line) && l.line <= line && best.is_none_or(|b| l.line >= b.line) {
                    best = Some(l);
                }
            }
            best.map(|l| (l.line, (l.type_name != si_core::model::NONE).then(|| f.name(l.type_name as usize).to_string())))
        });
        if let Some((at, ty)) = local {
            return ty.or_else(|| type_word(&uncommented(&self.line_text(file, at)), name));
        }
        // Declared since the file was indexed: as written above, as far as
        // the function's first line.
        for l in (line.saturating_sub(200).max(1)..=line).rev() {
            let raw = self.line_text(file, l);
            let text = uncommented(&raw);
            let b = text.as_bytes();
            let declared = text.match_indices(name).map(|m| m.0).any(|i| {
                let after = b.get(i + name.len()).copied();
                let before = text[..i].trim_end();
                (i == 0 || !is_ident(b[i - 1]))
                    && after.is_none_or(|c| !is_ident(c))
                    && before.ends_with(|c: char| c == '*' || c == '_' || c.is_ascii_alphanumeric())
                    && text[i + name.len()..].trim_start().starts_with([';', '=', ',', '[', ')'])
            });
            if declared {
                if let Some(t) = type_word(&text, name).filter(|t| !matches!(t.as_str(), "return" | "sizeof" | "case" | "goto" | "else")) {
                    return Some(t);
                }
            }
            if raw.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                break;
            }
        }
        let syms: Vec<u32> = {
            let g = self.g();
            g.find(name).iter().copied().filter(|&s| matches!(g.syms[s as usize].kind, kind::GLOBAL | kind::FIELD)).collect()
        };
        for s in syms {
            let (text, _) = self.written(s);
            if let Some(t) = type_word(&text, name) {
                return Some(t);
            }
        }
        None
    }

    /// The members a `.` or `->` before `at` on a line can be followed by:
    /// of the type of what it follows (`a.b->` as far as the index knows).
    fn members_after(&mut self, file: &str, line: u32, text: &str, at: usize) -> Option<Vec<u32>> {
        let b = text.as_bytes();
        // The chain of names before, as written: `a`, `b`.
        let mut i = at;
        let mut chain: Vec<String> = Vec::new();
        loop {
            let op = if i >= 1 && b[i - 1] == b'.' {
                1
            } else if i >= 2 && &b[i - 2..i] == b"->" {
                2
            } else if chain.is_empty() {
                return None;
            } else {
                break;
            };
            i -= op;
            while i > 0 && b[i - 1] == b' ' {
                i -= 1;
            }
            // (`a[i].`, `f(x)->`: what is indexed or called.)
            if i > 0 && (b[i - 1] == b']' || b[i - 1] == b')') {
                let (open, close) = if b[i - 1] == b']' { (b'[', b']') } else { (b'(', b')') };
                let mut depth = 0;
                while i > 0 {
                    i -= 1;
                    depth += (b[i] == close) as i32 - (b[i] == open) as i32;
                    if depth == 0 {
                        break;
                    }
                }
            }
            let end = i;
            while i > 0 && is_ident(b[i - 1]) {
                i -= 1;
            }
            if i == end {
                break;
            }
            chain.push(text[i..end].to_string());
        }
        chain.reverse();
        let mut ty = self.type_of_name(file, line, chain.first()?)?;
        for member in &chain[1..] {
            let fields = self.members(&ty, 0);
            let f = fields.into_iter().find(|&f| self.g().sym_name(f) == member)?;
            let (decl, _) = self.written(f);
            ty = type_word(&decl, member)?;
        }
        Some(self.members(&ty, 0))
    }

    /// `textDocument/signatureHelp`: in a call being written, how what is
    /// called is declared, and which argument the cursor is at.
    fn signature_help(&mut self, params: &Value) -> Value {
        if self.alongside {
            return Value::Null;
        }
        let Some(path) = params["textDocument"]["uri"].as_str().map(uri_to_path) else { return Value::Null };
        let Some(file) = self.relative(&path) else { return Value::Null };
        let (Some(line), Some(character)) = (params["position"]["line"].as_u64(), params["position"]["character"].as_u64()) else { return Value::Null };
        let line = line as u32 + 1;
        // The few lines up to the cursor, comments out.
        let mut text = String::new();
        for l in line.saturating_sub(5).max(1)..line {
            text.push_str(&uncommented(&self.line_text(&file, l)));
            text.push('\n');
        }
        let here = self.line_text(&file, line);
        let cut = (character as usize).min(here.len());
        text.push_str(&uncommented(here.get(..cut).unwrap_or(&here)));
        // The `(` not closed, and the commas after it.
        let b = text.as_bytes();
        let (mut depth, mut active, mut open) = (0i32, 0u32, None);
        for i in (0..b.len()).rev() {
            match b[i] {
                b')' | b']' | b'}' => depth += 1,
                b'[' | b'{' if depth > 0 => depth -= 1,
                b'{' | b';' => break,
                b'(' if depth == 0 => {
                    open = Some(i);
                    break;
                }
                b'(' => depth -= 1,
                b',' if depth == 0 => active += 1,
                _ => {}
            }
        }
        let Some(open) = open else { return Value::Null };
        let end = text[..open].trim_end().len();
        let start = (0..end).rev().take_while(|&i| is_ident(b[i])).last().unwrap_or(end);
        if start == end {
            return Value::Null;
        }
        let name = text[start..end].to_string();
        let hidden = self.hidden();
        let mut found: Vec<u32> = {
            let g = self.g();
            let mut f = si_core::definition_at(g, &file, line, &name);
            f.retain(|&s| matches!(g.syms[s as usize].kind, kind::FUNCTION | kind::MACRO) && !hidden[g.syms[s as usize].file as usize]);
            f.sort_by_key(|&s| (g.syms[s as usize].flags & flag::DECL != 0, s));
            f
        };
        found.truncate(4);
        let mut signatures: Vec<Value> = Vec::new();
        let mut labels: HashMap<String, usize> = HashMap::new();
        for s in found {
            let (code, mut doc) = self.written(s);
            if doc.is_empty() {
                doc = self.prototype_doc(s);
            }
            let one: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
            let one = one.strip_prefix("#define ").map_or(one.clone(), str::to_string);
            let b = one.as_bytes();
            let Some(at) = one.match_indices(name.as_str()).map(|m| m.0).find(|&i| (i == 0 || !is_ident(b[i - 1])) && b.get(i + name.len()).is_none_or(|&c| !is_ident(c))) else { continue };
            let Some(p) = one[at..].find('(').map(|p| at + p) else { continue };
            // The parameters, between the commas at the top.
            let (mut depth, mut from, mut parts, mut close) = (0i32, p + 1, Vec::new(), None);
            for (i, c) in one.bytes().enumerate().skip(p) {
                match c {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            parts.push((from, i));
                            close = Some(i);
                            break;
                        }
                    }
                    b',' if depth == 1 => {
                        parts.push((from, i));
                        from = i + 1;
                    }
                    _ => {}
                }
            }
            let Some(close) = close else { continue };
            let label = one[..=close].to_string();
            if let Some(&i) = labels.get(&label) {
                if signatures[i].get("documentation").is_none() && !doc.is_empty() {
                    signatures[i]["documentation"] = json!({ "kind": "markdown", "value": doc });
                }
                continue;
            }
            labels.insert(label.clone(), signatures.len());
            let parameters: Vec<Value> = parts
                .iter()
                .map(|&(a, e)| {
                    let t = &one[a..e];
                    let a = a + (t.len() - t.trim_start().len());
                    json!({ "label": [label[..a].encode_utf16().count(), label[..a].encode_utf16().count() + one[a..e].trim().encode_utf16().count()] })
                })
                .filter(|p| p["label"][0] != p["label"][1])
                .collect();
            let mut sig = json!({ "label": label, "parameters": parameters });
            if !doc.is_empty() {
                sig["documentation"] = json!({ "kind": "markdown", "value": doc });
            }
            signatures.push(sig);
        }
        if signatures.is_empty() {
            return Value::Null;
        }
        json!({ "signatures": signatures, "activeSignature": 0, "activeParameter": active })
    }

    /// Completion in an `#include`'s operand: the headers the index has,
    /// each by the shortest end of its path that what is typed begins.
    fn include_completion(&mut self, text: &str, at: usize, line: u64) -> Option<Value> {
        let t = text.trim_start();
        let rest = t.strip_prefix('#')?.trim_start().strip_prefix("include")?;
        let rest = rest.strip_prefix("_next").unwrap_or(rest);
        let open = text.len() - rest.len() + (rest.len() - rest.trim_start().len());
        if !matches!(text.as_bytes().get(open), Some(b'"' | b'<')) || at <= open {
            return None;
        }
        let typed = text[open + 1..at].replace('\\', "/").to_ascii_lowercase();
        let headers = ["h", "hpp", "hh", "hxx", "inl", "inc"];
        let g = self.g();
        let mut found: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for f in 0..g.files.len() as u32 {
            let p = g.file_path(f);
            if !p.rsplit_once('.').is_some_and(|(_, x)| headers.contains(&x.to_ascii_lowercase().as_str())) {
                continue;
            }
            let lower = p.to_ascii_lowercase();
            let mut starts = std::iter::once(0).chain(lower.match_indices('/').map(|m| m.0 + 1));
            if let Some(k) = starts.rfind(|&k| lower[k..].starts_with(&typed)) {
                if seen.insert(p[k..].to_string()) {
                    found.push(p[k..].to_string());
                }
            }
        }
        found.sort_by(|a, b| (a.matches('/').count(), a.len(), a).cmp(&(b.matches('/').count(), b.len(), b)));
        let complete = found.len() <= 200;
        found.truncate(200);
        let range = json!({ "start": { "line": line, "character": text[..open + 1].encode_utf16().count() }, "end": { "line": line, "character": text[..at].encode_utf16().count() } });
        let items: Vec<Value> = found
            .iter()
            .enumerate()
            .map(|(i, p)| json!({ "label": p, "kind": 17, "sortText": format!("{i:04}"), "filterText": p, "textEdit": { "range": range, "newText": p } }))
            .collect();
        Some(json!({ "isIncomplete": !complete, "items": items }))
    }

    /// `textDocument/completion`: the names the index has that the word
    /// being typed matches, best first.
    fn completion(&mut self, params: &Value) -> Value {
        let empty = json!({ "isIncomplete": true, "items": [] });
        let Some(path) = params["textDocument"]["uri"].as_str().map(uri_to_path) else { return empty };
        let Some(file) = self.relative(&path) else { return empty };
        let (Some(line), Some(character)) = (params["position"]["line"].as_u64(), params["position"]["character"].as_u64()) else { return empty };
        let text = self.line_text(&file, line as u32 + 1);
        let b = text.as_bytes();
        let at = (character as usize).min(b.len());
        let start = (0..at).rev().take_while(|&i| is_ident(b[i])).last().unwrap_or(at);
        let word = text[start..at].to_string();
        let word = word.as_str();
        if let Some(found) = self.include_completion(&text, at, line) {
            return found;
        }
        // After `.` or `->`: the members of what it follows (where the
        // C/C++ extension does not give them), else nothing.
        let before = text[..start].trim_end();
        if before.ends_with('.') || before.ends_with("->") {
            if self.alongside || before.ends_with("..") {
                return empty;
            }
            let Some(fields) = self.members_after(&file, line as u32 + 1, before, before.len()) else { return empty };
            let lower = word.to_ascii_lowercase();
            let g = self.g();
            let mut seen = std::collections::HashSet::new();
            let items: Vec<Value> = fields
                .into_iter()
                .filter(|&f| g.sym_name(f).to_ascii_lowercase().starts_with(&lower) && seen.insert(g.sym_name(f)))
                .map(|f| json!({ "label": g.sym_name(f), "kind": 5, "detail": format!("{}:{}", g.file_path(g.syms[f as usize].file), g.syms[f as usize].line) }))
                .collect();
            return json!({ "isIncomplete": false, "items": items });
        }
        if word.is_empty() || word.as_bytes()[0].is_ascii_digit() {
            return empty;
        }
        // The parameters and locals of the function the cursor is in, and
        // C's keywords (where the C/C++ extension does not give them).
        let mut extra: Vec<Value> = Vec::new();
        if !self.alongside {
            let lower = word.to_ascii_lowercase();
            let here = line as u32 + 1;
            if let Some(f) = self.session.facts_of(&file) {
                let mut seen = std::collections::HashSet::new();
                for l in &f.locals {
                    let Some(d) = f.defs.get(l.scope as usize) else { continue };
                    let name = f.name(l.name as usize);
                    if here >= d.line && here <= d.end_line.max(d.line) && (l.param || l.line <= here) && name.to_ascii_lowercase().starts_with(&lower) && name != word && seen.insert(name) {
                        let ty = (l.type_name != si_core::model::NONE).then(|| f.name(l.type_name as usize));
                        extra.push(json!({ "label": name, "kind": 6, "detail": format!("{}{}", if l.param { "parameter" } else { "local" }, ty.map_or(String::new(), |t| format!(" · {t}"))) }));
                    }
                }
            }
            for k in C_KEYWORDS.iter().filter(|k| k.starts_with(word) && **k != word) {
                extra.push(json!({ "label": k, "kind": 14 }));
            }
        }
        // The names that start with it, from the listing in name order
        // (made once): where they begin is found by halving. Few of them:
        // those that hold it otherwise too, as a search finds them.
        const MOST: usize = 150;
        self.make_listing(false);
        let g = &self.session.graph;
        let all = self.listing[0].as_deref().unwrap_or(&[]);
        let lower = word.to_ascii_lowercase();
        let first = all.partition_point(|&s| g.sym_name(s).to_ascii_lowercase().as_str() < lower.as_str());
        let mut picked: Vec<u32> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut complete = true;
        for &s in &all[first..] {
            let name = g.sym_name(s);
            if !name.as_bytes().get(..lower.len()).is_some_and(|b| b.eq_ignore_ascii_case(lower.as_bytes())) {
                break;
            }
            if seen.insert(name) {
                if picked.len() == MOST {
                    complete = false;
                    break;
                }
                picked.push(s);
            }
        }
        if complete && picked.len() < MOST / 3 {
            let hidden = self.hidden.clone().unwrap_or_default();
            let g = &self.session.graph;
            for (_, s) in si_core::search::search(g, word, MOST).into_iter().filter(|h| !hidden.get(g.syms[h.1 as usize].file as usize).copied().unwrap_or(false)) {
                if picked.len() >= MOST {
                    break;
                }
                if seen.insert(g.sym_name(s)) {
                    picked.push(s);
                }
            }
            complete = false;
        }
        let mut items = Vec::new();
        let ahead = extra.len();
        for (rank, mut item) in extra.into_iter().enumerate() {
            item["sortText"] = json!(format!("{rank:04}"));
            items.push(item);
        }
        for (rank, &s) in picked.iter().enumerate() {
            let rank = rank + ahead;
            let sym = &g.syms[s as usize];
            let name = g.sym_name(s);
            // (LSP's CompletionItemKind.)
            let kind = match sym.kind {
                kind::FUNCTION => 3,
                kind::MACRO => 21,
                kind::STRUCT | kind::UNION => 22,
                kind::ENUM => 13,
                kind::ENUM_CONST => 20,
                kind::TYPEDEF | kind::CLASS => 7,
                kind::FIELD => 5,
                kind::NAMESPACE => 9,
                _ => 6,
            };
            items.push(json!({ "label": name, "kind": kind, "detail": format!("{}:{}", g.file_path(sym.file), sym.line), "sortText": format!("{rank:04}") }));
        }
        // (All of them: the editor narrows the list itself as more is typed.)
        json!({ "isIncomplete": !complete, "items": items })
    }

    /// The operand of an `#include` on a line, and where it is in it.
    fn include_operand(text: &str) -> Option<(&str, usize, usize)> {
        let t = text.trim_start();
        let rest = t.strip_prefix('#')?.trim_start().strip_prefix("include")?;
        let rest = rest.strip_prefix("_next").unwrap_or(rest);
        let at = text.len() - rest.len() + (rest.len() - rest.trim_start().len());
        let close = match text[at..].chars().next()? {
            '"' => '"',
            '<' => '>',
            _ => return None,
        };
        let end = text[at + 1..].find(close)? + at + 1;
        Some((&text[at + 1..end], at + 1, end))
    }

    /// `textDocument/documentLink`: each `#include` to the file it means,
    /// where the index has one.
    fn document_links(&mut self, params: &Value) -> Value {
        let path = uri_to_path(params["textDocument"]["uri"].as_str().unwrap_or(""));
        let Some(file) = self.relative(&path) else { return json!([]) };
        let text = match self.open.get(&path) {
            Some(t) => t.clone(),
            None => String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()).into_owned(),
        };
        let mut found: Vec<(usize, &str, usize, usize)> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if let Some((operand, a, e)) = Self::include_operand(line) {
                found.push((i, operand, a, e));
            }
        }
        let operands: Vec<&str> = found.iter().map(|f| f.1).collect();
        let targets = si_core::includes_meant(self.g(), &file, &operands);
        let mut links = Vec::new();
        for ((line, _, a, e), files) in found.iter().zip(targets) {
            // (One of several: the nearest, as the index takes it.)
            if let Some(target) = files.first() {
                links.push(json!({ "range": { "start": { "line": line, "character": a }, "end": { "line": line, "character": e } },
                                   "target": path_to_uri(&self.root.join(target)) }));
            }
        }
        json!(links)
    }

    fn definition(&mut self, params: &Value) -> Value {
        // On an `#include`: the file it means.
        if let (Some(path), Some(line)) = (params["textDocument"]["uri"].as_str().map(uri_to_path), params["position"]["line"].as_u64()) {
            if let Some(file) = self.relative(&path) {
                let text = self.line_text(&file, line as u32 + 1);
                if let Some((operand, _, _)) = Self::include_operand(&text) {
                    let files = si_core::includes_meant(self.g(), &file, &[operand]).pop().unwrap_or_default();
                    let start = json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } });
                    return json!(files.iter().map(|f| json!({ "uri": path_to_uri(&self.root.join(f)), "range": start })).collect::<Vec<_>>());
                }
            }
        }
        if let Some(h) = self.local_at(params) {
            return json!([self.location(&h.file, h.line, &h.name)]);
        }
        let mut found = self.targets(params);
        let decl = |g: &Graph, s: u32| g.syms[s as usize].flags & flag::DECL != 0;
        // The definition, when there is one; else the declarations. On the
        // definition itself: the declarations, to go back and forth.
        let defs: Vec<u32> = found.iter().copied().filter(|&s| !decl(self.g(), s)).collect();
        let here = self.at(params).map(|(file, line, _)| (file, line));
        let on_def = defs.iter().any(|&d| {
            let loc = self.sym_location(d);
            let at = (uri_to_path(loc["uri"].as_str().unwrap_or("")), loc["range"]["start"]["line"].as_u64().unwrap_or(0) as u32 + 1);
            here.as_ref().is_some_and(|(file, line)| self.relative(&at.0).as_deref() == Some(file) && at.1 == *line)
        });
        if on_def && found.iter().any(|&s| decl(self.g(), s)) {
            found.retain(|&s| decl(self.g(), s));
        } else if !defs.is_empty() {
            found = defs;
        }
        json!(found.into_iter().take(LIMIT).map(|s| self.sym_location(s)).collect::<Vec<_>>())
    }

    /// The prototypes and other declarations, when there are any; else
    /// the definition.
    fn declaration(&mut self, params: &Value) -> Value {
        if let Some(h) = self.local_at(params) {
            return json!([self.location(&h.file, h.line, &h.name)]);
        }
        let mut found = self.targets(params);
        if found.iter().any(|&s| self.g().syms[s as usize].flags & flag::DECL != 0) {
            found.retain(|&s| self.g().syms[s as usize].flags & flag::DECL != 0);
        }
        json!(found.into_iter().take(LIMIT).map(|s| self.sym_location(s)).collect::<Vec<_>>())
    }

    /// Where the type of what is under the cursor is defined: of a variable,
    /// a parameter or a member, its type; of a function, what it returns;
    /// of a type, itself.
    fn type_definition(&mut self, params: &Value) -> Value {
        let mut types: Vec<String> = Vec::new();
        if let Some(h) = self.local_at(params) {
            types.extend(h.type_name);
        } else {
            for s in self.targets(params) {
                let g = self.g();
                let sym = g.syms[s as usize];
                let path = g.file_path(sym.file).to_string();
                let def = s - g.files[sym.file as usize].sym_start;
                match sym.kind {
                    kind::STRUCT | kind::UNION | kind::ENUM | kind::TYPEDEF | kind::CLASS => types.push(g.sym_name(s).to_string()),
                    _ => {
                        let Some(f) = self.session.facts_of(&path) else { continue };
                        let named = if sym.kind == kind::FIELD {
                            f.records.iter().find_map(|r| r.defs.iter().position(|&d| d == def).map(|i| r.types[i]))
                        } else {
                            f.typed.iter().find(|t| t.0 == def).map(|t| t.1)
                        };
                        types.extend(named.filter(|&t| t != si_core::model::NONE).map(|t| f.name(t as usize).to_string()));
                    }
                }
            }
        }
        types.sort();
        types.dedup();
        let g = self.g();
        let is_type = |s: u32| matches!(g.syms[s as usize].kind, kind::STRUCT | kind::UNION | kind::ENUM | kind::TYPEDEF | kind::CLASS);
        let mut found: Vec<u32> = types.iter().flat_map(|t| g.find(t).iter().copied().filter(|&s| is_type(s))).collect();
        if found.iter().any(|&s| g.syms[s as usize].flags & flag::DECL == 0) {
            found.retain(|&s| g.syms[s as usize].flags & flag::DECL == 0);
        }
        json!(found.into_iter().take(LIMIT).map(|s| self.sym_location(s)).collect::<Vec<_>>())
    }

    /// What carries out what is under the cursor: of a prototype or a
    /// function, its definitions (of a virtual method, those of every class
    /// that has one); of a member or variable that holds functions, the
    /// functions stored in it.
    fn implementation(&mut self, params: &Value) -> Value {
        let targets = self.targets(params);
        let g = self.g();
        let mut found: Vec<u32> = Vec::new();
        let slots: Vec<u32> = targets.iter().copied().filter(|&s| matches!(g.syms[s as usize].kind, kind::FIELD | kind::GLOBAL)).collect();
        for b in self.session.stats.bind_sites.iter().filter(|b| slots.contains(&b[2])) {
            found.push(b[3]);
        }
        for &t in targets.iter().filter(|&&t| g.syms[t as usize].kind == kind::FUNCTION) {
            found.extend(g.find(g.sym_name(t)).iter().copied().filter(|&s| g.syms[s as usize].kind == kind::FUNCTION && g.syms[s as usize].flags & flag::DECL == 0));
        }
        found.sort_unstable();
        found.dedup();
        json!(found.into_iter().take(LIMIT).map(|s| self.sym_location(s)).collect::<Vec<_>>())
    }

    fn references(&mut self, params: &Value) -> Value {
        if let Some(h) = self.local_at(params) {
            // Within its function: where it is declared, and each line it is used on.
            let declared = params["context"]["includeDeclaration"].as_bool().unwrap_or(true).then_some(h.line);
            let lines: Vec<u32> = declared.into_iter().chain(h.uses.iter().copied().filter(|&l| l != h.line)).collect();
            return json!(lines.into_iter().map(|l| self.location(&h.file, l, &h.name)).collect::<Vec<_>>());
        }
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

    /// `si/counterpart`: the header of a source file, or the source of a
    /// header: the files of the same name with the other kind of suffix,
    /// those in the same folder first, then the nearest.
    fn counterpart(&mut self, params: &Value) -> Value {
        let path = uri_to_path(params["textDocument"]["uri"].as_str().unwrap_or(""));
        let Some(file) = self.relative(&path) else { return json!([]) };
        let (dir, base) = file.rsplit_once('/').unwrap_or(("", &file));
        let Some((stem, suffix)) = base.rsplit_once('.') else { return json!([]) };
        let headers = ["h", "hpp", "hh", "hxx", "inl"];
        let sources = ["c", "cpp", "cc", "cxx", "c++"];
        let want: &[&str] = if headers.contains(&suffix.to_ascii_lowercase().as_str()) { &sources } else { &headers };
        let g = self.g();
        let mut found: Vec<(usize, String)> = (0..g.files.len() as u32)
            .map(|f| g.file_path(f))
            .filter(|p| {
                let b = p.rsplit('/').next().unwrap_or(p);
                b.rsplit_once('.').is_some_and(|(s, x)| s.eq_ignore_ascii_case(stem) && want.contains(&x.to_ascii_lowercase().as_str()))
            })
            .map(|p| {
                let d = p.rsplit_once('/').map_or("", |x| x.0);
                let shared = d.split('/').zip(dir.split('/')).take_while(|(a, b)| a == b).count();
                (usize::MAX - if d == dir { usize::MAX } else { shared }, p.to_string())
            })
            .collect();
        found.sort();
        json!(found.into_iter().map(|(_, p)| path_to_uri(&self.root.join(p))).collect::<Vec<_>>())
    }

    /// `textDocument/documentHighlight`: in this file, where the name under
    /// the cursor means what it means there: each of its spellings on the
    /// lines the index has it on (the definition, the uses).
    fn document_highlight(&mut self, params: &Value) -> Value {
        let Some((file, line, name)) = self.at(params) else { return Value::Null };
        let mut lines: Vec<u32> = vec![line];
        if let Some(h) = self.local_at(params) {
            lines.push(h.line);
            lines.extend(h.uses.iter().copied());
        } else {
            let g = self.g();
            for t in si_core::definition_at(g, &file, line, &name) {
                let s = &g.syms[t as usize];
                if g.file_path(s.file) == file {
                    lines.push(s.line);
                }
                for e in g.refs_to(t).iter().filter(|e| e.kind() != refkind::BIND) {
                    if g.file_path(g.syms[e.other as usize].file) == file {
                        lines.push(e.line());
                    }
                }
            }
        }
        lines.sort_unstable();
        lines.dedup();
        let mut out = Vec::new();
        for l in lines {
            let text = self.line_text(&file, l);
            let b = text.as_bytes();
            for (i, _) in text.match_indices(&name) {
                let end = i + name.len();
                if (i == 0 || !is_ident(b[i - 1])) && b.get(end).is_none_or(|&c| !is_ident(c)) {
                    out.push(json!({ "range": { "start": { "line": l - 1, "character": i }, "end": { "line": l - 1, "character": end } }, "kind": 1 }));
                }
            }
        }
        json!(out)
    }

    /// `si/references`: the references of the name under the cursor, by
    /// file, each with the text of its line: what a view lists needs no
    /// file opened in the editor, and comes from the index alone.
    fn grouped_references(&mut self, params: &Value) -> Value {
        let found = self.references(params);
        let mut groups: Vec<Value> = Vec::new();
        let mut total = 0;
        for loc in found.as_array().into_iter().flatten() {
            let uri = loc["uri"].as_str().unwrap_or("").to_string();
            let Some(file) = self.relative(&uri_to_path(&uri)) else { continue };
            let (line, character) = (loc["range"]["start"]["line"].as_u64().unwrap_or(0) as u32, loc["range"]["start"]["character"].as_u64().unwrap_or(0));
            let text = self.line_text(&file, line + 1).trim().to_string();
            let item = json!({ "line": line, "character": character, "text": text });
            match groups.last_mut().filter(|g| g["uri"] == uri.as_str()) {
                Some(g) => g["items"].as_array_mut().unwrap().push(item),
                None => groups.push(json!({ "file": file, "uri": uri, "items": [item] })),
            }
            total += 1;
        }
        json!({ "groups": groups, "total": total })
    }

    fn symbol_info(&mut self, sym: u32) -> Value {
        let (name, k, container) = {
            let g = self.g();
            let s = &g.syms[sym as usize];
            (display_name(g, sym), s.kind, g.file_path(s.file).to_string())
        };
        json!({ "name": name, "kind": symbol_kind(k), "location": self.sym_location(sym), "containerName": container })
    }

    fn document_symbols(&mut self, params: &Value) -> Value {
        let path = uri_to_path(params["textDocument"]["uri"].as_str().unwrap_or(""));
        let Some(file) = self.relative(&path) else { return json!([]) };
        let g = self.g();
        let Some(f) = (0..g.files.len() as u32).find(|&f| g.file_path(f) == file) else { return json!([]) };
        // (An editor refuses the whole list over one symbol without a name.)
        let syms: Vec<u32> = g.file_syms(f).skip(1).filter(|&s| !g.sym_name(s).trim().is_empty()).collect();
        json!(syms.into_iter().map(|s| self.symbol_info(s)).collect::<Vec<_>>())
    }

    fn workspace_symbols(&mut self, params: &Value) -> Value {
        let query = params["query"].as_str().unwrap_or("");
        // Fragments in any order, initials, letters in order.
        let syms: Vec<u32> = si_core::search::search(self.g(), query, 400).into_iter().map(|h| h.1).filter(|&s| self.shown(s)).take(200).collect();
        json!(syms.into_iter().map(|s| self.symbol_info(s)).collect::<Vec<_>>())
    }

    fn hierarchy_item(&mut self, sym: u32) -> Value {
        let loc = self.sym_location(sym);
        let g = self.g();
        let s = &g.syms[sym as usize];
        json!({ "name": display_name(g, sym), "kind": symbol_kind(s.kind), "detail": g.file_path(s.file),
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
        let hidden = self.hidden();
        let g = self.g();
        let edges = if incoming { si_core::callers_through_macros(g, sym) } else { si_core::callees_through_slots(g, sym) };
        let away = |s: u32| hidden[g.syms[s as usize].file as usize];
        let edges: Vec<_> = edges.into_iter().filter(|c| !away(c.caller) && !c.via.is_some_and(away)).collect();
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
        let hits = si_core::search::search(self.g(), params["query"].as_str().unwrap_or(""), limit * 2);
        let hits: Vec<(i32, u32)> = hits.into_iter().filter(|h| self.shown(h.1)).take(limit).collect();
        json!(hits.into_iter().map(|(score, s)| { let mut v = self.entry(s); v["score"] = json!(score); v }).collect::<Vec<_>>())
    }

    /// A symbol for the extension's own views.
    fn entry(&mut self, sym: u32) -> Value {
        let loc = self.sym_location(sym);
        let g = self.g();
        let s = &g.syms[sym as usize];
        json!({ "name": display_name(g, sym), "kind": kind::name(s.kind), "file": g.file_path(s.file), "line": s.line,
                "endLine": s.end_line, "declaration": s.flags & flag::DECL != 0, "uri": loc["uri"], "range": loc["range"],
                "data": { "file": g.file_path(s.file), "line": s.line, "name": g.sym_name(sym) } })
    }

    /// `si/context`: what the name under the cursor is defined as.
    fn context(&mut self, params: &Value) -> Value {
        if let Some(h) = self.local_at(params) {
            let mut v = self.local_entry(&h);
            v["candidates"] = json!(1);
            return v;
        }
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

    /// The `#if`s that decide whether what is on `lines` of `file` is
    /// compiled at all, as {line, text}, when every one of those lines is
    /// in a branch the conditions in force cannot tell about; nothing when
    /// one of them is compiled for certain.
    fn undecided_by(&mut self, file: &str, lines: &[u32]) -> Vec<Value> {
        let Some(facts) = self.session.facts_of(file) else { return Vec::new() };
        let mut deciding = Vec::new();
        for &line in lines {
            // The innermost branch around the line: the last to start.
            match facts.undecided.iter().rev().find(|&&(a, b, _, _)| a <= line && line <= b) {
                Some(&(_, _, directive, _)) => deciding.push(directive),
                None => return Vec::new(),
            }
        }
        deciding.sort_unstable();
        deciding.dedup();
        deciding.into_iter().map(|line| json!({ "line": line, "text": self.line_text(file, line).trim().to_string() })).collect()
    }

    /// `si/relation`: the callers or callees of the function under the
    /// cursor (or the function the cursor is in, or `item`), each saying how
    /// the call comes about: by name, through a macro, through a member or
    /// variable holding the function, through a pointer; certain or not.
    fn relation(&mut self, params: &Value) -> Value {
        let incoming = params["direction"].as_str() != Some("callees");
        // On a parameter or local variable: it is used by its function, on
        // these lines; from the function the tree goes on as for any other.
        if let Some(h) = (!params["item"].is_object()).then(|| self.local_at(params)).flatten() {
            let root = self.local_entry(&h);
            let function = self.item_symbol(&json!({ "data": { "file": h.file, "line": h.function_line, "name": h.function } }));
            let mut entries = Vec::new();
            if let Some(sym) = function.filter(|_| incoming) {
                let mut v = self.entry(sym);
                v["lines"] = json!(h.uses);
                v["viaMacro"] = Value::Null;
                v["through"] = Value::Null;
                v["pointer"] = json!(false);
                v["certain"] = json!(true);
                entries.push(v);
            }
            return json!({ "root": root, "entries": entries });
        }
        let root = if params["item"].is_object() {
            self.item_symbol(&json!({ "data": params["item"] }))
        } else {
            let callable = |s: &Server, x: u32| matches!(s.g().syms[x as usize].kind, kind::FUNCTION | kind::MACRO);
            let mut under: Vec<u32> = self.targets(params).into_iter().filter(|&x| callable(self, x)).collect();
            under.sort_by_key(|&x| self.g().syms[x as usize].flags & flag::DECL != 0);
            // On a variable, a type, a member or a constant: who uses it is
            // its "called by"; a member or variable that holds functions
            // "calls" those.
            let found = self.targets(params);
            let other = {
                let g = self.g();
                let wanted = |&x: &u32| g.syms[x as usize].kind != kind::FILE && (incoming || !si_core::stored_in(g, x).is_empty());
                found.into_iter().find(wanted)
            };
            under.first().copied().or(other).or_else(|| {
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
        let root_kind = g.syms[root as usize].kind;
        let edges = match (incoming, root_kind) {
            (true, kind::FUNCTION) => si_core::callers_through_macros(g, root),
            // A macro is called, and (as a constant, say) just used.
            (true, kind::MACRO) => {
                let mut all = si_core::callers_through_macros(g, root);
                let known: std::collections::HashSet<(u32, u32)> = all.iter().map(|c| (c.caller, c.line)).collect();
                all.extend(si_core::users(g, root).into_iter().filter(|c| !known.contains(&(c.caller, c.line))));
                all
            }
            (true, _) => si_core::users(g, root),
            (false, kind::FUNCTION | kind::MACRO) => si_core::callees_through_slots(g, root),
            (false, _) => si_core::stored_in(g, root),
        };
        let callable_root = matches!(root_kind, kind::FUNCTION | kind::MACRO);
        // (Nor what is in a file no compilation reads, or comes through one.)
        let hidden = self.hidden();
        let g = self.g();
        let away = |s: u32| hidden[g.syms[s as usize].file as usize];
        let edges: Vec<_> = edges.into_iter().filter(|c| !away(c.caller) && !c.via.is_some_and(away) && !c.slot.is_some_and(away)).collect();
        // (other end, macro, member or variable, pointer) -> (lines, certain)
        let mut groups: Vec<((u32, Option<u32>, Option<u32>, bool), Vec<u32>, bool)> = Vec::new();
        for c in edges {
            // A function is called from code; anything else may also be
            // mentioned by a type, or at the top of a file.
            if callable_root && !matches!(g.syms[c.caller as usize].kind, kind::FUNCTION | kind::MACRO | kind::GLOBAL) {
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
            lines.retain(|&l| l != 0);
            let names = { let g = self.g(); (via.map(|m| g.sym_name(m).to_string()), slot.map(|m| g.sym_name(m).to_string())) };
            // The lines are in the caller's file.
            let site_file = { let g = self.g(); g.file_path(g.syms[if incoming { other } else { root } as usize].file).to_string() };
            let mut v = self.entry(other);
            v["onlyIf"] = json!(self.undecided_by(&site_file, &lines));
            v["lines"] = json!(lines);
            v["viaMacro"] = json!(names.0);
            v["through"] = json!(names.1);
            v["pointer"] = json!(pointer);
            v["certain"] = json!(certain);
            entries.push(v);
        }
        json!({ "root": self.entry(root), "entries": entries })
    }

    /// `si/assignments`: where functions are stored in the member or
    /// variable under the cursor, or, on a function, where it is stored.
    /// Each is a place in the source: a line of an initializer or an
    /// assignment.
    fn assignments(&mut self, params: &Value) -> Value {
        let targets = self.targets(params);
        let g = self.g();
        let slot_like = |s: u32| matches!(g.syms[s as usize].kind, kind::FIELD | kind::GLOBAL);
        // On a member or variable: what is stored in it. Else on a function: where it is stored.
        let slots: Vec<u32> = targets.iter().copied().filter(|&s| slot_like(s)).collect();
        let functions: Vec<u32> = targets.iter().copied().filter(|&s| g.syms[s as usize].kind == kind::FUNCTION).collect();
        let by_slot = !slots.is_empty() && self.session.stats.bind_sites.iter().any(|b| slots.contains(&b[2]));
        let wanted: &[u32] = if by_slot { &slots } else { &functions };
        let mut sites: Vec<[u32; 4]> =
            self.session.stats.bind_sites.iter().filter(|b| wanted.contains(&b[if by_slot { 2 } else { 3 }])).copied().collect();
        sites.sort_unstable();
        sites.dedup();
        sites.truncate(LIMIT);
        let target = wanted.first().or(targets.first()).copied();
        let mut rows = Vec::new();
        for [file, line, slot, func] in sites {
            let (path, inside) = {
                let g = self.g();
                let inside = g.symbol_at(file, line).map(|s| g.sym_name(s).to_string());
                (g.file_path(file).to_string(), inside)
            };
            let at = { let g = self.g(); g.sym_name(func).to_string() };
            let place = self.location(&path, line, &at);
            rows.push(json!({ "member": self.entry(slot), "function": self.entry(func), "file": path, "line": line,
                              "uri": place["uri"], "range": place["range"], "inside": inside }));
        }
        json!({ "target": target.map(|t| self.entry(t)), "by": if by_slot { "member" } else { "function" }, "rows": rows })
    }

    /// `si/conditions`: the sets of conditions the project has and which is
    /// in force, what that one comes to macro by macro (and what says so),
    /// and the build configurations found in the tree that a set could
    /// follow.
    fn conditions(&self) -> Value {
        use si_core::cond;
        let (project, automatic) = cond::Project::load_told(&self.root);
        let effective = cond::effective(&project.current(), &self.root);
        let available: Vec<Value> = cond::discover(&self.root).into_iter().map(|(name, source)| json!({ "name": name, "source": source })).collect();
        let all = self.session.source_defines();
        let from_sources: Vec<Value> = all.iter().take(2000).map(|(name, value, files)| json!({ "name": name, "value": value, "files": files })).collect();
        json!({ "configurations": project.configurations, "active": project.active, "automatic": automatic, "available": available,
                "fromSources": from_sources, "fromSourcesTotal": all.len(), "files": self.session.files(),
                "sourceDefines": project.current().source_defines != Some(false),
                "effective": effective, "includePaths": cond::include_paths(&project.current(), &self.root),
                "headerDirs": self.session.header_dirs().iter().take(4000).map(|d| d.to_string_lossy().into_owned()).collect::<Vec<_>>(),
                "known": self.session.conditions_known() })
    }

    /// `si/scope`: which folders are indexed, and where that is written.
    fn scope(&self) -> Value {
        let s = self.session.scope();
        json!({ "include": s.include, "exclude": s.exclude,
                "config": si_core::scope::Scope::config_path(&self.root).to_string_lossy() })
    }

    fn synced(&mut self, up: si_core::session::Update) -> Value {
        trace(|| format!("  synced: {} added, {} removed, {} changed, {} parsed, {:.0} ms", up.added, up.removed, up.changed, up.extracted, up.extract_ms + up.link_ms));
        self.lines.clear();
        self.listing = [None, None];
        self.hidden = None;
        let mut v = self.scope();
        v["files"] = json!(self.session.files());
        v["symbols"] = json!(self.g().syms.len());
        v["added"] = json!(up.added);
        v["removed"] = json!(up.removed);
        v["changed"] = json!(up.changed);
        v["parsed"] = json!(up.extracted);
        v["ms"] = json!(up.extract_ms + up.link_ms);
        v
    }

    fn update(&mut self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            let up = self.session.update(&paths);
            trace(|| format!("  updated {} files: {} parsed, {:.0} ms", paths.len(), up.extracted, up.extract_ms + up.link_ms));
            self.lines.clear();
            self.listing = [None, None];
        self.hidden = None;
        }
    }

    /// `si/symbols`: a stretch of the whole project's symbols in name order
    /// (`offset`, `limit`), with how many there are. Prototypes are left to
    /// their definitions; a list this long is fetched as it is scrolled.
    fn symbols(&mut self, params: &Value) -> Value {
        // `order`: "name", or "kind" (functions together, then their
        // prototypes, macros, and so on, each kind by name).
        let by_kind = params["order"].as_str() == Some("kind");
        self.make_listing(by_kind);
        let all = self.listing[by_kind as usize].as_ref().unwrap();
        let offset = (params["offset"].as_u64().unwrap_or(0) as usize).min(all.len());
        let limit = params["limit"].as_u64().unwrap_or(200).min(2000) as usize;
        let g = self.g();
        let q = si_core::graph::Qualifier::new(g);
        let items: Vec<Value> = all[offset..(offset + limit).min(all.len())]
            .iter()
            .map(|&s| {
                let sym = &g.syms[s as usize];
                json!({ "name": q.name(g, s), "kind": kind::name(sym.kind), "declaration": sym.flags & flag::DECL != 0, "file": g.file_path(sym.file), "line": sym.line })
            })
            .collect();
        json!({ "total": all.len(), "offset": offset, "items": items })
    }

    /// Every symbol in name order, without regard to case (`by_kind`: each
    /// kind by name); made when first asked for.
    fn make_listing(&mut self, by_kind: bool) {
        if self.listing[by_kind as usize].is_none() {
            let hidden = self.hidden();
            let g = self.g();
            let mut all: Vec<u32> = (0..g.syms.len() as u32)
                .filter(|&s| g.syms[s as usize].kind != kind::FILE && !hidden[g.syms[s as usize].file as usize])
                .collect();
            let rank = |k: u8, flags: u8| match (k, flags & flag::DECL != 0) {
                (kind::FUNCTION, false) => 0,
                (kind::FUNCTION, true) => 1,
                (kind::MACRO, _) => 2,
                (kind::STRUCT, _) => 3,
                (kind::UNION, _) => 4,
                (kind::ENUM, _) => 5,
                (kind::ENUM_CONST, _) => 6,
                (kind::TYPEDEF, _) => 7,
                (kind::GLOBAL, false) => 8,
                (kind::GLOBAL, true) => 9,
                _ => 10,
            };
            // Without regard to case, as a person looks a name up; then by
            // the name itself and by place, so the order is always the same.
            all.sort_by_cached_key(|&s| {
                let sym = &g.syms[s as usize];
                (if by_kind { rank(sym.kind, sym.flags) } else { 0 }, g.sym_name(s).to_ascii_lowercase(), g.sym_name(s).to_string(), sym.flags & flag::DECL, sym.file, sym.line)
            });
            self.listing[by_kind as usize] = Some(all);
        }
    }

    /// `si/place`: where exactly the name of the symbol defined on `line`
    /// of `file` is written (a listing does not carry columns).
    fn place(&mut self, params: &Value) -> Value {
        let (file, line, name) =
            (params["file"].as_str().unwrap_or(""), params["line"].as_u64().unwrap_or(1) as u32, params["name"].as_str().unwrap_or(""));
        self.location(file, line, name)
    }

    /// The lines switched off by the conditions in force that a
    /// `textDocument` position is on, if it is on such a line: nothing
    /// there is compiled, so nothing is to be found from it.
    /// `si/inactive`: the lines of a file these conditions do not compile,
    /// as (first, last) from 1.
    fn inactive(&mut self, params: &Value) -> Value {
        let path = uri_to_path(params["textDocument"]["uri"].as_str().unwrap_or(""));
        let Some(file) = self.relative(&path) else { return json!([]) };
        json!(self.session.facts_of(&file).map_or(Vec::new(), |f| f.inactive.clone()))
    }

    fn inactive_at(&mut self, params: &Value) -> Option<(u32, u32)> {
        let file = self.relative(&uri_to_path(params["textDocument"]["uri"].as_str()?))?;
        let line = params["position"]["line"].as_u64()? as u32 + 1;
        self.session.facts_of(&file)?.inactive.iter().copied().find(|&(a, b)| a <= line && line <= b)
    }

    fn request(&mut self, method: &str, params: &Value) -> Option<Value> {
        // Asked from a line the conditions switch off: what a name there
        // means is not what the same name means where it is compiled.
        let from_position = matches!(
            method,
            "textDocument/definition"
                | "textDocument/hover"
                | "textDocument/documentHighlight"
                | "textDocument/declaration"
                | "textDocument/typeDefinition"
                | "textDocument/implementation"
                | "textDocument/references"
                | "textDocument/prepareRename"
                | "textDocument/rename"
                | "textDocument/prepareCallHierarchy"
                | "si/context"
                | "si/relation"
                | "si/assignments"
                | "si/references"
        );
        if let Some((first, last)) = from_position.then(|| self.inactive_at(params)).flatten() {
            return Some(match method {
                "textDocument/references" => json!([]),
                "si/context" | "si/relation" | "si/assignments" | "si/references" => json!({ "inactive": [first, last] }),
                _ => Value::Null,
            });
        }
        Some(match method {
            "textDocument/definition" => self.definition(params),
            "textDocument/hover" => self.hover(params),
            "textDocument/signatureHelp" => self.signature_help(params),
            "textDocument/completion" => self.completion(params),
            "textDocument/declaration" => self.declaration(params),
            "textDocument/typeDefinition" => self.type_definition(params),
            "textDocument/implementation" => self.implementation(params),
            "textDocument/references" => self.references(params),
            "textDocument/documentSymbol" => self.document_symbols(params),
            "workspace/symbol" => self.workspace_symbols(params),
            "textDocument/prepareCallHierarchy" => self.prepare_hierarchy(params),
            "callHierarchy/incomingCalls" => self.calls(params, true),
            "callHierarchy/outgoingCalls" => self.calls(params, false),
            "si/search" => self.search(params),
            "si/context" => self.context(params),
            "si/relation" => self.relation(params),
            "si/references" => self.grouped_references(params),
            "si/assignments" => self.assignments(params),
            "si/symbols" => self.symbols(params),
            "si/place" => self.place(params),
            "si/locals" => self.locals(params),
            "si/inactive" => self.inactive(params),
            "si/counterpart" => self.counterpart(params),
            "textDocument/documentLink" => self.document_links(params),
            "textDocument/foldingRange" => self.folding_ranges(params),
            "textDocument/semanticTokens/full" => self.semantic_tokens(params),
            "textDocument/prepareRename" => self.prepare_rename(params),
            "textDocument/rename" => self.rename(params),
            "textDocument/inlayHint" => self.inlay_hints(params),
            "textDocument/codeAction" => self.code_actions(params),
            "textDocument/documentHighlight" => self.document_highlight(params),
            "si/conditions" => self.conditions(),
            "si/setConditions" => {
                let project: si_core::cond::Project = serde_json::from_value(params.clone()).unwrap_or_default();
                match self.session.set_condition_project(&project) {
                    Ok(up) => {
                        let mut v = self.synced(up);
                        v["conditions"] = self.conditions();
                        v
                    }
                    Err(e) => json!({ "error": format!("could not save the conditions in {}: {e}", si_core::scope::Scope::config_path(&self.root).display()) }),
                }
            }
            "si/scope" => self.scope(),
            "si/setScope" => {
                let list = |key: &str| -> Vec<String> {
                    params[key].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_string).collect()
                };
                let scope = si_core::scope::Scope { include: list("include"), exclude: list("exclude") };
                match self.session.set_scope(scope) {
                    Ok(up) => self.synced(up),
                    Err(e) => json!({ "error": format!("could not save {}: {e}", si_core::scope::Scope::config_path(&self.root).display()) }),
                }
            }
            "si/sync" => {
                let up = self.session.sync();
                self.synced(up)
            }
            "si/status" => json!({ "files": self.session.files(), "symbols": self.g().syms.len(), "edges": self.g().out.len(),
                                   "loaded": self.opened.0, "parsed": self.opened.1, "loading": self.loading,
                                   "cache": self.session.cache_dir().map(|d| d.to_string_lossy().into_owned()) }),
            "shutdown" => {
                // The graph as it is now, for the next start to begin with.
                // And whatever of a first index is not saved yet.
                if !self.loading {
                    while self.session.save_some(4096) {}
                    self.session.save_graph();
                }
                Value::Null
            }
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
                let changes = params["changes"].as_array().into_iter().flatten();
                trace(|| format!("  {}", changes.clone().take(5).map(|c| format!("{} {}", c["type"], c["uri"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ")));
                let paths = changes.filter_map(|c| c["uri"].as_str()).map(uri_to_path).collect();
                self.update(paths);
            }
            _ => {}
        }
    }
}

/// What the main loop waits for.
enum Event {
    Message(Value),
    /// The index, read or made.
    Opened(Box<(Session, si_core::session::Update)>),
    /// The editor is gone.
    Closed,
}

/// `early` gives what the last session left to answer with at once (see
/// `Session::early`), `open` the index itself, which is read or made on a
/// thread of its own while requests are already answered.
pub fn run(
    root: Option<&Path>,
    early: impl Fn(&Path) -> Option<Session>,
    open: impl Fn(&Path, Option<u64>) -> (Session, si_core::session::Update) + Send + Sync + 'static,
) {
    let open = std::sync::Arc::new(open);
    let mut out = std::io::stdout();
    // Indexing takes its time; a thread says how far it has got
    // (`si/progress`), and that it is over.
    std::thread::spawn(|| {
        use si_core::progress::{now, Phase};
        let mut last = (Phase::Idle, 0, 0);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let at = now();
            if at == last || (at.0 == Phase::Idle && last.0 == Phase::Idle) {
                continue;
            }
            last = at;
            let phase = match at.0 {
                Phase::Idle => "done",
                Phase::Parsing => "parsing",
                Phase::Linking => "linking",
            };
            let params = json!({ "phase": phase, "done": at.1, "total": at.2 });
            write_message(&mut std::io::stdout(), &json!({ "jsonrpc": "2.0", "method": "si/progress", "params": params }));
        }
    });
    let (events, inbox) = std::sync::mpsc::channel::<Event>();
    {
        let events = events.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            while let Some(msg) = read_message(&mut input) {
                if events.send(Event::Message(msg)).is_err() {
                    return;
                }
            }
            let _ = events.send(Event::Closed);
        });
    }
    let mut server: Option<Server> = None;
    trace(|| format!("started at {} ms since 1970", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())));
    let handle = |server: &mut Option<Server>, msg: Value, out: &mut std::io::Stdout| -> bool {
        let method = msg["method"].as_str().unwrap_or("").to_string();
        trace(|| format!("{method}{}", if server.as_ref().is_some_and(|s| s.loading && changes(&method)) { " (waits for the index)" } else { "" }));
        let (id, params) = (msg.get("id").cloned(), &msg["params"]);
        if method == "initialize" {
            let from_client = params["rootUri"].as_str().map(uri_to_path).or(params["rootPath"].as_str().map(PathBuf::from));
            let dir = root.map(Path::to_path_buf).or(from_client).unwrap_or_else(|| PathBuf::from("."));
            // What was saved answers at once; the index follows.
            let session = early(&dir).unwrap_or_else(|| Session::empty(&dir));
            let have = session.early_digest();
            let (open, events, at) = (open.clone(), events.clone(), dir.clone());
            std::thread::spawn(move || {
                let _ = events.send(Event::Opened(Box::new(open(&at, have))));
            });
            *server = Some(Server { session, opened: (0, 0), root: dir, open: HashMap::new(), lines: HashMap::new(), listing: [None, None], hidden: None, alongside: params["initializationOptions"]["alongside"].as_bool().unwrap_or(false), loading: true, waiting: Vec::new() });
            let mut capabilities = json!({
                "textDocumentSync": { "openClose": true, "change": 1, "save": true },
                "definitionProvider": true, "declarationProvider": true, "typeDefinitionProvider": true, "implementationProvider": true, "referencesProvider": true,
                "documentSymbolProvider": true, "workspaceSymbolProvider": true, "callHierarchyProvider": true,
                "hoverProvider": true, "completionProvider": { "resolveProvider": false, "triggerCharacters": [".", ">", "\"", "<", "/"] },
                "signatureHelpProvider": { "triggerCharacters": ["(", ","], "retriggerCharacters": [")"] },
                "documentLinkProvider": { "resolveProvider": false }, "documentHighlightProvider": true,
                "codeActionProvider": { "codeActionKinds": ["quickfix"] },
            });
            // What the C/C++ extension gives itself, where it is: not twice.
            if !params["initializationOptions"]["alongside"].as_bool().unwrap_or(false) {
                capabilities["foldingRangeProvider"] = json!(true);
                capabilities["semanticTokensProvider"] = json!({ "legend": { "tokenTypes": more::TOKEN_TYPES, "tokenModifiers": more::TOKEN_MODIFIERS }, "full": true });
                capabilities["renameProvider"] = json!({ "prepareProvider": true });
                capabilities["inlayHintProvider"] = json!(true);
            }
            let result = json!({ "capabilities": capabilities, "serverInfo": { "name": "norn", "version": env!("CARGO_PKG_VERSION") } });
            write_message(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            return true;
        }
        if method == "exit" {
            return false;
        }
        match (server, id) {
            (Some(s), _) if s.loading && changes(&method) => s.waiting.push(msg.clone()),
            (Some(s), Some(id)) => {
                let reply = match s.request(&method, params) {
                    Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    None => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{method} is not supported") } }),
                };
                write_message(out, &reply);
            }
            (Some(s), None) => s.notification(&method, params),
            (None, Some(id)) => {
                let error = json!({ "code": -32002, "message": "not initialized" });
                write_message(out, &json!({ "jsonrpc": "2.0", "id": id, "error": error }));
            }
            (None, None) => {}
        }
        true
    };
    loop {
        // With nothing asked, what is not saved yet is, a little at a time.
        let saving = server.as_ref().is_some_and(|s| !s.loading && s.session.unsaved());
        let event = if saving {
            match inbox.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if let Some(s) = &mut server {
                        s.session.save_some(256);
                    }
                    continue;
                }
                Err(_) => break,
            }
        } else {
            match inbox.recv() {
                Ok(event) => event,
                Err(_) => break,
            }
        };
        match event {
            Event::Closed => break,
            Event::Message(msg) => {
                if !handle(&mut server, msg, &mut out) {
                    break;
                }
            }
            Event::Opened(opened) => {
                let (mut session, up) = *opened;
                trace(|| format!("index ready: {} read from the save, {} parsed, graph {}", up.loaded, up.extracted, if up.graph_kept { "kept" } else { "read or linked" }));
                let Some(s) = &mut server else { continue };
                if up.graph_kept {
                    session.adopt(&mut s.session);
                }
                s.session = session;
                // (What stood in for it is gone: give its memory back.)
                si_core::trim_memory();
                s.opened = (up.loaded, up.extracted);
                s.loading = false;
                s.lines.clear();
                s.listing = [None, None];
                let ready = json!({ "files": s.session.files(), "symbols": s.g().syms.len(), "edges": s.g().out.len(), "loaded": up.loaded, "parsed": up.extracted });
                write_message(&mut out, &json!({ "jsonrpc": "2.0", "method": "si/ready", "params": ready }));
                // What was asked of the index while there was none.
                for msg in std::mem::take(&mut s.waiting) {
                    handle(&mut server, msg, &mut out);
                }
            }
        }
    }
}
