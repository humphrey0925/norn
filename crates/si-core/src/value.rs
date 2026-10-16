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
    enums: FxHashMap<u32, Option<i64>>,
}

impl<'a> Values<'a> {
    pub fn new(g: &'a Graph, conditions: &'a Conditions, source: &'a mut dyn FnMut(&str) -> Option<std::sync::Arc<Vec<u8>>>) -> Values<'a> {
        Values { g, conditions, source, local: Local::default(), seen: FxHashSet::default(), types: FxHashMap::default(), enums: FxHashMap::default() }
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
        let g = self.g;
        let syms: Vec<u32> = g.find(name).to_vec();
        let macros: Vec<u32> = syms.iter().copied().filter(|&s| g.syms[s as usize].kind == kind::MACRO && g.syms[s as usize].flags & flag::DECL == 0).collect();
        if macros.is_empty() {
            // An enumerator: its number, as a value.
            if let Some(&e) = syms.iter().find(|&&s| g.syms[s as usize].kind == kind::ENUM_CONST) {
                if let Some(n) = self.enumerator(e) {
                    self.local.insert(name.as_bytes().to_vec(), Some(Some(n.to_string().into_bytes())));
                }
            }
            return;
        }
        // One definition, or several written alike; those certainly
        // compiled first.
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
        let [(at, d)] = &defs[..] else { return };
        let (at, d) = (*at, d.clone());
        let body = self.plain(&d.body);
        let params: Vec<String> = d.params.clone().unwrap_or_default();
        let value = match &d.params {
            None => crate::cond::value_of(body.as_bytes()),
            Some(p) => crate::cond::function_value(&p.iter().map(|p| p.as_bytes()).collect::<Vec<_>>(), body.as_bytes()),
        };
        let Some(value) = value else { return };
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
        let (number, unsigned) = self.conditions.number_in(&self.local, expr.as_bytes())?;
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
