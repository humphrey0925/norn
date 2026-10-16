//! What a macro or an enumerator stands for, worked out from the sources
//! the way an `#if` is: `#define BUF_BASE (0x1000)`, `#define BUF_Q (BUF_BASE
//! + 4 * 2)` is 0x1008. The macros a body names are read where the index
//! has them: what the settings and the build define first, then, of
//! several definitions, the one nearest the macro that names it (in its
//! file, else its directory, else the nearest directory); several no one
//! of which is nearer leave the value unknown. Casts to a type (`(U32)0x10`) are passed
//! over, as an `#if` would not take them. An enumerator is what it is set
//! to, or one more than the one before it.

use crate::cond::{Conditions, Local};
use crate::graph::Graph;
use crate::model::{flag, kind};
use rustc_hash::{FxHashMap, FxHashSet};

/// At most this many names are read for one value.
const MOST_NAMES: usize = 400;

/// A `#define` as written: its name, its parameters (none: object-like)
/// and its body, comments out and lines joined.
#[derive(Clone, Debug, PartialEq)]
pub struct Define {
    pub name: String,
    pub params: Option<Vec<String>>,
    pub body: String,
}

/// The `#define` on `line` (from 1) of `src`.
pub fn define_at(src: &[u8], line: u32) -> Option<Define> {
    let mut at = 0;
    for _ in 1..line {
        at += src[at..].iter().position(|&b| b == b'\n')? + 1;
    }
    // The directive, its lines joined, comments out.
    let mut text = Vec::new();
    let mut i = at;
    while i < src.len() {
        match src[i] {
            b'\\' if src.get(i + 1) == Some(&b'\n') => i += 2,
            b'\\' if src.get(i + 1) == Some(&b'\r') && src.get(i + 2) == Some(&b'\n') => i += 3,
            b'\n' => break,
            b'/' if src.get(i + 1) == Some(&b'/') => {
                while i < src.len() && src[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < src.len() && !(src[i] == b'*' && src[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                text.push(b' ');
            }
            b'\r' => i += 1,
            c => {
                text.push(c);
                i += 1;
            }
        }
    }
    let t = String::from_utf8_lossy(&text).into_owned();
    let rest = t.trim_start().strip_prefix('#')?.trim_start().strip_prefix("define")?;
    let rest = rest.trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
    let (name, rest) = rest.split_at(end);
    if name.is_empty() {
        return None;
    }
    let (params, body) = match rest.strip_prefix('(') {
        Some(r) => {
            let close = r.find(')')?;
            let params = r[..close].split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
            (Some(params), &r[close + 1..])
        }
        None => (None, rest),
    };
    Some(Define { name: name.to_string(), params, body: body.trim().to_string() })
}

/// The names a text holds, in order, each once.
fn names(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            if !out.contains(&&text[s..i]) {
                out.push(&text[s..i]);
            }
        } else if b[i].is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Words that name a type in C.
fn type_word(w: &str) -> bool {
    matches!(w, "unsigned" | "signed" | "int" | "long" | "short" | "char" | "const" | "volatile" | "bool" | "_Bool" | "size_t" | "ptrdiff_t" | "struct" | "union" | "enum")
        || w.strip_suffix("_t").is_some_and(|r| r.strip_prefix("int").or(r.strip_prefix("uint")).is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())))
}

/// `text` with its casts to a type taken out: `((U32)0x10 << 2)` reads
/// `((0x10) << 2)`... as `(0x10 << 2)`.
fn without_casts(text: &str, is_type: &mut dyn FnMut(&str) -> bool) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'(' {
            if let Some(close) = text[i + 1..].find(')').map(|c| i + 1 + c) {
                let inner = &text[i + 1..close];
                let words: Vec<&str> = inner.split(|c: char| c.is_whitespace() || c == '*').filter(|w| !w.is_empty()).collect();
                let plain = inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '*' || c.is_whitespace());
                let next = text[close + 1..].trim_start().chars().next();
                let operand = next.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '(' | '~' | '-' | '!' | '\''));
                if plain && operand && !words.is_empty() && words.iter().all(|w| type_word(w) || is_type(w)) {
                    out.push(' ');
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// A value worked out: the number, and whether it is unsigned.
#[derive(Debug, PartialEq)]
pub struct Value {
    pub number: i64,
    pub unsigned: bool,
}

/// Works out values from the index and the sources, which `source` reads
/// (a path as the index has it).
pub struct Values<'a> {
    g: &'a Graph,
    conditions: &'a Conditions,
    source: &'a mut dyn FnMut(&str) -> Option<std::sync::Arc<Vec<u8>>>,
    local: Local,
    seen: FxHashSet<String>,
    /// Names known to name a type, or not.
    types: FxHashMap<String, bool>,
    /// Per file, whether no compilation reads it: its definitions only
    /// where there are no others.
    unread: Vec<bool>,
    /// Where the value is asked for (as the conditions' sources number
    /// files): a name the compilations settle there has that value.
    at: Option<(u32, u32)>,
    /// The names whose value was not had, and why.
    pub missing: Vec<(String, String)>,
    /// The definition read for each name, as written (parameters, body).
    written: FxHashMap<String, (Option<Vec<String>>, String)>,
    enums: FxHashMap<u32, Option<i64>>,
}

impl<'a> Values<'a> {
    pub fn new(g: &'a Graph, conditions: &'a Conditions, source: &'a mut dyn FnMut(&str) -> Option<std::sync::Arc<Vec<u8>>>) -> Values<'a> {
        Values { g, conditions, source, local: Local::default(), seen: FxHashSet::default(), types: FxHashMap::default(), unread: Vec::new(), at: None, missing: Vec::new(), written: FxHashMap::default(), enums: FxHashMap::default() }
    }

    /// The same, asked on `line` of the file at `path`.
    pub fn asked_at(mut self, path: &str, line: u32) -> Values<'a> {
        self.at = self.conditions.source().and_then(|s| s.file(path)).map(|f| (f, line));
        self
    }

    /// The same, the files no compilation reads (by path) read last.
    pub fn without(mut self, unread: &[String]) -> Values<'a> {
        let set: FxHashSet<&str> = unread.iter().map(String::as_str).collect();
        self.unread = (0..self.g.files.len() as u32).map(|f| set.contains(self.g.file_path(f))).collect();
        self
    }

    /// The `#define` of macro `sym`.
    pub fn define(&mut self, sym: u32) -> Option<Define> {
        let s = &self.g.syms[sym as usize];
        let src = (self.source)(self.g.file_path(s.file))?;
        define_at(&src, s.line).filter(|d| d.name == self.g.sym_name(sym))
    }

    /// Whether `w` names a type: a typedef, a struct, a union or an enum,
    /// or a macro that stands for one (`#define U32 unsigned long`).
    fn is_type(&mut self, w: &str) -> bool {
        self.type_name(w, 0)
    }

    fn type_name(&mut self, w: &str, depth: u32) -> bool {
        if type_word(w) {
            return true;
        }
        if let Some(&t) = self.types.get(w) {
            return t;
        }
        let g = self.g;
        let syms: Vec<u32> = g.find(w).to_vec();
        let mut t = !syms.is_empty() && syms.iter().all(|&s| matches!(g.syms[s as usize].kind, kind::TYPEDEF | kind::STRUCT | kind::UNION | kind::ENUM | kind::CLASS));
        if !t && depth < 8 {
            // A macro every definition of which is words that name types.
            self.types.insert(w.to_string(), false);
            let macros: Vec<u32> = syms.iter().copied().filter(|&s| g.syms[s as usize].kind == kind::MACRO && g.syms[s as usize].flags & flag::DECL == 0).collect();
            let bodies: Vec<Define> = macros.iter().filter_map(|&s| self.define(s)).collect();
            let plain = |d: &Define| d.params.is_none() && !d.body.is_empty() && d.body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '*' || c.is_whitespace());
            t = !bodies.is_empty() && bodies.iter().all(plain);
            if t {
                let words: Vec<String> = bodies.iter().flat_map(|d| d.body.split(|c: char| c.is_whitespace() || c == '*').filter(|w| !w.is_empty()).map(str::to_string).collect::<Vec<_>>()).collect();
                t = !words.is_empty() && words.iter().all(|w| !w.as_bytes()[0].is_ascii_digit() && self.type_name(w, depth + 1));
            }
        }
        self.types.insert(w.to_string(), t);
        t
    }

    fn plain(&mut self, text: &str) -> String {
        // (Which of its words name types first, then the text without
        // the casts to them.)
        let mut known = FxHashMap::default();
        for w in names(text) {
            let t = self.is_type(w);
            known.insert(w.to_string(), t);
        }
        without_casts(text, &mut |w| known.get(w).copied().unwrap_or(false))
    }

    /// Gives `name` its value among the names, and those its value names;
    /// `from` is the file of what names it.
    fn learn(&mut self, name: &str, from: u32) {
        if self.seen.len() >= MOST_NAMES || self.conditions.settles(name.as_bytes()) || !self.seen.insert(name.to_string()) || self.is_type(name) {
            return;
        }
        // Known where it is asked for: as the compilations have it there.
        if let (Some((f, line)), Some(source)) = (self.at, self.conditions.source()) {
            if let Some(v) = source.get(f, line, name.as_bytes()) {
                let v = String::from_utf8_lossy(v).into_owned();
                self.written.insert(name.to_string(), (None, v.clone()));
                for n in names(&v).into_iter().map(str::to_string).collect::<Vec<_>>() {
                    self.learn(&n, from);
                }
                return;
            }
        }
        let g = self.g;
        let syms: Vec<u32> = g.find(name).to_vec();
        let macros: Vec<u32> = syms.iter().copied().filter(|&s| g.syms[s as usize].kind == kind::MACRO && g.syms[s as usize].flags & flag::DECL == 0).collect();
        if macros.is_empty() {
            // An enumerator: its number, as a value.
            match syms.iter().find(|&&s| g.syms[s as usize].kind == kind::ENUM_CONST) {
                Some(&e) => match self.enumerator(e) {
                    Some(n) => {
                        self.local.insert(name.as_bytes().to_vec(), Some(Some(n.to_string().into_bytes())));
                        self.written.insert(name.to_string(), (None, n.to_string()));
                    }
                    None => self.missing.push((name.to_string(), "an enumerator whose number is not worked out".into())),
                },
                None if syms.is_empty() => self.missing.push((name.to_string(), "no definition in the index".into())),
                None => self.missing.push((name.to_string(), "not a macro or an enumerator".into())),
            }
            return;
        }
        // One definition, or several written alike; those certainly
        // compiled first.
        let read = |s: &u32| !self.unread.get(g.syms[*s as usize].file as usize).copied().unwrap_or(false);
        let macros: Vec<u32> = if macros.iter().any(read) { macros.into_iter().filter(read).collect() } else { macros };
        let sure: Vec<u32> = macros.iter().copied().filter(|&s| g.syms[s as usize].flags & flag::CONDITIONAL == 0).collect();
        let pick = if sure.is_empty() { macros } else { sure };
        let mut defs: Vec<(u32, Define)> = pick.iter().filter_map(|&s| Some((g.syms[s as usize].file, self.define(s)?))).collect();
        defs.sort_by(|a, b| a.1.body.cmp(&b.1.body));
        defs.dedup_by(|a, b| a.1 == b.1);
        if defs.len() > 1 {
            // The nearest: in the same file, else the same directory, else
            // the one whose directory shares most with this one's.
            let dir_of = |f: u32| -> Vec<&str> {
                let p = g.file_path(f);
                p.rsplit_once('/').map_or(Vec::new(), |(d, _)| d.split('/').collect())
            };
            let here = dir_of(from);
            let near = |f: u32| -> usize {
                if f == from {
                    return usize::MAX;
                }
                let d = dir_of(f);
                let common = d.iter().zip(&here).take_while(|(a, b)| a == b).count();
                if common == d.len() && common == here.len() { usize::MAX - 1 } else { common }
            };
            let best = defs.iter().map(|d| near(d.0)).max().unwrap_or(0);
            defs.retain(|d| near(d.0) == best);
        }
        let [(at, d)] = &defs[..] else {
            let places: Vec<String> = defs.iter().take(3).map(|d| format!("{} = {}", g.file_path(d.0), d.1.body)).collect();
            self.missing.push((name.to_string(), format!("{} definitions, none nearer than the others: {}", defs.len(), places.join("; "))));
            return;
        };
        let (at, d) = (*at, d.clone());
        self.written.insert(name.to_string(), (d.params.clone(), d.body.clone()));
        let body = self.plain(&d.body);
        let params: Vec<String> = d.params.clone().unwrap_or_default();
        let value = match &d.params {
            None => crate::cond::value_of(body.as_bytes()),
            Some(p) => crate::cond::function_value(&p.iter().map(|p| p.as_bytes()).collect::<Vec<_>>(), body.as_bytes()),
        };
        let Some(value) = value else {
            self.missing.push((name.to_string(), "its body is not a value".into()));
            return;
        };
        self.local.insert(name.as_bytes().to_vec(), Some(Some(value)));
        for n in names(&body).into_iter().filter(|n| !params.iter().any(|p| p == n)).map(str::to_string).collect::<Vec<_>>() {
            self.learn(&n, at);
        }
    }

    /// The value of an expression written in file `at`, its names as the
    /// index has them.
    pub fn of(&mut self, expr: &str, at: u32) -> Option<Value> {
        let expr = self.plain(expr);
        for n in names(&expr).into_iter().map(str::to_string).collect::<Vec<_>>() {
            self.learn(&n, at);
        }
        let (number, unsigned) = self.conditions.number_in(&self.local, self.at, expr.as_bytes())?;
        Some(Value { number, unsigned })
    }

    /// The value of macro `sym`, when it takes no arguments.
    pub fn of_macro(&mut self, sym: u32) -> Option<Value> {
        let d = self.define(sym)?;
        if d.params.is_some() || d.body.is_empty() {
            return None;
        }
        let at = self.g.syms[sym as usize].file;
        self.of(&d.body, at)
    }

    /// The number of enumerator `sym`.
    pub fn enumerator(&mut self, sym: u32) -> Option<i64> {
        if let Some(&n) = self.enums.get(&sym) {
            return n;
        }
        self.enums.insert(sym, None);
        let g = self.g;
        let s = &g.syms[sym as usize];
        let name = g.sym_name(sym).to_string();
        let src = (self.source)(g.file_path(s.file))?;
        // From the line it is on back to the brace that opens the enum.
        let mut at = 0;
        for _ in 1..s.line {
            at += src[at..].iter().position(|&b| b == b'\n')? + 1;
        }
        let line_end = at + src[at..].iter().position(|&b| b == b'\n').unwrap_or(src.len() - at);
        let open = src[..line_end].iter().rposition(|&b| b == b'{')?;
        let text = String::from_utf8_lossy(&src[open + 1..line_end]).into_owned();
        let text = strip_comments(&text);
        let text = text.split('}').next().unwrap_or("");
        let mut next: i64 = 0;
        let mut found = None;
        for item in text.split(',') {
            let (n, v) = match item.split_once('=') {
                Some((n, v)) => (n.trim(), Some(v.trim())),
                None => (item.trim(), None),
            };
            if n.is_empty() {
                continue;
            }
            let value = match v {
                Some(v) => self.of(v, s.file)?.number,
                None => next,
            };
            if n == name {
                found = Some(value);
                break;
            }
            next = value + 1;
        }
        self.enums.insert(sym, found);
        found
    }
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i += 2;
            while i + 1 < b.len() && !b[i..].starts_with(b"*/") {
                i += 1;
            }
            i += 2;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// A token of an expression as written: a name, a number, a literal or
/// a punctuator.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Name(String),
    Num(String),
    Lit(String),
    Punct(&'static str),
}

const PUNCTS: &[&str] = &[
    "<<=", ">>=", "...", "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "##", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "+", "-", "*", "/",
    "%", "&", "|", "^", "~", "!", "<", ">", "=", "?", ":", ",", ".", "(", ")", "[", "]", "{", "}", "#", ";",
];

fn tokens_of(text: &str) -> Option<Vec<Tok>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Name(text[s..i].to_string()));
        } else if c.is_ascii_digit() || c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.' || (matches!(b[i], b'+' | b'-') && matches!(b[i - 1], b'e' | b'E' | b'p' | b'P'))) {
                i += 1;
            }
            out.push(Tok::Num(text[s..i].to_string()));
        } else if c == b'"' || c == b'\'' {
            let s = i;
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            out.push(Tok::Lit(text[s..i].to_string()));
        } else {
            let p = PUNCTS.iter().find(|p| text[i..].starts_with(**p))?;
            out.push(Tok::Punct(p));
            i += p.len();
        }
    }
    Some(out)
}

fn text_of(toks: &[Tok]) -> String {
    let mut out = String::new();
    for t in toks {
        let s = match t {
            Tok::Name(s) | Tok::Num(s) | Tok::Lit(s) => s.as_str(),
            Tok::Punct(p) => p,
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(s);
    }
    out
}

/// An integer literal: its value, and whether it is unsigned.
fn number(s: &str) -> Option<(i64, bool)> {
    let lower = s.to_ascii_lowercase();
    let digits = lower.trim_end_matches(['u', 'l']);
    let unsigned = lower[digits.len()..].contains('u');
    let v = if let Some(h) = digits.strip_prefix("0x") {
        u64::from_str_radix(h, 16).ok()?
    } else if let Some(bits) = digits.strip_prefix("0b") {
        u64::from_str_radix(bits, 2).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        u64::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse::<u64>().ok()?
    };
    Some((v as i64, unsigned || v > i64::MAX as u64))
}

/// An expression, what of it is known worked out.
#[derive(Clone, Debug)]
enum Node {
    Num(i64, bool),
    /// What is not a number: a name, a literal (and what follows it, as
    /// `a.b[2]`, `f(x)`), kept as written.
    Text(String),
    Unary(&'static str, Box<Node>),
    Binary(&'static str, Box<Node>, Box<Node>),
    Ternary(Box<Node>, Box<Node>, Box<Node>),
    /// `(type) operand`, the type as written.
    Cast(String, Box<Node>),
    Paren(Box<Node>),
}

fn binary_rank(op: &str) -> Option<u8> {
    Some(match op {
        "*" | "/" | "%" => 10,
        "+" | "-" => 9,
        "<<" | ">>" => 8,
        "<" | ">" | "<=" | ">=" => 7,
        "==" | "!=" => 6,
        "&" => 5,
        "^" => 4,
        "|" => 3,
        "&&" => 2,
        "||" => 1,
        _ => return None,
    })
}

struct Folder<'t, 'v, 'a> {
    toks: &'t [Tok],
    at: usize,
    values: &'v mut Values<'a>,
}

impl Folder<'_, '_, '_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn punct(&self, p: &str) -> bool {
        matches!(self.peek(), Some(Tok::Punct(q)) if *q == p)
    }

    fn expr(&mut self) -> Option<Node> {
        let c = self.binary(1)?;
        if self.punct("?") {
            self.at += 1;
            let a = self.expr()?;
            if !self.punct(":") {
                return None;
            }
            self.at += 1;
            let b = self.expr()?;
            return Some(Node::Ternary(Box::new(c), Box::new(a), Box::new(b)));
        }
        Some(c)
    }

    fn binary(&mut self, min: u8) -> Option<Node> {
        let mut left = self.unary()?;
        loop {
            let Some(Tok::Punct(op)) = self.peek() else { break };
            let op: &'static str = op;
            let Some(rank) = binary_rank(op).filter(|&r| r >= min) else { break };
            self.at += 1;
            let right = self.binary(rank + 1)?;
            left = Node::Binary(op, Box::new(left), Box::new(right));
        }
        Some(left)
    }

    /// Whether the tokens from `at` are a type in parentheses: `(U32)`,
    /// `(void *)`, `(volatile unsigned long *)`.
    fn cast_at(&mut self, at: usize) -> Option<usize> {
        let mut i = at + 1;
        let mut words = 0;
        while let Some(t) = self.toks.get(i) {
            match t {
                Tok::Name(n) if n == "void" || self.values.is_type(n) => words += 1,
                Tok::Punct("*") => {}
                Tok::Punct(")") if words > 0 => return Some(i),
                _ => return None,
            }
            i += 1;
        }
        None
    }

    fn unary(&mut self) -> Option<Node> {
        match self.peek()?.clone() {
            Tok::Punct(op @ ("-" | "+" | "~" | "!")) => {
                self.at += 1;
                Some(Node::Unary(op, Box::new(self.unary()?)))
            }
            Tok::Punct("(") => {
                if let Some(close) = self.cast_at(self.at) {
                    let ty = text_of(&self.toks[self.at + 1..close]).replace(" *", "*").replace("* ", "*").replace('*', " *");
                    self.at = close + 1;
                    return Some(Node::Cast(ty.trim().to_string(), Box::new(self.unary()?)));
                }
                self.at += 1;
                let inner = self.expr()?;
                if !self.punct(")") {
                    return None;
                }
                self.at += 1;
                self.postfix(Node::Paren(Box::new(inner)))
            }
            Tok::Num(n) => {
                self.at += 1;
                let node = match number(&n) {
                    Some((v, u)) => Node::Num(v, u),
                    None => Node::Text(n),
                };
                self.postfix(node)
            }
            Tok::Name(_) | Tok::Lit(_) => {
                let t = self.peek()?.clone();
                self.at += 1;
                self.postfix(Node::Text(text_of(&[t])))
            }
            _ => None,
        }
    }

    /// What follows a name: `.f`, `->f`, `[i]`, `(args)`, kept as written
    /// (what is in brackets and arguments worked out).
    fn postfix(&mut self, mut node: Node) -> Option<Node> {
        loop {
            match self.peek() {
                Some(Tok::Punct(p @ ("." | "->"))) => {
                    let p: &str = p;
                    let Some(Tok::Name(f)) = self.toks.get(self.at + 1).cloned() else { return None };
                    node = Node::Text(format!("{}{p}{f}", show(&node, 11)));
                    self.at += 2;
                }
                Some(Tok::Punct(p @ ("[" | "("))) => {
                    let close = if *p == "[" { "]" } else { ")" };
                    let open: &str = p;
                    self.at += 1;
                    let mut args = Vec::new();
                    while !self.punct(close) {
                        args.push(show(&fold(self.expr()?), 0));
                        if self.punct(",") {
                            self.at += 1;
                        } else if !self.punct(close) {
                            return None;
                        }
                    }
                    self.at += 1;
                    node = Node::Text(format!("{}{open}{}{close}", show(&node, 11), args.join(", ")));
                }
                _ => return Some(node),
            }
        }
    }
}

/// `n` cut to what a cast to `ty` keeps: an unsigned type of a known width.
fn cast_number(ty: &str, n: i64) -> Option<(i64, bool)> {
    if ty.contains('*') {
        return None;
    }
    let lower = ty.to_ascii_lowercase();
    let bits = ["8", "16", "32", "64"].iter().find(|w| lower.ends_with(**w) || lower.contains(&format!("{w}_t"))).map(|w| w.parse::<u32>().unwrap());
    let bits = bits.or(if lower.contains("char") { Some(8) } else if lower.contains("short") { Some(16) } else { None });
    let unsigned = lower.contains("unsigned") || lower.starts_with('u');
    match (bits, unsigned) {
        (Some(64), u) => Some((n, u)),
        (Some(b), true) => Some((n & ((1i64 << b) - 1), true)),
        (Some(b), false) => Some(((n << (64 - b)) >> (64 - b), false)),
        (None, u) => Some((n, u)),
    }
}

fn arith(op: &str, (a, au): (i64, bool), (b, bu): (i64, bool)) -> Option<(i64, bool)> {
    let u = au || bu;
    let v = match op {
        "*" => a.wrapping_mul(b),
        "/" if b == 0 => return None,
        "/" => if u { ((a as u64) / (b as u64)) as i64 } else { a.checked_div(b)? },
        "%" if b == 0 => return None,
        "%" => if u { ((a as u64) % (b as u64)) as i64 } else { a.checked_rem(b)? },
        "+" => a.wrapping_add(b),
        "-" => a.wrapping_sub(b),
        "<<" if !(0..64).contains(&b) => return None,
        "<<" => a.wrapping_shl(b as u32),
        ">>" if !(0..64).contains(&b) => return None,
        ">>" => if au { ((a as u64) >> b) as i64 } else { a >> b },
        "<" => return Some((if u { ((a as u64) < (b as u64)) as i64 } else { (a < b) as i64 }, false)),
        ">" => return Some((if u { ((a as u64) > (b as u64)) as i64 } else { (a > b) as i64 }, false)),
        "<=" => return Some((if u { ((a as u64) <= (b as u64)) as i64 } else { (a <= b) as i64 }, false)),
        ">=" => return Some((if u { ((a as u64) >= (b as u64)) as i64 } else { (a >= b) as i64 }, false)),
        "==" => return Some(((a == b) as i64, false)),
        "!=" => return Some(((a != b) as i64, false)),
        "&" => a & b,
        "^" => a ^ b,
        "|" => a | b,
        "&&" => return Some(((a != 0 && b != 0) as i64, false)),
        "||" => return Some(((a != 0 || b != 0) as i64, false)),
        _ => return None,
    };
    Some((v, if matches!(op, "<<" | ">>") { au } else { u }))
}

/// `node` with what of it is numbers worked out.
fn fold(node: Node) -> Node {
    match node {
        Node::Paren(inner) => match fold(*inner) {
            n @ Node::Num(..) => n,
            n => Node::Paren(Box::new(n)),
        },
        Node::Unary(op, x) => match (op, fold(*x)) {
            ("-", Node::Num(v, u)) => Node::Num(v.wrapping_neg(), u),
            ("+", n @ Node::Num(..)) => n,
            ("~", Node::Num(v, u)) => Node::Num(!v, u),
            ("!", Node::Num(v, _)) => Node::Num((v == 0) as i64, false),
            (op, x) => Node::Unary(op, Box::new(x)),
        },
        Node::Cast(ty, x) => match fold(*x) {
            Node::Num(v, _) if cast_number(&ty, v).is_some() => {
                let (v, u) = cast_number(&ty, v).unwrap();
                Node::Num(v, u)
            }
            x => Node::Cast(ty, Box::new(x)),
        },
        Node::Binary(op, a, b) => {
            let (a, b) = (fold(*a), fold(*b));
            // (`C + x + D`: the numbers of a sum gathered, `C + D + x`.)
            if let (Node::Binary(inner, x, y), Node::Num(..)) = (&a, &b) {
                if *inner == "+" && op == "+" {
                    if let (Node::Num(..), _) | (_, Node::Num(..)) = (x.as_ref(), y.as_ref()) {
                        let (num, other) = if let Node::Num(..) = x.as_ref() { (x.as_ref().clone(), y.as_ref().clone()) } else { (y.as_ref().clone(), x.as_ref().clone()) };
                        return fold(Node::Binary("+", Box::new(Node::Binary("+", Box::new(num), Box::new(b))), Box::new(other)));
                    }
                }
            }
            match (&a, &b) {
                (Node::Num(x, xu), Node::Num(y, yu)) => match arith(op, (*x, *xu), (*y, *yu)) {
                    Some((v, u)) => Node::Num(v, u),
                    None => Node::Binary(op, Box::new(a), Box::new(b)),
                },
                _ => Node::Binary(op, Box::new(a), Box::new(b)),
            }
        }
        Node::Ternary(c, a, b) => match fold(*c) {
            Node::Num(v, _) => fold(if v != 0 { *a } else { *b }),
            c => Node::Ternary(Box::new(c), Box::new(fold(*a)), Box::new(fold(*b))),
        },
        n => n,
    }
}

fn rank_of(node: &Node) -> u8 {
    match node {
        Node::Binary(op, ..) => binary_rank(op).unwrap_or(0),
        Node::Ternary(..) => 0,
        Node::Unary(..) | Node::Cast(..) => 11,
        _ => 12,
    }
}

fn show_number(v: i64, u: bool) -> String {
    if (0..10).contains(&v) {
        v.to_string()
    } else if v < 0 && !u {
        v.to_string()
    } else {
        format!("0x{:X}", v as u64)
    }
}

/// `node` written out, in parentheses where it binds less than `outer`.
fn show(node: &Node, outer: u8) -> String {
    let s = match node {
        Node::Num(v, u) => show_number(*v, *u),
        Node::Text(t) => t.clone(),
        Node::Paren(inner) => return show(inner, outer),
        Node::Unary(op, x) => format!("{op}{}", show(x, 11)),
        Node::Cast(ty, x) => format!("({ty}){}", show(x, 11)),
        Node::Binary(op, a, b) => {
            let r = binary_rank(op).unwrap_or(0);
            format!("{} {op} {}", show(a, r), show(b, r + 1))
        }
        Node::Ternary(c, a, b) => format!("{} ? {} : {}", show(c, 1), show(a, 0), show(b, 0)),
    };
    if rank_of(node) < outer { format!("({s})") } else { s }
}

impl Values<'_> {
    /// `toks` with the macros the index has expanded, as the preprocessor
    /// would (`##` and `#` too); names it cannot expand stay as written.
    fn expand(&mut self, toks: &[Tok], from: u32, hide: &FxHashSet<String>, depth: u32) -> Option<Vec<Tok>> {
        if depth > 32 {
            return None;
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            let Tok::Name(name) = &toks[i] else {
                out.push(toks[i].clone());
                i += 1;
                continue;
            };
            if hide.contains(name) {
                out.push(toks[i].clone());
                i += 1;
                continue;
            }
            let Some((params, body)) = self.definition(name, from) else {
                out.push(toks[i].clone());
                i += 1;
                continue;
            };
            let mut hide = hide.clone();
            hide.insert(name.clone());
            match params {
                None => {
                    let body = tokens_of(&body)?;
                    out.extend(self.expand(&body, from, &hide, depth + 1)?);
                    i += 1;
                }
                Some(params) => {
                    if toks.get(i + 1) != Some(&Tok::Punct("(")) {
                        out.push(toks[i].clone());
                        i += 1;
                        continue;
                    }
                    // The arguments, split at the commas outside brackets.
                    let (mut args, mut arg, mut level, mut j) = (Vec::new(), Vec::new(), 0, i + 2);
                    loop {
                        let t = toks.get(j)?;
                        match t {
                            Tok::Punct("(" | "[" | "{") => level += 1,
                            Tok::Punct(")") if level == 0 => break,
                            Tok::Punct(")" | "]" | "}") => level -= 1,
                            Tok::Punct(",") if level == 0 => {
                                args.push(std::mem::take(&mut arg));
                                j += 1;
                                continue;
                            }
                            _ => {}
                        }
                        arg.push(t.clone());
                        j += 1;
                    }
                    if !arg.is_empty() || !args.is_empty() {
                        args.push(arg);
                    }
                    if args.len() != params.len() && !(params.is_empty() && args.is_empty()) {
                        return None;
                    }
                    let body = tokens_of(&body)?;
                    let param = |t: &Tok| match t {
                        Tok::Name(n) => params.iter().position(|p| p == n),
                        _ => None,
                    };
                    // Each parameter by its argument: as written beside `##`
                    // or after `#`, else expanded first.
                    let mut made: Vec<Tok> = Vec::new();
                    let mut k = 0;
                    while k < body.len() {
                        let pasted = body.get(k + 1) == Some(&Tok::Punct("##")) || k > 0 && body[k - 1] == Tok::Punct("##");
                        if body[k] == Tok::Punct("#") {
                            if let Some(p) = body.get(k + 1).and_then(param) {
                                made.push(Tok::Lit(format!("\"{}\"", text_of(&args[p]))));
                                k += 2;
                                continue;
                            }
                        }
                        match param(&body[k]) {
                            Some(p) if pasted => made.extend(args[p].iter().cloned()),
                            Some(p) => made.extend(self.expand(&args[p], from, &FxHashSet::default(), depth + 1)?),
                            None => made.push(body[k].clone()),
                        }
                        k += 1;
                    }
                    // `a ## b`: one token of the two.
                    let mut joined: Vec<Tok> = Vec::new();
                    let mut k = 0;
                    while k < made.len() {
                        if made[k] == Tok::Punct("##") {
                            let left = joined.pop()?;
                            let right = made.get(k + 1)?;
                            let text = format!("{}{}", text_of(&[left]), text_of(std::slice::from_ref(right)));
                            joined.extend(tokens_of(&text)?);
                            k += 2;
                            continue;
                        }
                        joined.push(made[k].clone());
                        k += 1;
                    }
                    out.extend(self.expand(&joined, from, &hide, depth + 1)?);
                    i = j + 1;
                }
            }
        }
        Some(out)
    }

    /// What `name` is defined as, as [`Values::learn`] picks it: the
    /// parameters (none: object-like) and the body as written.
    fn definition(&mut self, name: &str, from: u32) -> Option<(Option<Vec<String>>, String)> {
        if let Some(v) = self.conditions.setting(name.as_bytes()) {
            return Some((None, String::from_utf8_lossy(v).into_owned()));
        }
        if self.is_type(name) {
            return None;
        }
        self.learn(name, from);
        self.written.get(name).cloned()
    }

    /// An expression with its macros expanded and what of it is numbers
    /// worked out, as written: `(void *)(0x400800 + ((U32)(q.cnt[2] & 3) << 4))`.
    pub fn folded(&mut self, expr: &str, from: u32) -> Option<String> {
        let toks = self.expand(&tokens_of(expr)?, from, &FxHashSet::default(), 0)?;
        let mut f = Folder { toks: &toks, at: 0, values: self };
        let node = f.expr()?;
        if f.at != toks.len() {
            return None;
        }
        Some(show(&fold(node), 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_define_is_read_with_its_lines_joined() {
        let src = b"int x;\n#define Q_SIZE(n) ((n) * \\\n   4) /* bytes */\n";
        assert_eq!(define_at(src, 2), Some(Define { name: "Q_SIZE".into(), params: Some(vec!["n".into()]), body: "((n) *    4)".into() }));
        assert_eq!(define_at(b"#  define  BASE   (0x1000u) // base\n", 1).map(|d| d.body), Some("(0x1000u)".into()));
    }

    #[test]
    fn a_cast_to_a_type_is_passed_over() {
        let mut never = |_: &str| false;
        assert_eq!(without_casts("((U32)0x10 << 2)", &mut |w| w == "U32"), "( 0x10 << 2)");
        assert_eq!(without_casts("((unsigned long)1 + (x))", &mut never), "( 1 + (x))");
        // Not a cast: a name in parentheses with nothing after it.
        assert_eq!(without_casts("(BASE)", &mut never), "(BASE)");
    }
}
