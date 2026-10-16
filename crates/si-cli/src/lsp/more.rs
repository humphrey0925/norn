//! What an editor has from a C language server besides finding things:
//! folding, colouring by what a name is, renaming, the names of a call's
//! parameters, and the `#include` a name wants.

use super::*;

/// A file's text with where its lines start and, per byte, whether it is
/// code (`CODE`), in a comment or in a string or character literal.
pub(super) struct Text {
    pub s: String,
    starts: Vec<usize>,
    pub mask: Vec<u8>,
}

pub(super) const CODE: u8 = 0;
const COMMENT: u8 = 1;
const STRING: u8 = 2;

impl Text {
    pub fn new(s: String) -> Text {
        let b = s.as_bytes();
        let mut starts = vec![0];
        starts.extend(b.iter().enumerate().filter(|(_, &c)| c == b'\n').map(|(i, _)| i + 1));
        let mut mask = vec![CODE; b.len()];
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    // (To the end of the line, a `\` at its end going on.)
                    let mut j = i;
                    while j < b.len() && !(b[j] == b'\n' && !(j > 0 && b[j - 1] == b'\\' || j > 1 && b[j - 1] == b'\r' && b[j - 2] == b'\\')) {
                        j += 1;
                    }
                    mask[i..j].fill(COMMENT);
                    i = j;
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    let j = s[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
                    mask[i..j].fill(COMMENT);
                    i = j;
                }
                q @ (b'"' | b'\'') => {
                    let mut j = i + 1;
                    while j < b.len() && b[j] != q && b[j] != b'\n' {
                        j += if b[j] == b'\\' { 2 } else { 1 };
                    }
                    let j = (j + 1).min(b.len());
                    mask[i..j].fill(STRING);
                    i = j;
                }
                _ => i += 1,
            }
        }
        Text { s, starts, mask }
    }

    pub fn lines(&self) -> usize {
        self.starts.len()
    }

    /// Line `l` (from 0) as bytes from..to, its line end left out.
    pub fn line(&self, l: usize) -> (usize, usize) {
        let from = self.starts[l];
        let mut to = self.starts.get(l + 1).map_or(self.s.len(), |&n| n - 1);
        if to > from && self.s.as_bytes()[to - 1] == b'\r' {
            to -= 1;
        }
        (from, to.max(from))
    }

    /// An offset as an LSP position: line from 0, UTF-16 column.
    pub fn pos(&self, at: usize) -> (usize, usize) {
        let l = self.starts.partition_point(|&s| s <= at) - 1;
        (l, self.s[self.starts[l]..at].encode_utf16().count())
    }

    pub fn range(&self, from: usize, to: usize) -> Value {
        let (a, b) = (self.pos(from), self.pos(to));
        json!({ "start": { "line": a.0, "character": a.1 }, "end": { "line": b.0, "character": b.1 } })
    }

    /// Where `name` is written in code in line `l` (from 0), whole.
    pub fn spellings(&self, l: usize, name: &str) -> Vec<usize> {
        let (from, to) = self.line(l);
        let b = self.s.as_bytes();
        self.s[from..to]
            .match_indices(name)
            .map(|(i, _)| from + i)
            .filter(|&i| self.mask[i] == CODE && (i == 0 || !is_ident(b[i - 1])) && b.get(i + name.len()).is_none_or(|&c| !is_ident(c)))
            .collect()
    }
}

/// The legend of the colouring: kinds of names, and what more is said of one.
pub(super) const TOKEN_TYPES: &[&str] = &["namespace", "type", "class", "enum", "struct", "parameter", "variable", "property", "enumMember", "function", "macro"];
pub(super) const TOKEN_MODIFIERS: &[&str] = &["declaration", "static", "readonly"];
const T_TYPE: u32 = 1;
const T_CLASS: u32 = 2;
const T_ENUM: u32 = 3;
const T_STRUCT: u32 = 4;
const T_PARAMETER: u32 = 5;
const T_VARIABLE: u32 = 6;
const T_PROPERTY: u32 = 7;
const T_ENUM_MEMBER: u32 = 8;
const T_FUNCTION: u32 = 9;
const T_MACRO: u32 = 10;
const M_DECLARATION: u32 = 1;
const M_STATIC: u32 = 2;

fn token_type(k: u8) -> Option<u32> {
    Some(match k {
        kind::FUNCTION => T_FUNCTION,
        kind::MACRO => T_MACRO,
        kind::STRUCT | kind::UNION => T_STRUCT,
        kind::ENUM => T_ENUM,
        kind::ENUM_CONST => T_ENUM_MEMBER,
        kind::TYPEDEF => T_TYPE,
        kind::GLOBAL => T_VARIABLE,
        kind::FIELD => T_PROPERTY,
        kind::CLASS => T_CLASS,
        _ => return None,
    })
}

/// The names of a declaration's parameters, from how it is written
/// (`int f(const char *s, int (*cb)(void), int)` gives `s`, `cb`, ``).
pub(super) fn parameter_names(written: &str, name: &str) -> Option<Vec<String>> {
    let one: String = written.split_whitespace().collect::<Vec<_>>().join(" ");
    let b = one.as_bytes();
    let at = one.match_indices(name).map(|m| m.0).find(|&i| (i == 0 || !is_ident(b[i - 1])) && b.get(i + name.len()).is_none_or(|&c| !is_ident(c)))?;
    let open = at + name.len() + one[at + name.len()..].find('(')?;
    if !one[at + name.len()..open].trim().is_empty() {
        return None;
    }
    let (mut depth, mut from, mut parts) = (0i32, open + 1, Vec::new());
    for (i, c) in one.bytes().enumerate().skip(open) {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    parts.push(&one[from..i]);
                    break;
                }
            }
            b',' if depth == 1 => {
                parts.push(&one[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    const NOT: &[&str] = &[
        "const", "volatile", "struct", "union", "enum", "unsigned", "signed", "register", "restrict", "__restrict", "int", "char", "short", "long", "float", "double",
        "void", "_Bool", "bool",
    ];
    let names = parts
        .iter()
        .map(|p| {
            let p = p.trim();
            // `int (*cb)(void)`: the name inside the first parentheses.
            let p = match (p.find("(*"), p.find('(')) {
                (Some(i), Some(j)) if i == j => &p[i + 2..p[i..].find(')').map_or(p.len(), |e| i + e)],
                _ => p.split('[').next().unwrap_or(p),
            };
            let words: Vec<&str> = p.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty() && !w.as_bytes()[0].is_ascii_digit()).collect();
            // (One word left is a type alone: no name.)
            match words.as_slice() {
                [only] if !p.contains('*') && NOT.contains(only) => String::new(),
                [_] if written.trim_start().starts_with("#define") => words[0].to_string(),
                [_] => String::new(),
                [.., last] if !NOT.contains(last) => last.to_string(),
                _ => String::new(),
            }
        })
        .collect();
    Some(names)
}

impl Server {
    /// The text of a file under the root: as open in the editor, else as saved.
    pub(super) fn text_of(&self, file: &str) -> Text {
        let path = self.root.join(file);
        Text::new(match self.open.get(&path) {
            Some(t) => t.clone(),
            None => String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()).into_owned(),
        })
    }

    fn file_of(&self, params: &Value) -> Option<String> {
        self.relative(&uri_to_path(params["textDocument"]["uri"].as_str()?))
    }

    /// `textDocument/foldingRange`: blocks in braces, `#if` branches,
    /// comments of more than a line, a run of `#include`s.
    pub(super) fn folding_ranges(&mut self, params: &Value) -> Value {
        let Some(file) = self.file_of(params) else { return json!([]) };
        let t = self.text_of(&file);
        let b = t.s.as_bytes();
        let mut out: Vec<Value> = Vec::new();
        let mut fold = |a: usize, e: usize, k: Option<&str>| {
            if e > a {
                let mut r = json!({ "startLine": a, "endLine": e });
                if let Some(k) = k {
                    r["kind"] = json!(k);
                }
                out.push(r);
            }
        };
        let mut braces: Vec<usize> = Vec::new();
        let mut ifs: Vec<usize> = Vec::new();
        let mut comments: Option<(usize, usize)> = None;
        let mut includes: Option<(usize, usize)> = None;
        for l in 0..t.lines() {
            let (from, to) = t.line(l);
            let line = &t.s[from..to];
            let trimmed = line.trim_start();
            // A run of `//` lines.
            let slashes = trimmed.starts_with("//");
            match (&mut comments, slashes) {
                (Some((_, e)), true) => *e = l,
                (None, true) => comments = Some((l, l)),
                (Some((a, e)), false) => {
                    fold(*a, *e, Some("comment"));
                    comments = None;
                }
                _ => {}
            }
            let include = trimmed.starts_with('#') && trimmed[1..].trim_start().starts_with("include");
            match (&mut includes, include) {
                (Some((_, e)), true) => *e = l,
                (None, true) => includes = Some((l, l)),
                (Some((a, e)), false) if !trimmed.is_empty() => {
                    fold(*a, *e, Some("imports"));
                    includes = None;
                }
                _ => {}
            }
            if trimmed.starts_with('#') && t.mask[from + (line.len() - trimmed.len())] == CODE {
                let word: String = trimmed[1..].trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
                match word.as_str() {
                    "if" | "ifdef" | "ifndef" => ifs.push(l),
                    "elif" | "else" | "elifdef" | "elifndef" => {
                        if let Some(a) = ifs.pop() {
                            fold(a, l - 1, Some("region"));
                        }
                        ifs.push(l);
                    }
                    "endif" => {
                        if let Some(a) = ifs.pop() {
                            fold(a, l - 1, Some("region"));
                        }
                    }
                    _ => {}
                }
            }
            for i in from..to {
                match (b[i], t.mask[i]) {
                    (b'{', CODE) => braces.push(l),
                    (b'}', CODE) => {
                        if let Some(a) = braces.pop() {
                            fold(a, l.saturating_sub(1), None);
                        }
                    }
                    // A comment of more lines: from its first to its last.
                    (b'/', COMMENT) if b.get(i + 1) == Some(&b'*') && (i == 0 || t.mask[i - 1] != COMMENT || b[i - 1] == b'/') => {
                        let end = t.s[i + 2..].find("*/").map_or(t.s.len(), |e| i + 2 + e);
                        fold(l, t.pos(end).0, Some("comment"));
                    }
                    _ => {}
                }
            }
        }
        if let Some((a, e)) = comments {
            fold(a, e, Some("comment"));
        }
        json!(out)
    }

    /// `textDocument/semanticTokens/full`: each name of the file by what it
    /// is: a macro, a function, a type, a variable of the file or of a
    /// function, a parameter, a member, an enumerator. Lines not compiled
    /// are left as they are.
    pub(super) fn semantic_tokens(&mut self, params: &Value) -> Value {
        let Some(file) = self.file_of(params) else { return json!({ "data": [] }) };
        let t = self.text_of(&file);
        let b = t.s.as_bytes();
        // What the file's facts say: lines off, its definitions by line,
        // the function each line is in, and their parameters and locals.
        let mut off = vec![false; t.lines() + 2];
        let mut here: HashMap<(u32, String), (u8, u8)> = HashMap::new();
        let mut scope_of = vec![u32::MAX; t.lines() + 2];
        let mut locals: HashMap<(u32, String), (bool, u32)> = HashMap::new();
        if let Some(f) = self.session.facts_of(&file) {
            for &(a, e) in &f.inactive {
                for l in a..=e.min(t.lines() as u32 + 1) {
                    off[l as usize] = true;
                }
            }
            for (i, d) in f.defs.iter().enumerate() {
                if d.kind == kind::FILE {
                    continue;
                }
                here.entry((d.line, f.name(d.name as usize).to_string())).or_insert((d.kind, d.flags));
                if d.kind == kind::FUNCTION {
                    // (GNU style: the name on the line after the type's.)
                    here.entry((d.line + 1, f.name(d.name as usize).to_string())).or_insert((d.kind, d.flags));
                    for l in d.line..=d.end_line.min(t.lines() as u32 + 1) {
                        scope_of[l as usize] = i as u32;
                    }
                }
            }
            for l in &f.locals {
                let e = locals.entry((l.scope, f.name(l.name as usize).to_string())).or_insert((l.param, l.line));
                e.1 = e.1.min(l.line);
                e.0 |= l.param;
            }
        }
        let hidden = self.hidden();
        let g = self.g();
        // Per name: the kinds the index has it as (a bit each), and
        // whether each function or variable of the name is static.
        let mut known: HashMap<&str, (u32, bool)> = HashMap::new();
        let mut data: Vec<u32> = Vec::new();
        let (mut last_line, mut last_col) = (0usize, 0usize);
        let mut define_params: Vec<String> = Vec::new();
        let mut continued = false;
        for l in 0..t.lines() {
            let (from, to) = t.line(l);
            let line = &t.s[from..to];
            let trimmed = line.trim_start();
            if !continued {
                define_params.clear();
            }
            let was_continued = continued;
            continued = line.ends_with('\\');
            if off[l + 1] {
                continue;
            }
            let mut start = from;
            // A directive: its word is no name; an `#include`'s operand
            // neither; a `#define`'s name is a macro's, its parameters
            // the body's.
            if !was_continued && trimmed.starts_with('#') {
                let after = &trimmed[1..];
                let word_at = from + (line.len() - trimmed.len()) + 1 + (after.len() - after.trim_start().len());
                let word: String = t.s[word_at..to].chars().take_while(|c| c.is_ascii_alphabetic() || *c == '_').collect();
                start = word_at + word.len();
                match word.as_str() {
                    "include" | "include_next" | "import" | "pragma" | "error" | "warning" | "line" => continue,
                    "define" => {
                        let rest = &t.s[start..to];
                        let name_at = start + (rest.len() - rest.trim_start().len());
                        let name: String = t.s[name_at..to].chars().take_while(|&c| c.is_ascii_alphanumeric() || c == '_').collect();
                        if !name.is_empty() {
                            push(&t, &mut data, &mut last_line, &mut last_col, name_at, name.len(), T_MACRO, M_DECLARATION);
                            start = name_at + name.len();
                            if b.get(start) == Some(&b'(') {
                                let close = t.s[start..to].find(')').map_or(to, |e| start + e);
                                define_params = t.s[start + 1..close].split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
                            }
                        }
                    }
                    _ => {}
                }
            }
            let mut i = start;
            let scope = scope_of[l + 1];
            while i < to {
                if !is_ident(b[i]) || t.mask[i] != CODE || (i > 0 && is_ident(b[i - 1])) {
                    i += 1;
                    continue;
                }
                let end = (i..to).find(|&j| !is_ident(b[j])).unwrap_or(to);
                let name = &t.s[i..end];
                let at = i;
                i = end;
                if name.as_bytes()[0].is_ascii_digit() || super::C_KEYWORDS.contains(&name) || name == "defined" {
                    continue;
                }
                if define_params.iter().any(|p| p == name) {
                    push(&t, &mut data, &mut last_line, &mut last_col, at, name.len(), T_PARAMETER, 0);
                    continue;
                }
                let before = t.s[from..at].trim_end();
                let member = before.ends_with("->") || before.ends_with('.') && !before.ends_with("..");
                // Declared here.
                if let Some(&(k, flags)) = here.get(&(l as u32 + 1, name.to_string())) {
                    if let Some(ty) = token_type(k) {
                        let m = M_DECLARATION | if flags & flag::STATIC != 0 && matches!(k, kind::FUNCTION | kind::GLOBAL) { M_STATIC } else { 0 };
                        push(&t, &mut data, &mut last_line, &mut last_col, at, name.len(), ty, m);
                        continue;
                    }
                }
                if member {
                    push(&t, &mut data, &mut last_line, &mut last_col, at, name.len(), T_PROPERTY, 0);
                    continue;
                }
                if scope != u32::MAX {
                    if let Some(&(param, decl)) = locals.get(&(scope, name.to_string())) {
                        if param || decl <= l as u32 + 1 {
                            let m = if decl == l as u32 + 1 && t.spellings(l, name).first() == Some(&at) { M_DECLARATION } else { 0 };
                            push(&t, &mut data, &mut last_line, &mut last_col, at, name.len(), if param { T_PARAMETER } else { T_VARIABLE }, m);
                            continue;
                        }
                    }
                }
                let (kinds, statics) = *known.entry(name).or_insert_with(|| {
                    let mut kinds = 0u32;
                    let mut statics = true;
                    for &s in g.find(name) {
                        let y = &g.syms[s as usize];
                        if hidden.get(y.file as usize).copied().unwrap_or(false) {
                            continue;
                        }
                        kinds |= 1 << y.kind;
                        if matches!(y.kind, kind::FUNCTION | kind::GLOBAL) {
                            statics &= y.flags & flag::STATIC != 0;
                        }
                    }
                    (kinds, statics)
                });
                if kinds == 0 {
                    continue;
                }
                let has = |k: u8| kinds & (1 << k) != 0;
                let called = t.s[end..to].trim_start().starts_with('(');
                let tagged = before.ends_with("struct") || before.ends_with("union");
                let ty = if has(kind::MACRO) {
                    T_MACRO
                } else if called && has(kind::FUNCTION) {
                    T_FUNCTION
                } else if tagged && (has(kind::STRUCT) || has(kind::UNION)) {
                    T_STRUCT
                } else if before.ends_with("enum") && has(kind::ENUM) {
                    T_ENUM
                } else if has(kind::ENUM_CONST) {
                    T_ENUM_MEMBER
                } else if has(kind::TYPEDEF) {
                    T_TYPE
                } else if has(kind::CLASS) {
                    T_CLASS
                } else if has(kind::GLOBAL) {
                    T_VARIABLE
                } else if has(kind::FUNCTION) {
                    T_FUNCTION
                } else if has(kind::STRUCT) || has(kind::UNION) {
                    T_STRUCT
                } else if has(kind::ENUM) {
                    T_ENUM
                } else {
                    continue;
                };
                let m = if statics && matches!(ty, T_FUNCTION | T_VARIABLE) { M_STATIC } else { 0 };
                push(&t, &mut data, &mut last_line, &mut last_col, at, name.len(), ty, m);
            }
        }
        json!({ "data": data })
    }

    /// The symbols a rename of a name changes with it: those of the name and
    /// kind that are the same thing (all of a macro's definitions; of a
    /// function or variable, the ones not static, or of the same file).
    fn same_thing(&self, targets: &[u32]) -> Vec<u32> {
        let g = self.g();
        let mut out: Vec<u32> = targets.to_vec();
        for &t in targets {
            let y = &g.syms[t as usize];
            if y.kind == kind::FIELD {
                continue;
            }
            for &s in g.find(g.sym_name(t)) {
                let z = &g.syms[s as usize];
                let linked = match z.kind {
                    kind::FUNCTION | kind::GLOBAL => (y.flags & flag::STATIC == 0 && z.flags & flag::STATIC == 0) || z.file == y.file,
                    _ => true,
                };
                if z.kind == y.kind && linked {
                    out.push(s);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// `textDocument/prepareRename`: the name under the cursor, if it is
    /// one the index knows.
    pub(super) fn prepare_rename(&mut self, params: &Value) -> Value {
        let Some((file, line, name)) = self.at(params) else { return Value::Null };
        if self.local_at(params).is_none() && self.targets(params).is_empty() {
            return Value::Null;
        }
        let t = self.text_of(&file);
        let col = params["position"]["character"].as_u64().unwrap_or(0) as usize;
        let found = t.spellings(line as usize - 1, &name).into_iter().find(|&i| {
            let (_, c) = t.pos(i);
            c <= col && col <= c + name.encode_utf16().count()
        });
        match found {
            Some(i) => json!({ "range": t.range(i, i + name.len()), "placeholder": name }),
            None => Value::Null,
        }
    }

    /// `textDocument/rename`: each spelling of the name where it means the
    /// same, by the index; and where the conditions switch lines off (not
    /// indexed), its spellings there too, for a name of more than one
    /// file: built another way, they are the same name.
    pub(super) fn rename(&mut self, params: &Value) -> Value {
        let new = params["newName"].as_str().unwrap_or("").to_string();
        if new.is_empty() || !new.bytes().all(is_ident) || new.as_bytes()[0].is_ascii_digit() || super::C_KEYWORDS.contains(&new.as_str()) {
            return Value::Null;
        }
        let Some((file, _, name)) = self.at(params) else { return Value::Null };
        let mut lines: HashMap<String, Vec<u32>> = HashMap::new();
        let mut wide = false;
        if let Some(h) = self.local_at(params) {
            let e = lines.entry(h.file.clone()).or_default();
            e.push(h.line);
            e.extend(h.uses.iter().copied());
            // (And in its function's lines switched off.)
            if let Some(f) = self.session.facts_of(&h.file) {
                let end = f.defs.iter().find(|d| d.line == h.function_line && d.kind == kind::FUNCTION).map_or(h.function_line, |d| d.end_line);
                for &(a, b) in &f.inactive {
                    if b >= h.function_line && a <= end {
                        e.extend(a.max(h.function_line)..=b.min(end));
                    }
                }
            }
        } else {
            let targets = self.targets(params);
            if targets.is_empty() {
                return Value::Null;
            }
            let all = self.same_thing(&targets);
            let g = self.g();
            for &s in &all {
                let y = &g.syms[s as usize];
                lines.entry(g.file_path(y.file).to_string()).or_default().push(y.line);
                // (GNU style: the name on the line after its type.)
                if y.kind == kind::FUNCTION {
                    lines.entry(g.file_path(y.file).to_string()).or_default().push(y.line + 1);
                }
                for e in g.refs_to(s) {
                    lines.entry(g.file_path(g.syms[e.other as usize].file).to_string()).or_default().push(e.line());
                }
                wide |= !(matches!(y.kind, kind::FUNCTION | kind::GLOBAL) && y.flags & flag::STATIC != 0) && y.kind != kind::FIELD;
            }
        }
        // Where lines are switched off: of the files where it is, those
        // lines; for a name of more than one file, of every file.
        let files: Vec<String> = if wide {
            let g = self.g();
            (0..g.files.len() as u32).map(|f| g.file_path(f).to_string()).collect()
        } else {
            lines.keys().cloned().collect()
        };
        for f in files {
            let Some(facts) = self.session.facts_of(&f) else { continue };
            if facts.inactive.is_empty() {
                continue;
            }
            let ranges = facts.inactive.clone();
            if !lines.contains_key(&f) {
                let text = std::fs::read(self.root.join(&f)).unwrap_or_default();
                if !text.windows(name.len()).any(|w| w == name.as_bytes()) {
                    continue;
                }
            }
            let e = lines.entry(f).or_default();
            for (a, b) in ranges {
                e.extend(a..=b);
            }
        }
        let _ = file;
        let mut changes = serde_json::Map::new();
        let mut keys: Vec<String> = lines.keys().cloned().collect();
        keys.sort();
        for f in keys {
            let mut ls = lines.remove(&f).unwrap_or_default();
            ls.sort_unstable();
            ls.dedup();
            let t = self.text_of(&f);
            let mut edits = Vec::new();
            for l in ls {
                if l == 0 || l as usize > t.lines() {
                    continue;
                }
                for i in t.spellings(l as usize - 1, &name) {
                    edits.push(json!({ "range": t.range(i, i + name.len()), "newText": new }));
                }
            }
            if !edits.is_empty() {
                changes.insert(path_to_uri(&self.root.join(&f)), json!(edits));
            }
        }
        json!({ "changes": changes })
    }

    /// `textDocument/inlayHint`: before each argument of a call, the name
    /// of the parameter it is (not where the argument says it already).
    pub(super) fn inlay_hints(&mut self, params: &Value) -> Value {
        let Some(file) = self.file_of(params) else { return json!([]) };
        let first = params["range"]["start"]["line"].as_u64().unwrap_or(0) as u32 + 1;
        let last = params["range"]["end"]["line"].as_u64().unwrap_or(u32::MAX as u64) as u32 + 1;
        let calls: Vec<(u32, String)> = match self.session.facts_of(&file) {
            Some(f) => {
                let mut c: Vec<(u32, String)> = f
                    .refs
                    .iter()
                    .filter(|r| r.kind & !refkind::MAYBE == refkind::CALL && r.line >= first && r.line <= last)
                    .map(|r| (r.line, f.name(r.name as usize).to_string()))
                    .collect();
                c.sort();
                c.dedup();
                c
            }
            None => return json!([]),
        };
        let t = self.text_of(&file);
        let b = t.s.as_bytes();
        let mut names: HashMap<String, Option<Vec<String>>> = HashMap::new();
        let mut out = Vec::new();
        for (line, name) in calls {
            if line as usize > t.lines() {
                continue;
            }
            if !names.contains_key(&name) {
                let found: Vec<u32> = {
                    let g = self.g();
                    let mut f = si_core::definition_at(g, &file, line, &name);
                    // (A macro's arguments are no parameters'.)
                    f.retain(|&s| g.syms[s as usize].kind == kind::FUNCTION);
                    f.sort_by_key(|&s| (g.syms[s as usize].flags & flag::DECL != 0, s));
                    f
                };
                // (Where its declarations differ in the names: none.)
                let mut agreed: Option<Vec<String>> = None;
                for s in found.into_iter().take(4) {
                    let (w, _) = self.written(s);
                    let Some(p) = parameter_names(&w, &name) else { continue };
                    agreed = match agreed {
                        None => Some(p),
                        Some(a) if a.len() == p.len() => Some(a.iter().zip(&p).map(|(x, y)| if x == y || y.is_empty() { x.clone() } else if x.is_empty() { y.clone() } else { String::new() }).collect()),
                        Some(_) => Some(Vec::new()),
                    };
                }
                names.insert(name.clone(), agreed);
            }
            let Some(Some(pnames)) = names.get(&name) else { continue };
            if pnames.iter().all(String::is_empty) {
                continue;
            }
            for at in t.spellings(line as usize - 1, &name) {
                let rest = &t.s[at + name.len()..];
                let open = at + name.len() + (rest.len() - rest.trim_start().len());
                if b.get(open) != Some(&b'(') {
                    continue;
                }
                // The arguments: between the commas at the top.
                let (mut depth, mut from, mut args) = (0i32, open + 1, Vec::new());
                let mut i = open;
                while i < b.len() && i < open + 4000 {
                    if t.mask[i] == CODE {
                        match b[i] {
                            b'(' | b'[' | b'{' => depth += 1,
                            b')' | b']' | b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    args.push((from, i));
                                    break;
                                }
                            }
                            b',' if depth == 1 => {
                                args.push((from, i));
                                from = i + 1;
                            }
                            _ => {}
                        }
                    }
                    i += 1;
                }
                for (n, &(a, e)) in args.iter().enumerate() {
                    let Some(p) = pnames.get(n).filter(|p| !p.is_empty()) else { continue };
                    let arg = t.s[a..e].trim();
                    let start = a + (t.s[a..e].len() - t.s[a..e].trim_start().len());
                    // (Comments before it are passed over.)
                    let mut start = start;
                    while start < e && (t.mask[start] != CODE || b[start].is_ascii_whitespace()) {
                        start += 1;
                    }
                    if arg.is_empty() || start >= e {
                        continue;
                    }
                    let said: String = arg.chars().rev().take_while(|&c| c.is_ascii_alphanumeric() || c == '_').collect::<String>().chars().rev().collect();
                    if said.eq_ignore_ascii_case(p) {
                        continue;
                    }
                    let (pl, pc) = t.pos(start);
                    out.push(json!({ "position": { "line": pl, "character": pc }, "label": format!("{p}:"), "kind": 2, "paddingRight": true }));
                }
            }
        }
        json!(out)
    }

    /// `textDocument/codeAction`: for a name the file reads no declaration
    /// of, which a header of the index has: the `#include` of that header.
    pub(super) fn code_actions(&mut self, params: &Value) -> Value {
        let at = json!({ "textDocument": params["textDocument"], "position": params["range"]["start"] });
        let Some((file, _, name)) = self.at(&at) else { return json!([]) };
        if super::C_KEYWORDS.contains(&name.as_str()) || self.local_at(&at).is_some() {
            return json!([]);
        }
        let headers = ["h", "hpp", "hh", "hxx", "inl"];
        let is_header = |p: &str| p.rsplit_once('.').is_some_and(|(_, x)| headers.contains(&x.to_ascii_lowercase().as_str()));
        let hidden = self.hidden();
        let (mut have, mut offered): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        {
            let g = self.g();
            for &s in g.find(&name) {
                let y = &g.syms[s as usize];
                if !matches!(y.kind, kind::FUNCTION | kind::MACRO | kind::TYPEDEF | kind::STRUCT | kind::UNION | kind::ENUM | kind::ENUM_CONST | kind::GLOBAL | kind::CLASS) {
                    continue;
                }
                let p = g.file_path(y.file).to_string();
                if p == file {
                    return json!([]);
                }
                if is_header(&p) && !(matches!(y.kind, kind::FUNCTION | kind::GLOBAL) && y.flags & flag::STATIC != 0) {
                    if hidden[y.file as usize] {
                        have.push(p);
                    } else {
                        offered.push(p);
                    }
                }
            }
        }
        if offered.is_empty() {
            offered = have;
        }
        offered.sort();
        offered.dedup();
        if offered.is_empty() {
            return json!([]);
        }
        // What the file reads already, `#include` by `#include`.
        let mut read: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue = vec![file.clone()];
        while let Some(f) = queue.pop() {
            if read.len() > 4000 {
                break;
            }
            let operands: Vec<String> = match self.session.facts_of(&f) {
                Some(facts) => facts.includes.iter().map(|&i| facts.name(i as usize).to_string()).collect(),
                None => continue,
            };
            let ops: Vec<&str> = operands.iter().map(String::as_str).collect();
            for meant in si_core::includes_meant(self.g(), &f, &ops) {
                for m in meant {
                    if read.insert(m.clone()) {
                        queue.push(m);
                    }
                }
            }
        }
        if offered.iter().any(|h| read.contains(h)) {
            return json!([]);
        }
        let t = self.text_of(&file);
        let last_include = (0..t.lines()).rev().find(|&l| {
            let (a, e) = t.line(l);
            let s = t.s[a..e].trim_start();
            s.starts_with('#') && s[1..].trim_start().starts_with("include")
        });
        let insert = last_include.map_or(0, |l| l + 1);
        let mut actions = Vec::new();
        for h in offered.iter().take(3) {
            // As the project's other files write it, most often; else its
            // path from here, if it is under this file's folder; else the
            // shortest end of its path the index finds it by.
            let base = h.rsplit('/').next().unwrap_or(h);
            let mut written: HashMap<String, usize> = HashMap::new();
            let all: Vec<String> = {
                let g = self.g();
                (0..g.files.len() as u32).map(|f| g.file_path(f).to_string()).collect()
            };
            for f in &all {
                let Some(facts) = self.session.facts_of(f) else { continue };
                let ops: Vec<String> = facts.includes.iter().map(|&i| facts.name(i as usize).to_string()).filter(|o| o.rsplit('/').next() == Some(base)).collect();
                if ops.is_empty() {
                    continue;
                }
                let refs: Vec<&str> = ops.iter().map(String::as_str).collect();
                for (o, meant) in ops.iter().zip(si_core::includes_meant(self.g(), f, &refs)) {
                    if meant.first() == Some(h) {
                        *written.entry(o.clone()).or_default() += 1;
                    }
                }
            }
            let finds = |e: &str| si_core::includes_meant(self.g(), &file, &[e]).first().and_then(|m| m.first()) == Some(h);
            let dir = file.rsplit_once('/').map_or("", |x| x.0);
            let mut usual: Vec<(usize, String)> = written.into_iter().map(|(o, n)| (n, o)).collect();
            usual.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let from_here = if dir.is_empty() { Some(h.as_str()) } else { h.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) };
            let mut ends: Vec<&str> = std::iter::once(h.as_str()).chain(h.match_indices('/').map(|(i, _)| &h[i + 1..])).collect();
            ends.reverse();
            let operand = usual
                .into_iter()
                .map(|u| u.1)
                .find(|o| finds(o))
                .or_else(|| from_here.filter(|o| finds(o)).map(str::to_string))
                .or_else(|| ends.into_iter().find(|e| finds(e)).map(str::to_string));
            let Some(operand) = operand else { continue };
            actions.push(json!({
                "title": format!("Add #include \"{operand}\""),
                "kind": "quickfix",
                "edit": { "changes": { path_to_uri(&self.root.join(&file)): [{
                    "range": { "start": { "line": insert, "character": 0 }, "end": { "line": insert, "character": 0 } },
                    "newText": format!("#include \"{operand}\"\n") }] } }
            }));
        }
        json!(actions)
    }
}

/// A token of the colouring, as LSP wants it: each from the last.
#[allow(clippy::too_many_arguments)]
fn push(t: &Text, data: &mut Vec<u32>, last_line: &mut usize, last_col: &mut usize, at: usize, len: usize, ty: u32, m: u32) {
    let (l, c) = t.pos(at);
    let dl = l - *last_line;
    let dc = if dl == 0 { c - *last_col } else { c };
    data.extend([dl as u32, dc as u32, t.s[at..at + len].encode_utf16().count() as u32, ty, m]);
    *last_line = l;
    *last_col = c;
}
