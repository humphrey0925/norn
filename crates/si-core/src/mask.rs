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

use crate::cond::{Conditions, Tri};
use rustc_hash::{FxHashMap, FxHashSet};

/// An identifier occurrence found lexically.
#[derive(Clone, Copy, Debug)]
pub struct Tok {
    pub start: u32,
    pub end: u32,
    pub line: u32,
    /// Directly followed by `(`.
    pub call: bool,
    /// A member name (`.x`, `->x`); `call` then allows for `(*p->x) (...)`.
    pub member: bool,
}

#[derive(Clone, Debug)]
pub struct Macro {
    pub name: Tok,
    pub end_line: u32,
    /// References in the body, parameters excluded.
    pub refs: Vec<Tok>,
    /// End of the directive: the parameter list and body are the text from
    /// the end of the name to here.
    pub end: u32,
    /// Whether expansion may need this body (see [`MacroSets`]).
    pub keep: bool,
    /// The `#if NAME == value` tests it is under: `NAME == 64;OTHER != 1`.
    pub guard: String,
    /// Inside an `#if` group other than the file's include guard.
    pub conditional: bool,
    /// Directly under `#ifndef` of its own name: a default, in force only
    /// where nothing else defines the name.
    pub fallback: bool,
}

/// A use of an *expanding* macro: one whose expansion holds names that are
/// not written at the use.
#[derive(Clone, Copy, Debug)]
pub struct SiteTok {
    pub name: Tok,
    /// End of the argument list, or 0 if the name stands alone.
    pub args_end: u32,
    /// `model::site` bits known lexically.
    pub flags: u8,
    /// `type NAME (args) = { ... }` at file scope: the braces (0, 0 if
    /// none), the word before the name, and whether `[]` follows the name.
    pub init: (u32, u32),
    pub init_type: (u32, u32),
    pub init_array: bool,
    /// Where the parser sees the name of the definition this use makes,
    /// when not at the use (see `Lexer::site`); 0 when at it.
    pub def_at: u32,
}

#[derive(Clone, Debug)]
pub enum Event {
    Macro(Macro),
    /// A reference that was blanked out of the parser's view.
    Ref(Tok),
    Site(SiteTok),
}

impl Event {
    pub fn pos(&self) -> usize {
        match self {
            Event::Macro(m) => m.name.start as usize,
            Event::Ref(t) => t.start as usize,
            Event::Site(s) => s.name.start as usize,
        }
    }
}

#[derive(Default)]
pub struct Lexed {
    pub masked: Vec<u8>,
    /// In source order.
    pub events: Vec<Event>,
    /// `#include` operands, quotes or angle brackets stripped, and whether
    /// the directive is under an `#if`.
    pub includes: Vec<(u32, u32, bool)>,
    /// Top-level `{ ... }` blocks by bracket matching alone: (start byte, end
    /// byte, last line). These are function bodies, and stay right where the
    /// parser's error recovery cuts a function short.
    pub blocks: Vec<(u32, u32, u32)>,
    /// The branches the conditions in force switch off, as byte ranges:
    /// from the end of the directive that opens one to the start of the
    /// one that ends it. Nothing in them is indexed.
    pub skipped: Vec<(u32, u32)>,
    /// The branches that may or may not be compiled (the conditions in
    /// force cannot tell), as (from, to, directive): from the end of the
    /// directive that opens one to the start of the one that ends it, and
    /// where that opening directive starts. What is in them is indexed.
    pub undecided: Vec<(u32, u32, u32)>,
}

/// A `#define` reduced to what decides transparency: `deps` is `Some` when
/// the body is nothing but attributes and invocations of the listed macros.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Define {
    pub name: Vec<u8>,
    pub function_like: bool,
    pub deps: Option<Vec<Vec<u8>>>,
    /// The body makes or declares names (`a ## b`, `int name;`, `X (a)`).
    pub hides: bool,
    /// Names the body refers to.
    pub refs: Vec<Vec<u8>>,
    /// The body as a value (see `cond::value_of`), for a macro without
    /// parameters.
    pub value: Option<Vec<u8>>,
    /// Nothing around it may keep it from being compiled (an include
    /// guard apart).
    pub certain: bool,
    /// Directly under `#ifndef` of its own name: a default.
    pub fallback: bool,
    /// The file's include guard.
    pub guard: bool,
    /// Not a definition but an `#undef` of the name.
    pub undef: bool,
    /// Not a definition but an `#include`, `name` being the file as written.
    pub include: bool,
    /// Not a definition but a name some `#if` of the file tests.
    pub test: bool,
    /// The line, for an `#include` that is certainly compiled (else 0).
    pub line: u32,
    /// Made in a `.c` file: it stands in that file alone.
    #[serde(default)]
    pub local: bool,
    /// An `#include <...>` (and not `"..."`): the system's, when it is not
    /// among the files.
    #[serde(default)]
    pub system: bool,
    /// The first word of the body (`for`, or the macro it begins with).
    #[serde(default)]
    pub head: Vec<u8>,
}

impl Define {
    /// A note about `name` that is not a definition of it.
    fn note(name: &[u8], certain: bool) -> Define {
        Define {
            name: name.to_vec(),
            function_like: false,
            deps: None,
            hides: false,
            refs: Vec::new(),
            value: None,
            certain,
            fallback: false,
            guard: false,
            undef: false,
            include: false,
            test: false,
            line: 0,
            local: false,
            system: false,
            head: Vec::new(),
        }
    }
}

/// What the sources settle about the macros their `#if`s test, and where
/// (see `cond`): `per_file` are the files' `#define`s with their
/// `#include`s, `#undef`s and tested names, `paths` the files as they are
/// indexed, in the same order.
///
/// A name has a value in a file when every source file the file is
/// compiled as part of certainly includes a certain definition of it to
/// that value, and none of them can reach a line that undefines it, may
/// define it, or defines it otherwise.
pub fn source_of(per_file: &[Vec<Define>], paths: &[String]) -> crate::cond::Source {
    use rayon::prelude::*;
    let n = paths.len();
    let mut source = crate::cond::Source::new(paths);
    // Who includes whom: every include line, and those certainly compiled.
    let mut by_base: FxHashMap<&str, Vec<u32>> = FxHashMap::default();
    for (i, t) in paths.iter().enumerate() {
        by_base.entry(t.rsplit('/').next().unwrap_or(t)).or_default().push(i as u32);
    }
    // The files an include line may mean: those whose path ends with it.
    let targets = |name: &[u8]| -> Vec<u32> {
        let name = String::from_utf8_lossy(name).replace('\\', "/");
        let name = name.trim_start_matches("./").trim_start_matches("../");
        let all = by_base.get(name.rsplit('/').next().unwrap_or(name)).map_or(&[][..], |v| &v[..]);
        let exact: Vec<u32> = all.iter().copied().filter(|&i| paths[i as usize] == name || paths[i as usize].ends_with(&format!("/{name}"))).collect();
        if exact.is_empty() { all.to_vec() } else { exact }
    };
    let (mut down, mut up, mut up_certain) = (vec![Vec::new(); n], vec![Vec::new(); n], vec![Vec::new(); n]);
    let mut tested: FxHashSet<&[u8]> = FxHashSet::default();
    for (f, defines) in per_file.iter().enumerate() {
        for d in defines {
            if d.test {
                tested.insert(&d.name);
            } else if d.include {
                for t in targets(&d.name) {
                    down[f].push(t);
                    up[t as usize].push(f as u32);
                    if d.certain {
                        up_certain[t as usize].push(f as u32);
                        source.include(f as u32, d.line, t);
                    }
                }
            }
        }
    }
    // What a compilation starts from: a file that is not a header, or a
    // header nothing here includes (it is somebody's, out of sight).
    let roots: Vec<u32> = (0..n as u32).filter(|&i| !crate::is_header(&paths[i as usize]) || up[i as usize].is_empty()).collect();
    // What is said of each tested name, and where.
    #[derive(Default)]
    struct Said<'a> {
        /// (value, file) of each definition that is certainly compiled.
        sure: Vec<(&'a [u8], u32)>,
        /// Files with a line that leaves the name's value open.
        open: Vec<u32>,
    }
    let mut said: FxHashMap<&[u8], Said> = FxHashMap::default();
    for (f, defines) in per_file.iter().enumerate() {
        for d in defines.iter().filter(|d| !d.include && !d.test && tested.contains(&d.name[..])) {
            let e = said.entry(&d.name).or_default();
            match d.value.as_deref().filter(|_| !d.undef && !d.guard && d.certain) {
                Some(value) => e.sure.push((value, f as u32)),
                // A default (`#ifndef X` / `#define X`) leaves things as they are where X is defined.
                None if d.fallback && !d.undef => {}
                None => e.open.push(f as u32),
            }
        }
    }
    let said: Vec<(&[u8], Said)> = said.into_iter().filter(|(_, s)| !s.sure.is_empty()).collect();
    // Too much to work out in reasonable time is left alone.
    let edges: usize = down.iter().map(Vec::len).sum();
    if said.len().saturating_mul(n + edges) > 400_000_000 {
        return source;
    }
    let reach = |from: &[u32], next: &[Vec<u32>]| -> Vec<bool> {
        let mut seen = vec![false; n];
        let mut work = from.to_vec();
        while let Some(f) = work.pop() {
            if !std::mem::replace(&mut seen[f as usize], true) {
                work.extend(next[f as usize].iter().copied().filter(|&t| !seen[t as usize]));
            }
        }
        seen
    };
    let found: Vec<(&[u8], &[u8], Vec<bool>, Vec<bool>)> = said
        .par_iter()
        .flat_map_iter(|(name, s)| {
            let mut values: Vec<&[u8]> = s.sure.iter().map(|x| x.0).collect();
            values.sort_unstable();
            values.dedup();
            let mut out = Vec::new();
            for value in values {
                let definers: Vec<u32> = s.sure.iter().filter(|x| x.0 == value).map(|x| x.1).collect();
                let others: Vec<u32> = s.sure.iter().filter(|x| x.0 != value).map(|x| x.1).chain(s.open.iter().copied()).collect();
                // Files that certainly get the definition, and files that may get something else.
                let sure = reach(&definers, &up_certain);
                let spoiled = reach(&others, &up);
                let (good, bad): (Vec<u32>, Vec<u32>) = roots.iter().partition(|&&r| sure[r as usize] && !spoiled[r as usize]);
                if good.is_empty() {
                    continue;
                }
                // Known where a good compilation gets to and no other does.
                let mut known = reach(&good, &down);
                let unknown = reach(&bad, &down);
                known.iter_mut().zip(&unknown).for_each(|(k, u)| *k &= !u);
                if known.iter().any(|k| *k) {
                    out.push((*name, value, known, sure));
                }
            }
            out
        })
        .collect();
    for (name, value, known, brings) in found {
        source.add(name, value, &known, &brings);
    }
    source
}

/// One preprocessor directive of a file, as far as reading the file the
/// way a compilation does needs it (see `units`).
pub enum Op {
    /// `#if`, `#ifdef`, `#ifndef`: where the directive starts, and its
    /// condition; `kind` is 0 for an expression, 1 for a name that has to
    /// be defined, 2 for one that has not to be.
    If { at: u32, kind: u8, expr: Vec<u8> },
    Elif { at: u32, kind: u8, expr: Vec<u8> },
    Else,
    Endif,
    /// `value` as `cond::value_of` has it; empty for a macro with
    /// parameters or a body too long to be a value.
    Define { name: Vec<u8>, value: Vec<u8> },
    Undef { name: Vec<u8> },
    Include { name: Vec<u8>, quoted: bool },
}

#[derive(Default)]
pub struct Script {
    pub ops: Vec<Op>,
    /// The file is read once in a compilation however often it is
    /// included: `#pragma once`, or an include guard (whose own three
    /// directives are then left out of `ops`).
    pub once: bool,
}

/// The directives of `src`, in order.
pub fn script(src: &[u8]) -> Script {
    let mut out = Script::default();
    let mut c = Cur { s: src, i: 0, line: 1 };
    let mut line_start = true;
    while c.i < src.len() {
        match src[c.i] {
            b'\n' => {
                c.i += 1;
                line_start = true;
            }
            b' ' | b'\t' | b'\r' | 0x0c => c.i += 1,
            b'/' if matches!(c.peek(1), b'*' | b'/') => {
                c.skip_comment();
            }
            b'"' | b'\'' => {
                c.skip_literal();
                line_start = false;
            }
            b'#' if line_start => {
                let at = c.i as u32;
                c.i += 1;
                c.skip_space(false);
                let (a, e) = c.ident();
                c.skip_directive();
                let rest = &src[e..c.i];
                // The first name of the rest, and where it ends.
                let named = || {
                    let mut r = Cur { s: rest, i: 0, line: 0 };
                    r.skip_space(false);
                    let (a, e) = r.ident();
                    (&rest[a..e], e)
                };
                match &src[a..e] {
                    b"if" => out.ops.push(Op::If { at, kind: 0, expr: rest.to_vec() }),
                    b"ifdef" => out.ops.push(Op::If { at, kind: 1, expr: named().0.to_vec() }),
                    b"ifndef" => out.ops.push(Op::If { at, kind: 2, expr: named().0.to_vec() }),
                    b"elif" => out.ops.push(Op::Elif { at, kind: 0, expr: rest.to_vec() }),
                    b"elifdef" => out.ops.push(Op::Elif { at, kind: 1, expr: named().0.to_vec() }),
                    b"elifndef" => out.ops.push(Op::Elif { at, kind: 2, expr: named().0.to_vec() }),
                    b"else" => out.ops.push(Op::Else),
                    b"endif" => out.ops.push(Op::Endif),
                    b"define" => {
                        let (name, end) = named();
                        if !name.is_empty() {
                            let plain = rest.get(end) != Some(&b'(');
                            let value = if plain { crate::cond::value_of(&rest[end..]).unwrap_or_default() } else { Vec::new() };
                            out.ops.push(Op::Define { name: name.to_vec(), value });
                        }
                    }
                    b"undef" => {
                        let name = named().0;
                        if !name.is_empty() {
                            out.ops.push(Op::Undef { name: name.to_vec() });
                        }
                    }
                    b"include" | b"include_next" => {
                        let text = rest.trim_ascii_start();
                        let close = match text.first() {
                            Some(b'"') => b'"',
                            Some(b'<') => b'>',
                            _ => 0,
                        };
                        match text.iter().skip(1).position(|&b| b == close) {
                            Some(len) => out.ops.push(Op::Include { name: text[1..1 + len].to_vec(), quoted: close == b'"' }),
                            // `#include NAME`: some file, which is not known.
                            None => out.ops.push(Op::Include { name: Vec::new(), quoted: true }),
                        }
                    }
                    b"pragma" if named().0 == b"once" => out.once = true,
                    _ => {}
                }
                line_start = false;
            }
            _ => {
                c.i += 1;
                line_start = false;
            }
        }
    }
    // An include guard: all of the file inside `#ifndef G`, which the
    // next directive defines.
    if let [Op::If { kind: 2, expr, .. }, Op::Define { name, .. }, ..] = &out.ops[..] {
        let mut depth = 0;
        let close = out.ops.iter().position(|op| {
            match op {
                Op::If { .. } => depth += 1,
                Op::Endif => depth -= 1,
                _ => {}
            }
            depth == 0
        });
        if expr == name && close == Some(out.ops.len() - 1) {
            out.ops.pop();
            out.ops.drain(..2);
            out.once = true;
        }
    }
    out
}

/// What the corpus-wide pre-pass learns about macros, by name.
///
/// * *Transparent* macros can be deleted from the token stream without
///   changing how the surrounding code parses.
/// * *Expanding* macros have to be expanded where they are used, because the
///   expansion holds names that are written nowhere: pasted (`a ## b`),
///   declared from an argument (`int name;`), or made by a macro passed in.
///   A macro that uses an expanding macro is one too.
/// * *Kept* macros are those an expansion can reach; only their bodies are
///   carried in the facts.
///
/// Values: whether the macro takes arguments.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct MacroSets {
    transparent: FxHashMap<Vec<u8>, bool>,
    expanding: FxHashMap<Vec<u8>, bool>,
    kept: FxHashSet<Vec<u8>>,
    /// Macros with arguments every definition of which is a loop: `for`
    /// or `while` first, or another of these (`for_each_online_cpu(i)`).
    loops: FxHashSet<Vec<u8>>,
    /// The macros only some files can see (see `scope`): name -> where it
    /// is defined, files numbered as in `ids`. A name not here is seen
    /// everywhere.
    scoped: FxHashMap<Vec<u8>, Vec<(u32, u32)>>,
    /// For each of those definitions (the file, and how many of its
    /// `#include`s come before it), the files that read it, in order.
    reach: FxHashMap<(u32, u32), Vec<u32>>,
    /// The files, by path, as `scoped` and `reach` number them; only when
    /// anything is scoped.
    ids: FxHashMap<String, u32>,
    /// What the build is known to define: branches it never compiles are
    /// skipped (see `cond`). (Not saved: made anew from the settings.)
    #[serde(skip)]
    conditions: Conditions,
}

/// One `#include`: the files it may mean, and whether it is in angle
/// brackets.
type Included = (Vec<u32>, bool);

/// Where a macro is defined: the file, and how many of its `#include`s
/// come before the definition.
type Definition = (u32, u32);

/// A header that more files read than this is taken as read by all: its
/// macros are everyone's (a list of who reads it would cost more than it
/// tells). A `.c` file's macros go no further than the headers it is made
/// with, so they may go further than that.
const WIDELY_READ: usize = 1024;
const WIDELY_READ_FROM_SOURCE: usize = 8192;

/// What a macro may expand to and still be dropped: attributes, and
/// `inline` and (C++, C23) `constexpr` and the like, which say nothing an
/// index needs.
const ATTRIBUTE_KEYWORDS: [&[u8]; 11] = [
    b"constexpr",
    b"consteval",
    b"constinit",
    b"__attribute__",
    b"__attribute",
    b"__declspec",
    b"__extension__",
    b"_Noreturn",
    b"inline",
    b"__inline",
    b"__inline__",
];

impl MacroSets {
    /// A macro is transparent when every one of its definitions is, which is
    /// a fixed point because attribute macros are defined in terms of others.
    pub fn from_defines(defines: impl IntoIterator<Item = Define>) -> Self {
        let defines: Vec<Define> = defines.into_iter().collect();
        MacroSets::of_defines(&defines)
    }

    /// The same, of definitions kept elsewhere (none copied: the kernel's
    /// are millions).
    pub fn of_defines<'a>(defines: impl IntoIterator<Item = &'a Define>) -> Self {
        let mut by_name: FxHashMap<&[u8], (bool, Vec<Option<&[Vec<u8>]>>)> = FxHashMap::default();
        // name -> (takes arguments, hides names, names its bodies refer to)
        let mut bodies: FxHashMap<&[u8], (bool, bool, Vec<&[u8]>)> = FxHashMap::default();
        // (name, first word of the body) of each macro with arguments.
        let mut loop_heads: Vec<(&[u8], &[u8])> = Vec::new();
        for d in defines {
            // `#define inline` and the like: the keyword still matters to us.
            if is_keyword(&d.name) || d.undef || d.include || d.test {
                continue;
            }
            // (One in a `.c` file is that file's.)
            if d.function_like && !d.local {
                loop_heads.push((&d.name, &d.head));
            }
            let b = bodies.entry(&d.name).or_default();
            b.0 |= d.function_like;
            b.1 |= d.hides;
            b.2.extend(d.refs.iter().map(|r| &r[..]));
            // (One in a `.c` file is that file's: see `Lexer::define`.)
            if d.local {
                continue;
            }
            let e = by_name.entry(&d.name).or_default();
            e.0 |= d.function_like;
            e.1.push(d.deps.as_deref());
        }
        // Transparent: least fixed point over "every body is only
        // transparent macros", each name counting down what it waits for.
        let mut set: FxHashMap<&[u8], bool> = FxHashMap::default();
        let mut waits: FxHashMap<&[u8], usize> = FxHashMap::default();
        let mut waiting: FxHashMap<&[u8], Vec<&[u8]>> = FxHashMap::default();
        let mut ready: Vec<&[u8]> = Vec::new();
        for (&name, (_, bodies)) in &by_name {
            if bodies.iter().any(Option::is_none) {
                continue;
            }
            let mut count = 0;
            for d in bodies.iter().flatten().flat_map(|deps| deps.iter()) {
                waiting.entry(&d[..]).or_default().push(name);
                count += 1;
            }
            if count == 0 {
                ready.push(name);
            }
            waits.insert(name, count);
        }
        while let Some(name) = ready.pop() {
            if set.insert(name, by_name[name].0).is_some() {
                continue;
            }
            for &user in waiting.get(name).into_iter().flatten() {
                let w = waits.get_mut(user).unwrap();
                *w -= 1;
                if *w == 0 {
                    ready.push(user);
                }
            }
        }
        // Expanding: least fixed point over "uses an expanding macro".
        let mut users: FxHashMap<&[u8], Vec<&[u8]>> = FxHashMap::default();
        for (&name, (_, _, refs)) in &bodies {
            for &r in refs {
                users.entry(r).or_default().push(name);
            }
        }
        let mut expanding: FxHashMap<Vec<u8>, bool> = FxHashMap::default();
        let mut work: Vec<&[u8]> = bodies.iter().filter(|(n, b)| b.1 && !set.contains_key(*n)).map(|(n, _)| *n).collect();
        while let Some(name) = work.pop() {
            if expanding.insert(name.to_vec(), bodies[name].0).is_none() {
                work.extend(users.get(name).into_iter().flatten().filter(|u| !set.contains_key(**u)));
            }
        }
        // Kept: everything those bodies can reach.
        let mut kept: FxHashSet<Vec<u8>> = FxHashSet::default();
        let mut work: Vec<&[u8]> = expanding.keys().map(|n| &n[..]).collect();
        while let Some(name) = work.pop() {
            if let Some((key, b)) = bodies.get_key_value(name) {
                if kept.insert(key.to_vec()) {
                    work.extend(b.2.iter().copied());
                }
            }
        }
        let set: FxHashMap<Vec<u8>, bool> = set.into_iter().map(|(n, f)| (n.to_vec(), f)).collect();
        // Loops: least fixed point over "begins with a loop".
        let mut heads: FxHashMap<&[u8], Vec<&[u8]>> = FxHashMap::default();
        for d in &loop_heads {
            heads.entry(d.0).or_default().push(d.1);
        }
        let mut loops: FxHashSet<Vec<u8>> = FxHashSet::default();
        loop {
            let before = loops.len();
            for (name, hs) in &heads {
                if !loops.contains(*name) && hs.iter().all(|h| matches!(*h, b"for" | b"while") || loops.contains(*h)) {
                    loops.insert(name.to_vec());
                }
            }
            if loops.len() == before {
                break;
            }
        }
        MacroSets { transparent: set, expanding, kept, loops, ..MacroSets::default() }
    }

    /// The sets of every file's `#define`s (`per_file`, the files being
    /// `paths`), each macro seen only where it can be: in the files that
    /// define it and those that read them. A test program's
    /// `#define kfree(p) free(p)` (in `tools/`) is no attribute or
    /// expansion of the kernel's `kfree`.
    pub fn of_files(per_file: &[Vec<Define>], paths: &[String]) -> MacroSets {
        let mut sets = MacroSets::of_defines(per_file.iter().flatten());
        sets.scope(per_file, paths);
        sets
    }

    pub(crate) fn scope(&mut self, per_file: &[Vec<Define>], paths: &[String]) {
        use rayon::prelude::*;
        let n = paths.len();
        // Who reads whom, as the link will find it: each file's `#include`s
        // in order, with what each means. (A file with an `#include "..."`
        // of nothing here may read anything, a header the build makes say:
        // it sees every macro.)
        let found = crate::link::Includes::new(paths.iter().map(String::as_str));
        let read: Vec<(Vec<Included>, bool)> = per_file
            .par_iter()
            .zip(paths)
            .map(|(defines, path)| {
                let (mut out, mut open) = (Vec::new(), false);
                for d in defines.iter().filter(|d| d.include) {
                    let mut targets = Vec::new();
                    open |= !found.resolve(path, &String::from_utf8_lossy(&d.name), &mut targets) && !d.system;
                    out.push((targets, d.system));
                }
                (out, open)
            })
            .collect();
        let open: Vec<u32> = read.iter().enumerate().filter(|(_, r)| r.1).map(|(f, _)| f as u32).collect();
        let includes: Vec<Vec<Included>> = read.into_iter().map(|r| r.0).collect();
        // Who includes each file, and at which of its `#include`s first.
        let mut up: Vec<Vec<(u32, u32)>> = vec![Vec::new(); n];
        for (f, list) in includes.iter().enumerate() {
            for (k, (targets, _)) in list.iter().enumerate() {
                for &t in targets {
                    if !up[t as usize].iter().any(|&(g, _)| g == f as u32) {
                        up[t as usize].push((f as u32, k as u32));
                    }
                }
            }
        }
        // Where each macro that changes how code is read is defined: the
        // file, and how many of its `#include`s come before the definition
        // (a macro reaches only what is read after it).
        let mut defined_in: FxHashMap<&[u8], Vec<(u32, u32)>> = FxHashMap::default();
        for (f, defines) in per_file.iter().enumerate() {
            let mut before = 0u32;
            for d in defines {
                if d.include {
                    before += 1;
                    continue;
                }
                if d.test || d.undef {
                    continue;
                }
                // (A `.c` file's attribute macro is its own already: see `Lexer::define`.)
                let own_attribute = d.local && !self.expanding.contains_key(&d.name);
                if !own_attribute && (self.transparent.contains_key(&d.name) || self.expanding.contains_key(&d.name)) {
                    let at = defined_in.entry(&d.name).or_default();
                    if !at.iter().any(|&(g, _)| g == f as u32) {
                        at.push((f as u32, before));
                    }
                }
            }
        }
        // Who reads each definition, as far as few do: the file itself and
        // what it includes after it; the files that include it (or include
        // what does), and what each of those includes after that.
        let mut defining: Vec<(u32, u32)> = defined_in.values().flatten().copied().collect();
        defining.sort_unstable();
        defining.dedup();
        let reach: Vec<(Definition, Option<Vec<u32>>)> = defining
            .par_iter()
            .map_init(
                || vec![false; n],
                |seen, &(f, from)| {
                    let mut out = vec![f];
                    seen[f as usize] = true;
                    // Read after the definition, and what they include in
                    // turn: what is included in quotes (the headers a file
                    // is made with, `"aout-target.h"`), not in angle
                    // brackets (the system's, the project's own for all).
                    let source = !crate::is_header(&paths[f as usize]);
                    let most = if source { WIDELY_READ_FROM_SOURCE } else { WIDELY_READ };
                    let quoted = |list: &[Included], from: usize| -> Vec<u32> { list.iter().skip(from).filter(|(_, system)| !*system).flat_map(|(t, _)| t).copied().collect() };
                    let mut after: Vec<u32> = quoted(&includes[f as usize], from as usize);
                    let mut i = 0;
                    while i < out.len() && out.len() <= most {
                        for &(g, at) in &up[out[i] as usize] {
                            if !std::mem::replace(&mut seen[g as usize], true) {
                                out.push(g);
                                after.extend(quoted(&includes[g as usize], at as usize + 1));
                            }
                        }
                        i += 1;
                    }
                    let mut i = 0;
                    while i < after.len() && out.len() <= most {
                        let g = after[i];
                        if !std::mem::replace(&mut seen[g as usize], true) {
                            out.push(g);
                            after.extend(quoted(&includes[g as usize], 0));
                        }
                        i += 1;
                    }
                    for &g in &out {
                        seen[g as usize] = false;
                    }
                    if out.len() > most {
                        return ((f, from), None);
                    }
                    out.sort_unstable();
                    ((f, from), Some(out))
                },
            )
            .collect();
        let reach: FxHashMap<(u32, u32), Option<Vec<u32>>> = reach.into_iter().collect();
        for (name, at) in defined_in {
            // Seen everywhere if one of its definitions is.
            if at.iter().all(|d| reach[d].is_some()) {
                self.scoped.insert(name.to_vec(), at);
            }
        }
        let used: FxHashSet<(u32, u32)> = self.scoped.values().flatten().copied().collect();
        self.reach = reach.into_iter().filter(|(d, _)| used.contains(d)).filter_map(|(d, r)| Some((d, r?))).collect();
        if std::env::var_os("SI_SCOPE_PROFILE").is_some() {
            eprintln!("scope     {} macros seen by few files, {} definitions, {} entries", self.scoped.len(), self.reach.len(), self.reach.values().map(Vec::len).sum::<usize>());
        }
        if !self.scoped.is_empty() {
            // Those who may read anything are not numbered: they see all.
            let open: FxHashSet<u32> = open.into_iter().collect();
            self.ids = paths.iter().enumerate().filter(|(i, _)| !open.contains(&(*i as u32))).map(|(i, p)| (p.clone(), i as u32)).collect();
        }
    }

    /// The number `scope` gave the file at `path`, if any.
    pub fn file_id(&self, path: &str) -> Option<u32> {
        self.ids.get(path).copied()
    }

    /// Can the file numbered `file` see a definition of `name`?
    pub fn visible(&self, name: &[u8], file: u32) -> bool {
        match self.scoped.get(name) {
            None => true,
            Some(defining) => defining.iter().any(|d| self.reach.get(d).is_some_and(|r| r.binary_search(&file).is_ok())),
        }
    }

    pub fn with_conditions(mut self, conditions: Conditions) -> MacroSets {
        self.conditions = conditions;
        self
    }

    pub fn conditions(&self) -> &Conditions {
        &self.conditions
    }

    /// Number of transparent macros.
    pub fn len(&self) -> usize {
        self.transparent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.transparent.is_empty()
    }

    /// Is `name` a transparent macro?
    pub fn contains(&self, name: &[u8]) -> bool {
        self.transparent.contains_key(name)
    }

    pub fn expanding_len(&self) -> usize {
        self.expanding.len()
    }

    pub fn is_expanding(&self, name: &[u8]) -> bool {
        self.expanding.contains_key(name)
    }

    /// Names that are not the same kind of macro here and in `other`: facts
    /// extracted under one view are wrong under the other for any file that
    /// mentions one.
    pub fn reclassified(&self, other: &MacroSets) -> FxHashSet<Vec<u8>> {
        let mut out = FxHashSet::default();
        for (a, b) in [(&self.transparent, &other.transparent), (&self.expanding, &other.expanding)] {
            out.extend(a.iter().filter(|(k, v)| b.get(*k) != Some(v)).map(|(k, _)| k.clone()));
            out.extend(b.iter().filter(|(k, v)| a.get(*k) != Some(v)).map(|(k, _)| k.clone()));
        }
        out.extend(self.kept.symmetric_difference(&other.kept).cloned());
        out.extend(self.loops.symmetric_difference(&other.loops).cloned());
        // Seen from other files now.
        let seen_from = |sets: &MacroSets, name: &Vec<u8>| -> Option<Vec<u32>> {
            let defining = sets.scoped.get(name)?;
            let mut all: Vec<u32> = defining.iter().flat_map(|d| sets.reach.get(d).into_iter().flatten().copied()).collect();
            all.sort_unstable();
            all.dedup();
            Some(all)
        };
        for name in self.scoped.keys().chain(other.scoped.keys()) {
            if seen_from(self, name) != seen_from(other, name) {
                out.insert(name.clone());
            }
        }
        out
    }

    /// Order-independent digest, so cached facts can be keyed on it.
    pub fn digest(&self) -> u64 {
        let h = |k: &Vec<u8>, seed: u64| xxhash_rust::xxh3::xxh3_64_with_seed(k, seed);
        self.transparent.iter().fold(0, |acc, (k, v)| acc ^ h(k, *v as u64))
            ^ self.expanding.iter().fold(0, |acc, (k, v)| acc ^ h(k, 2 + *v as u64))
            ^ self.kept.iter().fold(0, |acc, k| acc ^ h(k, 4))
            ^ self.loops.iter().fold(0, |acc, k| acc ^ h(k, 5))
            ^ self.scoped.iter().fold(0, |acc, (k, files)| {
                let seen: u64 = files.iter().flat_map(|f| self.reach.get(f).into_iter().flatten()).fold(0, |a, &x| a.rotate_left(5) ^ x as u64);
                acc ^ h(k, 6 ^ seen)
            })
            ^ self.conditions.digest()
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub(crate) fn is_keyword(s: &[u8]) -> bool {
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

    /// At `open`: advance past the matching `close`. False if there is none.
    fn skip_group(&mut self, open: u8, close: u8) -> bool {
        let mut depth = 0u32;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b if b == open => {
                    depth += 1;
                    self.i += 1;
                }
                b if b == close => {
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
/// arguments would contain them. Skips pasted and stringified tokens,
/// keywords and `params`.
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
            if !(prev[1] == b'#' || pasted || is_keyword(text) || params.contains(&text)) {
                let mut call = c.peek(0) == b'(';
                if member {
                    // `(*(p)->x) (args)`: closing parentheses, then the call.
                    let mut k = c.i;
                    while matches!(s.get(k), Some(b')' | b' ' | b'\t' | b'\n' | b'\r' | b'\\')) && k < end {
                        k += 1;
                    }
                    call = k < end && s[k] == b'(';
                }
                out.push(Tok { start: a as u32, end: e as u32, line, call, member });
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
/// What a macro's body is, when it is only the head of a loop: `for` (or
/// `while`) for `for (...)`, the macro's name for `other_loop(...)`; else
/// nothing. (`while (x) ++p` is a whole statement, no head.)
fn first_word(body: &[u8]) -> Vec<u8> {
    let text = crate::cond::value_of(body).unwrap_or_default();
    let end = text.iter().position(|&b| !is_ident(b)).unwrap_or(text.len());
    if end == 0 || !is_ident_start(text[0]) {
        return Vec::new();
    }
    // The parentheses after the word, and nothing after them.
    let rest = text[end..].trim_ascii_start();
    if rest.first() != Some(&b'(') {
        return Vec::new();
    }
    let mut depth = 0i32;
    let close = rest.iter().position(|&b| {
        depth += (b == b'(') as i32 - (b == b')') as i32;
        depth == 0
    });
    match close {
        Some(c) if rest[c + 1..].trim_ascii().is_empty() => text[..end].to_vec(),
        _ => Vec::new(),
    }
}

/// Is there nothing that could be a type before `at`: the start of the
/// file, a `;` or `}`, past comments and preprocessor lines?
fn typeless_before(s: &[u8], at: usize) -> bool {
    let mut i = at;
    loop {
        while i > 0 && s[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        if i == 0 {
            return true;
        }
        // A comment ends here: before it.
        if i >= 2 && &s[i - 2..i] == b"*/" {
            match s[..i - 2].windows(2).rposition(|w| w == b"/*") {
                Some(open) => {
                    i = open;
                    continue;
                }
                None => return false,
            }
        }
        // A preprocessor line ends here: before it.
        let line_start = s[..i].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
        if s[line_start..i].trim_ascii_start().first() == Some(&b'#') {
            i = line_start;
            continue;
        }
        return matches!(s[i - 1], b';' | b'}');
    }
}

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

/// With `SI_COND_PROFILE` set: the names that left a condition open, and
/// how many conditions each (the first of a condition's names is charged).
pub static OPEN: std::sync::Mutex<Option<FxHashMap<Vec<u8>, (u64, u64)>>> = std::sync::Mutex::new(None);

fn profile(conditions: &Conditions, local: &crate::cond::Local, expr: &[u8], here: Option<(u32, u32)>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("SI_COND_PROFILE").is_some()) || here.is_none() {
        return;
    }
    let mut c = Cur { s: expr, i: 0, line: 0 };
    let mut names: Vec<&[u8]> = Vec::new();
    while c.i < expr.len() {
        if is_ident_start(expr[c.i]) {
            let (a, e) = c.ident();
            if &expr[a..e] != b"defined" && conditions.defined_in(local, here, &expr[a..e]) == Tri::Unknown {
                names.push(&expr[a..e]);
            }
        } else if expr[c.i].is_ascii_digit() {
            c.skip_number();
        } else {
            c.i += 1;
        }
    }
    let mut open = OPEN.lock().unwrap();
    let open = open.get_or_insert_with(FxHashMap::default);
    for (k, name) in names.iter().enumerate() {
        let e = open.entry(name.to_vec()).or_default();
        e.0 += (k == 0) as u64;
        e.1 += 1;
    }
    if names.is_empty() {
        open.entry(b"(no name)".to_vec()).or_default().0 += 1;
    }
}

/// One open `#if` group.
struct Cond {
    nest: Nest,
    /// C++: the braces open where the group opened (see `open_blocks`).
    open_blocks: Vec<Option<usize>>,
    /// Its directives so far, to hide them all once one has to be.
    spans: Vec<(usize, usize)>,
    hidden: bool,
    /// The current branch holds exactly when this does: name, value, and
    /// whether it is the negation.
    test: Option<(String, i64, bool)>,
    /// The name tested by `#ifndef`, until the group is seen to define it
    /// (which makes it an include guard, and so no condition at all).
    ifndef: Option<Vec<u8>>,
    include_guard: bool,
    /// Whether one of the group's branches so far is known to be the one
    /// compiled (true), known none is (false), or it is not known.
    taken: Tri,
    /// Whether the branch being read is compiled: yes, or not known. (One
    /// that is not is never read.)
    compiled: Tri,
    /// Where the branch being read began (the end of its directive) and its
    /// directive did, while it is one not known to be compiled.
    undecided: Option<(usize, usize)>,
}

struct Lexer<'a> {
    c: Cur<'a>,
    transparent: &'a MacroSets,
    conditions: &'a Conditions,
    out: Lexed,
    defines: Option<Vec<Define>>,
    /// What the file has said of macros so far (see `cond::Local`).
    local: crate::cond::Local,
    /// The file, as the conditions' `Source` numbers it.
    file: Option<u32>,
    /// A `.c` file, whose macros stand in it alone.
    c_file: bool,
    /// This file as the macro sets number it, for which macros it sees.
    here: Option<u32>,
    /// Its own macros that are only attributes (see `define`).
    own_transparent: FxHashMap<Vec<u8>, bool>,
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
    /// C++: blocks at every depth are reported (a class or a function is
    /// in a namespace), and here is where each open brace started (`None`
    /// for an initializer's), matched on their own: not reset by a brace in
    /// column 0, restored at `#else` as `nest` is.
    all_blocks: bool,
    open_blocks: Vec<Option<usize>>,
    /// The identifier before the one being read.
    last_ident: (u32, u32),
    /// End of a macro use hidden from the parser whose arguments are being
    /// read: the names in them are reported from here.
    hidden_until: usize,
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
                    if self.all_blocks {
                        self.open_blocks.push((!init).then_some(self.c.i));
                    }
                    self.nest.parens = 0;
                    self.c.i += 1;
                }
                b'}' => {
                    // C++: the block this closes. (Matched on a stack of its
                    // own, which a brace in column 0 does not reset: there
                    // one closes a namespace as often as a function.)
                    let closed = if self.all_blocks { self.open_blocks.pop().flatten() } else { None };
                    if col0 {
                        self.nest = Nest::default();
                    } else if let Some((_, parens)) = self.nest.braces.pop() {
                        self.nest.parens = parens;
                    }
                    self.c.i += 1;
                    if let Some(start) = closed {
                        self.out.blocks.push((start as u32, self.c.i as u32, self.c.line));
                    }
                    // (C++'s are all reported above, matched exactly.)
                    if self.nest.braces.is_empty() && !self.all_blocks {
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
        let before_byte = std::mem::replace(&mut self.prev, b'a');
        let before = std::mem::replace(&mut self.last_ident, (a as u32, e as u32));
        // A macro no definition of which this file can see is a name like
        // any other here.
        let here = self.here;
        let sets = self.transparent;
        let seen = |name: &[u8]| here.is_none_or(|f| sets.visible(name, f));
        // A loop macro in a function (`for_each_online_cpu(i) sum += ...`):
        // the parser, which does not know it, takes it for a call with no
        // `;` after it, and loses the functions that follow. It is shown
        // `while` in the place of the name; the arguments stay.
        if e - a >= 5 && sets.loops.contains(&s[a..e]) && seen(&s[a..e]) && self.nest.braces.last().is_some_and(|b| b.0 == Brace::Block) {
            let save = (self.c.i, self.c.line);
            self.c.skip_space(true);
            // (With something in the parentheses: `while ()` is no loop.)
            let called = self.c.peek(0) == b'(' && {
                self.c.i += 1;
                self.c.skip_space(true);
                self.c.peek(0) != b')'
            };
            (self.c.i, self.c.line) = save;
            if called {
                // What it expands to is read all the same (`scoped_guard
                // (mutex, &m)` calls class_mutex_constructor in its head).
                if let Some(&function_like) = sets.expanding.get(&s[a..e]) {
                    self.site(Tok { start: a as u32, end: e as u32, line, call: false, member: false }, function_like, before);
                }
                blank(&mut self.out.masked, a, e);
                for (k, &b) in b"while".iter().enumerate() {
                    self.put(a + k, b);
                }
                self.out.events.push(Event::Ref(Tok { start: a as u32, end: e as u32, line, call: true, member: false }));
                return;
            }
        }
        if let Some(&function_like) = sets.expanding.get(&s[a..e]).filter(|_| seen(&s[a..e])) {
            self.site(Tok { start: a as u32, end: e as u32, line, call: false, member: false }, function_like, before);
        }
        let shared = sets.transparent.get(&s[a..e]).filter(|_| seen(&s[a..e]));
        let Some(&function_like) = shared.or_else(|| self.own_transparent.get(&s[a..e])) else {
            if a < self.hidden_until && !is_keyword(&s[a..e]) {
                let (call, member) = (self.c.peek(0) == b'(', matches!(before_byte, b'.' | b'>'));
                self.out.events.push(Event::Ref(Tok { start: a as u32, end: e as u32, line, call, member }));
            }
            return;
        };
        let mut tok = Tok { start: a as u32, end: e as u32, line, call: false, member: false };
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
            // Without arguments it is not a use of the macro: the name is
            // something else here (`static int dump;` beside a
            // `#define dump(a, b)`), and stays for the parser.
            if a < self.hidden_until {
                tok.member = matches!(before_byte, b'.' | b'>');
                self.out.events.push(Event::Ref(tok));
            }
            return;
        }
        self.out.events.push(Event::Ref(tok));
        blank(&mut self.out.masked, a, end);
    }

    /// Just past the name of an expanding macro: note the use and its
    /// arguments. The cursor does not move, so the arguments are still read
    /// as ordinary code.
    fn site(&mut self, name: Tok, function_like: bool, before: (u32, u32)) {
        use crate::model::site;
        let save = (self.c.i, self.c.line);
        let mut args_end = 0;
        if function_like {
            self.c.skip_space(true);
            if self.c.peek(0) == b'(' && self.c.skip_parens() {
                args_end = self.c.i as u32;
            } else {
                (self.c.i, self.c.line) = save;
            }
        }
        let last_line = self.c.line;
        self.c.skip_space(true);
        // A use that stands as a declaration of its own, alone on its lines
        // at file scope (`START_RELOC_NUMBERS (x)`, `DEFINE_LIST (y);`). The
        // parser would take it for the start of whatever follows; its
        // meaning is in its expansion, which the link reads.
        let alone = args_end != 0
            && self.nest.braces.is_empty()
            && self.nest.parens == 0
            && match self.c.peek(0) {
                b';' | 0 => true,
                b'(' | b'{' | b'=' | b'[' | b',' | b')' => false,
                _ => self.c.line > last_line,
            };
        if alone {
            // With the storage class before it, which would otherwise be
            // left to open the next declaration.
            let s = self.c.s;
            let mut start = name.start as usize;
            loop {
                let mut k = start;
                while k > 0 && matches!(s[k - 1], b' ' | b'\t') {
                    k -= 1;
                }
                let word_end = k;
                while k > 0 && is_ident(s[k - 1]) {
                    k -= 1;
                }
                if !matches!(&s[k..word_end], b"static" | b"extern" | b"inline" | b"const") {
                    break;
                }
                start = k;
            }
            blank(&mut self.out.masked, start, args_end as usize);
            self.hidden_until = args_end as usize;
        }
        let mut flags = match self.c.peek(0) {
            b'(' => site::CALLED,
            b'{' => site::BRACE_FOLLOWS,
            _ => 0,
        };
        // A name that takes no arguments, called: what it expands to may
        // take them (`#define H_PUT_16 bfd_h_put_16`).
        if flags == site::CALLED && args_end == 0 {
            let at = (self.c.i, self.c.line);
            if self.c.skip_parens() {
                args_end = self.c.i as u32;
                flags |= site::TRAILING;
                self.c.skip_space(true);
            } else {
                (self.c.i, self.c.line) = at;
            }
        }
        let top = self.nest.braces.is_empty() && self.nest.parens == 0;
        // `const bfd_target MY (vec) = { ... }`: the parser cannot make a
        // declaration of this, so the initializer is taken here.
        let (mut init, mut init_array) = ((0, 0), false);
        if top && before.1 > before.0 && !is_keyword(&self.c.s[before.0 as usize..before.1 as usize]) {
            if self.c.peek(0) == b'[' && self.c.skip_group(b'[', b']') {
                init_array = true;
                self.c.skip_space(true);
            }
            if self.c.peek(0) == b'=' && self.c.peek(1) != b'=' {
                self.c.i += 1;
                self.c.skip_space(true);
                let open = self.c.i;
                if self.c.peek(0) == b'{' && self.c.skip_group(b'{', b'}') {
                    init = (open as u32, self.c.i as u32);
                }
            }
        }
        (self.c.i, self.c.line) = save;
        if top {
            flags |= site::TOP;
        }
        if self.nest.braces.last().is_some_and(|b| b.0 == Brace::Init) {
            flags |= site::INIT;
        }
        // `SYSCALL_DEFINE2(open, ...)` then a body, and no type before it:
        // the parser makes no function of that. It is shown the name as a
        // type and a declarator in the place of the arguments
        // (`SYSCALL_DEFINE2 f(        )`), which the expansion then names.
        let mut def_at = 0;
        if top && flags & site::BRACE_FOLLOWS != 0 && args_end != 0 && typeless_before(self.c.s, name.start as usize) {
            let s = self.c.s;
            let open = (name.end as usize..args_end as usize).find(|&i| s[i] == b'(');
            if let Some(open) = open.filter(|&o| o + 3 < args_end as usize) {
                let close = args_end as usize - 1;
                blank(&mut self.out.masked, open, close);
                self.put(open + 1, b'f');
                self.put(open + 2, b'(');
                def_at = open as u32 + 1;
            }
        }
        self.out.events.push(Event::Site(SiteTok { name, args_end, flags, init, init_type: before, init_array, def_at }));
    }

    fn directive(&mut self) {
        let s = self.c.s;
        let start = self.c.i;
        // Where this is, for what is known of macros here.
        let here = self.file.map(|f| (f, self.c.line));
        let line = self.c.line;
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
            b"undef" => {
                self.c.skip_space(false);
                let (a, e) = self.c.ident();
                if e > a {
                    // From here on the name is not defined, if this line is compiled at all.
                    let certain = !self.conds.iter().any(|c| !c.include_guard && c.compiled != Tri::True);
                    self.local.insert(s[a..e].to_vec(), certain.then_some(None));
                    self.own_transparent.remove(&s[a..e]);
                    if let Some(defines) = &mut self.defines {
                        defines.push(Define { undef: true, ..Define::note(&s[a..e], certain) });
                    }
                }
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
                    let conditional = self.conds.iter().any(|c| !c.include_guard && c.compiled != Tri::True);
                    self.out.includes.push((open as u32 + 1, (open + 1 + len) as u32, conditional));
                    if let Some(defines) = &mut self.defines {
                        defines.push(Define { include: true, system: close == b'>', line: if conditional { 0 } else { line }, ..Define::note(&s[open + 1..open + 1 + len], !conditional) });
                    }
                }
            }
            _ => {}
        }
        self.c.skip_directive();
        let span = (start, self.c.i);
        // Set when the branch this directive opens is one the build never
        // compiles: its text is then passed over.
        let mut skip = false;
        // A group that cuts a statement in two (`if (x)` / `#endif` / `y;`)
        // is not a statement itself: hide all of it and read every branch.
        let in_block = self.nest.braces.last().is_some_and(|b| b.0 == Brace::Block);
        let cuts = hide || in_block && !matches!(self.prev, b';' | b'{' | b'}' | b':');
        match word {
            b"if" | b"ifdef" | b"ifndef" => {
                let test = if word == b"if" { equality(&s[e..span.1]) } else { None };
                let ifndef = (word == b"ifndef").then(|| {
                    let mut c = Cur { s: &s[..span.1], i: e, line: 0 };
                    c.skip_space(false);
                    let (a, e) = c.ident();
                    s[a..e].to_vec()
                });
                // Does the build compile this branch?
                let named = || {
                    let mut c = Cur { s: &s[..span.1], i: e, line: 0 };
                    c.skip_space(false);
                    let (a, e) = c.ident();
                    self.conditions.defined_in(&self.local, here, &s[a..e])
                };
                // What reading the compilations came to, where it came to one thing.
                let told = |at: usize| self.file.and_then(|f| self.conditions.source()?.verdict(f, at as u32));
                let holds = match (told(start), word) {
                    (Some(holds), _) => if holds { Tri::True } else { Tri::False },
                    (None, b"ifdef") => named(),
                    (None, b"ifndef") => named().not(),
                    _ => self.conditions.eval_in(&self.local, here, &s[e..span.1]),
                };
                if holds == Tri::Unknown {
                    self.profile(&s[e..span.1], here);
                }
                self.conds.push(Cond {
                    nest: self.nest.clone(),
                    open_blocks: self.open_blocks.clone(),
                    spans: vec![span],
                    hidden: cuts,
                    test,
                    ifndef,
                    include_guard: false,
                    taken: holds,
                    compiled: holds,
                    undecided: (holds == Tri::Unknown).then_some((span.1, start)),
                });
                hide = cuts;
                skip = holds == Tri::False;
            }
            b"elif" | b"else" | b"elifdef" | b"elifndef" | b"endif" => {
                if let Some(c) = self.conds.last_mut() {
                    // The branch read so far ends here. (An include guard
                    // is no condition: what it holds is the file.)
                    if let Some((from, directive)) = c.undecided.take().filter(|_| !c.include_guard) {
                        self.out.undecided.push((from as u32, start as u32, directive as u32));
                    }
                    if cuts && !c.hidden {
                        c.hidden = true;
                        for &(a, e) in &c.spans {
                            blank(&mut self.out.masked, a, e);
                        }
                    }
                    hide = c.hidden;
                    // This branch is compiled only if none before it was,
                    // and (for `#elif`) its own condition holds.
                    if word != b"endif" {
                        let told = self.file.and_then(|f| self.conditions.source()?.verdict(f, start as u32));
                        let own = match told {
                            Some(holds) if word != b"else" => if holds { Tri::True } else { Tri::False },
                            _ if word == b"elif" => self.conditions.eval_in(&self.local, here, &s[e..span.1]),
                            _ if word == b"else" => Tri::True,
                            _ => Tri::Unknown,
                        };
                        if word == b"elif" && own == Tri::Unknown {
                            let (conditions, local) = (self.conditions, &self.local);
                            profile(conditions, local, &s[e..span.1], here);
                        }
                        c.compiled = c.taken.not().and(own);
                        skip = c.compiled == Tri::False;
                        c.undecided = (c.compiled == Tri::Unknown).then_some((span.1, start));
                        c.taken = c.taken.or(own);
                    }
                    // `#else` of a lone `#if` is its negation; after an
                    // `#elif` only the `#elif`'s own test is known.
                    c.test = match word {
                        b"else" if c.spans.len() == 1 => c.test.take().map(|(n, v, _)| (n, v, true)),
                        b"elif" => equality(&s[e..span.1]),
                        _ => None,
                    };
                    c.spans.push(span);
                    if word == b"endif" {
                        self.conds.pop();
                    } else {
                        self.nest = c.nest.clone();
                        self.open_blocks.clone_from(&c.open_blocks);
                    }
                }
            }
            _ => {}
        }
        if hide {
            blank(&mut self.out.masked, span.0, span.1);
        }
        if skip {
            self.skip_branch();
        }
    }

    fn profile(&self, expr: &[u8], here: Option<(u32, u32)>) {
        profile(self.conditions, &self.local, expr, here);
    }

    /// Past a branch the build does not compile, to the `#elif`, `#else` or
    /// `#endif` that ends it. Nothing in it is read: no definitions, no
    /// references, and the parser sees blanks.
    fn skip_branch(&mut self) {
        let s = self.c.s;
        let from = self.c.i;
        let mut depth = 0u32;
        let mut line_start = false;
        while self.c.i < s.len() {
            match s[self.c.i] {
                b'\n' => {
                    self.c.bump();
                    line_start = true;
                }
                b' ' | b'\t' | b'\r' | 0x0c => self.c.i += 1,
                b'/' if matches!(self.c.peek(1), b'*' | b'/') => {
                    self.c.skip_comment();
                }
                b'"' | b'\'' => {
                    self.c.skip_literal();
                    line_start = false;
                }
                b'#' if line_start => {
                    let at = (self.c.i, self.c.line);
                    self.c.i += 1;
                    self.c.skip_space(false);
                    let (a, e) = self.c.ident();
                    match &s[a..e] {
                        b"if" | b"ifdef" | b"ifndef" => depth += 1,
                        b"endif" if depth > 0 => depth -= 1,
                        b"endif" | b"elif" | b"else" | b"elifdef" | b"elifndef" if depth == 0 => {
                            (self.c.i, self.c.line) = at;
                            break;
                        }
                        _ => {}
                    }
                    self.c.skip_directive();
                    line_start = false;
                }
                _ => {
                    self.c.i += 1;
                    line_start = false;
                }
            }
        }
        blank(&mut self.out.masked, from, self.c.i);
        self.out.skipped.push((from as u32, self.c.i as u32));
        self.line_start = true;
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
        let name = Tok { start: a as u32, end: e as u32, line, call: function_like, member: false };
        // `#ifndef FOO_H` / `#define FOO_H` with nothing or `1` after it,
        // around the whole file; or, named as a header's and with nothing
        // after it, within guards only (a header made of many, each with
        // its own: gmock's).
        let mut guard = false;
        if let Some((last, outer)) = self.conds.split_last_mut() {
            let text = s[body..end].trim_ascii();
            let name = s[a..e].trim_ascii_end();
            let names_header = [&b"_H"[..], b"_H_", b"_H__", b"_HPP", b"_HPP_", b"_INCLUDED", b"_INCLUDED_"].iter().any(|x| name.ends_with(x));
            let fits = if outer.is_empty() { text.is_empty() || text == b"1" } else { text.is_empty() && names_header };
            if last.ifndef.as_deref() == Some(&s[a..e]) && !function_like && fits && outer.iter().all(|c| c.include_guard) {
                last.include_guard = true;
                guard = true;
            }
        }
        let conditional = self.conds.iter().any(|c| !c.include_guard && c.compiled != Tri::True);
        let fallback = self.conds.last().is_some_and(|c| !c.include_guard && c.compiled != Tri::True && c.ifndef.as_deref() == Some(&s[a..e]));
        // From here on in this file the name has this value, if the line
        // is compiled at all. (One with parameters is kept with them, to be
        // called in a condition; one with `...` is defined, to nothing a
        // condition can use.)
        let variadic = function_like && s[e..body].windows(3).any(|w| w == b"...");
        let value = match (function_like, variadic) {
            (false, _) => crate::cond::value_of(&s[body..end]),
            (true, false) => crate::cond::function_value(&params, &s[body..end]),
            (true, true) => None,
        };
        let known = value.clone().or(Some(b"(".to_vec()));
        self.local.insert(s[a..e].to_vec(), (!conditional).then_some(known));
        if let Some(defines) = &mut self.defines {
            defines.push(Define {
                name: s[a..e].to_vec(),
                function_like,
                deps: transparent_deps(s, body, end, &refs),
                hides: crate::expand::hides_names(&s[body..end], &params),
                refs: refs.iter().filter(|t| !t.member).map(|t| s[t.start as usize..t.end as usize].to_vec()).collect(),
                value,
                certain: !conditional,
                fallback,
                guard,
                undef: false,
                include: false,
                test: false,
                line: 0,
                local: self.c_file,
                system: false,
                head: first_word(&s[body..end]),
            });
        }
        // A `.c` file's own macros stand in it alone, so they are not in
        // the sets made from every file's: what of them is only an
        // attribute is known here, as they come.
        if self.c_file && !is_keyword(&s[a..e]) {
            let deps = transparent_deps(s, body, end, &refs);
            let transparent = deps.is_some_and(|d| d.iter().all(|x| self.transparent.contains(x) || self.own_transparent.contains_key(x)));
            if transparent {
                self.own_transparent.insert(s[a..e].to_vec(), function_like);
            } else {
                self.own_transparent.remove(&s[a..e]);
            }
        }
        let keep = self.transparent.kept.contains(&s[a..e]);
        let guard_flag = guard;
        let _ = guard_flag;
        let mut guard = String::new();
        if keep {
            for (n, v, negated) in self.conds.iter().filter_map(|c| c.test.as_ref()) {
                let (sep, op) = (if guard.is_empty() { "" } else { ";" }, if *negated { "!=" } else { "==" });
                guard += &format!("{sep}{n} {op} {v}");
            }
        }
        self.out.events.push(Event::Macro(Macro { name, end_line, refs, end: end as u32, keep, guard, conditional, fallback }));
    }
}

/// The condition of an `#if`, when it is `NAME == integer` and no more.
fn equality(cond: &[u8]) -> Option<(String, i64, bool)> {
    let text = std::str::from_utf8(cond).ok()?;
    let text = text.split("/*").next()?.split("//").next()?;
    let (name, value) = text.split_once("==")?;
    let name = name.trim();
    let ident = !name.is_empty() && name.bytes().all(is_ident) && is_ident_start(name.as_bytes()[0]);
    ident.then_some(())?;
    Some((name.to_string(), value.trim().parse().ok()?, false))
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
        if c.peek(0) == b'[' && c.peek(1) == b'[' {
            // `[[nodiscard]]`: a standard attribute.
            let Some(close) = s[c.i..end].windows(2).position(|w| w == b"]]") else { return None };
            c.i += close + 2;
            continue;
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

fn lexer<'a>(src: &'a [u8], transparent: &'a MacroSets, conditions: &'a Conditions, mask: bool, path: Option<&str>) -> Lexer<'a> {
    // What the file itself defines or undefines is not known before the
    // line that does it, whatever is known of the name elsewhere.
    let mut local = crate::cond::Local::default();
    for line in src.split(|&b| b == b'\n') {
        let Some(rest) = line.trim_ascii_start().strip_prefix(b"#") else { continue };
        let rest = rest.trim_ascii_start();
        let Some(rest) = rest.strip_prefix(b"define").or(rest.strip_prefix(b"undef")) else { continue };
        if !rest.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
            continue;
        }
        let rest = rest.trim_ascii_start();
        let end = rest.iter().position(|&b| !is_ident(b)).unwrap_or(rest.len());
        // (What the settings say of a name stands until the file says otherwise.)
        if end > 0 && !conditions.settles(&rest[..end]) {
            local.insert(rest[..end].to_vec(), None);
        }
    }
    Lexer {
        file: path.and_then(|p| conditions.source()?.file(p)),
        c_file: path.is_some_and(crate::is_unit),
        here: path.and_then(|p| transparent.file_id(p)),
        own_transparent: FxHashMap::default(),
        c: Cur { s: src, i: 0, line: 1 },
        transparent,
        conditions,
        out: Lexed { masked: if mask { src.to_vec() } else { Vec::new() }, ..Default::default() },
        defines: None,
        local,
        nest: Nest::default(),
        conds: Vec::new(),
        prev: 0,
        line_start: true,
        arg: None,
        block: None,
        all_blocks: path.is_some_and(|p| crate::extract::is_cpp(p, src)),
        open_blocks: Vec::new(),
        last_ident: (0, 0),
        hidden_until: 0,
    }
}

/// Mask `src` for parsing and report what was hidden.
pub fn lex(src: &[u8], transparent: &MacroSets) -> Lexed {
    lex_file(src, transparent, None)
}

/// The same for the file indexed as `path`: what the sources settle about
/// macros is told file by file.
pub fn lex_file(src: &[u8], transparent: &MacroSets, path: Option<&str>) -> Lexed {
    let mut l = lexer(src, transparent, &transparent.conditions, true, path);
    l.run();
    if l.all_blocks {
        // (Reported as they closed: inner ones first.)
        l.out.blocks.sort_unstable();
        l.out.blocks.dedup_by_key(|b| b.0);
    }
    l.out
}

/// Every name the file `#define`s, in whatever branch: a plain scan of its
/// lines, for knowing which names the sources define at all.
pub fn defined_names(src: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    for line in src.split(|&b| b == b'\n') {
        let line = line.trim_ascii_start();
        let Some(rest) = line.strip_prefix(b"#") else { continue };
        let Some(rest) = rest.trim_ascii_start().strip_prefix(b"define") else { continue };
        if !rest.first().is_some_and(|b| matches!(b, b' ' | b'\t')) {
            continue;
        }
        let rest = rest.trim_ascii_start();
        let end = rest.iter().position(|&b| !is_ident(b)).unwrap_or(rest.len());
        if end > 0 {
            out.push(&rest[..end]);
        }
    }
    out
}

/// The `#define`s of one file, for building the [`MacroSets`] set: those
/// in branches the build compiles, as far as `conditions` say.
pub fn defines(src: &[u8], conditions: &Conditions) -> Vec<Define> {
    defines_of(src, conditions, None)
}

/// The same for the file indexed as `path`, with notes of what it includes
/// and which names its `#if`s test.
pub fn defines_of(src: &[u8], conditions: &Conditions, path: Option<&str>) -> Vec<Define> {
    static NONE: std::sync::OnceLock<MacroSets> = std::sync::OnceLock::new();
    let mut l = lexer(src, NONE.get_or_init(MacroSets::default), conditions, false, path);
    l.defines = Some(Vec::new());
    l.run();
    let mut defines = l.defines.unwrap_or_default();
    // Includes on lines that were passed over (a branch the build does not
    // compile) are includes all the same when it comes to whose a header
    // is: noted, as ones that may not happen. And the names tested.
    let mut seen: FxHashSet<Vec<u8>> = defines.iter().filter(|d| d.include).map(|d| d.name.clone()).collect();
    let mut tested: FxHashSet<&[u8]> = FxHashSet::default();
    for line in src.split(|&b| b == b'\n') {
        let Some(rest) = line.trim_ascii_start().strip_prefix(b"#") else { continue };
        let rest = rest.trim_ascii_start();
        if let Some(rest) = rest.strip_prefix(b"include_next").or(rest.strip_prefix(b"include")) {
            let rest = rest.trim_ascii_start();
            let close = match rest.first() {
                Some(b'"') => b'"',
                Some(b'<') => b'>',
                _ => continue,
            };
            let Some(len) = rest[1..].iter().position(|&b| b == close) else { continue };
            if seen.insert(rest[1..1 + len].to_vec()) {
                defines.push(Define { include: true, system: close == b'>', ..Define::note(&rest[1..1 + len], false) });
            }
        } else if rest.starts_with(b"if") || rest.starts_with(b"elif") {
            let mut i = 0;
            while i < rest.len() {
                if is_ident_start(rest[i]) {
                    let a = i;
                    while i < rest.len() && is_ident(rest[i]) {
                        i += 1;
                    }
                    if a > 0 && tested.insert(&rest[a..i]) {
                        defines.push(Define { test: true, ..Define::note(&rest[a..i], false) });
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    defines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transparent(src: &str) -> MacroSets {
        MacroSets::from_defines(defines(src.as_bytes(), &Conditions::default()))
    }

    fn masked(src: &str, t: &MacroSets) -> String {
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
            Event::Macro(_) | Event::Site(_) => "?",
        }).collect();
        assert_eq!(names, ["UNUSED", "NONNULL", "g"]);
    }

    #[test]
    fn defines_are_recorded_with_body_references() {
        let src = "#define CALL(x, f) \\\n  do_it (x->field, #f, pre##x, helper) /* c */\nint y;\n";
        let l = lex(src.as_bytes(), &MacroSets::default());
        assert_eq!(String::from_utf8(l.masked).unwrap().trim_start(), "int y;\n");
        let Event::Macro(m) = &l.events[0] else { panic!() };
        assert_eq!((m.name.line, m.end_line, m.name.call), (1, 2, true));
        let refs: Vec<(&str, bool)> = m.refs.iter().map(|t| (&src[t.start as usize..t.end as usize], t.call)).collect();
        assert_eq!(refs, [("do_it", true), ("field", false), ("helper", false)]);
        assert!(m.refs[1].member && !m.refs[0].member);
    }

    #[test]
    fn directives_are_hidden_only_where_the_grammar_rejects_them() {
        let t = MacroSets::default();
        let src = "#ifdef A\nint a;\n#endif\nint t[] = {\n#ifdef A\n 1,\n#else\n 2,\n#endif\n};\nvoid f () {\n#ifdef A\n g (1\n#if B\n , 2\n#endif\n );\n#endif\n}\n";
        let m = masked(src, &t);
        let kept: Vec<&str> = m.lines().filter(|l| l.starts_with('#')).collect();
        assert_eq!(kept, ["#ifdef A", "#endif", "#ifdef A", "#endif"]);
        assert_eq!(m.len(), src.len());
    }

    #[test]
    fn branches_the_build_does_not_compile_are_passed_over() {
        let src = "#if WIDTH == 64\nint wide (void);\n#define W 64\n#elif WIDTH == 32\nint narrow (void);\n#define W 32\n#else\nint none (void);\n#endif\n\
                   #ifdef WINDOWS\nint win (void);\n# if 1\nint nested (void);\n# endif\n#else\nint unix_ (void);\n#endif\n\
                   #ifdef UNKNOWN\nint maybe (void);\n#endif\n#if 0\nint dead (void);  /* don't */\n#endif\nint always (void);\n";
        let mut c = Conditions::default();
        c.define("WIDTH", "32");
        c.undefine("WINDOWS");
        let sets = MacroSets::default().with_conditions(c.clone());
        let l = lex(src.as_bytes(), &sets);
        let m = String::from_utf8(l.masked).unwrap();
        assert_eq!(m.len(), src.len());
        assert_eq!(m.matches('\n').count(), src.matches('\n').count(), "lines stay where they are");
        for (name, there) in [("wide", false), ("narrow", true), ("none", false), ("win", false), ("nested", false), ("unix_", true), ("maybe", true), ("dead", false), ("always", true)] {
            assert_eq!(m.contains(name), there, "{name}");
        }
        // Only the definition in the compiled branch counts.
        let noted = |d: &&Define| !(d.test || d.include || d.undef);
        let names: Vec<String> = defines(src.as_bytes(), &c).iter().filter(noted).map(|d| String::from_utf8(d.name.clone()).unwrap()).collect();
        assert_eq!(names, ["W"]);
        let macros = l.events.iter().filter(|e| matches!(e, Event::Macro(_))).count();
        assert_eq!(macros, 1);
        // With nothing configured only `#if 0` is passed over.
        let plain = masked(src, &MacroSets::default());
        assert!(plain.contains("wide") && plain.contains("win") && !plain.contains("dead"));
    }

    #[test]
    fn what_a_file_defines_settles_its_own_conditions_below() {
        let src = "#ifndef CFG_H\n#define CFG_H\n#if USE_A\nint early (void);\n#endif\n#define USE_A 1\n#define LEVEL (2)\n#undef OLD\n\
                   #if USE_A\nint a_on (void);\n#else\nint a_off (void);\n#endif\n\
                   #if LEVEL > 1 && !defined(OLD)\nint high (void);\n#else\nint low (void);\n#endif\n\
                   #ifdef MAYBE\n#define USE_B 1\n#endif\n#if USE_B\nint b_on (void);\n#else\nint b_off (void);\n#endif\n\
                   #ifndef DFLT\n#define DFLT 4\n#endif\n#undef USE_A\n#ifdef USE_A\nint still (void);\n#endif\n#endif\n";
        let m = masked(src, &MacroSets::default());
        for (name, there) in [("early", true), ("a_on", true), ("a_off", false), ("high", true), ("low", false), ("b_on", true), ("b_off", true), ("still", false)] {
            assert_eq!(m.contains(name), there, "{name}");
        }
    }

    #[test]
    fn what_the_sources_define_is_known_in_the_files_it_reaches() {
        let files: Vec<(&str, &str)> = vec![
            // A switch in a header everything includes...
            ("cfg.h", "#ifndef CFG_H\n#define CFG_H\n#define USE_NET 1\n#define LEVEL 2\n#endif\n"),
            ("a.c", "#if USE_NET\n#endif\n#include \"cfg.h\"\n#include \"shared.h\"\n#if USE_NET\n#endif\n"),
            // ...a macro one file sets before a shared header, which another does not...
            ("b.c", "#include \"cfg.h\"\n#define NO_RELOCS\n#include \"shared.h\"\n#include \"mine.h\"\n"),
            ("shared.h", "#ifdef NO_RELOCS\n#endif\n#if LEVEL > 1\n#endif\n#ifdef WIN\n#endif\n#if SIZE == 64\n#endif\n"),
            // ...a header of one file only...
            ("mine.h", "#ifdef NO_RELOCS\n#endif\n"),
            // ...a platform's header, included where a condition may not hold...
            ("win.h", "#define WIN 1\n"),
            ("c.c", "#include \"cfg.h\"\n#ifdef SOMETHING\n#include \"win.h\"\n#endif\n#include \"shared.h\"\n#ifdef WIN\n#endif\n"),
            // ...and files that give a name different values.
            ("d64.c", "#define SIZE 64\n#include \"sized.h\"\n"),
            ("d32.c", "#define SIZE 32\n#include \"sized.h\"\n#include \"only32.h\"\n"),
            ("sized.h", "#if SIZE == 64\n#endif\n"),
            ("only32.h", "#if SIZE == 64\n#endif\n"),
        ];
        let paths: Vec<String> = files.iter().map(|f| f.0.to_string()).collect();
        let defines: Vec<Vec<Define>> = files.iter().map(|f| defines(f.1.as_bytes(), &Conditions::default())).collect();
        let mut c = Conditions::default();
        c.set_source(Some(std::sync::Arc::new(source_of(&defines, &paths))));
        let none = crate::cond::Local::default();
        let at = |file: &str, line: u32, expr: &str| c.eval_in(&none, c.source().unwrap().file(file).map(|f| (f, line)), expr.as_bytes());
        let known = |file: &str, expr: &str| at(file, 99, expr);
        // a.c tests USE_NET on line 1 and includes cfg.h on line 3: not known before that.
        assert_eq!((at("a.c", 1, "USE_NET"), at("a.c", 3, "USE_NET"), at("a.c", 5, "USE_NET")), (Tri::Unknown, Tri::Unknown, Tri::True));
        // shared.h gets it from whoever includes it: known from its first line.
        assert_eq!(at("shared.h", 1, "LEVEL > 1"), Tri::True);
        // Everything includes cfg.h: its switches are known wherever they are tested.
        assert_eq!((known("a.c", "USE_NET"), known("shared.h", "LEVEL > 1"), known("mine.h", "LEVEL == 2")), (Tri::True, Tri::True, Tri::True));
        // Only b.c defines NO_RELOCS: not known in the header a.c includes too, known in b.c's own.
        assert_eq!((known("shared.h", "defined(NO_RELOCS)"), known("mine.h", "defined(NO_RELOCS)")), (Tri::Unknown, Tri::True));
        // Nobody certainly includes win.h.
        assert_eq!((known("c.c", "defined(WIN)"), known("shared.h", "defined(WIN)")), (Tri::Unknown, Tri::Unknown));
        // Two values: known where only one of them gets to.
        assert_eq!((known("sized.h", "SIZE == 64"), known("only32.h", "SIZE == 64"), known("shared.h", "SIZE == 64")), (Tri::Unknown, Tri::False, Tri::Unknown));
    }

    #[test]
    fn a_function_like_macro_without_arguments_is_just_a_name() {
        let t = transparent("#define dump(a, b)
");
        let m = masked("static int dump = 1;
void f (void) { dump (1, 2); }
", &t);
        assert!(m.contains("static int dump = 1;"), "{m}");
        assert!(!m.contains("dump (1, 2)"), "{m}");
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
        let m = masked(src, &MacroSets::default());
        let kept: Vec<&str> = m.lines().filter(|l| l.starts_with('#')).collect();
        assert_eq!(kept, ["#ifdef B", "#endif"], "{m}");
    }

    #[test]
    fn alternative_branches_do_not_unbalance_nesting() {
        let src = "#if A\nvoid f (int a\n#else\nvoid f (long a\n#endif\n ) { }\n#ifdef B\nint b;\n#endif\n";
        let m = masked(src, &MacroSets::default());
        assert!(m.contains("#ifdef B"), "{m}");
    }

    #[test]
    fn continued_strings_become_adjacent_literals() {
        let t = MacroSets::default();
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
        let m = masked(src, &MacroSets::default());
        assert_eq!(m.lines().filter(|l| l.starts_with('#')).count(), 2, "{m}");
    }

    #[test]
    fn operator_arguments_become_zero() {
        let t = MacroSets::default();
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
        let l = lex(src.as_bytes(), &MacroSets::default());
        let got: Vec<(&str, u32)> = l.blocks.iter().map(|&(a, e, line)| (&src[a as usize..e as usize], line)).collect();
        assert_eq!(got, [("{
  if (x) { g (); }
}", 5), ("{ int a; }", 6)]);
    }

    #[test]
    fn includes_are_collected() {
        let src = "#include \"a/b.h\"\n# include <c.h> /* x */\n";
        let l = lex(src.as_bytes(), &MacroSets::default());
        let got: Vec<&str> = l.includes.iter().map(|&(a, e, _)| &src[a as usize..e as usize]).collect();
        assert_eq!(got, ["a/b.h", "c.h"]);
    }

    #[test]
    fn a_guard_within_guards_is_one_and_a_default_is_not() {
        // gmock's header, made of many with their own guards.
        let src = "#ifndef GMOCK_H_
#define GMOCK_H_
#ifndef GMOCK_PORT_H_
#define GMOCK_PORT_H_
#include \"gtest.h\"
#endif
                   #ifndef MODE_T
#define MODE_T
#include \"mode.h\"
#endif
#endif
";
        let l = lex(src.as_bytes(), &MacroSets::default());
        let got: Vec<(&str, bool)> = l.includes.iter().map(|&(a, e, c)| (&src[a as usize..e as usize], c)).collect();
        assert_eq!(got, [("gtest.h", false), ("mode.h", true)]);
    }

    #[test]
    fn constexpr_and_standard_attribute_macros_are_transparent() {
        let t = transparent("#define FMT_CONSTEXPR constexpr
#define FMT_CONSTEXPR
#define NODISCARD [[nodiscard]]
#define BOTH [[nodiscard]] inline
");
        for name in ["FMT_CONSTEXPR", "NODISCARD", "BOTH"] {
            assert!(t.contains(name.as_bytes()), "{name}");
        }
    }
}
