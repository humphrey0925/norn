//! Lexical pre-pass run before tree-sitter. C is only parseable after
//! preprocessing; rather than preprocess (which needs a build configuration),
//! this pass removes the three things that most often derail the parser and
//! reports what it removed so nothing is lost from the index:
//!
//! * `#define` directives: recorded here (name, extent, references in the
//!   body) and blanked, so macro bodies are indexed instead of being opaque.
//! * *Transparent* macros (ones that expand to nothing or to an attribute,
//!   like `ATTRIBUTE_UNUSED`): blanked where used, and reported as references.
//! * Other directives in places the grammar cannot take them (inside an
//!   initializer or a parenthesized list): blanked, so both branches of an
//!   `#ifdef` there are parsed as one list.
//! * Macro arguments that are bare operators (`BOP (LSHIFT, <<)`), which can
//!   never be expressions: replaced by `0`.
//! * String literals continued with a backslash-newline, which the grammar
//!   rejects: rewritten as adjacent literals of the same length.
//!
//! Blanking overwrites bytes with spaces and keeps newlines, so every byte
//! offset and line number in the masked text matches the original.

use rustc_hash::FxHashMap;

/// An identifier occurrence found lexically.
#[derive(Clone, Copy, Debug)]
pub struct Tok {
    pub start: u32,
    pub end: u32,
    pub line: u32,
    /// Directly followed by `(`.
    pub call: bool,
}

#[derive(Clone, Debug)]
pub struct Macro {
    pub name: Tok,
    pub end_line: u32,
    /// References in the body, parameters excluded.
    pub refs: Vec<Tok>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Macro(Macro),
    /// A reference that was blanked out of the parser's view.
    Ref(Tok),
}

impl Event {
    pub fn pos(&self) -> usize {
        match self {
            Event::Macro(m) => m.name.start as usize,
            Event::Ref(t) => t.start as usize,
        }
    }
}

#[derive(Default)]
pub struct Lexed {
    pub masked: Vec<u8>,
    /// In source order.
    pub events: Vec<Event>,
    /// `#include` operands, quotes or angle brackets stripped.
    pub includes: Vec<(u32, u32)>,
    /// Top-level `{ ... }` blocks by bracket matching alone: (start byte, end
    /// byte, last line). These are function bodies, and stay right where the
    /// parser's error recovery cuts a function short.
    pub blocks: Vec<(u32, u32, u32)>,
}

/// A `#define` reduced to what decides transparency: `deps` is `Some` when
/// the body is nothing but attributes and invocations of the listed macros.
pub struct Define {
    pub name: Vec<u8>,
    pub function_like: bool,
    pub deps: Option<Vec<Vec<u8>>>,
}

/// Macros that can be deleted from the token stream without changing how the
/// surrounding code parses. Value: whether the macro takes arguments.
#[derive(Default)]
pub struct Transparent(FxHashMap<Vec<u8>, bool>);

const ATTRIBUTE_KEYWORDS: [&[u8]; 5] = [b"__attribute__", b"__attribute", b"__declspec", b"__extension__", b"_Noreturn"];

impl Transparent {
    /// A macro is transparent when every one of its definitions is, which is
    /// a fixed point because attribute macros are defined in terms of others.
    pub fn from_defines(defines: impl IntoIterator<Item = Define>) -> Self {
        let mut by_name: FxHashMap<Vec<u8>, (bool, Vec<Option<Vec<Vec<u8>>>>)> = FxHashMap::default();
        for d in defines {
            // `#define inline` and the like: the keyword still matters to us.
            if is_keyword(&d.name) {
                continue;
            }
            let e = by_name.entry(d.name).or_default();
            e.0 |= d.function_like;
            e.1.push(d.deps);
        }
        let mut set: FxHashMap<Vec<u8>, bool> = FxHashMap::default();
        loop {
            let mut added = Vec::new();
            for (name, (function_like, bodies)) in &by_name {
                let ok = |b: &Option<Vec<Vec<u8>>>| b.as_ref().is_some_and(|deps| deps.iter().all(|d| set.contains_key(d)));
                if !set.contains_key(name) && bodies.iter().all(ok) {
                    added.push((name.clone(), *function_like));
                }
            }
            if added.is_empty() {
                break;
            }
            set.extend(added);
        }
        Transparent(set)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, name: &[u8]) -> bool {
        self.0.contains_key(name)
    }

    /// Order-independent digest, so cached facts can be keyed on it.
    pub fn digest(&self) -> u64 {
        self.0.iter().fold(0, |acc, (k, v)| acc ^ xxhash_rust::xxh3::xxh3_64_with_seed(k, *v as u64))
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_keyword(s: &[u8]) -> bool {
    matches!(
        s,
        b"auto" | b"break" | b"case" | b"char" | b"const" | b"continue" | b"default" | b"do" | b"double" | b"else"
            | b"enum" | b"extern" | b"float" | b"for" | b"goto" | b"if" | b"inline" | b"int" | b"long" | b"register"
            | b"restrict" | b"return" | b"short" | b"signed" | b"sizeof" | b"static" | b"struct" | b"switch"
            | b"typedef" | b"union" | b"unsigned" | b"void" | b"volatile" | b"while" | b"defined" | b"__VA_ARGS__"
            | b"_Bool" | b"bool" | b"true" | b"false" | b"typeof" | b"__typeof__" | b"__extension__" | b"__attribute__"
            | b"__attribute" | b"__inline" | b"__inline__" | b"__asm__" | b"asm" | b"__restrict" | b"_Noreturn"
            | b"_Static_assert" | b"__builtin_va_arg" | b"__func__" | b"__FILE__" | b"__LINE__"
    )
}

/// Low-level cursor over C source: knows comments, literals and line splices.
struct Cur<'a> {
    s: &'a [u8],
    i: usize,
    line: u32,
}

impl<'a> Cur<'a> {
    fn peek(&self, k: usize) -> u8 {
        *self.s.get(self.i + k).unwrap_or(&0)
    }

    fn bump(&mut self) {
        if self.s[self.i] == b'\n' {
            self.line += 1;
        }
        self.i += 1;
    }

    /// At `/*` or `//`: skip the comment. A line comment stops before its newline.
    fn skip_comment(&mut self) -> bool {
        if self.peek(0) != b'/' {
            return false;
        }
        match self.peek(1) {
            b'*' => {
                self.i += 2;
                while self.i < self.s.len() && !(self.peek(0) == b'*' && self.peek(1) == b'/') {
                    self.bump();
                }
                self.i = (self.i + 2).min(self.s.len());
                true
            }
            b'/' => {
                while self.i < self.s.len() && self.peek(0) != b'\n' {
                    self.i += 1;
                }
                true
            }
            _ => false,
        }
    }

    /// At a quote: skip the literal. Never runs past the end of the line, so
    /// a stray apostrophe (in `#error` text, say) cannot swallow the file.
    fn skip_literal(&mut self) {
        let quote = self.s[self.i];
        self.i += 1;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'\\' if self.i + 1 < self.s.len() => {
                    self.bump();
                    self.bump();
                }
                b'\n' => return,
                c => {
                    self.i += 1;
                    if c == quote {
                        return;
                    }
                }
            }
        }
    }

    fn skip_number(&mut self) {
        while self.i < self.s.len() && (is_ident(self.s[self.i]) || self.s[self.i] == b'.') {
            self.i += 1;
        }
    }

    fn ident(&mut self) -> (usize, usize) {
        let start = self.i;
        while self.i < self.s.len() && is_ident(self.s[self.i]) {
            self.i += 1;
        }
        (start, self.i)
    }

    /// Skip spaces, comments and line splices; with `newlines`, line ends too.
    fn skip_space(&mut self, newlines: bool) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\r' | 0x0c => self.i += 1,
                b'\n' if newlines => self.bump(),
                b'\\' if matches!(self.peek(1), b'\n' | b'\r') => {
                    self.i += 1;
                    while self.peek(0) == b'\r' {
                        self.i += 1;
                    }
                    if self.peek(0) == b'\n' {
                        self.bump();
                    }
                }
                b'/' if matches!(self.peek(1), b'*' | b'/') => {
                    self.skip_comment();
                }
                _ => return,
            }
        }
    }

    /// From inside a directive: advance to the newline that ends it.
    fn skip_directive(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'\n' => return,
                b'\\' if matches!(self.peek(1), b'\n' | b'\r') => self.skip_space(false),
                b'/' if matches!(self.peek(1), b'*' | b'/') => {
                    self.skip_comment();
                }
                b'"' | b'\'' => self.skip_literal(),
                _ => self.i += 1,
            }
        }
    }

    /// At `(`: advance past the matching `)`. False if there is none.
    fn skip_parens(&mut self) -> bool {
        let mut depth = 0u32;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'(' => {
                    depth += 1;
                    self.i += 1;
                }
                b')' => {
                    depth -= 1;
                    self.i += 1;
                    if depth == 0 {
                        return true;
                    }
                }
                b'"' | b'\'' => self.skip_literal(),
                b'/' if matches!(self.peek(1), b'*' | b'/') => {
                    self.skip_comment();
                }
                _ => self.bump(),
            }
        }
        false
    }
}

/// Identifier references in `s[start..end]`, as a macro body or macro
/// arguments would contain them. Skips member names, pasted and stringified
/// tokens, keywords and `params`.
fn scan_refs(s: &[u8], start: usize, end: usize, line: u32, params: &[&[u8]], out: &mut Vec<Tok>) {
    let mut c = Cur { s: &s[..end], i: start, line };
    // The previous significant bytes decide whether an identifier is a member
    // name (`.x`, `->x`) or an operand of `#` / `##`.
    let mut prev = [0u8; 2];
    loop {
        c.skip_space(true);
        if c.i >= end {
            return;
        }
        let b = c.s[c.i];
        if is_ident_start(b) {
            let line = c.line;
            let (a, e) = c.ident();
            let text = &s[a..e];
            c.skip_space(true);
            let pasted = c.peek(0) == b'#' && c.peek(1) == b'#';
            let member = prev[1] == b'.' || prev == *b"->";
            if !(member || prev[1] == b'#' || pasted || is_keyword(text) || params.contains(&text)) {
                out.push(Tok { start: a as u32, end: e as u32, line, call: c.peek(0) == b'(' });
            }
            prev = [0, b'a'];
        } else if b.is_ascii_digit() {
            c.skip_number();
            prev = [0, b'0'];
        } else if b == b'"' || b == b'\'' {
            c.skip_literal();
            prev = [0, b'"'];
        } else {
            prev = [prev[1], b];
            c.bump();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Brace {
    Block,
    Init,
}

/// Bracket nesting, which decides whether a directive is somewhere the
/// grammar accepts it.
#[derive(Clone, Default)]
struct Nest {
    /// Open braces, each with the paren depth outside it.
    braces: Vec<(Brace, u32)>,
    parens: u32,
}

impl Nest {
    fn unparseable_here(&self) -> bool {
        self.parens > 0 || self.braces.last().is_some_and(|b| b.0 == Brace::Init)
    }
}

/// `buf` is empty when only the report is wanted.
fn blank(buf: &mut [u8], start: usize, end: usize) {
    if buf.is_empty() {
        return;
    }
    for b in &mut buf[start..end] {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

/// One open `#if` group.
struct Cond {
    nest: Nest,
    /// Its directives so far, to hide them all once one has to be.
    spans: Vec<(usize, usize)>,
    hidden: bool,
}

struct Lexer<'a> {
    c: Cur<'a>,
    transparent: &'a Transparent,
    out: Lexed,
    defines: Option<Vec<Define>>,
    nest: Nest,
    /// Nesting at each open `#if`, restored at `#else` so that alternative
    /// branches do not both count their brackets.
    conds: Vec<Cond>,
    /// Last significant byte, to tell `= {` from a block.
    prev: u8,
    line_start: bool,
    /// The argument being read, while it has held nothing but operators.
    arg: Option<OperatorArg>,
    /// Start of the top-level block being read.
    block: Option<usize>,
}

#[derive(Clone, Copy)]
struct OperatorArg {
    start: usize,
    first_op: usize,
    ops: u32,
    /// `(*)` is an abstract declarator, not an operator argument.
    only_stars: bool,
}

impl<'a> Lexer<'a> {
    fn run(&mut self) {
        let s = self.c.s;
        while self.c.i < s.len() {
            let b = s[self.c.i];
            let col0 = self.c.i == 0 || s[self.c.i - 1] == b'\n';
            match b {
                b'\n' => {
                    self.c.bump();
                    self.line_start = true;
                    continue;
                }
                b' ' | b'\t' | b'\r' | 0x0c => {
                    self.c.i += 1;
                    continue;
                }
                b'/' if matches!(self.c.peek(1), b'*' | b'/') => {
                    self.c.skip_comment();
                    continue;
                }
                b'#' if self.line_start => {
                    self.directive();
                    continue;
                }
                _ => {}
            }
            self.line_start = false;
            if is_ident_start(b) {
                self.arg = None;
                self.identifier();
                continue;
            }
            match (b, &mut self.arg) {
                (b'(' | b')' | b',', _) => {}
                (b'+' | b'-' | b'*' | b'/' | b'%' | b'<' | b'>' | b'=' | b'!' | b'&' | b'|' | b'^' | b'~', Some(a)) => {
                    if a.ops == 0 {
                        a.first_op = self.c.i;
                    }
                    a.ops += 1;
                    a.only_stars &= b == b'*';
                }
                _ => self.arg = None,
            }
            match b {
                b'0'..=b'9' => self.c.skip_number(),
                b'\'' => self.c.skip_literal(),
                b'"' => self.string(),
                b'{' => {
                    // A brace in column 0 opens a function body unless it
                    // continues an initializer; resynchronize in case brackets
                    // were unbalanced.
                    if col0 && !matches!(self.prev, b'=' | b',' | b'{') {
                        self.nest = Nest::default();
                    }
                    let init = self.prev == b'=' || self.nest.braces.last().is_some_and(|x| x.0 == Brace::Init);
                    if self.nest.braces.is_empty() {
                        self.block = (!init).then_some(self.c.i);
                    }
                    self.nest.braces.push((if init { Brace::Init } else { Brace::Block }, self.nest.parens));
                    self.nest.parens = 0;
                    self.c.i += 1;
                }
                b'}' => {
                    if col0 {
                        self.nest = Nest::default();
                    } else if let Some((_, parens)) = self.nest.braces.pop() {
                        self.nest.parens = parens;
                    }
                    self.c.i += 1;
                    if self.nest.braces.is_empty() {
                        if let Some(start) = self.block.take() {
                            self.out.blocks.push((start as u32, self.c.i as u32, self.c.line));
                        }
                    }
                }
                b'(' => {
                    self.nest.parens += 1;
                    self.c.i += 1;
                    self.begin_arg();
                }
                b',' => {
                    self.end_arg();
                    self.c.i += 1;
                    self.begin_arg();
                }
                b')' => {
                    self.end_arg();
                    self.nest.parens = self.nest.parens.saturating_sub(1);
                    self.c.i += 1;
                }
                b'=' => {
                    // Only a plain assignment `=` introduces an initializer.
                    let compare = matches!(self.prev, b'=' | b'!' | b'<' | b'>') || self.c.peek(1) == b'=';
                    self.c.i += 1;
                    if compare {
                        self.c.i += (self.c.peek(0) == b'=') as usize;
                        self.prev = b'?';
                        continue;
                    }
                }
                _ => self.c.i += 1,
            }
            self.prev = b;
        }
    }

    /// At `"`. A literal continued across lines with backslash-newline is
    /// split into adjacent literals, one per line. Only the content changes,
    /// and string content is never indexed.
    fn string(&mut self) {
        let s = self.c.s;
        self.c.i += 1;
        while self.c.i < s.len() {
            match s[self.c.i] {
                b'"' => {
                    self.c.i += 1;
                    return;
                }
                b'\n' => return,
                b'\\' => {
                    let at = self.c.i;
                    self.c.i += 1;
                    while self.c.peek(0) == b'\r' {
                        self.c.i += 1;
                    }
                    if self.c.peek(0) != b'\n' {
                        self.c.i = (at + 2).min(s.len());
                        continue;
                    }
                    self.c.bump();
                    // Close the literal here; reopen it on the next line that
                    // has a byte of content to overwrite.
                    self.put(at, b'"');
                    loop {
                        let k = self.c.i;
                        match (self.c.peek(0), self.c.peek(1)) {
                            // Nothing more: the literal ended with the splice.
                            (0 | b'\n', _) => return,
                            (b'"', _) => {
                                self.put(k, b' ');
                                self.c.i += 1;
                                return;
                            }
                            (b'\\', b'\n' | b'\r') => {
                                self.put(k, b' ');
                                self.c.i += 1;
                                while self.c.peek(0) == b'\r' {
                                    self.c.i += 1;
                                }
                                if self.c.peek(0) == b'\n' {
                                    self.c.bump();
                                }
                            }
                            (first, _) => {
                                self.put(k, b'"');
                                self.c.i += 1;
                                // Do not leave half an escape sequence behind.
                                if first == b'\\' && self.c.i < s.len() {
                                    self.put(k + 1, b' ');
                                    self.c.i += 1;
                                }
                                break;
                            }
                        }
                    }
                }
                _ => self.c.i += 1,
            }
        }
    }

    fn begin_arg(&mut self) {
        self.arg = Some(OperatorArg { start: self.c.i, first_op: 0, ops: 0, only_stars: true });
    }

    /// At the `,` or `)` that ends an argument.
    fn end_arg(&mut self) {
        if let Some(a) = self.arg.take().filter(|a| a.ops > 0 && !a.only_stars) {
            blank(&mut self.out.masked, a.start, self.c.i);
            self.put(a.first_op, b'0');
        }
    }

    fn put(&mut self, at: usize, b: u8) {
        if let Some(slot) = self.out.masked.get_mut(at) {
            *slot = b;
        }
    }

    fn identifier(&mut self) {
        let s = self.c.s;
        let line = self.c.line;
        let (a, e) = self.c.ident();
        self.prev = b'a';
        let Some(&function_like) = self.transparent.0.get(&s[a..e]) else { return };
        let mut tok = Tok { start: a as u32, end: e as u32, line, call: false };
        let mut end = e;
        if function_like {
            let save = (self.c.i, self.c.line);
            self.c.skip_space(true);
            let open = self.c.i;
            if self.c.peek(0) == b'(' && self.c.skip_parens() {
                tok.call = true;
                end = self.c.i;
                self.out.events.push(Event::Ref(tok));
                let mut args = Vec::new();
                scan_refs(s, open + 1, end - 1, line, &[], &mut args);
                self.out.events.extend(args.into_iter().map(Event::Ref));
                blank(&mut self.out.masked, a, end);
                return;
            }
            (self.c.i, self.c.line) = save;
        }
        self.out.events.push(Event::Ref(tok));
        blank(&mut self.out.masked, a, end);
    }

    fn directive(&mut self) {
        let s = self.c.s;
        let start = self.c.i;
        self.c.i += 1;
        self.c.skip_space(false);
        let (a, e) = self.c.ident();
        let word = &s[a..e];
        let mut hide = self.nest.unparseable_here();
        match word {
            b"define" => {
                self.define();
                hide = true;
            }
            b"include" | b"include_next" => {
                self.c.skip_space(false);
                let open = self.c.i;
                self.c.skip_directive();
                let text = &s[open..self.c.i];
                let close = match text.first() {
                    Some(b'"') => b'"',
                    Some(b'<') => b'>',
                    _ => 0,
                };
                if let Some(len) = text.iter().skip(1).position(|&b| b == close) {
                    self.out.includes.push((open as u32 + 1, (open + 1 + len) as u32));
                }
            }
            _ => {}
        }
        self.c.skip_directive();
        let span = (start, self.c.i);
        // A group that cuts a statement in two (`if (x)` / `#endif` / `y;`)
        // is not a statement itself: hide all of it and read every branch.
        let in_block = self.nest.braces.last().is_some_and(|b| b.0 == Brace::Block);
        let cuts = hide || in_block && !matches!(self.prev, b';' | b'{' | b'}' | b':');
        match word {
            b"if" | b"ifdef" | b"ifndef" => {
                self.conds.push(Cond { nest: self.nest.clone(), spans: vec![span], hidden: cuts });
                hide = cuts;
            }
            b"elif" | b"else" | b"elifdef" | b"elifndef" | b"endif" => {
                if let Some(c) = self.conds.last_mut() {
                    if cuts && !c.hidden {
                        c.hidden = true;
                        for &(a, e) in &c.spans {
                            blank(&mut self.out.masked, a, e);
                        }
                    }
                    hide = c.hidden;
                    c.spans.push(span);
                    if word == b"endif" {
                        self.conds.pop();
                    } else {
                        self.nest = c.nest.clone();
                    }
                }
            }
            _ => {}
        }
        if hide {
            blank(&mut self.out.masked, span.0, span.1);
        }
    }

    /// After the word `define`. Leaves the cursor inside the directive.
    fn define(&mut self) {
        let s = self.c.s;
        self.c.skip_space(false);
        if !is_ident_start(self.c.peek(0)) {
            return;
        }
        let line = self.c.line;
        let (a, e) = self.c.ident();
        let mut params: Vec<&[u8]> = Vec::new();
        let function_like = self.c.peek(0) == b'(';
        if function_like {
            self.c.i += 1;
            loop {
                self.c.skip_space(false);
                match self.c.peek(0) {
                    b')' => {
                        self.c.i += 1;
                        break;
                    }
                    b if is_ident_start(b) => {
                        let (pa, pe) = self.c.ident();
                        params.push(&s[pa..pe]);
                    }
                    b'\n' | 0 => break,
                    _ => self.c.i += 1,
                }
            }
        }
        let body = self.c.i;
        self.c.skip_directive();
        let (end, end_line) = (self.c.i, self.c.line);
        let mut refs = Vec::new();
        scan_refs(s, body, end, line, &params, &mut refs);
        if let Some(defines) = &mut self.defines {
            defines.push(Define { name: s[a..e].to_vec(), function_like, deps: transparent_deps(s, body, end, &refs) });
        }
        let name = Tok { start: a as u32, end: e as u32, line, call: function_like };
        self.out.events.push(Event::Macro(Macro { name, end_line, refs }));
    }
}

/// `Some(macros it invokes)` if the body `s[start..end]` is only attributes
/// and macro invocations, each optionally with a parenthesized argument list.
fn transparent_deps(s: &[u8], start: usize, end: usize, refs: &[Tok]) -> Option<Vec<Vec<u8>>> {
    let mut c = Cur { s: &s[..end], i: start, line: 0 };
    let mut deps = Vec::new();
    loop {
        c.skip_space(true);
        if c.i >= end {
            return Some(deps);
        }
        if !is_ident_start(c.s[c.i]) {
            return None;
        }
        let (a, e) = c.ident();
        let word = &s[a..e];
        if !ATTRIBUTE_KEYWORDS.contains(&word) {
            // A name `scan_refs` dropped is a parameter or a keyword: the
            // expansion is then real code, not decoration.
            if !refs.iter().any(|t| t.start as usize == a) {
                return None;
            }
            deps.push(word.to_vec());
        }
        c.skip_space(true);
        if c.peek(0) == b'(' && !c.skip_parens() {
            return None;
        }
    }
}

fn lexer<'a>(src: &'a [u8], transparent: &'a Transparent, mask: bool) -> Lexer<'a> {
    Lexer {
        c: Cur { s: src, i: 0, line: 1 },
        transparent,
        out: Lexed { masked: if mask { src.to_vec() } else { Vec::new() }, ..Default::default() },
        defines: None,
        nest: Nest::default(),
        conds: Vec::new(),
        prev: 0,
        line_start: true,
        arg: None,
        block: None,
    }
}

/// Mask `src` for parsing and report what was hidden.
pub fn lex(src: &[u8], transparent: &Transparent) -> Lexed {
    let mut l = lexer(src, transparent, true);
    l.run();
    l.out
}

/// The `#define`s of one file, for building the [`Transparent`] set.
pub fn defines(src: &[u8]) -> Vec<Define> {
    static NONE: std::sync::OnceLock<Transparent> = std::sync::OnceLock::new();
    let mut l = lexer(src, NONE.get_or_init(Transparent::default), false);
    l.defines = Some(Vec::new());
    l.run();
    l.defines.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transparent(src: &str) -> Transparent {
        Transparent::from_defines(defines(src.as_bytes()))
    }

    fn masked(src: &str, t: &Transparent) -> String {
        String::from_utf8(lex(src.as_bytes(), t).masked).unwrap()
    }

    #[test]
    fn attribute_macros_are_transparent_through_chains() {
        let t = transparent(
            "#define UNUSED __attribute__ ((__unused__))\n#define UNUSED\n#define AU UNUSED\n\
             #define NONNULL(m) __attribute__ ((__nonnull__ (m)))\n#define PRINTF_1 NONNULL(1) AU\n\
             #define ID(x) x\n#define LABEL UNUSED ;\n#define EXTERN extern\n",
        );
        for name in ["UNUSED", "AU", "NONNULL", "PRINTF_1"] {
            assert!(t.contains(name.as_bytes()), "{name}");
        }
        for name in ["ID", "LABEL", "EXTERN"] {
            assert!(!t.contains(name.as_bytes()), "{name}");
        }
    }

    #[test]
    fn blanking_keeps_offsets_and_reports_references() {
        let t = transparent("#define UNUSED\n#define NONNULL(m)\n");
        let src = "int f (bfd *abfd UNUSED) NONNULL (g (1));\n";
        let l = lex(src.as_bytes(), &t);
        assert_eq!(l.masked.len(), src.len());
        assert_eq!(String::from_utf8(l.masked).unwrap().replace(' ', ""), "intf(bfd*abfd);\n");
        let names: Vec<&str> = l.events.iter().map(|e| match e {
            Event::Ref(t) => &src[t.start as usize..t.end as usize],
            Event::Macro(_) => "?",
        }).collect();
        assert_eq!(names, ["UNUSED", "NONNULL", "g"]);
    }

    #[test]
    fn defines_are_recorded_with_body_references() {
        let src = "#define CALL(x, f) \\\n  do_it (x->field, #f, pre##x, helper) /* c */\nint y;\n";
        let l = lex(src.as_bytes(), &Transparent::default());
        assert_eq!(String::from_utf8(l.masked).unwrap().trim_start(), "int y;\n");
        let Event::Macro(m) = &l.events[0] else { panic!() };
        assert_eq!((m.name.line, m.end_line, m.name.call), (1, 2, true));
        let refs: Vec<(&str, bool)> = m.refs.iter().map(|t| (&src[t.start as usize..t.end as usize], t.call)).collect();
        assert_eq!(refs, [("do_it", true), ("helper", false)]);
    }

    #[test]
    fn directives_are_hidden_only_where_the_grammar_rejects_them() {
        let t = Transparent::default();
        let src = "#ifdef A\nint a;\n#endif\nint t[] = {\n#ifdef A\n 1,\n#else\n 2,\n#endif\n};\nvoid f () {\n#ifdef A\n g (1\n#if B\n , 2\n#endif\n );\n#endif\n}\n";
        let m = masked(src, &t);
        let kept: Vec<&str> = m.lines().filter(|l| l.starts_with('#')).collect();
        assert_eq!(kept, ["#ifdef A", "#endif", "#ifdef A", "#endif"]);
        assert_eq!(m.len(), src.len());
    }

    #[test]
    fn a_group_that_cuts_a_statement_is_hidden_whole() {
        let src = "void f ()
{
#ifdef A
  if (!a)
#endif
    a = 1;
#ifdef B
  b ();
#endif
}
";
        let m = masked(src, &Transparent::default());
        let kept: Vec<&str> = m.lines().filter(|l| l.starts_with('#')).collect();
        assert_eq!(kept, ["#ifdef B", "#endif"], "{m}");
    }

    #[test]
    fn alternative_branches_do_not_unbalance_nesting() {
        let src = "#if A\nvoid f (int a\n#else\nvoid f (long a\n#endif\n ) { }\n#ifdef B\nint b;\n#endif\n";
        let m = masked(src, &Transparent::default());
        assert!(m.contains("#ifdef B"), "{m}");
    }

    #[test]
    fn continued_strings_become_adjacent_literals() {
        let t = Transparent::default();
        for (src, want) in [
            ("f (\"ab\\\ncd\\n\");", "f (\"ab\"\n\"d\\n\");"),
            ("f (\"ab\\\n\");", "f (\"ab\"\n );"),
            ("f (\"\\\n\\\"x\");", "f (\"\"\n\" x\");"),
            ("f (\"a\\\r\n\\\r\nb\");", "f (\"a\"\r\n \r\n\"\");"),
            ("f (\"a\\\\\", '\"');", "f (\"a\\\\\", '\"');"),
        ] {
            assert_eq!(masked(src, &t), want, "{src:?}");
        }
    }

    #[test]
    fn table_rows_in_column_zero_stay_inside_the_initializer() {
        let src = "T t[] = {\n{\n#ifdef A\n 1,\n#endif\n 2},\n};\nvoid f ()\n{\n#ifdef A\n g ();\n#endif\n}\n";
        let m = masked(src, &Transparent::default());
        assert_eq!(m.lines().filter(|l| l.starts_with('#')).count(), 2, "{m}");
    }

    #[test]
    fn operator_arguments_become_zero() {
        let t = Transparent::default();
        for (src, want) in [
            ("BOP (LSHIFT, <<);", "BOP (LSHIFT, 0 );"),
            ("BOPN ('<', <);", "BOPN ('<', 0);"),
            ("X (&&, a, ||)", "X (0 , a, 0 )"),
            ("void (*) (int); f (-1, *p, -(x), a - b);", "void (*) (int); f (-1, *p, -(x), a - b);"),
        ] {
            assert_eq!(masked(src, &t), want, "{src:?}");
        }
    }

    #[test]
    fn top_level_blocks_are_function_bodies() {
        let src = "int t[] = { 1 };
void f ()
{
  if (x) { g (); }
}
struct s { int a; };
";
        let l = lex(src.as_bytes(), &Transparent::default());
        let got: Vec<(&str, u32)> = l.blocks.iter().map(|&(a, e, line)| (&src[a as usize..e as usize], line)).collect();
        assert_eq!(got, [("{
  if (x) { g (); }
}", 5), ("{ int a; }", 6)]);
    }

    #[test]
    fn includes_are_collected() {
        let src = "#include \"a/b.h\"\n# include <c.h> /* x */\n";
        let l = lex(src.as_bytes(), &Transparent::default());
        let got: Vec<&str> = l.includes.iter().map(|&(a, e)| &src[a as usize..e as usize]).collect();
        assert_eq!(got, ["a/b.h", "c.h"]);
    }
}
