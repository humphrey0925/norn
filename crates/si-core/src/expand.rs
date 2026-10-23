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
use crate::model::{flag, kind, refkind, root, site, Bind, Def, FileFacts, MemberHint, Ref, Site, NONE};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

/// Where expansion spends its time, in nanoseconds (`SI_EXPAND_PROFILE`).
static SPENT: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
fn spend(i: usize, since: std::time::Instant) {
    SPENT[i].fetch_add(since.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Distinct expansions kept for one use; where every compilation was read
/// (see [`Table::thorough`]), more.
const MAX_VARIANTS: usize = 24;
const MAX_VARIANTS_READ: usize = 40;
/// Definitions of one name tried at one use.
const MAX_CANDIDATES: usize = 16;
/// Macro invocations and tokens allowed in one expansion.
const MAX_STEPS: u32 = 500;
/// Expansions in a row that may bring no new name before the rest are left untried.
const BLIND_RUNS: usize = 3;
/// Where thorough: other definitions of one name read in a row that bring
/// nothing, after which the rest of that name's are not read; and uses of a
/// file in a row where that name's brought nothing, after which they are not
/// read at its uses that follow (Catch2's warning pragmas, one definition
/// per compiler, read again at every assertion of a test file).
const BLIND_ALTERNATIVES: usize = 2;
const BLIND_USES: usize = 20;
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

/// The text of a token: one copy of each distinct text for the life of the
/// process, and a token holds a pointer to it.
///
/// Tokens are copied far more often than they are made: every expansion
/// copies its macro's body and its arguments, and the kernel's are
/// expanded ten million times. Counting references on shared text made
/// every core wait on the others for the same few counters, and a copy of
/// its own for each engine made an allocation per token. A pointer costs
/// nothing to copy. What is kept is small: identifiers, numbers and
/// punctuation, which recur; string literals, which do not, are all the
/// one empty string (nothing here reads what a string says).
#[derive(Clone, Copy)]
struct Text(&'static str);

// (Every text is the pool's one copy of it: the same text is the same
// pointer, compared and hashed as one.)
impl PartialEq for Text {
    fn eq(&self, other: &Text) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for Text {}

impl std::hash::Hash for Text {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        (self.0.as_ptr() as usize).hash(h)
    }
}

const SHARDS: usize = 64;
/// Every text in use, by shard of its hash. The set owns the strings; a
/// `Text` points into one of them.
static POOL: [std::sync::Mutex<Option<FxHashSet<Box<str>>>>; SHARDS] = [const { std::sync::Mutex::new(None) }; SHARDS];

impl Text {
    fn of(s: &str) -> Text {
        let shard = xxhash_rust::xxh3::xxh3_64(s.as_bytes()) as usize % SHARDS;
        let mut pool = POOL[shard].lock().unwrap_or_else(|e| e.into_inner());
        let set = pool.get_or_insert_with(FxHashSet::default);
        if !set.contains(s) {
            set.insert(s.into());
        }
        let kept: &str = set.get(s).unwrap();
        // SAFETY: the string is on the heap and stays where it is while the
        // set holds it, whatever the set does with its own table; it is
        // only let go by `release`, which is called when no `Text` is left.
        Text(unsafe { &*(kept as *const str) })
    }

    /// Let go of every text. Only when no token made so far is still held:
    /// at the end of an expansion of the corpus, whose tokens, macro table
    /// and engines are all gone by then. In a tree the size of the kernel
    /// this is several hundred megabytes of identifiers and numbers.
    fn release() {
        for shard in &POOL {
            *shard.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}

/// Held by whoever makes or holds tokens: the texts are let go when the
/// last holder is done. (Not under test, where tokens are made freely by
/// tests running side by side.)
struct Holding;

static HOLDERS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

impl Holding {
    fn new() -> Holding {
        *HOLDERS.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        Holding
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        let mut holders = HOLDERS.lock().unwrap_or_else(|e| e.into_inner());
        *holders -= 1;
        // Under the lock: nobody starts holding while the texts go.
        if *holders == 0 && !cfg!(test) {
            Text::release();
        }
    }
}

impl std::ops::Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        self.0
    }
}

impl From<&str> for Text {
    fn from(s: &str) -> Text {
        Text::of(s)
    }
}

impl From<String> for Text {
    fn from(s: String) -> Text {
        Text::of(&s)
    }
}

impl From<std::borrow::Cow<'_, str>> for Text {
    fn from(s: std::borrow::Cow<'_, str>) -> Text {
        Text::of(&s)
    }
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug)]
struct T {
    text: Text,
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
                if b == b'"' {
                    // What a string says is nothing an expansion is read for.
                    out.push(T { text: "\"\"".into(), k: K::Other, origin, noexpand: false });
                    continue;
                }
                K::Other
            }
            _ => {
                let two = [b, at(i + 1)];
                i += if at(i + 1) == b'.' && at(i + 2) == b'.' && b == b'.' {
                    3
                } else if matches!(&two, b"##" | b"->" | b"::" | b"==" | b"!=" | b"<=" | b">=" | b"&&" | b"||" | b"<<" | b">>") {
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
/// (Its tokens are gone when it returns. It is called for every `#define`
/// of the corpus while files are read, which is over before any expansion
/// begins: it does not register as a holder, which would be a lock taken
/// by every thread for every definition.)
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
    // C++: a template defined (bitcoin's `SER_PARAMS_OPFUNC`, an
    // `operator ()`), which a class body takes as a member of its own.
    if toks.windows(2).any(|w| w[0].k == K::Ident && &*w[0].text == "template" && w[1].is("<")) {
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
    params: Vec<Text>,
    variadic: bool,
    function_like: bool,
    body: Vec<T>,
    /// See [`hides_names`].
    hides: bool,
    /// `#if NAME == value` (or, with true, its `#else`) around the definition.
    guards: Vec<(Text, i64, bool)>,
    /// Defined only under some `#if`.
    conditional: bool,
    /// Defined under `#ifndef` of its own name.
    fallback: bool,
    /// The file it is defined in, which of its definitions it is (they
    /// are in the order of the file), and the line.
    file: u32,
    at: u32,
    line: u32,
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
    let mut params: Vec<Text> = Vec::new();
    let mut variadic = false;
    if function_like {
        let close = toks.iter().position(|t| t.is(")")).unwrap_or(toks.len());
        for t in &toks[..close] {
            if t.k == K::Ident {
                params.push(t.text);
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
    Parsed { params, variadic, function_like, body: toks, hides, guards, conditional: false, fallback: false, file: 0, at: 0, line: 0, param_at }
}

struct Table<'a> {
    files: &'a [FileFacts],
    /// Per file, whether the build does not compile it, and whether it is
    /// in a folder the build does not compile (another program's; not a
    /// file of this one the configuration leaves out).
    outside: Vec<bool>,
    apart: Vec<bool>,
    /// Whether every compilation of the tree was read (`units`): what each
    /// file can see is known, and every definition of a name met is worth
    /// reading once (the kernel's `trace_events.h` stages, most of which
    /// make nothing new for a given use). Else the many a file may see are
    /// tried only while they bring something.
    thorough: bool,
    /// How each compilation reads the headers read differently, and per
    /// file the compilation it starts and the header it is there (or
    /// `NONE`): a definition on a line a compilation does not compile is
    /// not there for it (`mpu_wrappers.h`'s renames, under `#ifndef` of what
    /// `tasks.c` defines before including it).
    readings: Option<&'a crate::units::Readings>,
    unit_of: Vec<u32>,
    header_of: Vec<u32>,
    by_name: FxHashMap<&'a str, Vec<(u32, u32)>>,
    /// The same definitions in the order of their text, each with a number
    /// that definitions of the same text (spacing aside) share: sorted and
    /// compared once for the corpus, where every file's engine would
    /// otherwise do it again for every name it meets, and a common name
    /// has thousands of definitions.
    by_text: FxHashMap<&'a str, Vec<(u32, u32, u32)>>,
    parsed: Vec<Vec<Parsed>>,
    /// Some macro's body writes `__COUNTER__`.
    counts: bool,
    /// The macros whose expansion may hold `__COUNTER__`.
    counting: FxHashSet<&'a str>,
}

type Choice = FxHashMap<Text, usize>;

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
    cands: FxHashMap<Text, Vec<&'a Parsed>>,
    /// Which definition to use for each name with several.
    choice: Choice,
    /// Names with several definitions met in this run, with how many.
    met: Vec<(Text, usize)>,
    active: Vec<Text>,
    /// The definitions those are expanded by, in the same order.
    active_defs: Vec<&'a Parsed>,
    /// The use being expanded: its file and line. A definition its own
    /// file makes after that line is not there yet.
    site: Option<(u32, u32)>,
    steps: u32,
    hiding: FxHashMap<Text, bool>,
    /// Set when the engine stands for all the translation units of this
    /// file together.
    merged: Option<u32>,
    /// Distinct expansions wanted of one input.
    max_variants: usize,
    /// Files this unit reads whatever the configuration (no `#include`
    /// under an `#if` on the way). Empty: all of them.
    sure: Vec<bool>,
    /// Where thorough: for each name, how many uses in a row its other
    /// definitions, read, brought nothing (see `BLIND_USES`).
    blind_uses: FxHashMap<Text, usize>,
    /// Expansions run, all told.
    runs: u64,
    /// The compilation it stands for, as the readings number them, or
    /// `NONE`.
    unit: u32,
    /// The names it asks the table for: the file's expansion depends on
    /// their definitions, and on nothing else of other files'.
    asked: Asked<'a>,
    /// What `__COUNTER__` stands for at the use being expanded, where that
    /// is known (`expand_file` counts the file's uses in order), and next.
    counter_from: Option<u32>,
    counter: u32,
    /// The most any reading of the use counted to.
    counter_most: u32,
}

/// Names an engine asked the table for, told to `to` when it is done.
#[derive(Default)]
struct Asked<'a> {
    to: Option<&'a std::sync::Mutex<Vec<Text>>>,
    names: Vec<Text>,
}

impl<'a> Asked<'a> {
    fn to(to: &'a std::sync::Mutex<Vec<Text>>) -> Self {
        Asked { to: Some(to), names: Vec::new() }
    }
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        if let Some(to) = self.to {
            to.lock().unwrap_or_else(|e| e.into_inner()).append(&mut self.names);
        }
    }
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
        // The macros whose expansion may hold `__COUNTER__`: written in
        // their body, or through another that may. (Only C++ asks.)
        let mut counting: FxHashSet<&'a str> = FxHashSet::default();
        if files.iter().any(|f| f.cpp()) {
            loop {
                let more: Vec<&'a str> = by_name
                    .par_iter()
                    .filter(|(name, _)| !counting.contains(*name))
                    .filter(|(_, defs)| defs.iter().any(|&(f, i)| parsed[f as usize][i as usize].body.iter().any(|t| &*t.text == "__COUNTER__" || counting.contains(&*t.text))))
                    .map(|(&name, _)| name)
                    .collect();
                if more.is_empty() {
                    break;
                }
                counting.extend(more);
            }
        }
        let counts = !counting.is_empty();
        Table { files, outside: vec![false; files.len()], apart: vec![false; files.len()], thorough: false, readings: None, unit_of: Vec::new(), header_of: Vec::new(), by_name, by_text, parsed, counts, counting }
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
            active_defs: Vec::new(),
            site: None,
            steps: 0,
            hiding: FxHashMap::default(),
            merged: None,
            max_variants: if table.thorough { MAX_VARIANTS_READ } else { MAX_VARIANTS },
            sure: Vec::new(),
            blind_uses: FxHashMap::default(),
            runs: 0,
            unit: NONE,
            asked: Asked::default(),
            counter_from: None,
            counter: 0,
            counter_most: 0,
        }
    }

    /// Is the line of this definition compiled where it is read? Not when
    /// this compilation reads its header in a way that switches it off.
    fn there(&self, p: &Parsed) -> bool {
        let (Some(r), Some(&h)) = (self.table.readings, self.table.header_of.get(p.file as usize)) else { return true };
        self.unit == NONE || h == NONE || r.compiled(self.unit, h, p.line) != crate::cond::Tri::False
    }

    /// Is there a translation unit here in which `name` is no macro?
    fn maybe_undefined(&mut self, name: &str) -> bool {
        let text = Text::of(name);
        match self.merged {
            // Several units at once: sure only if the file itself always
            // defines the name.
            Some(file) => !{
                self.asked.names.push(text);
                self.table.by_name.get(name)
            }
            .is_some_and(|v| {
                v.iter().any(|&(f, i)| f == file && !self.table.files[f as usize].macros[i as usize].conditional)
            }),
            None => {
                let sure = std::mem::take(&mut self.sure);
                let all = self.candidates(text).iter().all(|p| p.conditional || !sure.get(p.file as usize).copied().unwrap_or(true));
                self.sure = sure;
                all
            }
        }
    }

    fn candidates(&mut self, name: Text) -> &[&'a Parsed] {
        if !self.cands.contains_key(&name) {
            self.asked.names.push(name);
            // The definitions this unit can see, one of each text, in the
            // order of the texts (which must not depend on which file was
            // read first).
            let all = self.table.by_text.get(name.0).map_or(&[][..], |v| &v[..]);
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
                let header = |d: &&(u32, u32, u32)| crate::is_header(&self.table.files[d.0 as usize].path);
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
            let mut list: Vec<&'a Parsed> = seen.iter().map(|&(f, i, _)| &self.table.parsed[f as usize][i as usize]).filter(|p| self.there(p)).collect();
            // `#ifndef X` / `#define X default` gives way to another
            // definition that is there for certain; one that may not be (the
            // tracepoint headers' `TRACE_EVENT`, read again under
            // `#ifdef CREATE_TRACE_POINTS`) is another reading beside it.
            let certain = |p: &&'a Parsed| !p.fallback && !p.conditional && self.sure.get(p.file as usize).copied().unwrap_or(true);
            if list.iter().any(certain) {
                list.retain(|p| !p.fallback);
            }
            // In the cache first, so that a guard that leads back here ends.
            self.cands.insert(name, list.clone());
            let open: Vec<&'a Parsed> = list.iter().copied().filter(|p| p.guards.iter().all(|g| self.feasible(g))).collect();
            if !open.is_empty() && open.len() < list.len() {
                self.cands.insert(name, open);
            }
        }
        &self.cands[&name]
    }

    /// Can the guard hold in some translation unit with this file? It cannot
    /// when every value the name is given there says otherwise.
    fn feasible(&mut self, guard: &(Text, i64, bool)) -> bool {
        let (name, value, negated) = guard;
        let values: Option<Vec<i64>> = self
            .candidates(*name)
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
    fn hides(&mut self, name: Text) -> bool {
        if let Some(&known) = self.hiding.get(&name) {
            return known;
        }
        self.hiding.insert(name, false);
        let cands = self.candidates(name).to_vec();
        let found = cands.iter().any(|p| p.hides || p.body.iter().any(|t| t.k == K::Ident && self.hides(t.text)));
        self.hiding.insert(name, found);
        found
    }

    fn pick(&mut self, name: Text) -> Option<&'a Parsed> {
        let n = self.candidates(name).len();
        if n == 0 {
            return None;
        }
        // In the body of a definition, a name its own file defined before
        // it is that definition (`#undef X` / `#define X`, stage after
        // stage, as the kernel's `trace_events.h` does, beside `perf.h`'s).
        // (Not where the definitions are branches of an `#if`: each is
        // one reading.)
        if let Some(parent) = self.active_defs.last().copied().filter(|p| !p.conditional) {
            let here: Vec<&'a Parsed> = self.cands[&name].iter().copied().filter(|p| p.file == parent.file).collect();
            if here.iter().all(|p| !p.conditional) {
                if let Some(p) = here.into_iter().filter(|p| p.at < parent.at).max_by_key(|p| p.at) {
                    return Some(p);
                }
            }
        }
        let i = match self.choice.get(&name) {
            Some(&i) => i.min(n - 1),
            None => {
                self.choice.insert(name, 0);
                0
            }
        };
        if n > 1 && !self.met.iter().any(|m| m.0 == name) {
            self.met.push((name, n));
        }
        let p = self.cands[&name][i];
        // (Defined further down the use's own file: not yet, unless an
        // earlier definition there or elsewhere may be meant.)
        if let Some((file, line)) = self.site.filter(|s| p.file == s.0 && p.line > s.1) {
            return self.cands[&name].iter().copied().find(|q| q.file != file || q.line <= line);
        }
        Some(p)
    }

    fn expand(&mut self, input: Vec<T>) -> Vec<T> {
        let mut out = Vec::new();
        let mut stack: Vec<Item> = input.into_iter().rev().map(Item::Tok).collect();
        while let Some(item) = stack.pop() {
            let mut t = match item {
                Item::End => {
                    self.active.pop();
                    self.active_defs.pop();
                    continue;
                }
                Item::Tok(t) => t,
            };
            if t.k != K::Ident || t.noexpand || self.steps > MAX_STEPS || out.len() + stack.len() > MAX_TOKENS {
                out.push(t);
                continue;
            }
            // The preprocessor's own: the line of the use, and how many
            // `__COUNTER__`s the unit had before (where that is known).
            if matches!(&*t.text, "__LINE__" | "__COUNTER__") && self.candidates(t.text).is_empty() {
                let n = if &*t.text == "__LINE__" {
                    self.site.map(|s| s.1)
                } else {
                    self.counter_from.map(|_| {
                        self.counter += 1;
                        self.counter - 1
                    })
                };
                if let Some(n) = n {
                    out.push(T { text: Text::of(&n.to_string()), k: K::Other, origin: t.origin, noexpand: false });
                    continue;
                }
            }
            if self.active.contains(&t.text) {
                t.noexpand = true;
                out.push(t);
                continue;
            }
            let Some(m) = self.pick(t.text) else {
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
                            self.active_defs.pop();
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
            self.active_defs.push(m);
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
                args[np - 1].push(comma);
                args[np - 1].extend(a);
            }
        }
        args.resize(np, Vec::new());
        let mut expanded: Vec<Option<Vec<T>>> = vec![None; np];
        let empty = || T { text: "".into(), k: K::Other, origin: Origin::Body, noexpand: false };
        let mut res: Vec<T> = Vec::with_capacity(m.body.len());
        let mut i = 0;
        while i < m.body.len() {
            let t = &m.body[i];
            let p = m.param_at[i];
            if t.is("#") && m.function_like && m.param_at.get(i + 1).is_some_and(|&p| p != NONE) {
                res.push(T { text: "\"\"".into(), k: K::Other, origin: Origin::Body, noexpand: false });
                i += 2;
                continue;
            }
            if p == NONE {
                res.push(*t);
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
                        self.active_defs.truncate(depth);
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
                let right = res[i + 1];
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
            out.push(res[i]);
            i += 1;
        }
        out.retain(|t| !t.text.is_empty());
        out
    }

    /// Every distinct expansion of `toks`, with the choices that gave it.
    /// Names in `pinned` keep the definition chosen there. Stops once a few
    /// expansions in a row hold nothing `novel`.
    /// (`keep`: return them; else only `novel` sees them.)
    fn variants(&mut self, toks: &[T], pinned: &Choice, keep: bool, mut novel: impl FnMut(&[T]) -> bool) -> Vec<(Vec<T>, Choice)> {
        let mut results: Vec<(Vec<T>, Choice)> = Vec::new();
        // The expansions read so far, by a hash of their texts.
        let mut read: FxHashSet<u64> = FxHashSet::default();
        let mut free: Vec<(Text, usize)> = Vec::new();
        // Every definition of every name met is read once before any two
        // are read together: one name, one other definition, the rest as
        // first read. (Else the outermost name, which turns slowest, had a
        // definition never read: `DEFINE_EVENT`'s tracepoint one among
        // five.)
        // (A name met only under another choice, as `DECLARE_EVENT_CLASS`
        // is only under `trace_events.h`'s `TRACE_EVENT`, is read under it.)
        let mut singles: std::collections::VecDeque<(Text, usize, Choice)> = std::collections::VecDeque::new();
        let mut combining = false;
        let mut choice = pinned.clone();
        let mut stale = 0;
        // (Where thorough: the name whose other definition was read just
        // now, and for each name how many of them in a row brought nothing.
        // Stages each bring something, as `trace_events.h`'s
        // `DECLARE_EVENT_CLASS` does; alternatives that bring nothing,
        // U-Boot's `__swab32` of every architecture, are read no further.)
        let mut single: Option<Text> = None;
        let mut blind_of: FxHashMap<Text, usize> = FxHashMap::default();
        // (Whether each name's other definitions brought something at this
        // use, for `blind_uses`.)
        let mut brought_by: FxHashMap<Text, bool> = FxHashMap::default();
        'reading: for _ in 0..self.max_variants * 2 {
            if stale >= BLIND_RUNS {
                break;
            }
            // (Where thorough, every definition is read once whatever it
            // brings: only readings of several together give up when blind.)
            stale += (combining || !self.table.thorough) as usize;
            self.choice = choice.clone();
            self.met.clear();
            self.active.clear();
            self.active_defs.clear();
            self.steps = 0;
            if let Some(from) = self.counter_from {
                self.counter = from;
            }
            let t = std::time::Instant::now();
            let out = self.expand(toks.to_vec());
            self.counter_most = self.counter_most.max(self.counter);
            spend(0, t);
            SPENT[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.runs += 1;
            SPENT[5].fetch_add(out.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let under = self.choice.clone();
            for m in self.met.drain(..) {
                if !pinned.contains_key(&m.0) && !free.iter().any(|f| f.0 == m.0) {
                    singles.extend((1..m.1).map(|i| (m.0, i, under.clone())));
                    free.push(m);
                }
            }
            let digest = {
                use std::hash::{Hash, Hasher};
                let mut h = rustc_hash::FxHasher::default();
                out.len().hash(&mut h);
                out.iter().for_each(|t| t.text.hash(&mut h));
                h.finish()
            };
            let mut brought = false;
            if read.insert(digest) {
                let t = std::time::Instant::now();
                let is_novel = novel(&out);
                spend(1, t);
                if is_novel {
                    stale = 0;
                    brought = true;
                }
                if keep {
                    results.push((out, self.choice.clone()));
                }
            }
            if let Some(name) = single.take() {
                let blind = blind_of.entry(name).or_default();
                *blind = if brought { 0 } else { *blind + 1 };
                *brought_by.entry(name).or_default() |= brought;
            }
            if read.len() >= self.max_variants {
                break;
            }
            if !combining {
                while self.table.thorough
                    && singles.front().is_some_and(|s| blind_of.get(&s.0).is_some_and(|&b| b >= BLIND_ALTERNATIVES) || self.blind_uses.get(&s.0).is_some_and(|&b| b >= BLIND_USES))
                {
                    singles.pop_front();
                }
                if let Some((name, i, under)) = singles.pop_front() {
                    single = Some(name);
                    choice = under;
                    choice.insert(name, i);
                    continue;
                }
                // Then together, from the first reading again.
                combining = true;
                choice = pinned.clone();
            }
            // Next combination, the most recently met name turning fastest.
            let mut k = free.len();
            loop {
                if k == 0 {
                    break 'reading;
                }
                k -= 1;
                let c = choice.get(&free[k].0).copied().unwrap_or(0) + 1;
                if c < free[k].1 {
                    choice.insert(free[k].0, c);
                    for f in &free[k + 1..] {
                        choice.remove(&f.0);
                    }
                    break;
                }
            }
        }
        if self.table.thorough {
            for (name, brought) in brought_by {
                let blind = self.blind_uses.entry(name).or_default();
                *blind = if brought { 0 } else { *blind + 1 };
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
    /// C++: what a member called is called on, by the member's name: a
    /// type (of an object the expansion declares, `Catch::AssertionHandler
    /// catchAssertionHandler (...)`), or a variable's name (written at the
    /// use, `EXPECT_CALL (handler, ...)`), as a [`root`] kind and name.
    receivers: FxHashMap<String, (u8, String)>,
    /// C++: classes the expansion defines, with a class they derive from
    /// (gtest's `TEST_F (Fixture, Name)`, a class of the fixture's).
    bases: Vec<(String, String)>,
    /// C++: functions whose result the expansion calls, `make (m) (x)`
    /// (gtest's `ASSERT_THAT`): the result's `operator()` is called.
    result_calls: Vec<String>,
}

impl Found {
    /// Add this expansion's names to `known`; was any of them new?
    fn novel_in(&self, known: &mut FxHashSet<String>) -> bool {
        let mut new = false;
        let mut see = |n: &str| {
            if !known.contains(n) {
                known.insert(n.to_string());
                new = true;
            }
        };
        self.single.iter().for_each(|n| see(n));
        self.decls.iter().for_each(|d| see(&d.0));
        self.refs.iter().for_each(|r| see(&r.0));
        self.bases.iter().for_each(|b| see(&format!("{}:{}", b.0, b.1)));
        self.result_calls.iter().for_each(|c| see(&format!("{c}()")));
        new
    }

    /// What another expansion of the same use found besides, once each.
    fn merge(&mut self, other: Found) {
        fn add<X: PartialEq>(into: &mut Vec<X>, more: Vec<X>) {
            for x in more {
                if !into.contains(&x) {
                    into.push(x);
                }
            }
        }
        if self.single.is_none() {
            self.single = other.single;
        }
        add(&mut self.decls, other.decls);
        for (k, v) in other.receivers {
            self.receivers.entry(k).or_insert(v);
        }
        add(&mut self.refs, other.refs);
        add(&mut self.bases, other.bases);
        add(&mut self.result_calls, other.result_calls);
    }

    fn is_empty(&self) -> bool {
        self.single.is_none() && self.decls.is_empty() && self.refs.is_empty() && self.bases.is_empty() && self.result_calls.is_empty()
    }

    /// What a definition whose name is this use is really called.
    fn def_name(&self) -> Option<&str> {
        self.single.as_deref().or(self.decls.iter().rev().find(|d| d.1 != kind::ENUM_CONST).map(|d| &*d.0))
    }
}

/// Read the names out of one expansion of the use `s` of `macro_name`.
fn recognise(toks: &[T], s: &Site, macro_name: &str, cpp: bool) -> Found {
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
    // In a `typedef`: what it names is a type, whatever follows.
    let mut typedef = false;
    // The function whose body, written in the expansion, is being read,
    // and the depth of braces its body ends at.
    let mut owner: Option<String> = None;
    let mut owner_depth = 0;
    // C++: the classes whose bodies the expansion writes (gtest's `TEST`
    // makes one, gmock's `ACTION_P` one inside another), each with the
    // depth of braces in it; and one named, its body not yet open.
    let mut classes: Vec<(String, i32)> = Vec::new();
    let mut naming: Option<String> = None;
    // A constructor's initializers, up to its body: calls, not members.
    let mut initializers_end = 0;
    // C++: the objects the expansion declares, and their types.
    let mut declared: FxHashMap<String, String> = FxHashMap::default();
    for i in 0..n {
        let t = &toks[i];
        if t.k == K::Punct {
            match &*t.text {
                "(" => paren += 1,
                ")" => paren -= 1,
                "{" => {
                    brace += 1;
                    if let Some(class) = naming.take().filter(|_| paren == 0) {
                        classes.push((class, brace));
                    }
                }
                "}" => {
                    brace -= 1;
                    if brace == owner_depth {
                        owner = None;
                    }
                    while classes.last().is_some_and(|c| c.1 > brace) {
                        classes.pop();
                    }
                }
                ";" if paren == 0 => {
                    naming = None;
                    if brace == 0 {
                        (storage, typedef) = (0, false);
                    }
                }
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
            "typedef" if paren == 0 && brace == 0 => typedef = true,
            _ => {}
        }
        // C++: `class Name : public Base {`, what it derives from.
        if cpp && matches!(&*t.text, "class" | "struct") && toks.get(i + 1).is_some_and(|x| x.k == K::Ident) && toks.get(i + 2).is_some_and(|x| x.is(":")) {
            let class = toks[i + 1].text.to_string();
            let mut j = i + 3;
            loop {
                while toks.get(j).is_some_and(|x| x.k == K::Ident && matches!(&*x.text, "public" | "protected" | "private" | "virtual")) {
                    j += 1;
                }
                let mut base = None;
                while let Some(x) = toks.get(j) {
                    if x.k == K::Ident {
                        base = Some(x.text.to_string());
                    } else if !x.is("::") {
                        break;
                    }
                    j += 1;
                }
                // (`Base<T>`: its arguments are no part of the name.)
                if toks.get(j).is_some_and(|x| x.is("<")) {
                    let mut depth = 0;
                    while let Some(x) = toks.get(j) {
                        depth += x.is("<") as i32 - x.is(">") as i32;
                        j += 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                let Some(base) = base else { break };
                found.bases.push((class.clone(), base));
                if !toks.get(j).is_some_and(|x| x.is(",")) {
                    break;
                }
                j += 1;
            }
        }
        if cpp && paren == 0 && matches!(&*t.text, "class" | "struct") && toks.get(i + 1).is_some_and(|x| x.k == K::Ident) {
            naming = Some(toks[i + 1].text.to_string());
        }
        if is_keyword(t.text.as_bytes()) {
            continue;
        }
        let prev = i.checked_sub(1).map(|j| &toks[j]);
        let next = toks.get(i + 1);
        let next_is = |s: &str| next.is_some_and(|x| x.is(s));
        let name = || t.text.to_string();
        // `Type object (...)`, `Type object = ...`, `Type object;`.
        if cpp && prev.is_some_and(|p| p.k == K::Ident && !is_keyword(p.text.as_bytes())) && (next_is("(") || next_is("{") || next_is("=") || next_is(";")) {
            declared.insert(name(), prev.unwrap().text.to_string());
        }
        // C++: a member written in the body of a class the expansion makes
        // (a constructor, `~` a destructor, a method), named with its
        // classes: defined where a body, a constructor's initializers or
        // `= default` follow it.
        if cpp && paren == 0 && next_is("(") && i >= initializers_end && classes.last().is_some_and(|c| c.1 == brace) {
            let dtor = prev.is_some_and(|p| p.is("~"));
            let placed = prev.is_none_or(|p| dtor || decl_prev(p) || [";", "{", "}", ":", "&", "&&", ">", "*"].iter().any(|s| p.is(s)));
            let not_member = matches!(
                &*t.text,
                "static_assert" | "decltype" | "alignas" | "alignof" | "noexcept" | "throw" | "new" | "delete" | "typeid" | "operator" | "requires"
            );
            if placed && !not_member {
                let mut depth = 0;
                let close = toks[i + 1..].iter().position(|x| {
                    depth += x.is("(") as i32 - x.is(")") as i32;
                    depth == 0
                });
                let mut after = close.map(|c| i + 2 + c);
                while let Some(a) = after.filter(|&a| toks.get(a).is_some_and(|x| (x.k == K::Ident && matches!(&*x.text, "const" | "override" | "final" | "noexcept" | "volatile")) || x.is("&") || x.is("&&"))) {
                    after = Some(a + 1);
                }
                let at = |k: usize, s: &str| toks.get(k).is_some_and(|x| x.is(s));
                let body = after.is_some_and(|a| at(a, "{") || at(a, ":"));
                let defaulted = after.is_some_and(|a| at(a, "=") && at(a + 1, "default"));
                let declared = after.is_some_and(|a| at(a, ";") || at(a, "="));
                if body || defaulted || declared {
                    let mut parts: Vec<&str> = classes.iter().map(|c| c.0.as_str()).collect();
                    let own = if dtor { format!("~{}", t.text) } else { name() };
                    parts.push(&own);
                    let qualified = parts.join("::");
                    found.decls.push((qualified.clone(), kind::FUNCTION, if body || defaulted { 0 } else { flag::DECL }));
                    if body {
                        owner = Some(qualified);
                        owner_depth = brace;
                        // (Its body opens after `)` or `}`, `a{1}` being an
                        // initializer; or at once, where the initializers
                        // came to nothing.)
                        if let Some(a) = after.filter(|&a| at(a, ":")) {
                            let mut depth = 0;
                            initializers_end = (a + 1..n)
                                .find(|&k| {
                                    depth += toks[k].is("(") as i32 - toks[k].is(")") as i32;
                                    depth == 0 && toks[k].is("{") && (toks[k - 1].is(")") || toks[k - 1].is("}") || k == a + 1)
                                })
                                .unwrap_or(n);
                        }
                    }
                    continue;
                }
            }
        }
        if cpp && next_is("(") && t.origin != Origin::Site {
            let mut depth = 0;
            let close = toks[i + 1..].iter().position(|x| {
                depth += x.is("(") as i32 - x.is(")") as i32;
                depth == 0
            });
            if close.is_some_and(|c| toks.get(i + 2 + c).is_some_and(|x| x.is("("))) {
                found.result_calls.push(name());
            }
        }
        if cpp && !in_fn && paren == 0 && brace == 0 && prev.is_some_and(|p| p.is("::")) && next_is("(") {
            // C++: `A::B::f (...)`, a member defined apart (what gtest's
            // `TEST (Suite, Name)` ends in: `Suite_Name_Test::TestBody ()`),
            // named with its classes.
            let mut parts = vec![name()];
            let mut j = i;
            while j >= 2 && toks[j - 1].is("::") && toks[j - 2].k == K::Ident && !is_keyword(toks[j - 2].text.as_bytes()) {
                parts.push(toks[j - 2].text.to_string());
                j -= 2;
            }
            parts.reverse();
            // What comes before a name defined is a type, or nothing: after
            // `::` of its own, `=`, `else` or `return` it is a call
            // (gtest's `else ::testing::internal::AssertHelper (...)`).
            let before = j.checked_sub(1).map(|k| &toks[k]);
            let declared = before.is_none_or(|b| {
                (decl_prev(b) && !matches!(&*b.text, "throw" | "new" | "delete" | "co_return")) || [";", "}", "{", ")", "&", "&&", ">"].iter().any(|s| b.is(s))
            });
            if !declared {
                continue;
            }
            let mut depth = 0;
            let close = toks[i + 1..].iter().position(|x| {
                depth += x.is("(") as i32 - x.is(")") as i32;
                depth == 0
            });
            // After the parameters, `const`, `override` and the like may come.
            let after = close.and_then(|c| toks[i + 2 + c..].iter().find(|x| !(x.k == K::Ident && matches!(&*x.text, "const" | "override" | "final" | "noexcept"))));
            let defined = match after {
                Some(a) => a.is("{"),
                None => s.flags & site::BRACE_FOLLOWS != 0,
            };
            found.decls.push((parts.join("::"), kind::FUNCTION, if defined { 0 } else { flag::DECL }));
            if after.is_some_and(|a| a.is("{")) {
                owner = Some(parts.join("::"));
                owner_depth = 0;
            }
            continue;
        }
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
                    // What it is called on: a name, or one in parentheses.
                    let object = match i.checked_sub(2).map(|j| &toks[j]) {
                        Some(o) if o.k == K::Ident => Some(o),
                        Some(o) if o.is(")") && i >= 4 && toks[i - 4].is("(") && toks[i - 3].k == K::Ident => Some(&toks[i - 3]),
                        _ => None,
                    };
                    if let Some(o) = object.filter(|o| cpp && &*o.text != "this" && !is_keyword(o.text.as_bytes())) {
                        let root = match declared.get(&*o.text) {
                            Some(ty) => (crate::model::root::TYPE, ty.clone()),
                            None => (crate::model::root::VAR, o.text.to_string()),
                        };
                        found.receivers.entry(name()).or_insert(root);
                    }
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
        // C++: a function returning a template or a reference
        // (`::testing::MockSpec<F> gmock_Name (...) const { }`, of gmock's
        // `MOCK_METHOD`), where it is defined.
        let returned = cpp && prev.is_some_and(|p| p.is(">") || p.is(">>") || p.is("&") || p.is("&&"));
        if !in_fn && paren == 0 && brace == 0 && (prev.is_some_and(decl_prev) || returned) {
            // (`typedef u64 (*btf_name) (...)`, of the kernel's `BPF_CALL_x`:
            // no function `u64`.)
            if next_is("(") && !typedef {
                let mut depth = 0;
                let close = toks[i + 1..].iter().position(|x| {
                    depth += x.is("(") as i32 - x.is(")") as i32;
                    depth == 0
                });
                // (After the parameters, in C++, `const` and the like.)
                let qualifier = |x: &&T| cpp && (x.k == K::Ident && matches!(&*x.text, "const" | "override" | "final" | "noexcept" | "volatile") || x.is("&") || x.is("&&"));
                let after = close.and_then(|c| toks[i + 2 + c..].iter().find(|x| !qualifier(x)));
                let defined = match after {
                    Some(a) => a.is("{"),
                    None => s.flags & site::BRACE_FOLLOWS != 0,
                };
                if returned && !prev.is_some_and(decl_prev) && !defined {
                    continue;
                }
                found.decls.push((name(), kind::FUNCTION, storage & flag::STATIC | if defined { 0 } else { flag::DECL }));
                // A body written out in the expansion: what it calls, it calls.
                if after.is_some_and(|a| a.is("{")) {
                    owner = Some(name());
                    owner_depth = 0;
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
            // Last of all, and the use is called: what follows the use is
            // its arguments (`guard(mutex)(m)`, which ends in
            // `class_mutex_constructor`).
            let called = next_is("(") || i + 1 == n && s.flags & site::CALLED != 0;
            found.refs.push((name(), if called { refkind::CALL } else { refkind::USE }, owner.clone()));
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

/// A file's names by their number among all files' names.
struct Names {
    ids: FxHashMap<u32, u32>,
}

impl Names {
    fn id(&mut self, f: &mut FileFacts, s: &str) -> u32 {
        *self.ids.entry(crate::names::id(s)).or_insert_with(|| f.push_name(s))
    }

    /// C++: a name an expansion defines as `A::B::f`: `f`, and its classes
    /// as written and the last (`A::B`, `B`).
    fn member(&mut self, f: &mut FileFacts, s: &str) -> (u32, Option<(u32, u32)>) {
        match s.rsplit_once("::") {
            Some((chain, own)) if !chain.is_empty() => {
                let class = chain.rsplit("::").next().unwrap_or(chain);
                (self.id(f, own), Some((self.id(f, chain), self.id(f, class))))
            }
            _ => (self.id(f, s), None),
        }
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
        if rev[f as usize].is_empty() || crate::is_unit(&files[f as usize].path) {
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
    /// The class whose body the use is in (`Site::class`).
    class: u32,
    /// The references some of its expansions make and others do not: each
    /// is one reading (another definition of a name, another branch of a
    /// `COND_CODE`), so they are not certain.
    partial: FxHashSet<(String, u8)>,
    found: Found,
    /// The initialized objects the expansions declare: each its type's
    /// name and the names in the initializer.
    stored: Vec<(String, Vec<(Place, String)>)>,
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
    asked: &std::sync::Mutex<Vec<Text>>,
) -> Option<(FileFacts, u64, u64)> {
    if f.sites.is_empty() && f.inits.is_empty() {
        return None;
    }
    // One engine per translation unit the file is in, so that what one macro
    // expands to agrees with what the others do there. A header in too many
    // gets a single engine that sees them all at once.
    let mut units = roots(fi as u32, table.files, rev);
    // A file the build compiles is read by the compilations it makes, and
    // sees nothing of folders it does not compile (another program's). So
    // does one of this program the configuration leaves out (a driver
    // Kbuild does not build): it is the program's all the same.
    let built = !table.apart[fi];
    if !table.outside[fi] && units.iter().any(|&r| !table.outside[r as usize]) {
        units.retain(|&r| !table.outside[r as usize]);
    }
    let seen = |mut v: Vec<bool>| {
        if built {
            v.iter_mut().zip(&table.apart).filter(|(_, &o)| o).for_each(|(v, _)| *v = false);
        }
        v
    };
    let make_engines = || -> Vec<Engine> {
        if units.len() <= MAX_UNITS {
            units
                .iter()
                .map(|&r| Engine {
                    sure: closure(r, sure_fwd),
                    unit: table.unit_of.get(r as usize).copied().unwrap_or(NONE),
                    asked: Asked::to(asked),
                    ..Engine::new(table, seen(closure(r, fwd)))
                })
                .collect()
        } else {
            // (What it reads for certain: what it includes so, itself.)
            vec![Engine { merged: Some(fi as u32), sure: closure(fi as u32, sure_fwd), asked: Asked::to(asked), ..Engine::new(table, seen(visible(fi as u32, fwd, rev))) }]
        }
    };
    let t_setup = std::time::Instant::now();
    let mut engines = make_engines();
    let mut out = f.clone();
    let mut names = Names { ids: f.shared_names().enumerate().filter(|&(_, id)| id != NONE).map(|(i, id)| (id, i as u32)).collect() };
    let (n_defs, n_refs) = (f.defs.len(), f.refs.len());
    let live: Vec<&Site> =
        f.sites.iter().filter(|s| engines.iter_mut().any(|e| e.hides(Text::of(f.name(s.name as usize))))).collect();

    // C++: what `__COUNTER__` stands for at each use, its uses counted in
    // the file's order (Catch2's `TEST_CASE` makes `CATCH2_INTERNAL_TEST_0`,
    // then `_2`), each use's readings expanded to count them: the most any
    // counts to (a build's own definitions agree with each other, and expand
    // in full; some readings of others together come to nothing).
    let at = |s: &Site| s as *const Site as usize;
    let mut counter_at: FxHashMap<usize, u32> = FxHashMap::default();
    if let Some(eng) = engines.first_mut().filter(|_| f.cpp() && table.counts) {
        let mut next = 0;
        for s in &live {
            // (A use that names no macro that counts counts nothing.)
            let toks = site_tokens(f, s);
            if !toks.iter().any(|t| &*t.text == "__COUNTER__" || table.counting.contains(&*t.text)) {
                counter_at.insert(at(s), next);
                continue;
            }
            (eng.counter_from, eng.counter_most, eng.site) = (Some(next), next, Some((fi as u32, s.line)));
            eng.variants(&toks, &Choice::default(), false, |_| true);
            eng.counter = eng.counter_most;
            counter_at.insert(at(s), next);
            next = eng.counter;
        }
        eng.counter_from = None;
    }

    spend(2, t_setup);
    // 1. Definitions written under a macro's name: one per expansion.
    let mut variants: FxHashMap<u32, Vec<(u32, Contexts)>> = FxHashMap::default();
    for s in live.iter().filter(|s| s.def != NONE) {
        let toks = site_tokens(f, s);
        let macro_name = f.name(s.name as usize);
        let mut named: Vec<(String, Contexts)> = Vec::new();
        for (ei, eng) in engines.iter_mut().enumerate() {
            (eng.site, eng.counter_from) = (Some((fi as u32, s.line)), counter_at.get(&at(s)).copied());
            // Where the name is not a macro, or may not be, it is the name.
            if (s.args == NONE || s.flags & site::TRAILING != 0) && eng.maybe_undefined(macro_name) {
                add_name(&mut named, macro_name, (ei, Choice::default()));
            }
            let mut known = FxHashSet::default();
            let novel = |exp: &[T]| recognise(exp, s, macro_name, f.cpp()).novel_in(&mut known);
            for (exp, choice) in eng.variants(&toks, &Choice::default(), true, novel) {
                if let Some(n) = recognise(&exp, s, macro_name, f.cpp()).def_name() {
                    add_name(&mut named, n, (ei, choice));
                }
            }
        }
        if named.iter().all(|x| x.0 == macro_name) {
            continue;
        }
        let base = out.defs[s.def as usize];
        for (i, (n, contexts)) in named.into_iter().enumerate() {
            let (name, classes) = names.member(&mut out, &n);
            let idx = if i == 0 {
                out.defs[s.def as usize].name = name;
                s.def
            } else {
                out.defs.push(Def { name, ..base });
                (out.defs.len() - 1) as u32
            };
            if let Some((chain, class)) = classes {
                out.cxx_mut().owners.push((idx, chain, class));
            }
            variants.entry(s.def).or_default().push((idx, contexts));
        }
    }

    // 2. Each of those has the whole body.
    let hinted: FxHashMap<u32, u32> = f.ref_hints.iter().copied().collect();
    for (i, r) in f.refs.iter().enumerate() {
        if let Some(v) = variants.get(&r.scope) {
            for &(scope, _) in &v[1..] {
                out.refs.push(Ref { scope, ..r });
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
                    engines[*ei].site = Some((fi as u32, s.line));
                    engines[*ei].counter_from = counter_at.get(&at(s)).copied();
                    let maybe = engines[*ei].maybe_undefined(macro_name);
                    // Each expansion read once, as it is made; what they
                    // find together kept once.
                    let mut known = FxHashSet::default();
                    let mut together = Made { line: s.line, scope, maybe, first: i == 0, class: s.class, partial: FxHashSet::default(), found: Found::default(), stored: Vec::new() };
                    // What every expansion refers to, and what any does.
                    let (mut every, mut any): (Option<FxHashSet<(String, u8)>>, FxHashSet<(String, u8)>) = (None, FxHashSet::default());
                    let novel = |exp: &[T]| {
                        let mut found = recognise(exp, s, macro_name, f.cpp());
                        let here: FxHashSet<(String, u8)> = found.refs.iter().map(|r| (r.0.clone(), r.1)).collect();
                        every = Some(match every.take() {
                            None => here.clone(),
                            Some(e) => e.intersection(&here).cloned().collect(),
                        });
                        any.extend(here);
                        let new = found.novel_in(&mut known);
                        // A use that names a definition: what else it makes,
                        // and what that calls; not its own name (a call of
                        // itself, from its own body).
                        if s.def != NONE {
                            found.single = None;
                            found.refs.retain(|r| r.2.is_some());
                        }
                        // A macro that declares a whole initialized object.
                        let stored = declared_initializer(exp)
                            .filter(|_| s.flags & site::TOP != 0)
                            .map(|(type_name, array, at)| (type_name, init_names(&exp[at..], array)));
                        if !found.is_empty() || stored.is_some() {
                            together.found.merge(found);
                            together.stored.extend(stored);
                        }
                        new
                    };
                    engines[*ei].variants(&toks, pinned, false, novel);
                    let every = every.unwrap_or_default();
                    together.partial = any.into_iter().filter(|r| !every.contains(r)).collect();
                    if !together.found.is_empty() || !together.stored.is_empty() {
                        fruitful = true;
                        made.push(together);
                    }
                }
            }
            let n = barren.entry(s.name).or_default();
            *n = if fruitful { 0 } else { *n + 1 };
        }
        made
    };
    // (A use that names a definition may make other things beside it:
    // `SYSCALL_DEFINE2(open, ...) { }` makes `__x64_sys_open` and others
    // that call the one with the body. What it names is made twice, once.)
    let rest: Vec<&Site> = live.to_vec();
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
    for mut m in made {
        for (n, kind, flags) in m.found.decls {
            // Written in a class's body: a member of it.
            let n = if kind == kind::FUNCTION && m.class != NONE { format!("{}::{n}", f.name(m.class as usize)) } else { n };
            // (Known to the uses inside it by its whole name, `A::f`.)
            let whole = names.id(&mut out, &n);
            let (name, classes) = names.member(&mut out, &n);
            // Made where it may not be compiled (`_PCI_NOP_ALL(read, *)` in
            // `#else /* CONFIG_PCI is not enabled */`): as one written there.
            let undecided = kind == kind::FUNCTION && f.undecided.iter().any(|&(a, b, _, _)| a <= m.line && m.line <= b);
            let flags = flags | if undecided { flag::CONDITIONAL } else { 0 };
            let def = Def { name, line: m.line, end_line: m.line, kind, flags };
            if m.first && seen_defs.insert(key(&def)) {
                made_here.insert((whole, m.line), out.defs.len() as u32);
                if let Some((chain, class)) = classes {
                    let at = out.defs.len() as u32;
                    out.cxx_mut().owners.push((at, chain, class));
                    // Written in its class's body (gmock's `MOCK_METHOD`): a
                    // member of it, called on its objects.
                    if m.class != NONE && kind == kind::FUNCTION {
                        // (By its text: a name may be in the file's table twice.)
                        let own = out.name(class as usize).to_string();
                        let record = (0..out.records.len()).rev().find(|&r| out.name(out.records[r].name as usize) == own);
                        if let Some(r) = record.map(|r| &mut out.records[r]) {
                            r.fields.push(name);
                            r.types.push(NONE);
                            r.defs.push(at);
                        }
                    }
                }
                out.defs.push(def);
            }
        }
        for (class, base) in m.found.bases.iter().filter(|_| m.first) {
            let (class, base) = (names.id(&mut out, class), names.id(&mut out, base));
            if !out.cxx().bases.contains(&(class, base)) {
                out.cxx_mut().bases.push((class, base));
            }
        }
        for callee in &m.found.result_calls {
            let (callee, op) = (names.id(&mut out, callee), names.id(&mut out, "operator()"));
            if seen_refs.insert((op, m.line, m.scope, refkind::CALL)) {
                let path = out.hint_path.len() as u32;
                out.hints.push(MemberHint { root_kind: root::FUNC, root: callee, path, len: 0 });
                let hint = out.hints.len() as u32 - 1;
                out.refs.push(Ref { name: op, line: m.line, scope: m.scope, kind: refkind::CALL });
                let at = out.refs.len() as u32 - 1;
                out.cxx_mut().operators.push((at, hint, NONE, false));
            }
        }
        let receivers = std::mem::take(&mut m.found.receivers);
        for (n, kind, owner) in m.found.refs {
            let receiver = (kind & refkind::ICALL == refkind::ICALL).then(|| receivers.get(&n)).flatten();
            let name = names.id(&mut out, &n);
            // Inside a function the expansion defines, that function is the
            // scope; and the call is there whenever the function is: that
            // the macro may not be defined says only that neither may be.
            let made = owner.and_then(|o| made_here.get(&(names.id(&mut out, &o), m.line)).copied());
            let scope = made.unwrap_or(m.scope);
            let partial = m.partial.contains(&(n.clone(), kind));
            let kind = kind | if (m.maybe || partial) && made.is_none() { refkind::MAYBE } else { 0 };
            if seen_refs.insert((name, m.line, scope, kind)) {
                out.refs.push(Ref { name, line: m.line, scope, kind });
                // The object a member is called on: a local of the function
                // the use is in, as its type; else as the expansion has it.
                if let Some((root_kind, root)) = receiver {
                    let local = (*root_kind == crate::model::root::VAR)
                        .then(|| f.locals.iter().rev().find(|l| l.scope == m.scope && f.name(l.name as usize) == root && l.type_name != NONE))
                        .flatten();
                    // In a method, a name not local is a member of its class
                    // (a test's fixture's `handler`), as the parser has it.
                    let class = (*root_kind == crate::model::root::VAR && local.is_none())
                        // (The last said of it: what the parser took for a function of
                        // a namespace, `TEST_F (...) { }`, the expansion made a method.)
                        .then(|| out.cxx.as_ref()?.owners.iter().rev().find(|o| o.0 == m.scope).map(|o| o.2))
                        .flatten();
                    let path = out.hint_path.len() as u32;
                    let (root_kind, root, len) = match (local, class) {
                        (Some(l), _) => (crate::model::root::TYPE, names.id(&mut out, f.name(l.type_name as usize)), 0),
                        (None, Some(c)) => {
                            let member = names.id(&mut out, root);
                            out.hint_path.push(member);
                            (crate::model::root::TYPE, c, 1)
                        }
                        (None, None) => (*root_kind, names.id(&mut out, root), 0),
                    };
                    out.hints.push(MemberHint { root_kind, root, path, len });
                    out.ref_hints.push((out.refs.len() as u32 - 1, out.hints.len() as u32 - 1));
                }
            }
        }
        for (type_name, stored) in m.stored {
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
                eng.variants(toks, &Choice::default(), false, novel);
            }
            eng.max_variants = if table.thorough { MAX_VARIANTS_READ } else { MAX_VARIANTS };
        };
        if units.len() <= MAX_UNITS {
            for (eng, &unit) in engines.iter_mut().zip(&units) {
                read(eng, unit);
            }
        } else {
            // What is stored where differs in every unit and all of it
            // counts, so here each unit is worth its own engine.
            for &unit in units.iter().take(MAX_INIT_UNITS) {
                read(&mut Engine::new(table, seen(closure(unit, fwd))), unit);
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
    out.refs.map_scopes(|scope| new_index[scope as usize]);
    for b in &mut out.binds {
        b.scope = new_index[b.scope as usize];
    }
    // What these say is shared with the file's own facts until it is
    // written to: written to only where the order changed what it says.
    let moved = |d: u32| new_index[d as usize] != d;
    if out.field_types.iter().any(|t| moved(t.0)) {
        for t in out.field_types.iter_mut() {
            t.0 = new_index[t.0 as usize];
        }
    }
    if out.param_calls.iter().any(|c| moved(c.scope)) {
        for c in out.param_calls.iter_mut() {
            c.scope = new_index[c.scope as usize];
        }
    }
    if out.typed.iter().any(|t| moved(t.0)) {
        for t in out.typed.iter_mut() {
            t.0 = new_index[t.0 as usize];
        }
    }
    if let Some(c) = out.cxx.as_mut() {
        for o in &mut c.owners {
            o.0 = new_index[o.0 as usize];
        }
        for p in &mut c.operator_params {
            p.0 = new_index[p.0 as usize];
        }
        for a in &mut c.arity {
            a.0 = new_index[a.0 as usize];
        }
        for p in &mut c.params {
            p.0 = new_index[p.0 as usize];
        }
        for d in &mut c.explicit {
            *d = new_index[*d as usize];
        }
        for f in &mut c.forwards {
            f.def = new_index[f.def as usize];
        }
    }
    if out.records.iter().flat_map(|r| &r.defs).any(|&d| d != NONE && moved(d)) {
        for d in out.records.iter_mut().flat_map(|r| r.defs.iter_mut()).filter(|d| **d != NONE) {
            *d = new_index[*d as usize];
        }
    }
    // The uses are expanded: the link has no use for them, nor for the
    // initializers that were read.
    out.sites = Vec::new();
    out.inits = Vec::new();
    // (Locals are looked up in the file's own facts, never in this copy.)
    out.locals = Vec::new();
    out.local_uses = Vec::new();
    out.shrink();
    let added = ((out.defs.len() - n_defs) as u64, (out.refs.len() - n_refs) as u64);
    Some((out, added.0, added.1))
}

/// Expansions kept from one link to the next. What a file expands to
/// depends on the file, on the include edges (and the readings and the
/// like) of the corpus, and on the macros of the names its expansion asks
/// for: while the edges stand (the usual edit touches none), only changed
/// files are expanded again, and those that asked for a name whose macros
/// changed.
#[derive(Default)]
pub struct Cache {
    /// Digest of the files and include edges, the readings and the like:
    /// all but the macros.
    environment: u64,
    /// Per file: a digest of its macros.
    macros: Vec<u64>,
    /// Per file: the names its expansion asked the table for, as a bloom
    /// filter (`Asked::bloom`; `None`: not known, as from a save of before
    /// they were).
    asked: Vec<Option<Box<[u64]>>>,
    /// The macros files had before they were read again (`forget`), each
    /// as its name and a digest of its definition (`macro_pairs`): those
    /// that are not as they were are the ones that changed.
    gone: FxHashMap<usize, Vec<(u64, u64)>>,
    /// The same of the headers compilations read more than one way, as the
    /// last expansion had them, with which stretch of the header each is in.
    stretched: FxHashMap<usize, Vec<(u64, u64)>>,
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
    /// Links are made again of these files after edits (a session's):
    /// each leaves what the next can start from (`last`), and is handed
    /// the graph before (`previous`).
    pub(crate) relinks: bool,
    pub(crate) last: Option<Box<crate::link::Last>>,
    pub(crate) previous: Option<crate::graph::Graph>,
    /// Of the files read again since the last link, what that link saw
    /// of them (`forget`): what they defined then.
    pub(crate) was: FxHashMap<usize, FileFacts>,
}

/// The names in use, in order, kept between links: an edit seldom brings
/// a name no file had, or takes the last use of one away, and when it does
/// it is a handful among millions.
#[derive(Default)]
pub(crate) struct NameCache {
    /// Which names ([`crate::names`]) some file uses, a bit each.
    used: Vec<u64>,
    /// Those, in lexicographic order, and each name's place in it.
    order: Vec<u32>,
    rank: Vec<u32>,
}

impl NameCache {
    /// The table for `files` as they are now: for each file, the number
    /// in the graph of each of its names, a name's number being its
    /// lexicographic rank among the names in use (`NONE` for what is the
    /// file's own text, which is no name); and the names in that order.
    /// The number a name (by its number among all files') has in the
    /// table last made, if it is in use.
    pub(crate) fn rank_of(&self, id: u32) -> Option<u32> {
        self.rank.get(id as usize).copied().filter(|&r| r != NONE)
    }

    pub(crate) fn table(&mut self, files: &[&FileFacts]) -> (Vec<Vec<u32>>, Vec<&'static str>) {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let n = crate::names::count() as usize;
        let bits: Vec<AtomicU64> = (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        files.par_iter().for_each(|f| {
            for id in f.shared_names().filter(|&id| id != NONE) {
                let (word, bit) = (&bits[id as usize / 64], 1u64 << (id % 64));
                if word.load(Relaxed) & bit == 0 {
                    word.fetch_or(bit, Relaxed);
                }
            }
        });
        let used: Vec<u64> = bits.into_iter().map(AtomicU64::into_inner).collect();
        self.used.resize(used.len(), 0);
        let changed: Vec<u32> = (0..used.len())
            .filter(|&w| used[w] != self.used[w])
            .flat_map(|w| {
                let differ = used[w] ^ self.used[w];
                (0..64).filter(move |b| differ >> b & 1 != 0).map(move |b| (w * 64 + b) as u32)
            })
            .collect();
        let is_used = |id: u32| used[id as usize / 64] >> (id % 64) & 1 != 0;
        if changed.len() > 256 {
            self.order = (0..n as u32).filter(|&id| is_used(id)).collect();
            self.order.par_sort_unstable_by_key(|&id| crate::names::text(id));
        } else {
            for &id in &changed {
                let at = self.order.binary_search_by_key(&crate::names::text(id), |&o| crate::names::text(o));
                match (is_used(id), at) {
                    (true, Err(at)) => self.order.insert(at, id),
                    (false, Ok(at)) => drop(self.order.remove(at)),
                    _ => {}
                }
            }
        }
        if !changed.is_empty() || self.rank.len() != n {
            self.rank.clear();
            self.rank.resize(n, NONE);
            for (place, &id) in self.order.iter().enumerate() {
                self.rank[id as usize] = place as u32;
            }
        }
        self.used = used;
        let rank = &self.rank;
        let remap = files.par_iter().map(|f| f.shared_names().map(|id| if id == NONE { NONE } else { rank[id as usize] }).collect()).collect();
        (remap, self.order.iter().map(|&id| crate::names::text(id)).collect())
    }
}

/// What of a [`Cache`] is one line per file: kept on disk between runs,
/// beside each file's expansion.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct CacheIndex {
    pub environment: u64,
    pub macros: Vec<u64>,
    pub asked: Vec<Option<Box<[u64]>>>,
    pub hashes: Vec<u64>,
    pub added: Vec<(u64, u64)>,
    /// Whether the file has an expansion saved.
    pub has: Vec<bool>,
}

impl Cache {
    pub fn index(&self) -> CacheIndex {
        CacheIndex {
            environment: self.environment,
            macros: self.macros.clone(),
            asked: self.asked.clone(),
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
        let mut expanded: Vec<Option<FileFacts>> = expanded
            .into_iter()
            .enumerate()
            .map(|(i, e)| {
                e.unwrap_or_else(|()| {
                    hashes[i] = !hashes[i];
                    None
                })
            })
            .collect();
        crate::gather(expanded.iter_mut().flatten());
        let made = vec![1; hashes.len()];
        Cache {
            environment: index.environment,
            macros: index.macros,
            asked: index.asked,
            gone: FxHashMap::default(),
            stretched: FxHashMap::default(),
            hashes,
            expanded,
            added: index.added,
            changed: Vec::new(),
            epoch: 1,
            made,
            names: NameCache::default(),
            relinks: false,
            last: None,
            previous: None,
            was: FxHashMap::default(),
        }
    }

    /// Keep what a link leaves for the next (see `link::Last`).
    pub fn relinking(&mut self) {
        self.relinks = true;
    }

    /// The graph the last link made, given back as the files change: the
    /// next link takes from it what still holds, which is the edges from
    /// each symbol. The rest is let go at once: the link makes the new
    /// graph beside what is kept.
    pub fn keep(&mut self, g: crate::graph::Graph) {
        self.previous = (self.relinks && self.last.is_some()).then(|| crate::graph::Graph { out_off: g.out_off, out: g.out, ..Default::default() });
    }

    /// The files expanded anew since the last call, for saving.
    pub fn take_changed(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.changed)
    }

    pub fn expansion(&self, i: usize) -> Option<&FileFacts> {
        self.expanded.get(i)?.as_ref()
    }

    /// Expand file `i` again at the next link even if its hash is the same.
    /// `was`: its facts as they were, whose macros may be gone.
    pub fn forget(&mut self, i: usize, was: &FileFacts) {
        if let Some(h) = self.hashes.get_mut(i) {
            *h = !*h;
        }
        // (What the link saw: the expansion, if the file had one.)
        if self.relinks && self.last.is_some() && !self.was.contains_key(&i) {
            let seen = self.expanded.get(i).and_then(Option::as_ref).unwrap_or(was);
            self.was.insert(i, seen.clone());
        }
        self.gone.insert(i, macro_pairs(was, None));
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
    }
    h
}

/// A digest of the macros of a file: what of it another file's expansion
/// may read. Of a header compilations read more than one way (`lines`:
/// those of its `#if`s and the like), which stretch between them each is
/// in: whether it is there for a compilation.
fn macro_digest(f: &FileFacts, lines: Option<&[u32]>) -> u64 {
    use xxhash_rust::xxh3::xxh3_64_with_seed as mix;
    let mut h = 3;
    for m in &f.macros {
        if let Some(lines) = lines {
            h = mix(&(lines.partition_point(|&l| l < m.line) as u64).to_le_bytes(), h);
        }
        h = mix(f.name(m.name as usize).as_bytes(), h);
        h = mix(f.name(m.text as usize).as_bytes(), h);
        h = mix(guard_text(f, m).as_bytes(), h ^ (m.function_like as u64 | (m.conditional as u64) << 1 | (m.fallback as u64) << 2));
    }
    h
}

fn name_hash(name: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(name.as_bytes())
}

/// The bits of a bloom filter of `bits` bits (a power of two) a name's
/// hash sets.
fn bloom_bits(h: u64, bits: usize) -> [usize; 3] {
    let (a, b) = (h as u32 as usize, (h >> 32) as usize | 1);
    [0, 1, 2].map(|i| a.wrapping_add(i * b) & (bits - 1))
}

/// A bloom filter of `names` (hashes), about ten bits each: a name that
/// is not among them is told so but for one in a hundred or so (a file is
/// then expanded again that need not be; none that should is not).
fn bloom(names: &[u64]) -> Box<[u64]> {
    if names.is_empty() {
        return Box::default();
    }
    let bits = (names.len() * 10).next_power_of_two().max(64);
    let mut words = vec![0u64; bits / 64];
    for &h in names {
        for b in bloom_bits(h, bits) {
            words[b / 64] |= 1 << (b % 64);
        }
    }
    words.into()
}

fn in_bloom(words: &[u64], h: u64) -> bool {
    !words.is_empty() && bloom_bits(h, words.len() * 64).iter().all(|&b| words[b / 64] & (1 << (b % 64)) != 0)
}

/// Each macro of a file as its name and a digest of all `macro_digest`
/// reads of it, sorted.
fn macro_pairs(f: &FileFacts, lines: Option<&[u32]>) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = f
        .macros
        .iter()
        .map(|m| {
            let stretch = lines.map_or(0, |l| l.partition_point(|&x| x < m.line) as u64 + 1);
            let flags = m.function_like as u64 | (m.conditional as u64) << 1 | (m.fallback as u64) << 2;
            let mut h = xxhash_rust::xxh3::xxh3_64_with_seed(f.name(m.text as usize).as_bytes(), flags ^ stretch << 8);
            h = xxhash_rust::xxh3::xxh3_64_with_seed(guard_text(f, m).as_bytes(), h);
            (name_hash(f.name(m.name as usize)), h)
        })
        .collect();
    v.sort_unstable();
    v
}

/// The names whose macros differ between two lists of `macro_pairs`.
fn differ(old: &[(u64, u64)], new: &[(u64, u64)], out: &mut FxHashSet<u64>) {
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let name = match (old.get(i), new.get(j)) {
            (Some(a), Some(b)) => a.0.min(b.0),
            (Some(a), None) => a.0,
            (None, Some(b)) => b.0,
            (None, None) => break,
        };
        let (a0, b0) = (i, j);
        while old.get(i).is_some_and(|a| a.0 == name) {
            i += 1;
        }
        while new.get(j).is_some_and(|b| b.0 == name) {
            j += 1;
        }
        if old[a0..i] != new[b0..j] {
            out.insert(name);
        }
    }
}

/// Expand the macro uses recorded in `files` into `cache.expanded`: a
/// replacement for each file that had any. Returns (uses, definitions added,
/// references added).
pub fn expand(
    files: &[FileFacts],
    includes: &[Vec<u32>],
    sure_includes: &[Vec<u32>],
    not_built: &[String],
    readings: &crate::units::Readings,
    cache: &mut Cache,
) -> (u64, u64, u64) {
    let thorough = !readings.is_empty();
    // Dropped last, after the macro table and the engines below.
    let _holding = Holding::new();
    let sites: u64 = files.iter().map(|f| (f.sites.len() + f.inits.len()) as u64).sum();
    let environment = environment(files, includes)
        ^ environment(&[], sure_includes).rotate_left(17)
        ^ sure_digest(sure_includes)
        ^ not_built.iter().fold(0, |acc, d| acc ^ xxhash_rust::xxh3::xxh3_64(d.as_bytes()))
        ^ (thorough as u64) << 63
        ^ readings.digest();
    let lines: FxHashMap<&str, &[u32]> = readings.files.iter().map(|(p, l)| (p.as_str(), &l[..])).collect();
    let macros: Vec<u64> = files.par_iter().map(|f| macro_digest(f, lines.get(f.path.as_str()).copied())).collect();
    let mut kept = cache.environment == environment && cache.hashes.len() == files.len() && cache.macros.len() == files.len();
    if kept {
        // Macros of some files changed: those files were read again (and
        // what they had is known), and the files that asked for a name of
        // theirs, then or now, are expanded again; else all are.
        let changed: Vec<usize> = (0..files.len()).filter(|&i| macros[i] != cache.macros[i]).collect();
        if !changed.is_empty() {
            kept = changed.iter().all(|i| cache.gone.contains_key(i)) && cache.asked.len() == files.len() && cache.asked.iter().all(Option::is_some);
            if kept {
                // The names whose macros are not as they were: compared with
                // what the file had, where it is in what it was in (a header
                // read more than one way as such, with the stretches).
                let mut names: FxHashSet<u64> = FxHashSet::default();
                for &i in &changed {
                    let here = lines.get(files[i].path.as_str()).copied();
                    match (here, cache.stretched.get(&i)) {
                        (Some(_), Some(old)) => differ(old, &macro_pairs(&files[i], here), &mut names),
                        (None, None) => differ(&cache.gone[&i], &macro_pairs(&files[i], None), &mut names),
                        _ => {
                            names.extend(cache.gone[&i].iter().map(|p| p.0));
                            names.extend(files[i].macros.iter().map(|m| name_hash(files[i].name(m.name as usize))));
                        }
                    }
                }
                let again: Vec<usize> = (0..files.len())
                    .into_par_iter()
                    .filter(|&i| cache.asked[i].as_ref().is_some_and(|a| names.iter().any(|&h| in_bloom(a, h))))
                    .collect();
                for i in again {
                    cache.hashes[i] = !files[i].hash;
                }
            }
        }
    }
    if std::env::var_os("SI_EXPAND_PROFILE").is_some() {
        let changed = (0..files.len().min(cache.macros.len())).filter(|&i| macros[i] != cache.macros[i]).count();
        eprintln!(
            "expand    cache kept {kept}: environment same {}, files {} -> {}, macros known {}, macros changed in {changed} files ({} told), names asked known {}",
            cache.environment == environment,
            cache.hashes.len(),
            files.len(),
            cache.macros.len(),
            cache.gone.len(),
            cache.asked.iter().filter(|a| a.is_some()).count()
        );
    }
    if !kept {
        *cache = Cache {
            environment,
            macros: Vec::new(),
            asked: vec![None; files.len()],
            gone: FxHashMap::default(),
            stretched: FxHashMap::default(),
            // No file hashes to its own complement, so every file is stale.
            hashes: files.iter().map(|f| !f.hash).collect(),
            expanded: files.iter().map(|_| None).collect(),
            added: vec![(0, 0); files.len()],
            changed: Vec::new(),
            epoch: cache.epoch,
            made: vec![0; files.len()],
            names: std::mem::take(&mut cache.names),
            relinks: cache.relinks,
            last: cache.last.take(),
            previous: cache.previous.take(),
            was: std::mem::take(&mut cache.was),
        };
    }
    cache.macros = macros;
    cache.gone.clear();
    cache.stretched = files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| lines.get(f.path.as_str()).map(|l| (i, macro_pairs(f, Some(l)))))
        .collect();
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
                let one = |(i, m): (usize, &crate::model::MacroBody)| Parsed {
                    conditional: m.conditional,
                    fallback: m.fallback,
                    file: fi as u32,
                    at: i as u32,
                    line: m.line,
                    ..parse(f.name(m.text as usize), m.function_like, guard_text(f, m))
                };
                f.macros.iter().enumerate().map(one).collect()
            })
            .collect();
        let mut table = Table::new(files, by_name, parsed);
        table.outside = files.iter().map(|f| crate::link::outside(&f.path, not_built)).collect();
        let folders: Vec<String> = not_built.iter().filter(|f| f.ends_with('/')).cloned().collect();
        table.apart = files.iter().map(|f| crate::link::outside(&f.path, &folders)).collect();
        table.thorough = thorough;
        if thorough {
            let numbered = |list: Vec<&str>| -> Vec<u32> {
                let at: FxHashMap<&str, u32> = list.into_iter().enumerate().map(|(i, p)| (p, i as u32)).collect();
                files.iter().map(|f| at.get(f.path.as_str()).copied().unwrap_or(NONE)).collect()
            };
            table.unit_of = numbered(readings.units.iter().map(|u| u.0.as_str()).collect());
            table.header_of = numbered(readings.files.iter().map(|f| f.0.as_str()).collect());
            table.readings = Some(readings);
        }
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); files.len()];
        for (from, tos) in includes.iter().enumerate() {
            for &to in tos {
                rev[to as usize].push(from as u32);
            }
        }
        let profile = std::env::var_os("SI_EXPAND_PROFILE").is_some();
        let spent: Vec<std::sync::atomic::AtomicU64> = if profile { files.iter().map(|_| Default::default()).collect() } else { Vec::new() };
        let done: Vec<(Option<(FileFacts, u64, u64)>, Box<[u64]>)> = stale
            .par_iter()
            .map(|&fi| {
                let t = std::time::Instant::now();
                let asked = std::sync::Mutex::new(Vec::new());
                let out = expand_file(fi, &files[fi], &table, includes, sure_includes, &rev, &asked);
                let mut asked: Vec<u64> = asked.into_inner().unwrap_or_else(|e| e.into_inner()).into_iter().map(|t| name_hash(t.0)).collect();
                asked.sort_unstable();
                asked.dedup();
                if profile {
                    spent[fi].store(t.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
                }
                (out, bloom(&asked))
            })
            .collect();
        if profile {
            let mut all: Vec<(u64, usize)> = spent.iter().enumerate().map(|(i, s)| (s.load(std::sync::atomic::Ordering::Relaxed), i)).collect();
            all.sort_unstable_by(|a, b| b.cmp(a));
            let total: u64 = all.iter().map(|a| a.0).sum();
            let headers: u64 = all.iter().filter(|a| crate::is_header(&files[a.1].path)).map(|a| a.0).sum();
            let at = |i: usize| SPENT[i].load(std::sync::atomic::Ordering::Relaxed);
            eprintln!("  expanding {:.1} s, recognising new {:.1} s, engines and live {:.1} s; {} runs, {} tokens out", at(0) as f64 / 1e9, at(1) as f64 / 1e9, at(2) as f64 / 1e9, at(4), at(5));
            eprintln!("expand: {:.1} cpu-s in all, {:.1} in headers; top 1% of files {:.1}", total as f64 / 1e6, headers as f64 / 1e6, all.iter().take(all.len() / 100).map(|a| a.0).sum::<u64>() as f64 / 1e6);
            for (us, i) in all.iter().take(15) {
                let f = &files[*i];
                eprintln!("  {:8.1} ms  {} sites {} inits {} rev {}", *us as f64 / 1e3, f.path, f.sites.len(), f.inits.len(), rev[*i].len());
            }
        }
        for (&fi, (d, asked)) in stale.iter().zip(done) {
            cache.asked[fi] = Some(asked.to_vec().into());
            (cache.expanded[fi], cache.added[fi]) = match d {
                // Kept in memory of this thread's, not among what the
                // workers made on the way and let go (see `crate::gather`).
                Some((f, defs, refs)) => (Some(f.clone()), (defs, refs)),
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
            ("/** a (b). */ \\\n template <typename T> \\\n auto operator()(T&& t) const { return W{*this, t}; }", true),
        ] {
            assert_eq!(hides_names(body.as_bytes(), &p), want, "{body}");
        }
    }

    #[test]
    fn a_qualified_name_after_else_or_an_operator_is_called_not_defined() {
        let s = Site { name: 0, args: NONE, line: 1, scope: 0, def: NONE, flags: 0, class: NONE };
        let decls = |src: &str| -> Vec<String> {
            recognise(&tokenize(src.as_bytes(), Origin::Body), &s, "M", true).decls.into_iter().map(|d| d.0).collect()
        };
        assert_eq!(decls("void Suite_Name_Test::TestBody ()"), ["Suite_Name_Test::TestBody"]);
        assert_eq!(decls("Suite_Name_Test::TestBody () {"), ["Suite_Name_Test::TestBody"]);
        for src in [
            "if (ok) ; else ::testing::internal::AssertHelper (1, \"m\") = ::testing::Message ()",
            "x = ::testing::Message ()",
            "return internal::Make (1)",
        ] {
            assert!(decls(src).is_empty(), "{src}: {:?}", decls(src));
        }
    }

    #[test]
    fn members_written_in_a_class_an_expansion_makes() {
        let s = Site { name: 0, args: NONE, line: 1, scope: 0, def: NONE, flags: site::TOP, class: NONE };
        let decls = |src: &str| -> Vec<(String, u8)> {
            recognise(&tokenize(src.as_bytes(), Origin::Body), &s, "M", true).decls.into_iter().filter(|d| d.1 == kind::FUNCTION).map(|d| (d.0, d.2)).collect()
        };
        // gmock's `MATCHER_P`.
        let got = decls("template <typename p0_type> class IsMatcherP : public ::testing::internal::MatcherBaseImpl<IsMatcherP<p0_type>> { public: using IsMatcherP::MatcherBaseImpl::MatcherBaseImpl; template <typename arg_type> class gmock_Impl : public ::testing::MatcherInterface<const arg_type&> { public: explicit gmock_Impl(p0_type gmock_p0) : p0(::std::forward<p0_type>(gmock_p0)) {} bool MatchAndExplain(const arg_type& arg, ::testing::MatchResultListener* result_listener) const override; void DescribeTo(::std::ostream* gmock_os) const override { *gmock_os << FormatDescription(false); } const p0_type p0; private: ::std::string FormatDescription(bool negation) const { return d(negation); } }; };");
        let names: Vec<&str> = got.iter().map(|d| d.0.as_str()).collect();
        assert_eq!(names, ["IsMatcherP::gmock_Impl::gmock_Impl", "IsMatcherP::gmock_Impl::MatchAndExplain", "IsMatcherP::gmock_Impl::DescribeTo", "IsMatcherP::gmock_Impl::FormatDescription"], "{got:?}");
        // cppcheck's `REGISTER_TEST`: the base's constructor is called, not defined.
        let got = decls("namespace { class TestCondInstance : public TestInstance { public: TestCondInstance() : TestInstance(\"TestCond\") {} TestFixture* create() override { return new TestCond; } }; }");
        let names: Vec<&str> = got.iter().map(|d| d.0.as_str()).collect();
        assert_eq!(names, ["TestCondInstance::TestCondInstance", "TestCondInstance::create"]);
    }

    #[test]
    fn a_function_returning_a_template_is_defined_where_its_body_follows() {
        let s = Site { name: 0, args: NONE, line: 1, scope: 0, def: NONE, flags: site::TOP, class: NONE };
        let decls = |src: &str, cpp: bool| -> Vec<(String, u8)> {
            recognise(&tokenize(src.as_bytes(), Origin::Body), &s, "M", cpp).decls.into_iter().filter(|d| d.1 == kind::FUNCTION).map(|d| (d.0, d.2)).collect()
        };
        // gmock's `MOCK_METHOD`: the mocker, const and all.
        assert_eq!(decls("::testing::MockSpec<F> gmock_Scalar (int a) const { return m.With (a); }", true), [("gmock_Scalar".to_string(), 0)]);
        assert_eq!(decls("std::string& name () { return n; }", true), [("name".to_string(), 0)]);
        // An object made at the top, not a function; and C's comparison.
        assert!(decls("std::vector<int> v (10);", true).is_empty());
        assert!(decls("int x = a > f (1);", false).is_empty());
    }

    #[test]
    fn a_typedef_declares_no_function() {
        // The kernel's `BPF_CALL_x`: a function pointer type, then the
        // function.
        let s = Site { name: 0, args: NONE, line: 1, scope: 0, def: NONE, flags: 0, class: NONE };
        let found = recognise(&tokenize(b"typedef u64 (*btf_f) (u32 a); u64 f (u64 a);", Origin::Body), &s, "M", false);
        let functions: Vec<&str> = found.decls.iter().filter(|d| d.1 == kind::FUNCTION).map(|d| d.0.as_str()).collect();
        assert_eq!(functions, ["f"]);
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
            // gmock's `GMOCK_PP_IF`: a name made in an argument, then its
            // arguments after it.
            ("IF_0", "(a, b) b", true),
            ("IF_1", "(a, b) a", true),
            ("PICK_one", " picked", false),
            ("CHOOSE", "(c) CAT (PICK_, CAT (IF_, c) (one, two))", true),
            // Catch2's `TEST_CASE`: a unique name, twice over.
            ("UNIQ", "(n) LINE1 (n, __COUNTER__)", true),
            ("LINE1", "(n, l) LINE2 (n, l)", true),
            ("LINE2", "(n, l) n ## l", true),
            ("TC2", "(T) void T (); int UNIQ (reg); void T ()", true),
            ("TC", "() TC2 (UNIQ (TEST_))", true),
        ];
        let mut f = FileFacts::new("a.c".into());
        for (name, text, function_like) in macros {
            let (name, text) = (f.push_name(name), f.push_text(text));
            f.macros.push(crate::model::MacroBody { name, text, function_like, guard: NONE, conditional: false, fallback: false, line: 0 });
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
            // (What a string says is not kept: every string is the empty one.)
            ("LOG (\"x\")", "printf ( \"\" )"),
            ("LOG (\"x\", 1, 2)", "printf ( \"\" , 1 , 2 )"),
            ("SELF", "SELF + 1"),
            ("CAT", "CAT"),
            ("CHOOSE (1)", "picked"),
            ("CHOOSE (0)", "PICK_two"),
            ("TC ()", "void TEST_0 ( ) ; int reg1 ; void TEST_0 ( )"),
            ("__LINE__", "1"),
        ] {
            eng.counter_from = Some(0);
            eng.site = Some((0, 1));
            let v = eng.variants(&tokenize(src.as_bytes(), Origin::Site), &Choice::default(), true, |_| true);
            assert_eq!(texts(&v[0].0), want, "{src}");
        }
    }

    #[test]
    fn where_every_compilation_was_read_every_definition_is_read_once() {
        // OUTER uses four names whose second definitions bring nothing, then
        // STAGE, whose second makes something.
        let defs: &[(&str, &str)] = &[
            ("OUTER", "(n) A (n) B (n) C (n) D (n) STAGE (n)"),
            ("A", "(n) n"), ("A", "(n) n n"),
            ("B", "(n) n"), ("B", "(n) (n)"),
            ("C", "(n) n"), ("C", "(n) n ;"),
            ("D", "(n) n"), ("D", "(n) { n }"),
            ("STAGE", "(n) n"), ("STAGE", "(n) ~ made_ ## n"),
        ];
        let files: Vec<FileFacts> = defs
            .iter()
            .enumerate()
            .map(|(i, (name, text))| {
                let mut f = FileFacts::new(format!("s{i}.h"));
                let (name, text) = (f.push_name(name), f.push_text(text));
                f.macros.push(crate::model::MacroBody { name, text, function_like: true, guard: NONE, conditional: false, fallback: false, line: 0 });
                f
            })
            .collect();
        let parsed: Vec<Vec<Parsed>> = files.iter().enumerate().map(|(fi, f)| vec![Parsed { file: fi as u32, ..parse(f.name(f.macros[0].text as usize), true, "") }]).collect();
        let mut by_name: FxHashMap<&str, Vec<(u32, u32)>> = FxHashMap::default();
        for (fi, (name, _)) in defs.iter().enumerate() {
            by_name.entry(name).or_default().push((fi as u32, 0));
        }
        let mut table = Table::new(&files, by_name, parsed);
        let made = |table: &Table| {
            let mut eng = Engine::new(table, vec![true; defs.len()]);
            // (Novel: what has `made_` in it.)
            let v = eng.variants(&tokenize(b"OUTER (x)", Origin::Site), &Choice::default(), true, |t| t.iter().any(|t| t.text.starts_with("made_")));
            v.iter().any(|(t, _)| texts_of(t).contains("made_x"))
        };
        // Else three readings that bring nothing end it, before STAGE's.
        assert!(!made(&table));
        // Where every compilation was read, each name's are read, until two
        // of one name in a row bring nothing (see `BLIND_ALTERNATIVES`).
        table.thorough = true;
        assert!(made(&table));
    }

    #[test]
    fn alternatives_that_brought_nothing_at_uses_in_a_row_are_not_read_at_the_next() {
        // WARN has a definition per compiler, which makes nothing new.
        let defs: &[(&str, &str)] = &[("WARN", "(n) n"), ("WARN", "(n) _Pragma (\"x\") n")];
        let files: Vec<FileFacts> = defs
            .iter()
            .enumerate()
            .map(|(i, (name, text))| {
                let mut f = FileFacts::new(format!("w{i}.h"));
                let (name, text) = (f.push_name(name), f.push_text(text));
                f.macros.push(crate::model::MacroBody { name, text, function_like: true, guard: NONE, conditional: false, fallback: false, line: 0 });
                f
            })
            .collect();
        let parsed: Vec<Vec<Parsed>> = files.iter().enumerate().map(|(fi, f)| vec![Parsed { file: fi as u32, ..parse(f.name(f.macros[0].text as usize), true, "") }]).collect();
        let by_name: FxHashMap<&str, Vec<(u32, u32)>> = [("WARN", vec![(0, 0), (1, 0)])].into_iter().collect();
        let mut table = Table::new(&files, by_name, parsed);
        table.thorough = true;
        let mut eng = Engine::new(&table, vec![true; 2]);
        let runs: Vec<u64> = (0..BLIND_USES + 1)
            .map(|_| {
                let before = eng.runs;
                eng.variants(&tokenize(b"WARN (x)", Origin::Site), &Choice::default(), true, |_| false);
                eng.runs - before
            })
            .collect();
        // Each read on its own at each use, until they brought nothing at
        // that many: then only as the readings together come to them.
        assert!(runs[..BLIND_USES].iter().all(|&n| n == runs[0]), "{runs:?}");
        assert!(runs[BLIND_USES] < runs[0], "{runs:?}");
    }

    fn texts_of(t: &[T]) -> String {
        t.iter().map(|t| &*t.text).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn in_a_body_a_name_is_what_its_own_file_defined_before() {
        // `perf.h` and `events.h` each define INNER, then OUTER with it.
        let defs: [(&str, &[(&str, &str)]); 2] = [
            ("perf.h", &[("INNER", "(n) aaa_ ## n"), ("OUTER", "(n) INNER (n)")]),
            ("events.h", &[("INNER", "(n) do_ ## n"), ("OUTER", "(n) INNER (n) raw_ ## n")]),
        ];
        let files: Vec<FileFacts> = defs
            .iter()
            .map(|(path, macros)| {
                let mut f = FileFacts::new((*path).into());
                for (name, text) in macros.iter() {
                    let (name, text) = (f.push_name(name), f.push_text(text));
                    f.macros.push(crate::model::MacroBody { name, text, function_like: true, guard: NONE, conditional: false, fallback: false, line: 0 });
                }
                f
            })
            .collect();
        let parsed: Vec<Vec<Parsed>> = files
            .iter()
            .enumerate()
            .map(|(fi, f)| f.macros.iter().enumerate().map(|(i, m)| Parsed { file: fi as u32, at: i as u32, ..parse(f.name(m.text as usize), true, "") }).collect())
            .collect();
        let mut by_name: FxHashMap<&str, Vec<(u32, u32)>> = FxHashMap::default();
        for (fi, f) in files.iter().enumerate() {
            for (i, m) in f.macros.iter().enumerate() {
                by_name.entry(f.name(m.name as usize)).or_default().push((fi as u32, i as u32));
            }
        }
        let table = Table::new(&files, by_name, parsed);
        let mut eng = Engine::new(&table, vec![true, true]);
        let v = eng.variants(&tokenize(b"OUTER (sched)", Origin::Site), &Choice::default(), true, |_| true);
        let all: Vec<String> = v.iter().map(|(t, _)| texts(t)).collect();
        // Never `events.h`'s OUTER with `perf.h`'s INNER.
        assert!(all.contains(&"do_sched raw_sched".to_string()), "{all:?}");
        assert!(!all.contains(&"aaa_sched raw_sched".to_string()), "{all:?}");
    }
}
