//! Macro expansion at link time, for the uses that hide names from the
//! parser: `NAME (aout, final_link)` is `aout_32_final_link` in one
//! translation unit and `aout_64_final_link` in another, and
//! `START_RELOC_NUMBERS (elf_avr_reloc_type)` declares an enum or a function.
//!
//! There is no build configuration, so nothing says which `#define` of a
//! name is in force. Every definition that can share a translation unit with
//! the use is tried, and each distinct result is recorded: a function written
//! once under a pasted name becomes one definition per result, each with its
//! own copy of the references in its body.
//!
//! Only *expanding* macros are expanded (see `mask::MacroSets`), and only the
//! names an expansion makes are taken from it; what is written at the use is
//! indexed by the ordinary extraction.

use crate::mask::is_keyword;
use crate::model::{flag, kind, refkind, site, Bind, Def, FileFacts, Ref, Site, NONE};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

/// Where expansion spends its time, in nanoseconds (`SI_EXPAND_PROFILE`).
static SPENT: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
fn spend(i: usize, since: std::time::Instant) {
    SPENT[i].fetch_add(since.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Distinct expansions kept for one use.
const MAX_VARIANTS: usize = 24;
/// Definitions of one name tried at one use.
const MAX_CANDIDATES: usize = 16;
/// Macro invocations and tokens allowed in one expansion.
const MAX_STEPS: u32 = 500;
/// Expansions in a row that may bring no new name before the rest are left untried.
const BLIND_RUNS: usize = 3;
/// Uses of one macro in one file that may come to nothing before the rest
/// are skipped.
const BARREN_USES: u32 = 16;
/// Translation units a file may be in and still be expanded in each apart.
const MAX_UNITS: usize = 32;
/// Places remembered for one name of a definition.
const MAX_CONTEXTS: usize = 8;
/// Uses in one file expanded in one stretch; more are split across threads.
const PARALLEL_USES: usize = 512;
/// Translation units an initializer is expanded in.
const MAX_INIT_UNITS: usize = 2048;
/// Distinct expansions kept of one initializer in one unit.
const MAX_INIT_VARIANTS: usize = 4;
const MAX_TOKENS: usize = 20_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Ident,
    Punct,
    /// Number or literal.
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    /// Written at the use.
    Site,
    /// From a macro body.
    Body,
    /// Made by `##`.
    Pasted,
}

#[derive(Clone, Debug)]
struct T {
    /// Shared: tokens are copied far more often than they are made.
    text: Arc<str>,
    k: K,
    origin: Origin,
    /// A macro name met inside its own expansion: never expanded again.
    noexpand: bool,
}

impl T {
    fn is(&self, s: &str) -> bool {
        self.k == K::Punct && &*self.text == s
    }
}

fn int(s: &str) -> Option<i64> {
    let s = s.trim_end_matches(['u', 'U', 'l', 'L']);
    match s.strip_prefix("0x").or(s.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

fn is_ident_text(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn tokenize(s: &[u8], origin: Origin) -> Vec<T> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = s.len();
    let at = |k: usize| *s.get(k).unwrap_or(&0);
    while i < n {
        let b = s[i];
        let start = i;
        let k = match b {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0c => {
                i += 1;
                continue;
            }
            b'\\' if matches!(at(i + 1), b'\n' | b'\r') => {
                i += 1;
                continue;
            }
            // `a/**/b`: how tokens were pasted before `##`.
            b'/' if origin == Origin::Body && s[i..].starts_with(b"/**/") && !out.is_empty() => {
                i += 4;
                out.push(T { text: "##".into(), k: K::Punct, origin, noexpand: false });
                continue;
            }
            b'/' if at(i + 1) == b'*' => {
                i += 2;
                while i < n && !(s[i] == b'*' && at(i + 1) == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                continue;
            }
            b'/' if at(i + 1) == b'/' => {
                while i < n && s[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'_') {
                    i += 1;
                }
                K::Ident
            }
            _ if b.is_ascii_digit() => {
                while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'_' || s[i] == b'.') {
                    i += 1;
                }
                K::Other
            }
            b'"' | b'\'' => {
                i += 1;
                while i < n && s[i] != b && s[i] != b'\n' {
                    i += 1 + (s[i] == b'\\' && i + 1 < n) as usize;
                }
                i = (i + 1).min(n);
                K::Other
            }
            _ => {
                let two = [b, at(i + 1)];
                i += if at(i + 1) == b'.' && at(i + 2) == b'.' && b == b'.' {
                    3
                } else if matches!(&two, b"##" | b"->" | b"==" | b"!=" | b"<=" | b">=" | b"&&" | b"||" | b"<<" | b">>") {
                    2
                } else {
                    1
                };
                K::Punct
            }
        };
        out.push(T { text: String::from_utf8_lossy(&s[start..i]).into(), k, origin, noexpand: false });
    }
    out
}

/// May a declared name follow this token (`int x`, `char *x`)?
fn decl_prev(t: &T) -> bool {
    match t.k {
        K::Ident => !matches!(
            &*t.text,
            "return" | "goto" | "case" | "sizeof" | "else" | "do" | "if" | "while" | "for" | "switch" | "typeof"
                | "__typeof__" | "defined" | "break" | "continue"
        ),
        _ => t.is("*"),
    }
}

fn is_tag(t: &T) -> bool {
    t.k == K::Ident && matches!(&*t.text, "enum" | "struct" | "union")
}

/// Does a macro with this body put names in its expansion that are not
/// written at its uses? True when it pastes tokens, calls one of its
/// parameters (which may then be such a macro), declares one, or is just
/// another name (`#define pe_dll_build pep_dll_build`).
pub fn hides_names(body: &[u8], params: &[&[u8]]) -> bool {
    hides(&tokenize(body, Origin::Body), |t| params.contains(&t.text.as_bytes()))
}

fn hides(toks: &[T], is_param: impl Fn(&T) -> bool) -> bool {
    let n = toks.len();
    if n == 1 && toks[0].k == K::Ident && !is_param(&toks[0]) && !is_keyword(toks[0].text.as_bytes()) {
        return true;
    }
    // A braced initializer (`HOWTO (...)` for a table row): which member
    // each argument lands in is only visible in the expansion.
    if n >= 2 && toks[0].is("{") && toks[n - 1].is("}") {
        return true;
    }
    let (mut paren, mut brace) = (0i32, 0i32);
    for i in 0..n {
        let t = &toks[i];
        let prev = i.checked_sub(1).map(|j| &toks[j]);
        let next = toks.get(i + 1);
        match (&*t.text, t.k) {
            // `, ## __VA_ARGS__` only drops a comma.
            ("##", K::Punct) if !prev.is_some_and(|p| p.is(",")) => return true,
            ("(", K::Punct) => paren += 1,
            (")", K::Punct) => paren -= 1,
            ("{", K::Punct) => brace += 1,
            ("}", K::Punct) => brace -= 1,
            // Several values in a row (`#define ROW(n, f, a) n, a, f`): a
            // row of a table, or arguments. Which place each one takes is
            // only visible in the expansion.
            (",", K::Punct) if paren == 0 && brace == 0 => return true,
            _ => {}
        }
        if t.k != K::Ident || !is_param(t) {
            continue;
        }
        let next_is = |s: &str| next.is_some_and(|x| x.is(s));
        // Calls a parameter, or names a member by one (`(p)->msg (args)`).
        if next_is("(") || prev.is_some_and(|p| p.is("->") || p.is(".")) {
            return true;
        }
        if paren != 0 || brace != 0 {
            continue;
        }
        if prev.is_some_and(is_tag) && next_is("{") {
            return true;
        }
        if prev.is_some_and(decl_prev) && (next.is_none() || ["=", ";", "[", ","].iter().any(|s| next_is(s))) {
            return true;
        }
        if i == 0 && (next_is("=") && toks[n - 1].is(",") || n == 2 && next_is(",")) {
            return true;
        }
    }
    false
}

struct Parsed {
    params: Vec<Arc<str>>,
    variadic: bool,
    function_like: bool,
    body: Vec<T>,
    /// See [`hides_names`].
    hides: bool,
    /// `#if NAME == value` (or, with true, its `#else`) around the definition.
    guards: Vec<(Arc<str>, i64, bool)>,
    /// Defined only under some `#if`.
    conditional: bool,
    /// Defined under `#ifndef` of its own name.
    fallback: bool,
    /// The file it is defined in.
    file: u32,
    /// For each body token, the parameter it names, or `NONE`.
    param_at: Vec<u32>,
}

/// `text` is what follows the macro's name in its `#define`.
fn parse(text: &str, function_like: bool, guard: &str) -> Parsed {
    let guards = guard
        .split(';')
        .filter_map(|g| {
            let (name, rest) = g.split_once(' ')?;
            let (op, value) = rest.split_once(' ')?;
            Some((name.into(), value.parse().ok()?, op == "!="))
        })
        .collect();
    let mut toks = tokenize(text.as_bytes(), Origin::Body);
    let mut params: Vec<Arc<str>> = Vec::new();
    let mut variadic = false;
    if function_like {
        let close = toks.iter().position(|t| t.is(")")).unwrap_or(toks.len());
        for t in &toks[..close] {
            if t.k == K::Ident {
                params.push(t.text.clone());
            } else if t.is("...") {
                variadic = true;
            }
        }
        // `...` alone is `__VA_ARGS__`; `args...` names the rest `args`.
        if variadic && !toks[..close].iter().rev().nth(1).is_some_and(|t| t.k == K::Ident) {
            params.push("__VA_ARGS__".into());
        }
        toks.drain(..(close + 1).min(toks.len()));
    }
    let param_at =
        toks.iter().map(|t| params.iter().position(|p| t.k == K::Ident && *p == t.text).map_or(NONE, |i| i as u32)).collect();
    let hides = hides(&toks, |t| params.contains(&t.text));
    Parsed { params, variadic, function_like, body: toks, hides, guards, conditional: false, fallback: false, file: 0, param_at }
}

struct Table<'a> {
    files: &'a [FileFacts],
    by_name: FxHashMap<&'a str, Vec<(u32, u32)>>,
    /// The same definitions in the order of their text, each with a number
    /// that definitions of the same text (spacing aside) share: sorted and
    /// compared once for the corpus, where every file's engine would
    /// otherwise do it again for every name it meets, and a common name
    /// has thousands of definitions.
    by_text: FxHashMap<&'a str, Vec<(u32, u32, u32)>>,
    parsed: Vec<Vec<Parsed>>,
}

type Choice = FxHashMap<Box<str>, usize>;

enum Item {
    Tok(T),
    /// The expansion of the innermost active macro ends here.
    End,
}

/// Expands token lists as seen from one file.
struct Engine<'a> {
    table: &'a Table<'a>,
    /// Files that can be in a translation unit with this one.
    visible: Vec<bool>,
    cands: FxHashMap<Box<str>, Vec<&'a Parsed>>,
    /// Which definition to use for each name with several.
    choice: Choice,
    /// Names with several definitions met in this run, with how many.
    met: Vec<(Box<str>, usize)>,
    active: Vec<Arc<str>>,
    steps: u32,
    hiding: FxHashMap<Box<str>, bool>,
    /// This engine's own copy of each macro body it has used. The tokens
    /// of the bodies in the table are shared by every thread, and copying
    /// one counts a reference on it: with every core expanding the same
    /// few macros (`BIT`, `GENMASK`), they spend their time taking the
    /// counter from each other. A copy made once is this thread's alone.
    bodies: FxHashMap<usize, std::rc::Rc<Vec<T>>>,
    /// Set when the engine stands for all the translation units of this
    /// file together.
    merged: Option<u32>,
    /// Distinct expansions wanted of one input.
    max_variants: usize,
    /// Files this unit reads whatever the configuration (no `#include`
    /// under an `#if` on the way). Empty: all of them.
    sure: Vec<bool>,
}

impl<'a> Table<'a> {
    fn new(files: &'a [FileFacts], by_name: FxHashMap<&'a str, Vec<(u32, u32)>>, parsed: Vec<Vec<Parsed>>) -> Self {
        let text = |f: u32, i: u32| files[f as usize].name(files[f as usize].macros[i as usize].text as usize);
        let by_text = by_name
            .par_iter()
            .map(|(&name, defs)| {
                let mut sorted: Vec<(&str, u32, u32)> = defs.iter().map(|&(f, i)| (text(f, i), f, i)).collect();
                sorted.sort_unstable();
                // Texts that differ only in spacing are one definition,
                // wherever the order of the texts puts them.
                let mut classes: FxHashMap<Vec<&str>, u32> = FxHashMap::default();
                let mut out = Vec::with_capacity(sorted.len());
                for &(t, f, i) in &sorted {
                    let n = classes.len() as u32;
                    out.push((f, i, *classes.entry(t.split_whitespace().collect()).or_insert(n)));
                }
                (name, out)
            })
            .collect();
        Table { files, by_name, by_text, parsed }
    }
}

impl<'a> Engine<'a> {
    fn new(table: &'a Table<'a>, visible: Vec<bool>) -> Self {
        Engine {
            table,
            visible,
            cands: FxHashMap::default(),
            choice: Choice::default(),
            met: Vec::new(),
            active: Vec::new(),
            steps: 0,
            hiding: FxHashMap::default(),
            bodies: FxHashMap::default(),
            merged: None,
            max_variants: MAX_VARIANTS,
            sure: Vec::new(),
        }
    }

    /// Is there a translation unit here in which `name` is no macro?
    fn maybe_undefined(&mut self, name: &str) -> bool {
        match self.merged {
            // Several units at once: sure only if the file itself always
            // defines the name.
            Some(file) => !self.table.by_name.get(name).is_some_and(|v| {
                v.iter().any(|&(f, i)| f == file && !self.table.files[f as usize].macros[i as usize].conditional)
            }),
            None => {
                let sure = std::mem::take(&mut self.sure);
                let all = self.candidates(name).iter().all(|p| p.conditional || !sure.get(p.file as usize).copied().unwrap_or(true));
                self.sure = sure;
                all
            }
        }
    }

    fn candidates(&mut self, name: &str) -> &[&'a Parsed] {
        if !self.cands.contains_key(name) {
            // The definitions this unit can see, one of each text, in the
            // order of the texts (which must not depend on which file was
            // read first).
            let all = self.table.by_text.get(name).map_or(&[][..], |v| &v[..]);
            let one_of_each = |defs: &mut dyn Iterator<Item = &(u32, u32, u32)>| -> Vec<(u32, u32, u32)> {
                let mut out: Vec<(u32, u32, u32)> = Vec::new();
                for &d in defs {
                    if out.last().is_none_or(|l| l.2 != d.2) {
                        out.push(d);
                    }
                }
                out
            };
            let mut seen = one_of_each(&mut all.iter().filter(|d| self.visible[d.0 as usize]));
            if seen.is_empty() {
                // An `#include` that could not be followed (a generated
                // header, say) may hide the definition. A header can reach
                // any file, a source file only itself. If several headers
                // define the name differently there is no telling which is
                // meant.
                let header = |d: &&(u32, u32, u32)| self.table.files[d.0 as usize].path.ends_with(".h");
                let mut headers = all.iter().filter(header);
                if let Some(first) = headers.next() {
                    let (mut one_file, mut one_text) = (true, true);
                    for d in headers {
                        one_file &= d.0 == first.0;
                        one_text &= d.2 == first.2;
                    }
                    if one_file || one_text {
                        seen = one_of_each(&mut all.iter().filter(header));
                    }
                }
            }
            seen.truncate(MAX_CANDIDATES);
            let mut list: Vec<&'a Parsed> = seen.iter().map(|&(f, i, _)| &self.table.parsed[f as usize][i as usize]).collect();
            // `#ifndef X` / `#define X default` gives way to any other
            // definition there is.
            if list.iter().any(|p| !p.fallback) {
                list.retain(|p| !p.fallback);
            }
            // In the cache first, so that a guard that leads back here ends.
            self.cands.insert(name.into(), list.clone());
            let open: Vec<&'a Parsed> = list.iter().copied().filter(|p| p.guards.iter().all(|g| self.feasible(g))).collect();
            if !open.is_empty() && open.len() < list.len() {
                self.cands.insert(name.into(), open);
            }
        }
        &self.cands[name]
    }

    /// Can the guard hold in some translation unit with this file? It cannot
    /// when every value the name is given there says otherwise.
    fn feasible(&mut self, guard: &(Arc<str>, i64, bool)) -> bool {
        let (name, value, negated) = guard;
        let values: Option<Vec<i64>> = self
            .candidates(name)
            .iter()
            .map(|p| match &p.body[..] {
                [t] if !p.function_like => int(&t.text),
                _ => None,
            })
            .collect();
        match values {
            Some(v) if !v.is_empty() => v.iter().any(|x| (x == value) != *negated),
            _ => true,
        }
    }

    /// Can a use of `name` here expand to names not written at the use? The
    /// set of expanding macros is by name over the whole corpus; this asks
    /// the definitions this file can see.
    fn hides(&mut self, name: &str) -> bool {
        if let Some(&known) = self.hiding.get(name) {
            return known;
        }
        self.hiding.insert(name.into(), false);
        let cands = self.candidates(name).to_vec();
        let found = cands.iter().any(|p| p.hides || p.body.iter().any(|t| t.k == K::Ident && self.hides(&t.text)));
        self.hiding.insert(name.into(), found);
        found
    }

    fn pick(&mut self, name: &str) -> Option<&'a Parsed> {
        let n = self.candidates(name).len();
        if n == 0 {
            return None;
        }
        let i = match self.choice.get(name) {
            Some(&i) => i.min(n - 1),
            None => {
                self.choice.insert(name.into(), 0);
                0
            }
        };
        if n > 1 && !self.met.iter().any(|m| &*m.0 == name) {
            self.met.push((name.into(), n));
        }
        Some(self.cands[name][i])
    }

    fn expand(&mut self, input: Vec<T>) -> Vec<T> {
        let mut out = Vec::new();
        let mut stack: Vec<Item> = input.into_iter().rev().map(Item::Tok).collect();
        while let Some(item) = stack.pop() {
            let mut t = match item {
                Item::End => {
                    self.active.pop();
                    continue;
                }
                Item::Tok(t) => t,
            };
            if t.k != K::Ident || t.noexpand || self.steps > MAX_STEPS || out.len() + stack.len() > MAX_TOKENS {
                out.push(t);
                continue;
            }
            if self.active.contains(&t.text) {
                t.noexpand = true;
                out.push(t);
                continue;
            }
            let Some(m) = self.pick(&t.text) else {
                out.push(t);
                continue;
            };
            let mut args: Vec<Vec<T>> = Vec::new();
            if m.function_like {
                // The argument list may begin after the end of the expansion
                // that produced the name.
                let next = stack.iter().rev().find_map(|i| match i {
                    Item::Tok(t) => Some(t.is("(")),
                    Item::End => None,
                });
                if next != Some(true) {
                    out.push(t);
                    continue;
                }
                let mut depth = 0;
                while let Some(item) = stack.pop() {
                    let a = match item {
                        Item::End => {
                            self.active.pop();
                            continue;
                        }
                        Item::Tok(a) => a,
                    };
                    if a.is("(") {
                        depth += 1;
                        if depth == 1 {
                            args.push(Vec::new());
                            continue;
                        }
                    } else if a.is(")") {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    } else if a.is(",") && depth == 1 {
                        args.push(Vec::new());
                        continue;
                    }
                    if let Some(last) = args.last_mut() {
                        last.push(a);
                    }
                }
            }
            self.steps += 1;
            let body = self.substitute(m, args);
            self.active.push(t.text);
            stack.push(Item::End);
            stack.extend(body.into_iter().rev().map(Item::Tok));
        }
        out
    }

    fn substitute(&mut self, m: &Parsed, mut args: Vec<Vec<T>>) -> Vec<T> {
        let np = m.params.len();
        if m.variadic && np > 0 && args.len() > np {
            // Everything past the named parameters is one argument, commas and all.
            let rest = args.split_off(np);
            let comma = T { text: ",".into(), k: K::Punct, origin: Origin::Site, noexpand: false };
            for a in rest {
                args[np - 1].push(comma.clone());
                args[np - 1].extend(a);
            }
        }
        args.resize(np, Vec::new());
        let mut expanded: Vec<Option<Vec<T>>> = vec![None; np];
        let empty = || T { text: "".into(), k: K::Other, origin: Origin::Body, noexpand: false };
        let own = self
            .bodies
            .entry(m as *const Parsed as usize)
            .or_insert_with(|| std::rc::Rc::new(m.body.iter().map(|t| T { text: Arc::from(&*t.text), k: t.k, origin: t.origin, noexpand: t.noexpand }).collect()))
            .clone();
        let mut res: Vec<T> = Vec::with_capacity(m.body.len());
        let mut i = 0;
        while i < m.body.len() {
            let t = &own[i];
            let p = m.param_at[i];
            if t.is("#") && m.function_like && m.param_at.get(i + 1).is_some_and(|&p| p != NONE) {
                res.push(T { text: "\"\"".into(), k: K::Other, origin: Origin::Body, noexpand: false });
                i += 2;
                continue;
            }
            if p == NONE {
                res.push(t.clone());
            } else {
                let p = p as usize;
                let pasted = i > 0 && m.body[i - 1].is("##") || m.body.get(i + 1).is_some_and(|n| n.is("##"));
                if pasted {
                    // Operands of `##` are not expanded first.
                    if args[p].is_empty() {
                        res.push(empty());
                    } else {
                        res.extend(args[p].iter().cloned());
                    }
                } else {
                    if expanded[p].is_none() {
                        let depth = self.active.len();
                        let e = self.expand(args[p].clone());
                        self.active.truncate(depth);
                        expanded[p] = Some(e);
                    }
                    res.extend(expanded[p].iter().flatten().cloned());
                }
            }
            i += 1;
        }
        // Paste.
        let mut out: Vec<T> = Vec::with_capacity(res.len());
        let mut i = 0;
        while i < res.len() {
            if res[i].is("##") && res[i].origin == Origin::Body && !out.is_empty() && i + 1 < res.len() {
                let right = res[i + 1].clone();
                let left = out.pop().unwrap();
                if left.is(",") {
                    // GNU `, ## args`: the comma goes when there are none.
                    if !right.text.is_empty() {
                        out.push(left);
                        out.push(right);
                    }
                } else {
                    let text = format!("{}{}", left.text, right.text);
                    let k = if is_ident_text(&text) { K::Ident } else { K::Other };
                    out.push(T { text: text.into(), k, origin: Origin::Pasted, noexpand: false });
                }
                i += 2;
                continue;
            }
            out.push(res[i].clone());
            i += 1;
        }
        out.retain(|t| !t.text.is_empty());
        out
    }

    /// Every distinct expansion of `toks`, with the choices that gave it.
    /// Names in `pinned` keep the definition chosen there. Stops once a few
    /// expansions in a row hold nothing `novel`.
    fn variants(&mut self, toks: &[T], pinned: &Choice, mut novel: impl FnMut(&[T]) -> bool) -> Vec<(Vec<T>, Choice)> {
        let mut results: Vec<(Vec<T>, Choice)> = Vec::new();
        let mut free: Vec<(Box<str>, usize)> = Vec::new();
        let mut choice = pinned.clone();
        let mut stale = 0;
        for _ in 0..self.max_variants * 2 {
            if stale >= BLIND_RUNS {
                break;
            }
            stale += 1;
            self.choice = choice.clone();
            self.met.clear();
            self.active.clear();
            self.steps = 0;
            let t = std::time::Instant::now();
            let out = self.expand(toks.to_vec());
            spend(0, t);
            SPENT[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            SPENT[5].fetch_add(out.len() as u64, std::sync::atomic::Ordering::Relaxed);
            for m in self.met.drain(..) {
                if !pinned.contains_key(&m.0) && !free.iter().any(|f| f.0 == m.0) {
                    free.push(m);
                }
            }
            if !results.iter().any(|r| r.0.len() == out.len() && r.0.iter().zip(&out).all(|(a, b)| a.text == b.text)) {
                let t = std::time::Instant::now();
                let is_novel = novel(&out);
                spend(1, t);
                if is_novel {
                    stale = 0;
                }
                results.push((out, self.choice.clone()));
            }
            if results.len() >= self.max_variants {
                break;
            }
            // Next combination, the most recently met name turning fastest.
            let mut k = free.len();
            loop {
                if k == 0 {
                    return results;
                }
                k -= 1;
                let c = choice.get(&free[k].0).copied().unwrap_or(0) + 1;
                if c < free[k].1 {
                    choice.insert(free[k].0.clone(), c);
                    for f in &free[k + 1..] {
                        choice.remove(&f.0);
                    }
                    break;
                }
            }
        }
        results
    }
}

#[derive(Default)]
struct Found {
    /// The expansion is this one name.
    single: Option<String>,
    /// Declared in the expansion: name, kind, flags.
    decls: Vec<(String, u8, u8)>,
    /// Names the expansion uses that are not written at the use: name,
    /// kind, and the function the expansion defines around it, if any.
    refs: Vec<(String, u8, Option<String>)>,
}

impl Found {
    /// Add this expansion's names to `known`; was any of them new?
    fn novel(self, known: &mut FxHashSet<String>) -> bool {
        let names = self.single.into_iter().chain(self.decls.into_iter().map(|d| d.0)).chain(self.refs.into_iter().map(|r| r.0));
        names.fold(false, |new, n| known.insert(n) || new)
    }

    fn is_empty(&self) -> bool {
        self.single.is_none() && self.decls.is_empty() && self.refs.is_empty()
    }

    /// What a definition whose name is this use is really called.
    fn def_name(&self) -> Option<&str> {
        self.single.as_deref().or(self.decls.iter().rev().find(|d| d.1 != kind::ENUM_CONST).map(|d| &*d.0))
    }
}

/// Read the names out of one expansion of the use `s` of `macro_name`.
fn recognise(toks: &[T], s: &Site, macro_name: &str) -> Found {
    let mut found = Found::default();
    let n = toks.len();
    if n == 1 {
        if toks[0].k == K::Ident && &*toks[0].text != macro_name && !is_keyword(toks[0].text.as_bytes()) && &*toks[0].text != "NULL" {
            found.single = Some(toks[0].text.to_string());
            let kind = if s.flags & site::CALLED != 0 { refkind::CALL } else { refkind::USE };
            found.refs.push((toks[0].text.to_string(), kind, None));
        }
        return found;
    }
    // Another name for a function, followed by what followed the use:
    // `re_search (bufp, ...)` with `#define re_search xre_search`.
    let renamed = n >= 2
        && toks[0].k == K::Ident
        && toks[0].origin != Origin::Site
        && toks[1].is("(")
        && toks[1].origin == Origin::Site
        && {
            // ... all of it, in one pair of parentheses.
            let mut depth = 0;
            toks[1..].iter().position(|t| {
                depth += t.is("(") as i32 - t.is(")") as i32;
                depth == 0
            }) == Some(n - 2)
        }
        && !is_keyword(toks[0].text.as_bytes())
        && &*toks[0].text != "NULL";
    if renamed {
        found.single = Some(toks[0].text.to_string());
        found.refs.push((toks[0].text.to_string(), refkind::CALL, None));
        return found;
    }
    let in_fn = s.flags & site::IN_FN != 0;
    let top = s.flags & site::TOP != 0;
    let init = s.flags & site::INIT != 0;
    let (mut paren, mut brace) = (0i32, 0i32);
    let mut storage = 0u8;
    // The function whose body, written in the expansion, is being read.
    let mut owner: Option<String> = None;
    for i in 0..n {
        let t = &toks[i];
        if t.k == K::Punct {
            match &*t.text {
                "(" => paren += 1,
                ")" => paren -= 1,
                "{" => brace += 1,
                "}" => {
                    brace -= 1;
                    if brace == 0 {
                        owner = None;
                    }
                }
                ";" if paren == 0 && brace == 0 => storage = 0,
                _ => {}
            }
            continue;
        }
        if t.k != K::Ident {
            continue;
        }
        match &*t.text {
            "static" => storage |= flag::STATIC,
            "extern" => storage |= flag::DECL,
            _ => {}
        }
        if is_keyword(t.text.as_bytes()) {
            continue;
        }
        let prev = i.checked_sub(1).map(|j| &toks[j]);
        let next = toks.get(i + 1);
        let next_is = |s: &str| next.is_some_and(|x| x.is(s));
        let name = || t.text.to_string();
        if let Some(p) = prev.filter(|p| p.is("->") || p.is(".")) {
            // A member the expansion names. One written at the use, operator
            // and all, the parser has seen.
            if t.origin != Origin::Site || p.origin != Origin::Site {
                // `(*(p)->member) (args)` or `(p->member[i]) (args)`:
                // subscripts and closing parentheses, then the call.
                let mut k = i + 1;
                let mut depth = 0;
                while let Some(x) = toks.get(k) {
                    depth += x.is("[") as i32 - x.is("]") as i32;
                    if depth == 0 && !x.is(")") && !x.is("]") {
                        break;
                    }
                    k += 1;
                }
                if toks.get(k).is_some_and(|x| x.is("(")) {
                    found.refs.push((name(), refkind::ICALL, owner.clone()));
                } else if t.origin == Origin::Pasted {
                    found.refs.push((name(), refkind::MEMBER, owner.clone()));
                }
            }
            continue;
        }
        if !in_fn && prev.is_some_and(is_tag) && next_is("{") {
            let k = match &*prev.unwrap().text {
                "enum" => kind::ENUM,
                "struct" => kind::STRUCT,
                _ => kind::UNION,
            };
            found.decls.push((name(), k, 0));
            continue;
        }
        if !in_fn && paren == 0 && brace == 0 && prev.is_some_and(decl_prev) {
            if next_is("(") {
                let mut depth = 0;
                let close = toks[i + 1..].iter().position(|x| {
                    depth += x.is("(") as i32 - x.is(")") as i32;
                    depth == 0
                });
                let after = close.and_then(|c| toks.get(i + 2 + c));
                let defined = match after {
                    Some(a) => a.is("{"),
                    None => s.flags & site::BRACE_FOLLOWS != 0,
                };
                found.decls.push((name(), kind::FUNCTION, storage & flag::STATIC | if defined { 0 } else { flag::DECL }));
                // A body written out in the expansion: what it calls, it calls.
                if after.is_some_and(|a| a.is("{")) {
                    owner = Some(name());
                }
                continue;
            }
            if top && (next.is_none() || ["=", ";", "[", ","].iter().any(|s| next_is(s))) {
                found.decls.push((name(), kind::GLOBAL, storage));
                continue;
            }
        }
        if !in_fn && !init && i == 0 && (next_is("=") && toks[n - 1].is(",") || n == 2 && next_is(",") && !top) {
            found.decls.push((name(), kind::ENUM_CONST, 0));
            continue;
        }
        if owner.is_some() && brace > 0 && next_is("(") {
            found.refs.push((name(), refkind::CALL, owner.clone()));
        } else if t.origin == Origin::Pasted {
            found.refs.push((name(), if next_is("(") { refkind::CALL } else { refkind::USE }, owner.clone()));
        }
    }
    found
}

/// Files that can be in a translation unit with `file`: whatever includes it,
/// directly or not, and everything those include.
fn visible(file: u32, fwd: &[Vec<u32>], rev: &[Vec<u32>]) -> Vec<bool> {
    let mut up = vec![false; fwd.len()];
    let mut all = vec![false; fwd.len()];
    let mut stack = vec![file];
    up[file as usize] = true;
    while let Some(f) = stack.pop() {
        stack.extend(rev[f as usize].iter().copied().filter(|&r| !std::mem::replace(&mut up[r as usize], true)));
    }
    stack.extend((0..fwd.len() as u32).filter(|&f| up[f as usize]));
    for &f in &stack {
        all[f as usize] = true;
    }
    while let Some(f) = stack.pop() {
        stack.extend(fwd[f as usize].iter().copied().filter(|&t| !std::mem::replace(&mut all[t as usize], true)));
    }
    all
}

struct Names {
    ids: FxHashMap<String, u32>,
}

impl Names {
    fn id(&mut self, f: &mut FileFacts, s: &str) -> u32 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = f.push_name(s);
        self.ids.insert(s.to_string(), id);
        id
    }
}

/// `text` with its `#if` groups resolved one way: with `first`, each group's
/// first branch; without, its `#else` branch (nothing if it has none). An
/// initializer read both ways gives every element its position under either
/// configuration, where reading all branches at once would shift them.
fn one_branch(text: &str, first: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut taking: Vec<bool> = Vec::new();
    let mut continued = false;
    for line in text.split('\n') {
        let directive = continued || line.trim_start().starts_with('#');
        if directive {
            let word = line.trim_start().trim_start_matches('#').trim_start();
            if !continued {
                if word.starts_with("if") {
                    taking.push(first);
                } else if word.starts_with("elif") {
                    if let Some(t) = taking.last_mut() {
                        *t = false;
                    }
                } else if word.starts_with("else") {
                    if let Some(t) = taking.last_mut() {
                        *t = !first;
                    }
                } else if word.starts_with("endif") {
                    taking.pop();
                }
            }
            continued = line.trim_end().ends_with('\\');
        } else if taking.iter().all(|&t| t) {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// `type name = {` or `type name[] = {` in an expansion: the type's name,
/// whether it is an array, and where the brace is.
fn declared_initializer(toks: &[T]) -> Option<(String, bool, usize)> {
    let (mut paren, mut brace) = (0i32, 0i32);
    let mut idents: Vec<&T> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match (&*t.text, t.k) {
            ("(", K::Punct) => paren += 1,
            (")", K::Punct) => paren -= 1,
            ("{", K::Punct) => brace += 1,
            ("}", K::Punct) => brace -= 1,
            (";", K::Punct) if paren == 0 && brace == 0 => idents.clear(),
            ("=", K::Punct) if paren == 0 && brace == 0 && toks.get(i + 1).is_some_and(|n| n.is("{")) => {
                let n = idents.len();
                return (n >= 2).then(|| (idents[n - 2].text.to_string(), toks[i - 1].is("]"), i + 1));
            }
            (_, K::Ident) if paren == 0 && brace == 0 && !is_keyword(t.text.as_bytes()) => idents.push(t),
            _ => {}
        }
    }
    None
}

/// Where a name in a braced initializer goes: a member named by a
/// designator, or the member at a position.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Place {
    Member(String),
    Index(u32),
}

/// The names in an expanded initializer of a struct (or, with `array`, of an
/// array of structs), each with the member it initializes. A name inside
/// nested braces belongs to the member the braces initialize, unless a
/// designator in there says which member of that one it is
/// (`.interface = { .set = fn }` stores into `set`).
fn init_names(toks: &[T], array: bool) -> Vec<(Place, String)> {
    let level = if array { 2 } else { 1 };
    let (mut depth, mut paren, mut index) = (0i32, 0i32, 0u32);
    let mut designated: Option<String> = None;
    // The designator in force at each level of braces below the struct's own.
    let mut inner: Vec<Option<String>> = Vec::new();
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| &toks[j]);
        let next = toks.get(i + 1);
        if t.k == K::Punct {
            match &*t.text {
                "{" => {
                    depth += 1;
                    if depth == level {
                        index = 0;
                        designated = None;
                        inner.clear();
                    } else if depth > level {
                        inner.push(None);
                    }
                }
                "}" => {
                    if depth > level {
                        inner.pop();
                    }
                    depth -= 1;
                }
                "(" => paren += 1,
                ")" => paren -= 1,
                "," if paren == 0 && depth == level => {
                    index += 1;
                    designated = None;
                }
                "," if paren == 0 && depth > level => {
                    if let Some(d) = inner.last_mut() {
                        *d = None;
                    }
                }
                "." if depth == level && prev.is_some_and(|p| p.is(",") || p.is("{")) => {
                    designated = next.filter(|n| n.k == K::Ident).map(|n| n.text.to_string());
                }
                "." if depth > level && prev.is_some_and(|p| p.is(",") || p.is("{")) => {
                    if let Some(d) = inner.last_mut() {
                        *d = next.filter(|n| n.k == K::Ident).map(|n| n.text.to_string());
                    }
                }
                _ => {}
            }
            continue;
        }
        if t.k != K::Ident || depth < level || is_keyword(t.text.as_bytes()) {
            continue;
        }
        if prev.is_some_and(|p| p.is(".") || p.is("->")) || next.is_some_and(|n| n.is("(")) {
            continue;
        }
        // The innermost designator says the member; else the struct's own place.
        let place = match inner.iter().rev().flatten().next() {
            Some(member) => Place::Member(member.clone()),
            None => designated.clone().map_or(Place::Index(index), Place::Member),
        };
        out.push((place, t.text.to_string()));
    }
    out
}

fn guard_text<'a>(f: &'a FileFacts, m: &crate::model::MacroBody) -> &'a str {
    if m.guard == NONE {
        ""
    } else {
        f.name(m.guard as usize)
    }
}

fn site_tokens(f: &FileFacts, s: &Site) -> Vec<T> {
    let mut toks = tokenize(f.name(s.name as usize).as_bytes(), Origin::Site);
    if s.args != NONE {
        toks.extend(tokenize(f.name(s.args as usize).as_bytes(), Origin::Site));
    }
    toks
}

/// The translation units `file` is part of: itself or whatever includes it,
/// when that is a source file or is included by nothing.
fn roots(file: u32, files: &[FileFacts], rev: &[Vec<u32>]) -> Vec<u32> {
    let mut seen = vec![false; rev.len()];
    seen[file as usize] = true;
    let mut stack = vec![file];
    let mut out = Vec::new();
    while let Some(f) = stack.pop() {
        if rev[f as usize].is_empty() || files[f as usize].path.ends_with(".c") {
            out.push(f);
        }
        stack.extend(rev[f as usize].iter().copied().filter(|&r| !std::mem::replace(&mut seen[r as usize], true)));
    }
    out.sort_unstable();
    out
}

/// `root` and everything it includes.
fn closure(root: u32, fwd: &[Vec<u32>]) -> Vec<bool> {
    let mut seen = vec![false; fwd.len()];
    seen[root as usize] = true;
    let mut stack = vec![root];
    while let Some(f) = stack.pop() {
        stack.extend(fwd[f as usize].iter().copied().filter(|&t| !std::mem::replace(&mut seen[t as usize], true)));
    }
    seen
}

/// What one expansion of one use adds to its file.
struct Made {
    line: u32,
    scope: u32,
    /// The macro is not defined in every configuration of the unit.
    maybe: bool,
    /// The first of the scopes the use is expanded for; declarations are
    /// taken from that one only.
    first: bool,
    found: Found,
    /// An initialized object the expansion declares: its type's name and
    /// the names in the initializer.
    stored: Option<(String, Vec<(Place, String)>)>,
}

/// Where an expansion holds: which translation unit (as an index into the
/// file's engines) and which definitions were chosen there.
type Contexts = Vec<(usize, Choice)>;

fn add_name(named: &mut Vec<(String, Contexts)>, name: &str, context: (usize, Choice)) {
    match named.iter_mut().find(|x| x.0 == name) {
        Some(x) if x.1.len() < MAX_CONTEXTS => x.1.push(context),
        Some(_) => {}
        None => named.push((name.to_string(), vec![context])),
    }
}

fn expand_file(
    fi: usize,
    f: &FileFacts,
    table: &Table,
    fwd: &[Vec<u32>],
    sure_fwd: &[Vec<u32>],
    rev: &[Vec<u32>],
) -> Option<(FileFacts, u64, u64)> {
    if f.sites.is_empty() && f.inits.is_empty() {
        return None;
    }
    // One engine per translation unit the file is in, so that what one macro
    // expands to agrees with what the others do there. A header in too many
    // gets a single engine that sees them all at once.
    let units = roots(fi as u32, table.files, rev);
    let make_engines = || -> Vec<Engine> {
        if units.len() <= MAX_UNITS {
            units.iter().map(|&r| Engine { sure: closure(r, sure_fwd), ..Engine::new(table, closure(r, fwd)) }).collect()
        } else {
            vec![Engine { merged: Some(fi as u32), ..Engine::new(table, visible(fi as u32, fwd, rev)) }]
        }
    };
    let t_setup = std::time::Instant::now();
    let mut engines = make_engines();
    let mut out = f.clone();
    let mut names = Names { ids: (0..f.n_names()).map(|i| (f.name(i).to_string(), i as u32)).collect() };
    let (n_defs, n_refs) = (f.defs.len(), f.refs.len());
    let live: Vec<&Site> =
        f.sites.iter().filter(|s| engines.iter_mut().any(|e| e.hides(f.name(s.name as usize)))).collect();

    spend(2, t_setup);
    // 1. Definitions written under a macro's name: one per expansion.
    let mut variants: FxHashMap<u32, Vec<(u32, Contexts)>> = FxHashMap::default();
    for s in live.iter().filter(|s| s.def != NONE) {
        let toks = site_tokens(f, s);
        let macro_name = f.name(s.name as usize);
        let mut named: Vec<(String, Contexts)> = Vec::new();
        for (ei, eng) in engines.iter_mut().enumerate() {
            // Where the name is not a macro, or may not be, it is the name.
            if (s.args == NONE || s.flags & site::TRAILING != 0) && eng.maybe_undefined(macro_name) {
                add_name(&mut named, macro_name, (ei, Choice::default()));
            }
            let mut known = FxHashSet::default();
            let novel = |exp: &[T]| recognise(exp, s, macro_name).novel(&mut known);
            for (exp, choice) in eng.variants(&toks, &Choice::default(), novel) {
                if let Some(n) = recognise(&exp, s, macro_name).def_name() {
                    add_name(&mut named, n, (ei, choice));
                }
            }
        }
        if named.iter().all(|x| x.0 == macro_name) {
            continue;
        }
        let base = out.defs[s.def as usize];
        for (i, (n, contexts)) in named.into_iter().enumerate() {
            let name = names.id(&mut out, &n);
            let idx = if i == 0 {
                out.defs[s.def as usize].name = name;
                s.def
            } else {
                out.defs.push(Def { name, ..base });
                (out.defs.len() - 1) as u32
            };
            variants.entry(s.def).or_default().push((idx, contexts));
        }
    }

    // 2. Each of those has the whole body.
    let hinted: FxHashMap<u32, u32> = f.ref_hints.iter().copied().collect();
    for (i, r) in f.refs.iter().enumerate() {
        if let Some(v) = variants.get(&r.scope) {
            for &(scope, _) in &v[1..] {
                out.refs.push(Ref { scope, ..*r });
                if let Some(&h) = hinted.get(&(i as u32)) {
                    out.ref_hints.push((out.refs.len() as u32 - 1, h));
                }
            }
        }
    }

    // 3. Every other use: the names its expansions make. Inside one of the
    //    definitions above, only where that definition has that name.
    let everywhere: Contexts = (0..engines.len()).map(|e| (e, Choice::default())).collect();
    let read = |engines: &mut [Engine], sites: &[&Site]| -> Vec<Made> {
        let mut made = Vec::new();
        // A macro whose first uses here all expand to nothing of interest
        // (a table row, typically) is not tried again.
        let mut barren: FxHashMap<u32, u32> = FxHashMap::default();
        for s in sites {
            if barren.get(&s.name).is_some_and(|&n| n >= BARREN_USES) {
                continue;
            }
            let mut fruitful = false;
            let toks = site_tokens(f, s);
            let macro_name = f.name(s.name as usize);
            let scopes: Vec<(u32, &Contexts)> = match variants.get(&s.scope) {
                Some(v) => v.iter().map(|(scope, contexts)| (*scope, contexts)).collect(),
                None => vec![(s.scope, &everywhere)],
            };
            for (i, (scope, contexts)) in scopes.into_iter().enumerate() {
                for (ei, pinned) in contexts {
                    let maybe = engines[*ei].maybe_undefined(macro_name);
                    let mut known = FxHashSet::default();
                    let novel = |exp: &[T]| recognise(exp, s, macro_name).novel(&mut known);
                    for (exp, _) in engines[*ei].variants(&toks, pinned, novel) {
                        let found = recognise(&exp, s, macro_name);
                        // A macro that declares a whole initialized object.
                        let stored = declared_initializer(&exp)
                            .filter(|_| s.flags & site::TOP != 0)
                            .map(|(type_name, array, at)| (type_name, init_names(&exp[at..], array)));
                        if found.is_empty() && stored.is_none() {
                            continue;
                        }
                        fruitful = true;
                        made.push(Made { line: s.line, scope, maybe, first: i == 0, found, stored });
                    }
                }
            }
            let n = barren.entry(s.name).or_default();
            *n = if fruitful { 0 } else { *n + 1 };
        }
        made
    };
    let rest: Vec<&Site> = live.iter().copied().filter(|s| s.def == NONE).collect();
    let made: Vec<Made> = if rest.len() <= PARALLEL_USES {
        read(&mut engines, &rest)
    } else {
        // A table of thousands of rows: each stretch with engines of its own.
        rest.par_chunks(PARALLEL_USES).flat_map_iter(|chunk| read(&mut make_engines(), chunk)).collect()
    };
    // A prototype and the definition it announces are two things.
    let key = |d: &Def| (d.name, d.line, d.kind, d.flags & flag::DECL);
    let mut seen_defs: FxHashSet<(u32, u32, u8, u8)> = out.defs.iter().map(key).collect();
    let mut seen_refs: FxHashSet<(u32, u32, u32, u8)> = FxHashSet::default();
    // Definitions added by expansions: (name, line) -> index.
    let mut made_here: FxHashMap<(u32, u32), u32> = FxHashMap::default();
    let mut seen_binds: FxHashSet<Bind> = FxHashSet::default();
    for m in made {
        for (n, kind, flags) in m.found.decls {
            let name = names.id(&mut out, &n);
            let def = Def { name, line: m.line, end_line: m.line, kind, flags };
            if m.first && seen_defs.insert(key(&def)) {
                made_here.insert((name, m.line), out.defs.len() as u32);
                out.defs.push(def);
            }
        }
        for (n, kind, owner) in m.found.refs {
            let name = names.id(&mut out, &n);
            // Inside a function the expansion defines, that function is the scope.
            let scope = owner
                .and_then(|o| made_here.get(&(names.id(&mut out, &o), m.line)).copied())
                .unwrap_or(m.scope);
            let kind = kind | if m.maybe { refkind::MAYBE } else { 0 };
            if seen_refs.insert((name, m.line, scope, kind)) {
                out.refs.push(Ref { name, line: m.line, scope, kind });
            }
        }
        if let Some((type_name, stored)) = m.stored {
            let type_name = names.id(&mut out, &type_name);
            for (place, func) in stored {
                let func = names.id(&mut out, &func);
                let (slot, index) = match place {
                    Place::Member(m) => (names.id(&mut out, &m), NONE),
                    Place::Index(i) => (type_name, i),
                };
                let bind = Bind { slot, index, func, line: m.line, scope: m.scope, var: false, unit: NONE, hint: NONE };
                if seen_binds.insert(bind) {
                    out.binds.push(bind);
                }
            }
        }
    }

    // 4. Initializers with macros in them: now that they can be expanded,
    //    which member each name initializes.
    for init in &f.inits {
        let text = f.name(init.text as usize);
        let conditional = text.split('\n').any(|l| l.trim_start().starts_with('#'));
        let readings: Vec<Vec<T>> = if conditional {
            [true, false].iter().map(|&first| tokenize(one_branch(text, first).as_bytes(), Origin::Site)).collect()
        } else {
            vec![tokenize(text.as_bytes(), Origin::Site)]
        };
        // (member, name, the unit it was read in)
        let mut known: FxHashSet<(Place, String, u32)> = FxHashSet::default();
        let mut read = |eng: &mut Engine, unit: u32| {
            // A whole table at a time: a few readings have to do.
            eng.max_variants = MAX_INIT_VARIANTS;
            for toks in &readings {
                let novel = |exp: &[T]| {
                    init_names(exp, init.array).into_iter().fold(false, |new, (place, name)| known.insert((place, name, unit)) || new)
                };
                eng.variants(toks, &Choice::default(), novel);
            }
            eng.max_variants = MAX_VARIANTS;
        };
        if units.len() <= MAX_UNITS {
            for (eng, &unit) in engines.iter_mut().zip(&units) {
                read(eng, unit);
            }
        } else {
            // What is stored where differs in every unit and all of it
            // counts, so here each unit is worth its own engine.
            for &unit in units.iter().take(MAX_INIT_UNITS) {
                read(&mut Engine::new(table, closure(unit, fwd)), unit);
            }
        }
        for (place, func, unit) in known {
            let func = names.id(&mut out, &func);
            let (slot, index) = match place {
                Place::Member(m) => (names.id(&mut out, &m), NONE),
                Place::Index(_) if init.type_name == NONE => continue,
                Place::Index(i) => (init.type_name, i),
            };
            let bind = Bind { slot, index, func, line: init.line, scope: init.scope, var: false, unit, hint: NONE };
            if seen_binds.insert(bind) {
                out.binds.push(bind);
            }
        }
    }

    // Nothing came of it (the usual case: most uses expand to nothing that
    // names anything): the file stands as it was, and no second copy of
    // its facts is kept.
    let same = out.defs.len() == f.defs.len()
        && out.refs.len() == f.refs.len()
        && out.binds.len() == f.binds.len()
        && out.ref_hints.len() == f.ref_hints.len()
        && out.defs.iter().zip(&f.defs).all(|(a, b)| a.name == b.name);
    if same {
        return None;
    }

    // Keep the definitions in line order, as the link expects.
    let mut order: Vec<u32> = (1..out.defs.len() as u32).collect();
    order.sort_by_key(|&i| out.defs[i as usize].line);
    let mut new_index = vec![0u32; out.defs.len()];
    let mut defs = vec![out.defs[0]];
    for &old in &order {
        new_index[old as usize] = defs.len() as u32;
        defs.push(out.defs[old as usize]);
    }
    out.defs = defs;
    for r in &mut out.refs {
        r.scope = new_index[r.scope as usize];
    }
    for b in &mut out.binds {
        b.scope = new_index[b.scope as usize];
    }
    for t in &mut out.field_types {
        t.0 = new_index[t.0 as usize];
    }
    for c in &mut out.param_calls {
        c.scope = new_index[c.scope as usize];
    }
    for t in &mut out.typed {
        t.0 = new_index[t.0 as usize];
    }
    for d in out.records.iter_mut().flat_map(|r| r.defs.iter_mut()).filter(|d| **d != NONE) {
        *d = new_index[*d as usize];
    }
    // The uses are expanded: the link has no use for them, nor for the
    // initializers that were read.
    out.sites = Vec::new();
    out.inits = Vec::new();
    out.shrink();
    let added = ((out.defs.len() - n_defs) as u64, (out.refs.len() - n_refs) as u64);
    Some((out, added.0, added.1))
}

/// Expansions kept from one link to the next. What a file expands to
/// depends on the file and on every macro body and include edge of the
/// corpus; while the latter stand (the usual edit touches neither), only
/// changed files are expanded again.
#[derive(Default)]
pub struct Cache {
    /// Digest of all macro bodies and include edges.
    environment: u64,
    /// Per file: its content hash when expanded, the replacement if it had
    /// anything to expand, and the definitions and references that added.
    hashes: Vec<u64>,
    pub(crate) expanded: Vec<Option<FileFacts>>,
    added: Vec<(u64, u64)>,
    /// Files expanded anew since this was last asked (`take_changed`).
    changed: Vec<usize>,
    /// Counts the times expansions were made; `made[i]` is the count when
    /// file `i` was last expanded, so that with the file's hash it says
    /// whether what the link sees of the file is what it saw before.
    epoch: u64,
    pub(crate) made: Vec<u64>,
    /// The link's table of names, kept from one link to the next.
    pub(crate) names: NameCache,
}

/// Every name of every file under one number, kept between links: giving
/// millions of names their numbers again at every edit is most of what a
/// link costs in a large tree, and an edit changes one file's.
#[derive(Default)]
pub(crate) struct NameCache {
    /// The names' text, in a few large blocks rather than one allocation
    /// each: there are millions. A block is never moved or freed while
    /// this lives, which is what lets `ids` and `strings` point into them.
    blocks: Vec<Vec<u8>>,
    ids: FxHashMap<&'static str, u32>,
    strings: Vec<&'static str>,
    /// How many files use each name.
    uses: Vec<u32>,
    /// Per file: its hash and expansion count when read, and its names.
    files: Vec<(u64, u64, Vec<u32>)>,
    /// The names in use in lexicographic order, and each name's place in it.
    order: Vec<u32>,
    rank: Vec<u32>,
    stale: bool,
}

impl NameCache {
    /// A copy of `name` that lives as long as this does.
    fn keep(&mut self, name: &str) -> &'static str {
        const BLOCK: usize = 4 << 20;
        if self.blocks.last().is_none_or(|b| b.capacity() - b.len() < name.len()) {
            self.blocks.push(Vec::with_capacity(BLOCK.max(name.len())));
        }
        let block = self.blocks.last_mut().unwrap();
        let at = block.len();
        block.extend_from_slice(name.as_bytes());
        // SAFETY: the bytes are a copy of a `str`; the block was made with
        // room for them, so the push did not move it, and blocks are only
        // dropped with `self`, together with `ids` and `strings` (which
        // hold these references and hand them out no longer than `&self`).
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(block.as_ptr().add(at), name.len())) }
    }

    /// The table for `files` as they are now: for each file, the global
    /// number of each of its names, a name's number being its
    /// lexicographic rank; and the names in that order.
    pub(crate) fn table(&mut self, files: &[&FileFacts], made: &[u64]) -> (Vec<Vec<u32>>, Vec<&str>) {
        if self.files.len() != files.len() {
            *self = NameCache { files: vec![(0, 0, Vec::new()); files.len()], stale: true, ..NameCache::default() };
        }
        for (i, f) in files.iter().enumerate() {
            let at = made.get(i).copied().unwrap_or(0);
            let known = &self.files[i];
            if known.0 == f.hash && known.1 == at && known.2.len() == f.n_names() && at != 0 {
                continue;
            }
            for &id in &self.files[i].2 {
                self.uses[id as usize] -= 1;
                self.stale |= self.uses[id as usize] == 0;
            }
            let mut ids = Vec::with_capacity(f.n_names());
            for n in 0..f.n_names() {
                let name = f.name(n);
                let id = match self.ids.get(name) {
                    Some(&id) => id,
                    None => {
                        let id = self.strings.len() as u32;
                        let kept = self.keep(name);
                        self.ids.insert(kept, id);
                        self.strings.push(kept);
                        self.uses.push(0);
                        id
                    }
                };
                self.stale |= self.uses[id as usize] == 0;
                self.uses[id as usize] += 1;
                ids.push(id);
            }
            self.files[i] = (f.hash, at, ids);
        }
        if std::mem::take(&mut self.stale) {
            let (strings, uses) = (&self.strings, &self.uses);
            self.order = (0..strings.len() as u32).filter(|&id| uses[id as usize] > 0).collect();
            self.order.par_sort_unstable_by(|&a, &b| strings[a as usize].cmp(strings[b as usize]));
            self.rank = vec![NONE; strings.len()];
            for (place, &id) in self.order.iter().enumerate() {
                self.rank[id as usize] = place as u32;
            }
        }
        let rank = &self.rank;
        let remap = self.files.par_iter().map(|(_, _, ids)| ids.iter().map(|&id| rank[id as usize]).collect()).collect();
        (remap, self.order.iter().map(|&id| self.strings[id as usize]).collect())
    }
}

/// What of a [`Cache`] is one line per file: kept on disk between runs,
/// beside each file's expansion.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CacheIndex {
    pub environment: u64,
    pub hashes: Vec<u64>,
    pub added: Vec<(u64, u64)>,
    /// Whether the file has an expansion saved.
    pub has: Vec<bool>,
}

impl Cache {
    pub fn index(&self) -> CacheIndex {
        CacheIndex {
            environment: self.environment,
            hashes: self.hashes.clone(),
            added: self.added.clone(),
            has: self.expanded.iter().map(Option::is_some).collect(),
        }
    }

    /// A cache as it was saved: `expanded` holds each file's expansion, or
    /// `Err` where one that should be there could not be read (that file
    /// is then expanded again).
    pub fn restored(index: CacheIndex, expanded: Vec<Result<Option<FileFacts>, ()>>) -> Cache {
        let mut hashes = index.hashes;
        let expanded = expanded
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                e.unwrap_or_else(|()| {
                    hashes[i] = !hashes[i];
                    None
                })
            })
            .collect();
        let made = vec![1; hashes.len()];
        Cache { environment: index.environment, hashes, expanded, added: index.added, changed: Vec::new(), epoch: 1, made, names: NameCache::default() }
    }

    /// The files expanded anew since the last call, for saving.
    pub fn take_changed(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.changed)
    }

    pub fn expansion(&self, i: usize) -> Option<&FileFacts> {
        self.expanded.get(i)?.as_ref()
    }

    /// Expand file `i` again at the next link even if its hash is the same.
    pub fn forget(&mut self, i: usize) {
        if let Some(h) = self.hashes.get_mut(i) {
            *h = !*h;
        }
    }
}

fn sure_digest(edges: &[Vec<u32>]) -> u64 {
    edges.iter().fold(7, |h, e| xxhash_rust::xxh3::xxh3_64_with_seed(bytemuck::cast_slice(e), h))
}

fn environment(files: &[FileFacts], includes: &[Vec<u32>]) -> u64 {
    use xxhash_rust::xxh3::xxh3_64_with_seed as mix;
    let mut h = files.len() as u64;
    for (f, edges) in files.iter().zip(includes) {
        h = mix(f.path.as_bytes(), h);
        h = mix(bytemuck::cast_slice(edges), h);
        for m in &f.macros {
            h = mix(f.name(m.name as usize).as_bytes(), h);
            h = mix(f.name(m.text as usize).as_bytes(), h);
            h = mix(guard_text(f, m).as_bytes(), h ^ (m.function_like as u64 | (m.conditional as u64) << 1 | (m.fallback as u64) << 2));
        }
    }
    h
}

/// Expand the macro uses recorded in `files` into `cache.expanded`: a
/// replacement for each file that had any. Returns (uses, definitions added,
/// references added).
pub fn expand(files: &[FileFacts], includes: &[Vec<u32>], sure_includes: &[Vec<u32>], cache: &mut Cache) -> (u64, u64, u64) {
    let sites: u64 = files.iter().map(|f| (f.sites.len() + f.inits.len()) as u64).sum();
    let environment = environment(files, includes) ^ environment(&[], sure_includes).rotate_left(17) ^ sure_digest(sure_includes);
    if cache.environment != environment || cache.hashes.len() != files.len() {
        *cache = Cache {
            environment,
            // No file hashes to its own complement, so every file is stale.
            hashes: files.iter().map(|f| !f.hash).collect(),
            expanded: files.iter().map(|_| None).collect(),
            added: vec![(0, 0); files.len()],
            changed: Vec::new(),
            epoch: cache.epoch,
            made: vec![0; files.len()],
            names: std::mem::take(&mut cache.names),
        };
    }
    let stale: Vec<usize> = (0..files.len()).filter(|&i| cache.hashes[i] != files[i].hash).collect();
    if sites > 0 && !stale.is_empty() {
        let mut by_name: FxHashMap<&str, Vec<(u32, u32)>> = FxHashMap::default();
        for (fi, f) in files.iter().enumerate() {
            for (i, m) in f.macros.iter().enumerate() {
                by_name.entry(f.name(m.name as usize)).or_default().push((fi as u32, i as u32));
            }
        }
        let parsed: Vec<Vec<Parsed>> = files
            .par_iter()
            .enumerate()
            .map(|(fi, f)| {
                let one = |m: &crate::model::MacroBody| Parsed {
                    conditional: m.conditional,
                    fallback: m.fallback,
                    file: fi as u32,
                    ..parse(f.name(m.text as usize), m.function_like, guard_text(f, m))
                };
                f.macros.iter().map(one).collect()
            })
            .collect();
        let table = Table::new(files, by_name, parsed);
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); files.len()];
        for (from, tos) in includes.iter().enumerate() {
            for &to in tos {
                rev[to as usize].push(from as u32);
            }
        }
        let profile = std::env::var_os("SI_EXPAND_PROFILE").is_some();
        let spent: Vec<std::sync::atomic::AtomicU64> = if profile { files.iter().map(|_| Default::default()).collect() } else { Vec::new() };
        let done: Vec<Option<(FileFacts, u64, u64)>> = stale
            .par_iter()
            .map(|&fi| {
                let t = std::time::Instant::now();
                let out = expand_file(fi, &files[fi], &table, includes, sure_includes, &rev);
                if profile {
                    spent[fi].store(t.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
                }
                out
            })
            .collect();
        if profile {
            let mut all: Vec<(u64, usize)> = spent.iter().enumerate().map(|(i, s)| (s.load(std::sync::atomic::Ordering::Relaxed), i)).collect();
            all.sort_unstable_by(|a, b| b.cmp(a));
            let total: u64 = all.iter().map(|a| a.0).sum();
            let headers: u64 = all.iter().filter(|a| files[a.1].path.ends_with(".h")).map(|a| a.0).sum();
            let at = |i: usize| SPENT[i].load(std::sync::atomic::Ordering::Relaxed);
            eprintln!("  expanding {:.1} s, recognising new {:.1} s, engines and live {:.1} s; {} runs, {} tokens out", at(0) as f64 / 1e9, at(1) as f64 / 1e9, at(2) as f64 / 1e9, at(4), at(5));
            eprintln!("expand: {:.1} cpu-s in all, {:.1} in headers; top 1% of files {:.1}", total as f64 / 1e6, headers as f64 / 1e6, all.iter().take(all.len() / 100).map(|a| a.0).sum::<u64>() as f64 / 1e6);
            for (us, i) in all.iter().take(15) {
                let f = &files[*i];
                eprintln!("  {:8.1} ms  {} sites {} inits {} rev {}", *us as f64 / 1e3, f.path, f.sites.len(), f.inits.len(), rev[*i].len());
            }
        }
        for (&fi, d) in stale.iter().zip(done) {
            (cache.expanded[fi], cache.added[fi]) = match d {
                Some((f, defs, refs)) => (Some(f), (defs, refs)),
                None => (None, (0, 0)),
            };
        }
    }
    cache.epoch += 1;
    for &i in &stale {
        cache.hashes[i] = files[i].hash;
        cache.made[i] = cache.epoch;
    }
    cache.changed.extend(stale);
    let (defs, refs) = cache.added.iter().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    (sites, defs, refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(toks: &[T]) -> String {
        toks.iter().map(|t| &*t.text).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn what_hides_names() {
        let p: [&[u8]; 2] = [b"x", b"y"];
        for (body, want) in [
            ("x ## _ ## y", true),
            ("static const char *x (unsigned long rtype) { switch (rtype) {", true),
            ("enum x {", true),
            ("x = y,", true),
            ("x,", true),
            ("x (a) x (b)", true),
            ("struct mutex x = INIT (x)", true),
            ("((x) > (y) ? (x) : (y))", false),
            ("printf (x, ## y)", false),
            ("return x;", false),
            ("do { x = y; } while (0)", false),
            ("case y: return #x;", false),
        ] {
            assert_eq!(hides_names(body.as_bytes(), &p), want, "{body}");
        }
    }

    #[test]
    fn pasting_stringifying_and_rescanning() {
        let macros = [
            ("CAT", "(a,b) XCAT (a, b)", true),
            ("XCAT", "(a,b) a ## b", true),
            ("SIZE", " 32", false),
            ("NAME", "(x) CAT (x, SIZE)", true),
            ("STR", "(x) #x", true),
            ("LOG", "(fmt, ...) printf (fmt, ## __VA_ARGS__)", true),
            ("SELF", " SELF + 1", false),
        ];
        let mut f = FileFacts::new("a.c".into());
        for (name, text, function_like) in macros {
            let (name, text) = (f.push_name(name), f.push_name(text));
            f.macros.push(crate::model::MacroBody { name, text, function_like, guard: NONE, conditional: false, fallback: false });
        }
        let files = [f];
        let parsed = vec![files[0].macros.iter().map(|m| parse(files[0].name(m.text as usize), m.function_like, "")).collect()];
        let mut by_name: FxHashMap<&str, Vec<(u32, u32)>> = FxHashMap::default();
        for (i, m) in files[0].macros.iter().enumerate() {
            by_name.entry(files[0].name(m.name as usize)).or_default().push((0, i as u32));
        }
        let table = Table::new(&files, by_name, parsed);
        let mut eng = Engine::new(&table, vec![true]);
        for (src, want) in [
            ("NAME (elf)", "elf32"),
            ("XCAT (elf, SIZE)", "elfSIZE"),
            ("STR (a b)", "\"\""),
            ("LOG (\"x\")", "printf ( \"x\" )"),
            ("LOG (\"x\", 1, 2)", "printf ( \"x\" , 1 , 2 )"),
            ("SELF", "SELF + 1"),
            ("CAT", "CAT"),
        ] {
            let v = eng.variants(&tokenize(src.as_bytes(), Origin::Site), &Choice::default(), |_| true);
            assert_eq!(texts(&v[0].0), want, "{src}");
        }
    }
}
