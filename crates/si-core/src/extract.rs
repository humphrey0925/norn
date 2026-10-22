//! Single-file extraction: lexical mask, tree-sitter parse, one preorder walk. O(tokens).

use crate::mask::{self, Event, Tok, MacroSets};
use crate::model::{
    flag, kind, refkind, root, site, ArgPass, Bind, Def, FileFacts, Forward, InitSite, MacroBody, MemberHint, ParamCall, Record,
    Ref, Site, NONE,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::num::NonZeroU16;
use tree_sitter::{Language, Node, Parser, Point, Range};

struct Ids {
    identifier: u16,
    type_identifier: u16,
    function_definition: u16,
    declaration: u16,
    parameter_declaration: u16,
    type_definition: u16,
    struct_specifier: u16,
    union_specifier: u16,
    enum_specifier: u16,
    enumerator: u16,
    call_expression: u16,
    function_declarator: u16,
    storage_class_specifier: u16,
    field_identifier: u16,
    field_declaration: u16,
    field_expression: u16,
    field_designator: u16,
    initializer_pair: u16,
    initializer_list: u16,
    init_declarator: u16,
    assignment_expression: u16,
    parenthesized_expression: u16,
    pointer_expression: u16,
    cast_expression: u16,
    subscript_expression: u16,
    conditional_expression: u16,
    array_declarator: u16,
    f_field: u16,
    f_argument: u16,
    f_value: u16,
    f_left: u16,
    f_right: u16,
    f_type: u16,
    f_operator: u16,
    f_consequence: u16,
    f_alternative: u16,
    f_designator: NonZeroU16,
    f_parameters: u16,
    f_arguments: u16,
    f_declarator: NonZeroU16,
    f_name: u16,
    f_body: u16,
    f_function: u16,
    // C++ (0, matching no node, in C's grammar).
    class_specifier: u16,
    namespace_definition: u16,
    qualified_identifier: u16,
    template_type: u16,
    template_function: u16,
    template_method: u16,
    destructor_name: u16,
    operator_name: u16,
    this: u16,
    base_class_clause: u16,
    dependent_name: u16,
    virtual_specifier: u16,
    friend_declaration: u16,
    type_descriptor: u16,
    f_scope: u16,
}

impl Ids {
    fn new(lang: &Language) -> Self {
        let k = |s: &str| lang.id_for_node_kind(s, true);
        let f = |s: &str| lang.field_id_for_name(s).expect("field");
        Ids {
            identifier: k("identifier"),
            type_identifier: k("type_identifier"),
            function_definition: k("function_definition"),
            declaration: k("declaration"),
            parameter_declaration: k("parameter_declaration"),
            type_definition: k("type_definition"),
            struct_specifier: k("struct_specifier"),
            union_specifier: k("union_specifier"),
            enum_specifier: k("enum_specifier"),
            enumerator: k("enumerator"),
            call_expression: k("call_expression"),
            function_declarator: k("function_declarator"),
            storage_class_specifier: k("storage_class_specifier"),
            field_identifier: k("field_identifier"),
            field_declaration: k("field_declaration"),
            field_expression: k("field_expression"),
            field_designator: k("field_designator"),
            initializer_pair: k("initializer_pair"),
            initializer_list: k("initializer_list"),
            init_declarator: k("init_declarator"),
            assignment_expression: k("assignment_expression"),
            parenthesized_expression: k("parenthesized_expression"),
            pointer_expression: k("pointer_expression"),
            cast_expression: k("cast_expression"),
            subscript_expression: k("subscript_expression"),
            conditional_expression: k("conditional_expression"),
            array_declarator: k("array_declarator"),
            f_field: f("field").get(),
            f_argument: f("argument").get(),
            f_value: f("value").get(),
            f_left: f("left").get(),
            f_right: f("right").get(),
            f_type: f("type").get(),
            f_operator: f("operator").get(),
            f_consequence: f("consequence").get(),
            f_alternative: f("alternative").get(),
            f_designator: f("designator"),
            f_parameters: f("parameters").get(),
            f_arguments: f("arguments").get(),
            f_declarator: f("declarator"),
            f_name: f("name").get(),
            f_body: f("body").get(),
            f_function: f("function").get(),
            class_specifier: k("class_specifier"),
            namespace_definition: k("namespace_definition"),
            qualified_identifier: k("qualified_identifier"),
            template_type: k("template_type"),
            template_function: k("template_function"),
            template_method: k("template_method"),
            destructor_name: k("destructor_name"),
            operator_name: k("operator_name"),
            this: k("this"),
            base_class_clause: k("base_class_clause"),
            dependent_name: k("dependent_name"),
            virtual_specifier: k("virtual_specifier"),
            friend_declaration: k("friend_declaration"),
            type_descriptor: k("type_descriptor"),
            f_scope: lang.field_id_for_name("scope").map_or(0, |f| f.get()),
        }
    }
}

/// Is the file C++? By its name, and a header (or a file for including,
/// `.inl`, which may be C) by what it says: C has no `namespace`,
/// `template <`, `class X {`, access labels, `std::` or `extern "C++"`.
/// (Not `::`: a C file's `asm` has it.) What a C header has for C++ only,
/// under `#ifdef __cplusplus`, counts only where it defines a class or a
/// template (minisketch.h's wrapper class), which the C parser would read
/// as nothing; not `extern "C" {` nor a namespace.
pub fn is_cpp(path: &str, src: &[u8]) -> bool {
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "cc" | "cpp" | "cxx" | "c++" | "C" | "hh" | "hpp" | "hxx" | "h++" | "ixx" | "cppm" => true,
        "h" | "H" | "inl" | "ipp" | "tcc" => looks_cpp(src),
        _ => false,
    }
}

fn looks_cpp(src: &[u8]) -> bool {
    let mut words: Vec<&[u8]> = Vec::with_capacity(4);
    let mut i = 0;
    let n = src.len();
    let mut line_start = true;
    // Inside `#ifdef __cplusplus`: how many `#if`s deep.
    let mut for_cpp = 0u32;
    // A class or a template there.
    let mut defined_for_cpp = false;
    let skip_comment = |i: &mut usize| {
        *i += 2;
        while *i + 1 < n && !(src[*i] == b'*' && src[*i + 1] == b'/') {
            *i += 1;
        }
        *i += 2;
    };
    while i < n {
        let b = src[i];
        if b == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if b == b' ' || b == b'\t' || b == b'\r' {
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < n && src[i + 1] == b'/' {
            while i < n && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < n && src[i + 1] == b'*' {
            skip_comment(&mut i);
            continue;
        }
        if b == b'#' && line_start {
            // A directive, its continuation lines and comments (which may
            // go on past its end) included.
            i += 1;
            let mut text = Vec::new();
            while i < n && src[i] != b'\n' {
                if src[i] == b'\\' && src.get(i + 1) == Some(&b'\n') {
                    i += 2;
                } else if src[i] == b'/' && src.get(i + 1) == Some(&b'*') {
                    skip_comment(&mut i);
                    text.push(b' ');
                } else if src[i] == b'/' && src.get(i + 1) == Some(&b'/') {
                    while i < n && src[i] != b'\n' {
                        i += 1;
                    }
                } else {
                    text.push(src[i]);
                    i += 1;
                }
            }
            let text = text.trim_ascii();
            let word_end = text.iter().position(|c| !c.is_ascii_alphabetic()).unwrap_or(text.len());
            let (word, rest) = text.split_at(word_end);
            let mentions = |s: &[u8]| s.windows(11).any(|w| w == b"__cplusplus");
            if for_cpp > 0 {
                match word {
                    b"if" | b"ifdef" | b"ifndef" => for_cpp += 1,
                    b"endif" => for_cpp -= 1,
                    b"else" | b"elif" if for_cpp == 1 => for_cpp = 0,
                    _ => {}
                }
            } else if (word == b"ifdef" || word == b"if" || word == b"elif") && mentions(rest) && !rest.contains(&b'!') {
                for_cpp = 1;
            }
            continue;
        }
        let first_on_line = line_start;
        line_start = false;
        if b == b'"' || b == b'\'' {
            let q = b;
            let start = i;
            i += 1;
            while i < n && src[i] != q && src[i] != b'\n' {
                if src[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            if q == b'"' && words.last() == Some(&&b"extern"[..]) && src.get(start..i) == Some(b"\"C++\"") && for_cpp == 0 {
                return true;
            }
            words.clear();
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < n && (src[i].is_ascii_alphanumeric() || src[i] == b'_') {
                i += 1;
            }
            let w = &src[start..i];
            // An access label stands first on its line (`unsigned public: 1;`
            // is a C bit field).
            if first_on_line && (w == b"public" || w == b"private" || w == b"protected") && for_cpp == 0 {
                let colon = i + src[i..].iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(0);
                if src.get(colon) == Some(&b':') && src.get(colon + 1) != Some(&b':') {
                    return true;
                }
            }
            if w == b"std" && src.get(i..i + 2) == Some(b"::") && for_cpp == 0 {
                return true;
            }
            if words.len() == 4 {
                words.remove(0);
            }
            words.push(w);
            continue;
        }
        // A punctuator ends a run of words: what came before it decides.
        let last = words.last().copied();
        let before = words.len().checked_sub(2).map(|k| words[k]);
        let cpp = match b {
            b'<' => last == Some(b"template"),
            b'{' => before == Some(b"namespace") || before == Some(b"class") || last == Some(b"namespace"),
            b':' => before == Some(b"class") && src.get(i + 1) != Some(&b':'),
            _ => false,
        };
        if cpp && for_cpp == 0 {
            return true;
        }
        // (Not a namespace: nedmalloc.h gives C++ users its functions in one.)
        defined_for_cpp |= cpp && !(b == b'{' && (before == Some(b"namespace") || last == Some(b"namespace")));
        words.clear();
        i += 1;
    }
    defined_for_cpp
}

/// C++ templates whose `->` (or `*`) reaches the object of the type they
/// are given.
const SMART_POINTERS: &[&[u8]] = &[
    b"unique_ptr", b"shared_ptr", b"weak_ptr", b"scoped_ptr", b"auto_ptr", b"intrusive_ptr", b"scoped_refptr",
    b"ComPtr", b"optional", b"reference_wrapper", b"not_null",
];

/// C++ operators a class can define, as written between operands.
const BINARY_OPERATORS: &[&[u8]] = &[
    b"+", b"-", b"*", b"/", b"%", b"^", b"&", b"|", b"<", b">", b"<=", b">=", b"==", b"!=", b"<=>", b"<<", b">>", b"&&", b"||",
];
const ASSIGNMENT_OPERATORS: &[&[u8]] = &[b"=", b"+=", b"-=", b"*=", b"/=", b"%=", b"^=", b"&=", b"|=", b"<<=", b">>="];

/// C++ containers whose elements are their first template argument, and
/// those (maps) whose values are their second.
const CONTAINERS: &[&[u8]] = &[
    b"vector", b"array", b"deque", b"list", b"forward_list", b"set", b"multiset", b"unordered_set", b"unordered_multiset",
    b"span", b"valarray", b"queue", b"priority_queue", b"stack", b"initializer_list", b"prevector", b"SmallVector",
];
const MAPS: &[&[u8]] = &[b"map", b"multimap", b"unordered_map", b"unordered_multimap", b"flat_map"];

/// Longest initializer kept as text for expansion.
const MAX_INIT_TEXT: usize = 256 << 10;
/// An array initializer longer than this is expanded a row at a time.
const ROWS_APART: usize = 8 << 10;

/// Reusable per-thread extractor (a tree-sitter parser is not `Sync`).
pub struct Extractor {
    parser: Parser,
    ids: Ids,
    /// C++'s, made when the first C++ file comes.
    cpp: Option<(Parser, Ids)>,
}

/// A class whose body is being read.
struct OpenClass {
    end: usize,
    /// `Outer::Inner`.
    chain: String,
}

#[derive(Clone, Copy)]
struct Scope {
    def: u32,
    end: usize,
    is_fn: bool,
}

struct Walk<'a> {
    /// Masked text: what the parser saw. Same offsets as the original.
    src: &'a [u8],
    orig: &'a [u8],
    /// What the mask hid, still to be merged in at its source position.
    events: std::slice::Iter<'a, Event>,
    next_event: Option<&'a Event>,
    blocks: &'a [(u32, u32, u32)],
    ids: &'a Ids,
    /// The macro sets, for the parameters macros leave unused.
    sets: &'a MacroSets,
    /// The arguments of macros that do not use them (byte ranges): what is
    /// called there is not.
    quiet: Vec<(usize, usize)>,
    out: FileFacts,
    names: FxHashMap<&'a [u8], u32>,
    /// Start bytes of identifier nodes already consumed as a def name or call target.
    skip: FxHashSet<usize>,
    locals: FxHashSet<&'a [u8]>,
    /// C++: the locals and parameters of the function being read that are
    /// `std::string`s, which a parameter of another class may be made of.
    strings: FxHashSet<&'a [u8]>,
    /// The declaration in force for each local name of the function being
    /// read, as an index into the facts' `locals`.
    local_ids: FxHashMap<&'a [u8], u32>,
    scope: Option<Scope>,
    /// Name position of each expanding-macro use, and the definition it
    /// turned out to name.
    site_defs: FxHashMap<usize, u32>,
    /// The same positions, sorted.
    site_starts: Vec<usize>,
    /// In the function being read: what each local variable is assigned
    /// that a call could go through (a member, or a name that is not
    /// local), and the calls made through locals.
    local_sources: FxHashMap<&'a [u8], Vec<(bool, u32, u32)>>,
    local_calls: Vec<(&'a [u8], u32)>,
    /// The parameters of the function being read, by position.
    params: FxHashMap<&'a [u8], u32>,
    /// The struct, union or typedef type of each local of that function.
    local_types: FxHashMap<&'a [u8], u32>,
    /// C++: of an `auto` local, where a member access on it starts (as a
    /// member hint's root and path): what it was made from.
    local_roots: FxHashMap<&'a [u8], (u8, u32, Vec<u32>)>,
    /// C++: of a local container, its elements' type; of a local map, its
    /// keys'.
    local_elems: FxHashMap<&'a [u8], u32>,
    local_keys: FxHashMap<&'a [u8], u32>,
    /// Hints worked out at a member access, for the reference its member
    /// name is about to become (by the name's position).
    pending_hints: FxHashMap<usize, u32>,
    /// Struct bodies being read: (end, index in `out.records`).
    open_records: Vec<(usize, usize)>,
    /// C++.
    cpp: bool,
    /// Class bodies being read, outermost first.
    classes: Vec<OpenClass>,
    /// The class of the method being read (`this`), or `NONE`.
    method_class: u32,
    /// C++: the type the function being read returns by value, or `NONE`.
    returns_object: u32,
    /// The end of the `friend` declaration being read: what it declares
    /// is no member.
    friend_end: usize,
    /// Names made rather than read (`A::B`, `operator==`), once each.
    made_names: FxHashMap<String, u32>,
    /// C++: the file's namespaces, by where they start: (start, end, name;
    /// "" for an anonymous one). See `namespace_spans`.
    namespaces: Vec<(usize, usize, String)>,
    /// C++: the Boost.Test suites of the file: (start, end, fixture).
    suites: Vec<(usize, usize, Option<String>)>,
    /// C++: where the macros that make a definition are named (`TEST_F`
    /// of `TEST_F (A, b) { ... }`).
    defining_macros: FxHashSet<usize>,
}

impl Default for Extractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor {
    pub fn new() -> Self {
        let lang: Language = tree_sitter_c::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&lang).expect("tree-sitter-c ABI");
        Extractor { parser, ids: Ids::new(&lang), cpp: None }
    }

    /// The facts depend on the file's content and on `transparent`.
    pub fn extract(&mut self, path: &str, src: &[u8], transparent: &MacroSets) -> FileFacts {
        self.extract_reusing(path, src, transparent, &|_| None).0
    }

    /// The same, but the facts already made of this very text under these
    /// very conditions (`reuse` by `FileFacts::lex_key`, from another
    /// root's index) are taken rather than made again, if every macro they
    /// asked about discards what it did then. Says whether they were.
    pub fn extract_reusing(&mut self, path: &str, src: &[u8], transparent: &MacroSets, reuse: &dyn Fn(u64) -> Option<FileFacts>) -> (FileFacts, bool) {
        let t = std::time::Instant::now();
        let mut lexed = mask::lex_file(src, transparent, Some(path));
        let cpp = is_cpp(path, src);
        if cpp {
            mask::template_macros(&mut lexed.masked);
        }
        let key = lexed.key(src, cpp);
        if let Some(saved) = reuse(key) {
            let same = saved.lex_key == key
                && saved.discards_asked.iter().all(|&(n, d, p)| {
                    let name = saved.name(n as usize).as_bytes();
                    transparent.discards(name) == d && transparent.pasted(name) == p
                });
            if same {
                return (saved.rebased(path.to_string()), true);
            }
        }
        let mask_ns = t.elapsed().as_nanos() as u64;
        let mut out = FileFacts::new(path.to_string());
        out.lex_key = key;
        out.hash = xxhash_rust::xxh3::xxh3_64(src);
        out.bytes = src.len() as u32;
        let file_name = out.push_name(path);
        out.defs.push(Def { name: file_name, line: 1, end_line: 0, kind: kind::FILE, flags: 0 });

        let mut names = mask::defined_names(src);
        names.sort_unstable();
        names.dedup();
        for name in names {
            let id = out.push_name(&String::from_utf8_lossy(name));
            out.defined_names.push(id);
        }
        for &(a, e, conditional) in &lexed.includes {
            let id = out.push_name(&String::from_utf8_lossy(&src[a as usize..e as usize]));
            out.includes.push(id);
            out.include_conditional.push(conditional);
        }
        out.inactive = inactive_lines(src, &lexed.skipped);
        out.undecided = undecided_lines(src, &lexed.undecided);
        out.mask_ns = mask_ns;
        if cpp {
            out.cxx = Some(Box::default());
        }
        let (parser, ids) = if cpp {
            let c = self.cpp.get_or_insert_with(|| {
                let lang: Language = tree_sitter_cpp::LANGUAGE.into();
                let mut parser = Parser::new();
                parser.set_language(&lang).expect("tree-sitter-cpp ABI");
                (parser, Ids::new(&lang))
            });
            (&mut c.0, &c.1)
        } else {
            (&mut self.parser, &self.ids)
        };
        let t = std::time::Instant::now();
        out.defs[0].end_line = src.iter().filter(|&&b| b == b'\n').count() as u32 + 1;

        let mut events = lexed.events.iter();
        let mut w = Walk {
            src: &lexed.masked,
            orig: src,
            next_event: events.next(),
            events,
            blocks: &lexed.blocks,
            ids,
            sets: transparent,
            quiet: Vec::new(),
            out,
            names: FxHashMap::default(),
            skip: FxHashSet::default(),
            locals: FxHashSet::default(),
            strings: FxHashSet::default(),
            local_ids: FxHashMap::default(),
            scope: None,
            site_defs: lexed
                .events
                .iter()
                .filter_map(|e| match e {
                    Event::Site(s) => Some((site_def_at(s), NONE)),
                    _ => None,
                })
                .collect(),
            site_starts: Vec::new(),
            local_sources: FxHashMap::default(),
            local_calls: Vec::new(),
            params: FxHashMap::default(),
            local_types: FxHashMap::default(),
            local_roots: FxHashMap::default(),
            local_elems: FxHashMap::default(),
            local_keys: FxHashMap::default(),
            pending_hints: FxHashMap::default(),
            open_records: Vec::new(),
            cpp,
            classes: Vec::new(),
            method_class: NONE,
            returns_object: NONE,
            friend_end: 0,
            made_names: FxHashMap::default(),
            namespaces: if cpp { namespace_spans(&lexed.masked, &lexed.blocks) } else { Vec::new() },
            suites: if cpp { test_suites(&lexed.masked) } else { Vec::new() },
            defining_macros: lexed
                .events
                .iter()
                .filter_map(|e| match e {
                    Event::Site(s) if s.def_at != 0 => Some(s.name.start as usize),
                    _ => None,
                })
                .collect(),
        };
        w.site_starts = w.site_defs.keys().copied().collect();
        w.site_starts.sort_unstable();
        // A piece at a time, each tree dropped once walked: what is held is
        // the largest piece's tree rather than the file's.
        let mut parse_ns = 0;
        let pieces = pieces(&lexed.masked, chunk_min());
        let cut = pieces.len() > 1;
        for range in pieces {
            let tp = std::time::Instant::now();
            if cut {
                parser.set_included_ranges(&[range]).expect("ranges in order");
            }
            let tree = parser.parse(&lexed.masked, None);
            parse_ns += tp.elapsed().as_nanos() as u64;
            let Some(tree) = tree else {
                w.out.has_error = true;
                continue;
            };
            let root = tree.root_node();
            w.out.has_error |= root.has_error();
            let mut cursor = root.walk();
            'outer: loop {
                if w.visit(cursor.node()) && cursor.goto_first_child() {
                    continue;
                }
                loop {
                    if cursor.goto_next_sibling() {
                        break;
                    }
                    if !cursor.goto_parent() {
                        break 'outer;
                    }
                }
            }
        }
        if cut {
            parser.set_included_ranges(&[]).expect("whole text");
        }
        w.out.parse_ns = parse_ns;
        w.flush_events(usize::MAX);
        w.flush_local_calls();
        let mut names: Vec<String> = w.namespaces.iter().flat_map(|n| n.2.split("::").map(str::to_string)).filter(|n| !n.is_empty()).collect();
        let aliases = if w.cpp { namespace_aliases(&lexed.masked) } else { Vec::new() };
        names.extend(aliases.iter().map(|a| a.0.clone()));
        names.sort_unstable();
        names.dedup();
        for name in names {
            let id = w.intern_str(&name);
            w.out.cxx_mut().namespaces.push(id);
        }
        for (alias, target) in aliases {
            let (alias, target) = (w.intern_str(&alias), w.intern_str(&target));
            w.out.cxx_mut().namespace_aliases.push((alias, target));
        }
        w.out.walk_ns = (t.elapsed().as_nanos() as u64).saturating_sub(w.out.parse_ns);
        let mut out = w.out;
        out.discards_asked.sort_unstable();
        out.discards_asked.dedup();
        // A function defined where it may not be compiled (the stub in the
        // `#else` of a header) is only one reading of its name.
        for d in out.defs.iter_mut().filter(|d| d.kind == kind::FUNCTION) {
            if out.undecided.iter().any(|&(a, b, _, _)| a <= d.line && d.line <= b) {
                d.flags |= flag::CONDITIONAL;
            }
        }
        out.shrink();
        (out, false)
    }
}

impl<'a, 't> Walk<'a> {
    /// A parameter or local variable is declared at `name`.
    fn note_local(&mut self, name: Node<'t>, param: bool) {
        let text = self.text(name);
        self.locals.insert(text);
        let Some(scope) = self.scope.filter(|s| s.is_fn) else { return };
        let type_name = self.local_types.get(text).copied().unwrap_or(NONE);
        let id = self.intern(text);
        self.local_ids.insert(text, self.out.locals.len() as u32);
        self.out.locals.push(crate::model::Local { name: id, line: name.start_position().row as u32 + 1, scope: scope.def, param, type_name });
    }

    /// The local `name` is used here.
    fn use_local(&mut self, name: Node<'t>) {
        if let Some(&id) = self.local_ids.get(self.text(name)) {
            self.out.local_uses.push((id, name.start_position().row as u32 + 1));
        }
    }

    fn text(&self, n: Node) -> &'a [u8] {
        &self.src[n.byte_range()]
    }

    fn intern(&mut self, s: &'a [u8]) -> u32 {
        if let Some(&id) = self.names.get(s) {
            return id;
        }
        let id = self.out.push_name(&String::from_utf8_lossy(s));
        self.names.insert(s, id);
        id
    }

    /// A name made here rather than read (`operator==` from `operator ==`).
    fn intern_str(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.made_names.get(s) {
            return id;
        }
        let id = self.out.push_name(s);
        self.made_names.insert(s.to_string(), id);
        id
    }

    /// The name a definition is known by: its text, but a C++ operator's or
    /// destructor's without spaces.
    fn def_name(&mut self, name: Node<'t>) -> u32 {
        let k = name.kind_id();
        if self.cpp && (k == self.ids.operator_name || k == self.ids.destructor_name) {
            // One space between words (`operator bool`), none elsewhere
            // (`operator==`, `~Foo`).
            let text = String::from_utf8_lossy(self.text(name));
            let mut s = String::with_capacity(text.len());
            for word in text.split_whitespace() {
                let joins = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                if joins(s.chars().last()) && joins(word.chars().next()) {
                    s.push(' ');
                }
                s.push_str(word);
            }
            return self.intern_str(&s);
        }
        self.intern(self.text(name))
    }

    /// C++: the parts of the scope a qualified name is written with (`A`
    /// and `B` of `A::B<T>::f`, without template arguments), and the name.
    /// `::f` has one empty part.
    fn qualified_parts(&self, mut n: Node<'t>) -> (Vec<&'a [u8]>, Node<'t>) {
        let ids = self.ids;
        let mut parts = Vec::new();
        for _ in 0..16 {
            let k = n.kind_id();
            if k == ids.qualified_identifier {
                let part = match n.child_by_field_id(ids.f_scope) {
                    Some(s) if s.kind_id() == ids.template_type => s.child_by_field_id(ids.f_name).map(|t| self.text(t)),
                    Some(s) => Some(self.text(s)),
                    None => Some(&b""[..]),
                };
                // A name, not whatever the parser made of text it could not
                // read (`decltype (...)`, or a stretch of code past an error).
                match part {
                    Some(p) if p.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80) => parts.push(p),
                    _ => parts.push(b"?"),
                }
                match n.child_by_field_id(ids.f_name) {
                    Some(name) => n = name,
                    None => break,
                }
            } else if k == ids.template_function || k == ids.template_method {
                match n.child_by_field_id(ids.f_name) {
                    Some(name) => n = name,
                    None => break,
                }
            } else if k == ids.dependent_name {
                match n.named_child(0) {
                    Some(inner) => n = inner,
                    None => break,
                }
            } else {
                break;
            }
        }
        (parts, n)
    }

    /// C++: the scope written in a declarator's name (`A::B` of
    /// `A::B::f (...)`), as its parts.
    fn declared_scope(&self, mut n: Node<'t>) -> Vec<&'a [u8]> {
        let ids = self.ids;
        for _ in 0..32 {
            let k = n.kind_id();
            if k == ids.qualified_identifier {
                let (mut parts, _) = self.qualified_parts(n);
                parts.retain(|p| !p.is_empty());
                return parts;
            }
            if k == ids.identifier || k == ids.field_identifier || k == ids.type_identifier || k == ids.destructor_name || k == ids.operator_name {
                break;
            }
            match n.child_by_field_id(ids.f_declarator.get()).or_else(|| n.named_child(0)) {
                Some(inner) => n = inner,
                None => break,
            }
        }
        Vec::new()
    }

    /// C++: the class whose body a member declared at `pos` is in: the
    /// innermost being read, outside any function or `friend`.
    fn class_at(&mut self, pos: usize) -> Option<usize> {
        while self.classes.last().is_some_and(|c| pos >= c.end) {
            self.classes.pop();
        }
        (!self.in_fn() && pos >= self.friend_end && !self.classes.is_empty()).then(|| self.classes.len() - 1)
    }

    /// C++: note that `def` is a member of the class `class` (being read)
    /// and of the classes `scope` names. Returns the class's own name.
    fn note_owner(&mut self, def: u32, scope: &[&[u8]], class: Option<usize>, pos: usize) -> u32 {
        // The namespaces it is in, then its classes.
        let mut chain = self
            .namespaces
            .iter()
            .filter(|n| n.0 <= pos && pos < n.1 && !n.2.is_empty())
            .map(|n| n.2.as_str())
            .collect::<Vec<_>>()
            .join("::");
        let member = class.is_some() || !scope.is_empty();
        let class_chain = class.map(|c| self.classes[c].chain.clone());
        for part in class_chain.iter().map(|c| c.as_bytes()).chain(scope.iter().copied().filter(|p| *p != b"?")) {
            if !chain.is_empty() {
                chain.push_str("::");
            }
            chain.push_str(&String::from_utf8_lossy(part));
        }
        if chain.is_empty() {
            return NONE;
        }
        let own = chain.rsplit("::").next().unwrap_or("").to_string();
        let (chain, own) = (self.intern_str(&chain), self.intern_str(&own));
        self.out.cxx_mut().owners.push((def, chain, own));
        // (A function only in a namespace has no class for `this`.)
        if member { own } else { NONE }
    }

    /// C++: a test written with Boost.Test's or GoogleTest's macros where the
    /// corpus does not define them (their headers are the system's): the
    /// class the macro makes, the method the body is, and the class that
    /// derives from (the fixture). `TEST_F (Suite, Name) { ... }` is
    /// `Suite_Name_Test::TestBody`, `BOOST_AUTO_TEST_CASE (name) { ... }`
    /// is `name::test_method`, of the fixture of the suite it is in.
    fn test_case(&self, name: Node<'t>, def: Node<'t>) -> Option<(String, &'static str, Option<String>)> {
        if self.site_defs.contains_key(&name.start_byte()) {
            // (The macro is the corpus's own, and is expanded.)
            return None;
        }
        let ids = self.ids;
        let macro_name = self.text(name);
        if !matches!(macro_name, b"BOOST_AUTO_TEST_CASE" | b"BOOST_FIXTURE_TEST_CASE" | b"TEST" | b"TEST_F" | b"TEST_P") {
            return None;
        }
        let params = def.child_by_field_id(ids.f_declarator.get())?.child_by_field_id(ids.f_parameters)?;
        let mut c = params.walk();
        let args: Vec<String> = params
            .named_children(&mut c)
            .map(|p| String::from_utf8_lossy(self.text(p)).split_whitespace().collect::<String>())
            .collect();
        // A name, of a class or a test (of a qualified one, the last part).
        let arg = |i: usize| {
            let a = args.get(i)?.rsplit("::").next()?.to_string();
            (!a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then_some(a)
        };
        match macro_name {
            b"BOOST_AUTO_TEST_CASE" => Some((arg(0)?, "test_method", self.fixture_at(def.start_byte()))),
            b"BOOST_FIXTURE_TEST_CASE" => Some((arg(0)?, "test_method", arg(1))),
            b"TEST" => Some((format!("{}_{}_Test", arg(0)?, arg(1)?), "TestBody", None)),
            _ => Some((format!("{}_{}_Test", arg(0)?, arg(1)?), "TestBody", arg(0))),
        }
    }

    /// The fixture of the Boost.Test suite `pos` is in, if it has one.
    fn fixture_at(&self, pos: usize) -> Option<String> {
        // (A suite with none has its enclosing suite's.)
        self.suites.iter().rev().filter(|s| s.0 <= pos && pos < s.1).find_map(|s| s.2.clone())
    }

    /// C++: is `pos` in an anonymous namespace? What is defined there is
    /// the translation unit's own, as if `static`.
    fn anonymous(&self, pos: usize) -> bool {
        self.namespaces.iter().any(|n| n.2.is_empty() && n.0 <= pos && pos < n.1)
    }


    /// C++: is the function declared `virtual`, `override` or `final`?
    fn is_virtual(&self, decl: Node<'t>) -> bool {
        let ids = self.ids;
        let mut c = decl.walk();
        if decl.children(&mut c).any(|ch| !ch.is_named() && self.text(ch) == b"virtual") {
            return true;
        }
        let mut n = decl.child_by_field_id(ids.f_declarator.get());
        for _ in 0..8 {
            let Some(d) = n else { break };
            if d.kind_id() == ids.function_declarator {
                let mut c = d.walk();
                return d.children(&mut c).any(|ch| ch.kind_id() == ids.virtual_specifier);
            }
            n = d.child_by_field_id(ids.f_declarator.get());
        }
        false
    }

    /// C++: a member just defined takes its place in the class being read,
    /// for what the class's layout says (`x.f ()` with `x` of the class).
    fn place_in_record(&mut self, def: u32, pos: usize) {
        while self.open_records.last().is_some_and(|r| pos >= r.0) {
            self.open_records.pop();
        }
        if let Some(&(_, r)) = self.open_records.last() {
            let id = self.out.defs[def as usize].name;
            let record = &mut self.out.records[r];
            if let Some(at) = (0..record.fields.len()).find(|&i| record.fields[i] == id && record.defs[i] == NONE) {
                record.defs[at] = def;
            }
        }
    }

    fn push_def(&mut self, name: Node<'t>, span: Node, kind: u8, flags: u8) -> u32 {
        self.skip.insert(name.start_byte());
        if let Some(slot) = self.site_defs.get_mut(&name.start_byte()) {
            *slot = self.out.defs.len() as u32;
        }
        let at = name;
        let name = self.def_name(name);
        self.out.defs.push(Def {
            name,
            line: span.start_position().row as u32 + 1,
            end_line: span.end_position().row as u32 + 1,
            kind,
            flags,
        });
        let def = (self.out.defs.len() - 1) as u32;
        if self.cpp && kind == kind::FUNCTION {
            self.note_arity(def, at);
        }
        def
    }

    /// C++: how many arguments the function whose name is at `name` takes,
    /// from its parameter list (defaults and `...` counted as such).
    fn note_arity(&mut self, def: u32, name: Node<'t>) {
        let ids = self.ids;
        let mut n = name;
        for _ in 0..4 {
            match n.parent() {
                Some(p) if p.kind_id() == ids.function_declarator => {
                    let Some(list) = p.child_by_field_id(ids.f_parameters) else { return };
                    let (mut least, mut most) = (0u8, 0u8);
                    let mut c = list.walk();
                    let params: Vec<Node<'t>> = list.named_children(&mut c).filter(|q| !q.is_extra()).collect();
                    for q in params {
                        let at = most;
                        match q.kind() {
                            "optional_parameter_declaration" => most = most.saturating_add(1),
                            "variadic_parameter_declaration" | "variadic_parameter" => most = u8::MAX,
                            // `(void)`.
                            _ if q.named_child_count() == 1 && self.text(q) == b"void" => {}
                            _ => {
                                least = least.saturating_add(1);
                                most = most.saturating_add(1);
                            }
                        }
                        // Of a parameter that is an object or a reference to
                        // one, its class (`const Status& s`: `Status`); one
                        // that takes a string or a number as it is, `""` or `0`.
                        let pointer = q.child_by_field_id(ids.f_declarator.get()).is_some_and(|d| matches!(d.kind(), "pointer_declarator" | "abstract_pointer_declarator"));
                        let builtin = q.child_by_field_id(ids.f_type).filter(|t| matches!(t.kind(), "primitive_type" | "sized_type_specifier"));
                        if most != at && most != u8::MAX {
                            let t = match builtin {
                                Some(b) if pointer && self.text(b).ends_with(b"char") => self.intern_str("\"\""),
                                Some(_) if !pointer => self.intern_str("0"),
                                _ if self.is_string(q.child_by_field_id(ids.f_type), q.child_by_field_id(ids.f_declarator.get())) => self.intern_str("std::string"),
                                _ if !pointer => self.type_name_of(q),
                                _ => NONE,
                            };
                            if t != NONE {
                                self.out.cxx_mut().params.push((def, at as u32, t));
                            }
                        }
                    }
                    if list.child_count() > 0 && (0..list.child_count()).any(|i| list.child(i).is_some_and(|x| x.kind() == "...")) {
                        most = u8::MAX;
                    }
                    self.out.cxx_mut().arity.push((def, least, most));
                    if let Some(d) = p.parent() {
                        let mut c = d.walk();
                        if d.children(&mut c).any(|x| x.kind() == "explicit_function_specifier") {
                            self.out.cxx_mut().explicit.push(def);
                        }
                    }
                    return;
                }
                Some(p) => n = p,
                None => return,
            }
        }
    }

    /// C++: is what a declaration of type `ty` declares with `declarator` a
    /// `std::string` (or a reference to one), not a pointer?
    fn is_string(&self, ty: Option<Node<'t>>, declarator: Option<Node<'t>>) -> bool {
        ty.is_some_and(|t| matches!(self.text(t), b"std::string" | b"::std::string" | b"string"))
            && declarator.is_none_or(|d| {
                let d = if d.kind_id() == self.ids.init_declarator { d.child_by_field_id(self.ids.f_declarator.get()) } else { Some(d) };
                d.is_some_and(|d| matches!(d.kind(), "identifier" | "reference_declarator" | "abstract_reference_declarator"))
            })
    }

    /// C++: how many arguments the reference just pushed was given, from the
    /// list in parentheses at `list`.
    fn note_args(&mut self, list: Option<Node<'t>>) {
        if let Some(n) = self.count_args(list) {
            let r = self.out.refs.len() as u32 - 1;
            self.out.cxx_mut().args.push((r, n));
        }
        self.note_arg_types(list);
    }

    /// C++: of the arguments of the reference just pushed, what is known of
    /// their types: a string or a number written (`""`, `0`), or a local
    /// object's class. An argument of another type than its parameter's
    /// class is made into one by a constructor of that class.
    fn note_arg_types(&mut self, list: Option<Node<'t>>) {
        let Some(list) = list.filter(|l| l.kind() == "argument_list") else { return };
        let r = self.out.refs.len() as u32 - 1;
        let mut c = list.walk();
        let args: Vec<Node<'t>> = list.named_children(&mut c).filter(|a| !a.is_extra()).collect();
        for (i, a) in args.into_iter().enumerate() {
            let a = self.strip(a);
            let t = match a.kind() {
                "string_literal" | "concatenated_string" | "raw_string_literal" => self.intern_str("\"\""),
                "number_literal" | "char_literal" => self.intern_str("0"),
                "identifier" if self.in_fn() && self.strings.contains(self.text(a)) => self.intern_str("std::string"),
                "identifier" if self.in_fn() => match self.local_types.get(self.text(a)) {
                    Some(&t) => t,
                    None => continue,
                },
                _ => continue,
            };
            self.out.cxx_mut().arg_types.push((r, i as u32, t));
        }
    }

    /// How many arguments a list in parentheses holds (not one in braces,
    /// which may be one `std::initializer_list`).
    fn count_args(&self, list: Option<Node<'t>>) -> Option<u8> {
        let list = list.filter(|l| matches!(l.kind(), "argument_list" | "parameter_list"))?;
        let mut c = list.walk();
        let n = list.named_children(&mut c).filter(|a| !a.is_extra()).count();
        Some(n.min(u8::MAX as usize - 1) as u8)
    }

    fn leave_scope(&mut self, pos: usize) {
        if self.scope.is_some_and(|s| pos >= s.end) {
            self.flush_local_calls();
            self.scope = None;
            self.locals.clear();
            self.strings.clear();
        }
    }

    /// A call through a local variable goes wherever the function gets the
    /// variable's values from, anywhere in the function.
    fn flush_local_calls(&mut self) {
        let scope = self.scope.map_or(0, |s| s.def);
        for (local, line) in std::mem::take(&mut self.local_calls) {
            if let Some(&index) = self.params.get(local) {
                self.out.param_calls.push(ParamCall { scope, index, line });
            }
            for &(member, name, hint) in self.local_sources.get(local).into_iter().flatten() {
                let kind = if member { refkind::ICALL } else { refkind::PCALL };
                self.out.refs.push(Ref { name, line, scope, kind });
                if hint != NONE {
                    self.out.ref_hints.push((self.out.refs.len() as u32 - 1, hint));
                }
            }
        }
        self.local_sources.clear();
        self.params.clear();
        self.local_types.clear();
        self.local_roots.clear();
        self.local_elems.clear();
        self.local_keys.clear();
    }

    /// The name of a declaration's type when it is a struct, a union or a
    /// typedef: what a member access on the declared thing is looked up in.
    fn type_name_of(&mut self, decl: Node<'t>) -> u32 {
        match decl.child_by_field_id(self.ids.f_type) {
            Some(t) => self.type_name(t),
            None => NONE,
        }
    }

    fn type_name(&mut self, t: Node<'t>) -> u32 {
        let ids = self.ids;
        let k = t.kind_id();
        if k == ids.type_identifier {
            self.intern(self.text(t))
        } else if self.cpp && k == ids.qualified_identifier {
            // `ns::Type`: the type. (Not `std::map<K, V>::iterator`, nor
            // `std::string`: a name in a template's or the library's scope is
            // no class of the corpus that happens to have it.)
            let name = self.qualified_parts(t).1;
            let foreign = || {
                let text = self.text(t);
                text.starts_with(b"std::") || text.starts_with(b"::std::") || text.contains(&b'<')
            };
            if name.id() == t.id() || (name.kind_id() == ids.type_identifier && foreign()) { NONE } else { self.type_name(name) }
        } else if self.cpp && k == ids.template_type {
            // What a smart pointer points to is what `->` reaches; any
            // other template is taken for the class it names.
            let Some(name) = t.child_by_field_id(ids.f_name) else { return NONE };
            let text = self.text(name);
            if SMART_POINTERS.contains(&text) {
                let arg = t
                    .child_by_field_id(ids.f_arguments)
                    .and_then(|a| a.named_child(0))
                    .filter(|a| a.kind_id() == ids.type_descriptor)
                    .and_then(|d| d.child_by_field_id(ids.f_type));
                return arg.map_or(NONE, |a| self.type_name(a));
            }
            self.type_name(name)
        } else if k == ids.struct_specifier || k == ids.union_specifier || (self.cpp && k == ids.class_specifier) {
            match t.child_by_field_id(ids.f_name) {
                Some(n) => self.intern(self.text(n)),
                None if t.child_by_field_id(ids.f_body).is_some() => self.unnamed(t),
                None => NONE,
            }
        } else {
            NONE
        }
    }

    /// A name for a struct or union that has none, the same wherever it is
    /// asked for: by where it is written.
    fn unnamed(&mut self, t: Node<'t>) -> u32 {
        let name = format!("@{}:{}", self.out.name(0), t.start_byte());
        self.out.push_name(&name)
    }

    /// What `object` in `object.member` is: its root (a type, a variable or
    /// a call) and the members followed from there, appended to `path`.
    fn object_root(&mut self, mut n: Node<'t>, path: &mut Vec<u32>) -> Option<(u8, u32)> {
        let ids = self.ids;
        for _ in 0..32 {
            let k = n.kind_id();
            if self.cpp && k == ids.subscript_expression {
                // `v[i]`: an element.
                let of = n.child_by_field_id(ids.f_argument)?;
                if let Some((kind, root, prefix)) = self.element_of(of) {
                    path.extend(prefix);
                    return Some((kind, root));
                }
                // `val[0]` of a class with an `operator[]`: what that returns
                // (of an object whose type is known).
                let r = self.object_root(of, path).filter(|r| r.0 == root::TYPE)?;
                path.push(self.intern_str("[]"));
                return Some(r);
            }
            if k == ids.parenthesized_expression {
                n = n.named_child(0)?;
            } else if k == ids.pointer_expression || k == ids.subscript_expression {
                n = n.child_by_field_id(ids.f_argument)?;
            } else {
                break;
            }
        }
        let k = n.kind_id();
        if self.cpp && k == ids.this && self.method_class != NONE {
            return Some((root::TYPE, self.method_class));
        }
        if k == ids.identifier {
            let text = self.text(n);
            if self.in_fn() && self.locals.contains(text) {
                if let Some((kind, root, prefix)) = self.local_roots.get(text) {
                    path.extend_from_slice(prefix);
                    return Some((*kind, *root));
                }
                return self.local_types.get(text).map(|&t| (root::TYPE, t));
            }
            if self.cpp && self.in_fn() && self.method_class != NONE {
                // In a method, a name that is not local is first of all a
                // member of its class: `iter_->Next ()`.
                path.push(self.intern(text));
                return Some((root::TYPE, self.method_class));
            }
            return Some((root::VAR, self.intern(text)));
        }
        if k == ids.field_expression {
            let r = self.object_root(n.child_by_field_id(ids.f_argument)?, path)?;
            let member = n.child_by_field_id(ids.f_field)?;
            path.push(self.intern(self.text(member)));
            return Some(r);
        }
        if k == ids.call_expression {
            let f = n.child_by_field_id(ids.f_function)?;
            if self.cpp && f.kind_id() == ids.field_expression {
                let (_, method) = self.qualified_parts(f.child_by_field_id(ids.f_field)?);
                if matches!(self.text(method), b"at" | b"back" | b"front" | b"top") {
                    // `v.back ()`: an element.
                    let (kind, root, prefix) = self.element_of(f.child_by_field_id(ids.f_argument)?)?;
                    path.extend(prefix);
                    return Some((kind, root));
                }
                // `x.Get ().f ()`: what the method returns, as if a member.
                let r = self.object_root(f.child_by_field_id(ids.f_argument)?, path)?;
                path.push(self.intern(self.text(method)));
                return Some(r);
            }
            // (`ns::Make ()`, `T ()`: by the name.)
            let f = if self.cpp { self.qualified_parts(f).1 } else { f };
            let f = Some(f).filter(|f| f.kind_id() == ids.identifier || (self.cpp && f.kind_id() == ids.type_identifier))?;
            return Some((root::FUNC, self.intern(self.text(f))));
        }
        if k == ids.cast_expression {
            let t = n.child_by_field_id(ids.f_type).and_then(|d| d.child_by_field_id(ids.f_type))?;
            let name = self.type_name(t);
            return (name != NONE).then_some((root::TYPE, name));
        }
        None
    }

    /// C++: what an `auto` variable is made from, as where a member access on
    /// it starts: `new T`, `T {...}`, `std::make_unique<T> (...)` a T; a
    /// call, a variable or a member, as a member access on them would.
    fn value_root(&mut self, v: Node<'t>) -> Option<(u8, u32, Vec<u32>)> {
        let ids = self.ids;
        let v = self.strip(v);
        match v.kind() {
            "new_expression" | "compound_literal_expression" => {
                let t = self.type_name(v.child_by_field_id(ids.f_type)?);
                return (t != NONE).then(|| (root::TYPE, t, Vec::new()));
            }
            _ => {}
        }
        if v.kind_id() == ids.call_expression {
            // (`std::make_unique<T>`: through the scope to the template.)
            let mut f = v.child_by_field_id(ids.f_function)?;
            while f.kind_id() == ids.qualified_identifier {
                f = f.child_by_field_id(ids.f_name)?;
            }
            if f.kind_id() == ids.template_function
                && matches!(f.child_by_field_id(ids.f_name).map(|n| self.text(n)), Some(b"make_unique" | b"make_shared" | b"allocate_shared"))
            {
                let arg = f.child_by_field_id(ids.f_arguments)?.named_child(0).filter(|a| a.kind_id() == ids.type_descriptor)?;
                let t = self.type_name(arg.child_by_field_id(ids.f_type)?);
                return (t != NONE).then(|| (root::TYPE, t, Vec::new()));
            }
        }
        let mut path = Vec::new();
        let (kind, root) = self.object_root(v, &mut path)?;
        Some((kind, root, path))
    }

    /// C++: where a member access on an operand would start, as a hint, or
    /// `NONE` (a literal, an expression of no known type).
    fn operand_hint(&mut self, n: Node<'t>) -> u32 {
        let mut path = Vec::new();
        match self.object_root(n, &mut path) {
            Some((kind, root)) => self.push_hint(kind, root, &path),
            None => NONE,
        }
    }

    /// C++: `operator<op>` used at `at` on `left` (and `right`): a call of
    /// the class's member of that name, or of a function of no class taking
    /// operands of their types; which, the link tells. Nothing is noted of
    /// operands of no known type: nothing would tell.
    fn push_operator(&mut self, op: &[u8], left: Node<'t>, right: Option<Node<'t>>, at: Node<'t>) {
        let (l, r) = (self.operand_hint(left), right.map_or(NONE, |r| self.operand_hint(r)));
        if l == NONE && r == NONE {
            return;
        }
        let name = self.intern_str(&format!("operator{}", String::from_utf8_lossy(op)));
        let scope = self.scope.map_or(0, |s| s.def);
        self.out.refs.push(Ref { name, line: at.start_position().row as u32 + 1, scope, kind: refkind::CALL });
        let at = self.out.refs.len() as u32 - 1;
        self.out.cxx_mut().operators.push((at, l, r, right.is_some()));
    }

    /// C++: of a function named `operator...` of no class, its parameters'
    /// types (`NONE` where not known), for which operands it takes.
    fn note_operator_params(&mut self, def: u32, declarator: Option<Node<'t>>) {
        let ids = self.ids;
        let Some(mut n) = declarator else { return };
        for _ in 0..8 {
            if n.kind_id() == ids.function_declarator {
                break;
            }
            // (`Status& operator= (...)`: a reference's declarator has no field name.)
            match n.child_by_field_id(ids.f_declarator.get()).or_else(|| n.named_child(0)) {
                Some(inner) => n = inner,
                None => return,
            }
        }
        let Some(list) = n.child_by_field_id(ids.f_parameters) else { return };
        let mut c = list.walk();
        let params: Vec<Node<'t>> = list.named_children(&mut c).filter(|p| p.kind_id() == ids.parameter_declaration).collect();
        let p0 = params.first().map_or(NONE, |&p| self.type_name_of(p));
        let p1 = params.get(1).map_or(NONE, |&p| self.type_name_of(p));
        if p0 != NONE || p1 != NONE {
            self.out.cxx_mut().operator_params.push((def, p0, p1));
        }
    }

    /// C++: the class an object of type `t` is made of, for its constructor:
    /// its name, through scopes, not into a template's arguments (a
    /// `std::unique_ptr<T>` is no `T`), or `NONE`.
    fn constructed_class(&mut self, t: Node<'t>) -> u32 {
        let ids = self.ids;
        let mut t = t;
        for _ in 0..4 {
            let k = t.kind_id();
            if k == ids.qualified_identifier {
                t = self.qualified_parts(t).1;
            } else if k == ids.template_type {
                match t.child_by_field_id(ids.f_name) {
                    Some(n) => t = n,
                    None => return NONE,
                }
            } else {
                break;
            }
        }
        if t.kind_id() == ids.type_identifier { self.intern(self.text(t)) } else { NONE }
    }

    /// C++: a call of a constructor of `class` (or of the type of the member
    /// so named, with `hint` saying whose) made at `at`.
    fn push_construction(&mut self, class: u32, at: Node<'t>, hint: u32, args: Option<u8>) {
        let scope = self.scope.map_or(0, |s| s.def);
        self.out.refs.push(Ref { name: class, line: at.start_position().row as u32 + 1, scope, kind: refkind::CALL });
        let r = self.out.refs.len() as u32 - 1;
        if hint != NONE {
            self.out.ref_hints.push((r, hint));
        }
        self.out.cxx_mut().constructs.push(r);
        if let Some(n) = args {
            self.out.cxx_mut().args.push((r, n));
        }
    }

    /// C++: the type of the elements of a container type (`std::vector<T>`:
    /// `T`; a map's: its values'), or `NONE`.
    /// C++: the first of the template arguments of a template type, a type
    /// (`StrictMock<MockHandler>`: `MockHandler`), or `NONE`.
    fn first_template_arg(&mut self, t: Node<'t>) -> u32 {
        let ids = self.ids;
        let t = if t.kind_id() == ids.qualified_identifier { self.qualified_parts(t).1 } else { t };
        if t.kind_id() != ids.template_type {
            return NONE;
        }
        let arg = t.child_by_field_id(ids.f_arguments).and_then(|a| a.named_child(0)).filter(|a| a.kind_id() == ids.type_descriptor);
        match arg.and_then(|a| a.child_by_field_id(ids.f_type)) {
            Some(a) => self.type_name(a),
            None => NONE,
        }
    }

    fn element_type(&mut self, t: Node<'t>) -> u32 {
        let ids = self.ids;
        let t = if t.kind_id() == ids.qualified_identifier { self.qualified_parts(t).1 } else { t };
        if t.kind_id() != ids.template_type {
            return NONE;
        }
        let Some(name) = t.child_by_field_id(ids.f_name) else { return NONE };
        let which = match self.text(name) {
            n if CONTAINERS.contains(&n) => 0,
            n if MAPS.contains(&n) => 1,
            _ => return NONE,
        };
        let Some(args) = t.child_by_field_id(ids.f_arguments) else { return NONE };
        let mut c = args.walk();
        let arg = args.named_children(&mut c).nth(which).filter(|a| a.kind_id() == ids.type_descriptor);
        match arg.and_then(|a| a.child_by_field_id(ids.f_type)) {
            Some(a) => self.type_name(a),
            None => NONE,
        }
    }

    /// C++: the type of the keys of a map type (`std::map<K, V>`: `K`), or
    /// `NONE`.
    fn key_type(&mut self, t: Node<'t>) -> u32 {
        let ids = self.ids;
        let t = if t.kind_id() == ids.qualified_identifier { self.qualified_parts(t).1 } else { t };
        if t.kind_id() != ids.template_type || !t.child_by_field_id(ids.f_name).is_some_and(|n| MAPS.contains(&self.text(n))) {
            return NONE;
        }
        let arg = t.child_by_field_id(ids.f_arguments).and_then(|a| a.named_child(0)).filter(|a| a.kind_id() == ids.type_descriptor);
        match arg.and_then(|a| a.child_by_field_id(ids.f_type)) {
            Some(a) => self.type_name(a),
            None => NONE,
        }
    }

    /// C++: the names a structured binding declares (`auto& [k, v]`), if
    /// `d` is one.
    fn binding_names(&self, d: Node<'t>) -> Option<Vec<Node<'t>>> {
        let mut n = d;
        for _ in 0..4 {
            if n.kind() == "structured_binding_declarator" {
                let mut c = n.walk();
                return Some(n.named_children(&mut c).filter(|x| x.kind_id() == self.ids.identifier).collect());
            }
            n = n.child_by_field_id(self.ids.f_declarator.get()).or_else(|| n.named_child(0))?;
        }
        None
    }

    /// C++: the `i`th of what a structured binding of `whole` (a root and
    /// path) declares: its `i`th member (`#i`).
    fn binding_root(&mut self, whole: Option<(u8, u32, Vec<u32>)>, i: usize) -> Option<(u8, u32, Vec<u32>)> {
        let (kind, root, mut path) = whole?;
        path.push(self.intern_str(&format!("#{i}")));
        Some((kind, root, path))
    }

    /// C++: where a member access on an element of `container` starts: of a
    /// local container, its elements' type; else the container's own root
    /// and path, and the mark of an element (`[]`).
    fn element_of(&mut self, container: Node<'t>) -> Option<(u8, u32, Vec<u32>)> {
        let c = self.strip(container);
        if c.kind_id() == self.ids.identifier && self.in_fn() && self.locals.contains(self.text(c)) {
            return self.local_elems.get(self.text(c)).map(|&e| (root::TYPE, e, Vec::new()));
        }
        let mut path = Vec::new();
        let (kind, root) = self.object_root(c, &mut path)?;
        path.push(self.intern_str("[]"));
        Some((kind, root, path))
    }

    /// The hint for the member of a member access, or `NONE`.
    fn member_hint(&mut self, access: Node<'t>) -> u32 {
        let mut path = Vec::new();
        let Some(object) = access.child_by_field_id(self.ids.f_argument) else { return NONE };
        match self.object_root(object, &mut path) {
            Some((root_kind, root)) => self.push_hint(root_kind, root, &path),
            None => NONE,
        }
    }

    fn push_hint(&mut self, root_kind: u8, root: u32, path: &[u32]) -> u32 {
        let start = self.out.hint_path.len() as u32;
        self.out.hint_path.extend_from_slice(path);
        self.out.hints.push(MemberHint { root_kind, root, path: start, len: path.len() as u32 });
        self.out.hints.len() as u32 - 1
    }

    /// The names of a function's own parameters, in order.
    fn note_params(&mut self, declarator: Node<'t>) {
        let ids = self.ids;
        // Down to the declarator that holds the parameter list of the
        // function itself: the one whose own declarator is the name.
        let mut n = declarator;
        for _ in 0..32 {
            let Some(inner) = n.child_by_field_id(ids.f_declarator.get()).or_else(|| n.named_child(0)) else { return };
            let ik = inner.kind_id();
            let named = ik == ids.identifier
                || (self.cpp
                    && (ik == ids.field_identifier || ik == ids.qualified_identifier || ik == ids.destructor_name
                        || ik == ids.operator_name || ik == ids.template_function));
            if n.kind_id() == ids.function_declarator && named {
                break;
            }
            n = inner;
        }
        let Some(list) = n.child_by_field_id(ids.f_parameters) else { return };
        let mut c = list.walk();
        let params: Vec<Node<'t>> = list.named_children(&mut c).filter(|p| p.kind_id() == ids.parameter_declaration).collect();
        for (i, p) in params.into_iter().enumerate() {
            if let Some((name, _)) = p.child_by_field_id(ids.f_declarator.get()).and_then(|d| self.decl_name(d)) {
                self.params.insert(self.text(name), i as u32);
                let t = self.type_name_of(p);
                if t != NONE {
                    self.local_types.insert(self.text(name), t);
                }
            }
        }
    }

    /// Note what `value` is, as something assigned to the local `local`.
    fn local_source(&mut self, local: &'a [u8], value: Node<'t>) {
        let value = self.strip(value);
        let k = value.kind_id();
        if k == self.ids.field_expression {
            if let Some(member) = value.child_by_field_id(self.ids.f_field) {
                let name = self.intern(self.text(member));
                let hint = self.member_hint(value);
                self.local_sources.entry(local).or_default().push((true, name, hint));
            }
        } else if k == self.ids.identifier {
            if !self.locals.contains(self.text(value)) {
                let name = self.intern(self.text(value));
                self.local_sources.entry(local).or_default().push((false, name, NONE));
            }
        } else if k == self.ids.conditional_expression {
            for f in [self.ids.f_consequence, self.ids.f_alternative] {
                if let Some(c) = value.child_by_field_id(f) {
                    self.local_source(local, c);
                }
            }
        }
    }

    /// Merge in everything the mask hid before byte `pos`.
    fn flush_events(&mut self, pos: usize) {
        while let Some(ev) = self.next_event.filter(|e| e.pos() < pos) {
            self.next_event = self.events.next();
            self.leave_scope(ev.pos());
            match ev {
                Event::Macro(m) => {
                    let name = self.intern(self.tok_text(&m.name));
                    let flags = if m.conditional { flag::CONDITIONAL } else { 0 } | if m.name.call { flag::CALLABLE } else { 0 };
                    self.out.defs.push(Def { name, line: m.name.line, end_line: m.end_line, kind: kind::MACRO, flags });
                    let scope = (self.out.defs.len() - 1) as u32;
                    for t in &m.refs {
                        self.push_tok_ref(t, scope);
                        if self.cpp && t.call {
                            if let Some(n) = self.args_in_text(t.end as usize, m.end as usize) {
                                let r = self.out.refs.len() as u32 - 1;
                                self.out.cxx_mut().args.push((r, n));
                            }
                        }
                        // C++: `gtest_ar.failure_message ()` of a `gtest_ar`
                        // the body declares: of its class.
                        if self.cpp && t.member {
                            if let Some(ty) = self.declared_in_body(t.start as usize, m.name.end as usize, m.end as usize) {
                                let (ty, at) = (self.intern(ty), self.out.refs.len() as u32 - 1);
                                let hint = self.push_hint(root::TYPE, ty, &[]);
                                self.out.ref_hints.push((at, hint));
                            }
                        }
                        // C++: `::testing::internal::EqHelper::Compare (...)` in
                        // the body, read as text: what it is qualified with.
                        if self.cpp {
                            if let Some(q) = self.qualifier_before(t.start as usize) {
                                let (q, at) = (self.intern(q), self.out.refs.len() as u32 - 1);
                                self.out.cxx_mut().ref_quals.push((at, q));
                            }
                            // `UniqueLock<T> name (...)` in the body (bitcoin's
                            // `LOCK`): an object of that class made, if it is one.
                            // `SourceLocation{__func__}` (bitcoin's `LogPrintLevel_`)
                            // too: one made.
                            let braced = !t.member && self.next_byte(t.end as usize) == Some(b'{') && !self.tag_before(t.start as usize);
                            if self.declares_object_after(t.end as usize) || braced {
                                let name = self.intern(self.tok_text(t));
                                self.out.refs.push(Ref { name, line: t.line, scope, kind: refkind::CALL });
                                let r = self.out.refs.len() as u32 - 1;
                                self.out.cxx_mut().constructs.push(r);
                            }
                            // `::testing::internal::AssertHelper (...) = ::testing::Message ()`
                            // (gtest's `GTEST_MESSAGE_AT_`): the class's `operator=`,
                            // if it is a class, given what is made on the right.
                            if let Some(right) = (!t.member).then(|| self.assigned_after(t.end as usize, m.end as usize)).flatten() {
                                let ty = self.intern(self.tok_text(t));
                                let left = self.push_hint(root::TYPE, ty, &[]);
                                let right = match right {
                                    Some(r) => {
                                        let r = self.intern(r);
                                        self.push_hint(root::TYPE, r, &[])
                                    }
                                    None => NONE,
                                };
                                let name = self.intern_str("operator=");
                                self.out.refs.push(Ref { name, line: t.line, scope, kind: refkind::CALL });
                                let at = self.out.refs.len() as u32 - 1;
                                self.out.cxx_mut().operators.push((at, left, right, true));
                            }
                            // `gtest_ar << "..."` of a `gtest_ar` the body declares
                            // (fmt's FMT_TEST_THROW_): its class's `operator<<`.
                            let at = self.skip_space(t.end as usize);
                            let shifted = self.orig.get(at..at + 2) == Some(b"<<") && self.orig.get(at + 2) != Some(&b'=');
                            if let Some(ty) = (!t.member && shifted).then(|| self.body_type_of(self.tok_text(t), m.name.end as usize, m.end as usize)).flatten() {
                                let ty = self.intern(ty);
                                let left = self.push_hint(root::TYPE, ty, &[]);
                                let name = self.intern_str("operator<<");
                                self.out.refs.push(Ref { name, line: t.line, scope, kind: refkind::CALL });
                                let at = self.out.refs.len() as u32 - 1;
                                self.out.cxx_mut().operators.push((at, left, NONE, true));
                            }
                        }
                    }
                    if self.cpp {
                        self.note_forwards(scope, m.name.end as usize, m.end as usize, m.name.call);
                    }
                    if m.keep {
                        let text = String::from_utf8_lossy(&self.orig[m.name.end as usize..m.end as usize]);
                        let text = self.out.push_text(&text);
                        let guard = if m.guard.is_empty() { NONE } else { self.out.push_text(&m.guard) };
                        self.out.macros.push(MacroBody { name, text, function_like: m.name.call, guard, conditional: m.conditional, fallback: m.fallback, line: m.name.line });
                    }
                }
                Event::Site(s) => {
                    let name = self.intern(self.tok_text(&s.name));
                    let args = if s.args_end == 0 {
                        NONE
                    } else {
                        let text = String::from_utf8_lossy(&self.orig[s.name.end as usize..s.args_end as usize]);
                        self.out.push_text(&text)
                    };
                    // Outside every bracket is outside every function, whatever
                    // the parser made of the lines before.
                    let top = s.flags & site::TOP != 0;
                    let flags = s.flags | if self.in_fn() && !top { site::IN_FN } else { 0 };
                    let def = self.site_defs[&site_def_at(s)];
                    let scope = if top && def == NONE { 0 } else { self.scope.map_or(0, |s| s.def) };
                    let class = if self.cpp { self.class_at(s.name.start as usize).map_or(NONE, |c| self.intern_str(&self.classes[c].chain.clone())) } else { NONE };
                    self.out.sites.push(Site { name, args, line: s.name.line, scope, def, flags, class });
                    let (open, close) = (s.init.0 as usize, s.init.1 as usize);
                    if close > open && close - open <= MAX_INIT_TEXT {
                        let type_name = self.intern(&self.orig[s.init_type.0 as usize..s.init_type.1 as usize]);
                        let text = String::from_utf8_lossy(&self.orig[open..close]);
                        let text = self.out.push_text(&text);
                        let scope = if def == NONE { scope } else { def };
                        self.out.inits.push(InitSite { type_name, text, line: s.name.line, scope, array: s.init_array });
                    }
                }
                Event::Ref(t) => {
                    if !(self.in_fn() && self.locals.contains(self.tok_text(t))) {
                        self.push_tok_ref(t, self.scope.map_or(0, |s| s.def));
                    }
                }
            }
        }
    }

    /// C++: of the object of the member at `member` in a macro body
    /// `s[start..end]` (`gtest_ar` of `gtest_ar.failure_message`), the type
    /// the body declares it of (`const AssertionResult gtest_ar = ...`).
    fn declared_in_body(&self, member: usize, start: usize, end: usize) -> Option<&'a [u8]> {
        let s = self.orig;
        let back = |mut i: usize| {
            while i > start && matches!(s[i - 1], b' ' | b'\t' | b'\\' | b'\n' | b'\r') {
                i -= 1;
            }
            i
        };
        let word_before = |i: usize| {
            let mut a = i;
            while a > start && (s[a - 1].is_ascii_alphanumeric() || s[a - 1] == b'_') {
                a -= 1;
            }
            (a < i && !s[a].is_ascii_digit()).then_some(a)
        };
        let mut i = back(member);
        if i > start && s[i - 1] == b'.' {
            i -= 1;
        } else if i > start + 1 && &s[i - 2..i] == b"->" {
            i -= 2;
        } else {
            return None;
        }
        let i = back(i);
        let object = &s[word_before(i)?..i];
        self.body_type_of(object, start, end)
    }

    /// C++: the type a macro body `s[start..end]` declares the object
    /// `object` of (`const AssertionResult gtest_ar = ...`), if it does.
    fn body_type_of(&self, object: &'a [u8], start: usize, end: usize) -> Option<&'a [u8]> {
        let s = self.orig;
        let back = |mut i: usize| {
            while i > start && matches!(s[i - 1], b' ' | b'\t' | b'\\' | b'\n' | b'\r') {
                i -= 1;
            }
            i
        };
        let word_before = |i: usize| {
            let mut a = i;
            while a > start && (s[a - 1].is_ascii_alphanumeric() || s[a - 1] == b'_') {
                a -= 1;
            }
            (a < i && !s[a].is_ascii_digit()).then_some(a)
        };
        let body = &s[start..end];
        let mut from = 0;
        while let Some(at) = body[from..].windows(object.len()).position(|w| w == object).map(|p| p + from) {
            from = at + object.len();
            let (a, e) = (start + at, start + at + object.len());
            let whole = (a == start || !(s[a - 1].is_ascii_alphanumeric() || s[a - 1] == b'_'))
                && !(e < end && (s[e].is_ascii_alphanumeric() || s[e] == b'_'));
            if !whole {
                continue;
            }
            let mut j = e;
            while j < end && matches!(s[j], b' ' | b'\t') {
                j += 1;
            }
            if !matches!(s.get(j), Some(b'=' | b'(' | b'{' | b';')) || s.get(j + 1) == Some(&b'=') {
                continue;
            }
            let k = back(a);
            if k == a {
                continue;
            }
            let Some(t) = word_before(k) else { continue };
            let ty = &s[t..k];
            if !crate::mask::is_keyword(ty) && !matches!(ty, b"return" | b"new" | b"delete" | b"throw" | b"case" | b"goto") {
                return Some(ty);
            }
        }
        None
    }

    /// C++: does the text at `pos`, after a name, declare an object of it:
    /// template arguments maybe, a name, then `(`, `{` or `;`?
    fn declares_object_after(&self, pos: usize) -> bool {
        let s = self.orig;
        let skip = |mut i: usize| {
            while i < s.len() && (s[i].is_ascii_whitespace() || (s[i] == b'\\' && s.get(i + 1).is_some_and(|b| *b == b'\n' || *b == b'\r'))) {
                i += 1;
            }
            i
        };
        let mut i = skip(pos);
        if s.get(i) == Some(&b'<') {
            let mut depth = 0;
            while i < s.len() {
                match s[i] {
                    b'<' => depth += 1,
                    b'>' => depth -= 1,
                    b';' | b'{' | b'}' => return false,
                    _ => {}
                }
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            i = skip(i);
        }
        let start = i;
        loop {
            while i < s.len() && (s[i].is_ascii_alphanumeric() || s[i] == b'_') {
                i += 1;
            }
            // (`held_##v`: a name pasted.)
            let j = skip(i);
            if s.get(j..j + 2) == Some(b"##") {
                i = skip(j + 2);
                continue;
            }
            break;
        }
        if i == start || s[start].is_ascii_digit() {
            return false;
        }
        matches!(s.get(skip(i)), Some(b'(' | b'{' | b';'))
    }

    /// Past spaces and line continuations from `pos` in the text.
    fn skip_space(&self, mut i: usize) -> usize {
        let s = self.orig;
        while i < s.len() && (s[i].is_ascii_whitespace() || (s[i] == b'\\' && s.get(i + 1).is_some_and(|b| *b == b'\n' || *b == b'\r'))) {
            i += 1;
        }
        i
    }

    /// The first byte of the text at `pos` past spaces.
    fn next_byte(&self, pos: usize) -> Option<u8> {
        self.orig.get(self.skip_space(pos)).copied()
    }

    /// Is the word before `pos` one that makes a type of the name after it
    /// (`struct X {`)?
    fn tag_before(&self, pos: usize) -> bool {
        let s = self.orig;
        let mut i = pos;
        while i > 0 && matches!(s[i - 1], b' ' | b'\t' | b'\\' | b'\n' | b'\r') {
            i -= 1;
        }
        let end = i;
        while i > 0 && (s[i - 1].is_ascii_alphanumeric() || s[i - 1] == b'_') {
            i -= 1;
        }
        matches!(&s[i..end], b"struct" | b"class" | b"union" | b"enum" | b"namespace" | b"typename")
    }

    /// C++: which of its parameters the macro `def`, whose text after its
    /// name is `orig[from..end]`, passes on as arguments of the calls its
    /// body makes; or, of a name standing for a function (`#define
    /// strprintf tfm::format`), that its use's arguments are the function's.
    fn note_forwards(&mut self, def: u32, from: usize, end: usize, function_like: bool) {
        let s = self.orig;
        let skip = |mut i: usize| {
            while i < end && (s[i].is_ascii_whitespace() || s[i] == b'\\') {
                i += 1;
            }
            i
        };
        let word = |i: usize| {
            let mut e = i;
            while e < end && (s[e].is_ascii_alphanumeric() || s[e] == b'_') {
                e += 1;
            }
            e
        };
        // The parameters, `...` (or GNU's `args...`) last.
        let mut params: Vec<&'a [u8]> = Vec::new();
        let mut rest: Option<(u32, &'a [u8])> = None;
        let mut i = from;
        if function_like {
            i = skip(i);
            if s.get(i) != Some(&b'(') {
                return;
            }
            i += 1;
            loop {
                i = skip(i);
                if i >= end {
                    return;
                }
                if s[i] == b')' {
                    i += 1;
                    break;
                }
                if s[i..].starts_with(b"...") {
                    rest = Some((params.len() as u32, b"__VA_ARGS__"));
                    i += 3;
                } else {
                    let e = word(i);
                    if e == i {
                        return;
                    }
                    let name = &s[i..e];
                    i = skip(e);
                    if s[i..].starts_with(b"...") {
                        rest = Some((params.len() as u32, name));
                        i += 3;
                    } else {
                        params.push(name);
                    }
                }
                i = skip(i);
                if s.get(i) == Some(&b',') {
                    i += 1;
                }
            }
        }
        // A name through scopes at `i`: (qualifier or `NONE`, name, end).
        let qualified = |mut i: usize| -> Option<(Option<&'a [u8]>, &'a [u8], usize)> {
            let (mut qual, mut name) = (None, None);
            loop {
                if s.get(i..i + 2) == Some(b"::") {
                    i = skip(i + 2);
                }
                let e = word(i);
                if e == i || s[i].is_ascii_digit() {
                    return name.map(|n| (qual, n, i));
                }
                qual = name.or(qual);
                name = Some(&s[i..e]);
                let j = skip(e);
                if s.get(j..j + 2) != Some(b"::") {
                    return Some((qual, &s[i..e], j));
                }
                i = j;
            }
        };
        let body = skip(i);
        if !function_like {
            // `#define strprintf tfm::format`.
            if let Some((qual, name, _)) = qualified(body).filter(|q| skip(q.2) >= end) {
                let (callee, qual) = (self.intern(name), qual.map_or(NONE, |q| self.intern(q)));
                self.out.cxx_mut().forwards.push(Forward { def, callee, qual, param: 0, arg: 0, rest: true });
            }
            return;
        }
        let mut i = body;
        while i < end {
            let b = s[i];
            if b == b'"' || b == b'\'' {
                i += 1;
                while i < end && s[i] != b {
                    i += 1 + (s[i] == b'\\') as usize;
                }
                i += 1;
                continue;
            }
            let starts = (b.is_ascii_alphabetic() || b == b'_' || (b == b':' && s.get(i + 1) == Some(&b':')))
                && (i == 0 || !(s[i - 1].is_ascii_alphanumeric() || s[i - 1] == b'_' || s[i - 1] == b'.' || (s[i - 1] == b'>' && i >= 2 && s[i - 2] == b'-')));
            let Some((qual, name, open)) = starts.then(|| qualified(i)).flatten() else {
                i += 1;
                continue;
            };
            if s.get(open) != Some(&b'(') || crate::mask::is_keyword(name) || name == b"defined" {
                i = open.max(i + 1);
                continue;
            }
            // The arguments, at the top level of the parentheses.
            let (mut depth, mut k, mut a) = (0i32, 0u32, open + 1);
            let mut j = open;
            let mut found: Vec<(u32, u32, bool)> = Vec::new();
            while j < end {
                match s[j] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' | b',' if depth == 1 => {
                        let mut text = &s[a..j];
                        while let [b' ' | b'\t' | b'\\' | b'\r' | b'\n', more @ ..] = text {
                            text = more;
                        }
                        while let [more @ .., b' ' | b'\t' | b'\\' | b'\r' | b'\n'] = text {
                            text = more;
                        }
                        let text = text.strip_prefix(b"##").map_or(text, |t| t.trim_ascii_start());
                        if let Some(p) = params.iter().position(|&p| p == text) {
                            found.push((p as u32, k, false));
                        } else if let Some((p, _)) = rest.filter(|r| r.1 == text) {
                            found.push((p, k, true));
                        }
                        if s[j] == b',' {
                            k += 1;
                            a = j + 1;
                        } else {
                            break;
                        }
                    }
                    b')' | b']' | b'}' => depth -= 1,
                    b'"' | b'\'' => {
                        let q = s[j];
                        j += 1;
                        while j < end && s[j] != q {
                            j += 1 + (s[j] == b'\\') as usize;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if !found.is_empty() {
                let (callee, qual) = (self.intern(name), qual.map_or(NONE, |q| self.intern(q)));
                for (param, arg, rest) in found {
                    self.out.cxx_mut().forwards.push(Forward { def, callee, qual, param, arg, rest });
                }
            }
            i = open + 1;
        }
    }

    /// How many arguments the parentheses at `pos` in the text (a macro
    /// body ending at `end`) hold; not known where `__VA_ARGS__` is one.
    fn args_in_text(&self, pos: usize, end: usize) -> Option<u8> {
        let s = self.orig;
        let open = self.skip_space(pos);
        if s.get(open) != Some(&b'(') {
            return None;
        }
        let (mut depth, mut commas, mut empty) = (0, 0usize, true);
        let mut i = open;
        while i < end {
            match s[i] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let n = if empty { 0 } else { commas + 1 };
                        let vararg = s[open..i].windows(11).any(|w| w == b"__VA_ARGS__");
                        return (!vararg).then_some(n.min(u8::MAX as usize - 1) as u8);
                    }
                }
                b',' if depth == 1 => commas += 1,
                b'"' | b'\'' => {
                    let q = s[i];
                    i += 1;
                    while i < end && s[i] != q {
                        i += 1 + (s[i] == b'\\') as usize;
                    }
                }
                _ => {}
            }
            if depth >= 1 && i > open && !s[i].is_ascii_whitespace() && s[i] != b'\\' {
                empty = false;
            }
            i += 1;
        }
        None
    }

    /// C++, in a macro body ending at `end`: is the name before `pos` made
    /// and then assigned to, `T (...) = ...`? Then the class the right side
    /// makes, if it does (`= ::testing::Message ()`).
    fn assigned_after(&self, pos: usize, end: usize) -> Option<Option<&'a [u8]>> {
        let s = self.orig;
        let mut i = self.skip_space(pos);
        if s.get(i) != Some(&b'(') {
            return None;
        }
        let mut depth = 0;
        while i < end {
            depth += (s[i] == b'(') as i32 - (s[i] == b')') as i32;
            i += 1;
            if depth == 0 {
                break;
            }
        }
        i = self.skip_space(i);
        if depth != 0 || i + 1 >= end || s[i] != b'=' || s[i + 1] == b'=' {
            return None;
        }
        // The right side: a name, through scopes, then `(` or `{`.
        let mut i = self.skip_space(i + 1);
        let mut last = None;
        loop {
            if s.get(i..i + 2) == Some(b"::") {
                i = self.skip_space(i + 2);
            }
            let a = i;
            while i < end && (s[i].is_ascii_alphanumeric() || s[i] == b'_') {
                i += 1;
            }
            if a == i || s[a].is_ascii_digit() {
                break;
            }
            last = Some(&s[a..i]);
            i = self.skip_space(i);
            if s.get(i..i + 2) != Some(b"::") {
                break;
            }
        }
        Some(last.filter(|_| matches!(s.get(i), Some(b'(' | b'{'))))
    }

    /// C++: the name before `::` before `pos` in the text (`B` of `A::B::f`,
    /// "" of `::f`), if `::` is there.
    fn qualifier_before(&self, pos: usize) -> Option<&'a [u8]> {
        let s = self.orig;
        let mut i = pos;
        while i > 0 && matches!(s[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        if i < 2 || &s[i - 2..i] != b"::" {
            return None;
        }
        i -= 2;
        while i > 0 && matches!(s[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        let end = i;
        while i > 0 && (s[i - 1].is_ascii_alphanumeric() || s[i - 1] == b'_') {
            i -= 1;
        }
        // (`Base<T>::f`: the template's arguments are no name.)
        Some(&s[i..end])
    }

    fn tok_text(&self, t: &Tok) -> &'a [u8] {
        &self.orig[t.start as usize..t.end as usize]
    }

    fn push_tok_ref(&mut self, t: &Tok, scope: u32) {
        let name = self.intern(self.tok_text(t));
        let kind = match (t.member, t.call) {
            (true, true) => refkind::ICALL,
            (true, false) => refkind::MEMBER,
            (false, true) => refkind::CALL,
            (false, false) => refkind::USE,
        };
        self.out.refs.push(Ref { name, line: t.line, scope, kind });
    }

    fn push_ref(&mut self, n: Node, kind: u8) {
        let at = n.start_byte();
        let kind = if matches!(kind, refkind::CALL | refkind::PCALL | refkind::ICALL) && self.quiet.iter().any(|&(a, e)| a <= at && at < e) { refkind::USE } else { kind };
        let name = self.intern(self.text(n));
        let scope = self.scope.map_or(0, |s| s.def);
        self.out.refs.push(Ref { name, line: n.start_position().row as u32 + 1, scope, kind });
    }

    fn in_fn(&self) -> bool {
        self.scope.is_some_and(|s| s.is_fn)
    }

    /// Follow a declarator chain down to the declared name. The bool tells
    /// whether the name itself is declared as a function (`f(...)`, as opposed
    /// to a function pointer `(*f)(...)`).
    fn decl_name(&self, mut n: Node<'t>) -> Option<(Node<'t>, bool)> {
        let ids = self.ids;
        let mut is_fn = false;
        for _ in 0..32 {
            let k = n.kind_id();
            if k == ids.identifier || k == ids.type_identifier || k == ids.field_identifier
                || (self.cpp && (k == ids.destructor_name || k == ids.operator_name))
            {
                // A name the parser made up to get past an error (where a
                // macro stood, say) has no text, and names nothing.
                return (!n.byte_range().is_empty()).then_some((n, is_fn));
            }
            if self.cpp && (k == ids.qualified_identifier || k == ids.template_function) {
                // `A::f`, `f<int>`: the name, the scope aside.
                n = self.qualified_parts(n).1;
                continue;
            }
            let inner = n.child_by_field_id(ids.f_declarator.get()).or_else(|| n.named_child(0))?;
            if k == ids.function_declarator {
                let ik = inner.kind_id();
                // (In C++ a method's name is a field's, and may be qualified.)
                is_fn = ik == ids.identifier
                    || (self.cpp
                        && (ik == ids.field_identifier || ik == ids.qualified_identifier || ik == ids.destructor_name
                            || ik == ids.operator_name || ik == ids.template_function));
            }
            n = inner;
        }
        None
    }

    fn storage_flags(&self, n: Node) -> u8 {
        let mut flags = 0;
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            if ch.kind_id() == self.ids.storage_class_specifier {
                match self.text(ch) {
                    b"static" => flags |= flag::STATIC,
                    b"extern" => flags |= flag::DECL,
                    _ => {}
                }
            }
        }
        flags
    }

    fn declarators(&self, n: Node<'t>) -> Vec<Node<'t>> {
        let mut c = n.walk();
        n.children_by_field_id(self.ids.f_declarator, &mut c).collect()
    }

    /// C++, in a function: `UniValue result (UniValue::VOBJ);` or `T x (n);`,
    /// which the parser reads as a function's declaration: an object made,
    /// its arguments taken for parameters with a type and no name. (With no
    /// arguments, `T x ();` declares a function, as the compiler has it.)
    fn object_not_function(&self, d: Node<'t>) -> bool {
        let ids = self.ids;
        if d.kind_id() != ids.function_declarator || d.child_by_field_id(ids.f_declarator.get()).is_none_or(|n| n.kind_id() != ids.identifier) {
            return false;
        }
        let Some(list) = d.child_by_field_id(ids.f_parameters) else { return false };
        let mut c = list.walk();
        let params: Vec<Node<'t>> = list.named_children(&mut c).filter(|p| p.kind() != "comment").collect();
        // (`CPubKey pubKey (vSolutions[0]);`: an element taken for an array.)
        let element = |p: &Node<'t>| p.named_child_count() == 2 && p.child_by_field_id(ids.f_declarator.get()).is_some_and(|d| d.kind() == "abstract_array_declarator" && d.named_child_count() == 1);
        !params.is_empty()
            && params.iter().all(|p| {
                let ty = p.child_by_field_id(ids.f_type);
                p.kind_id() == ids.parameter_declaration
                    && (p.named_child_count() == 1 || element(p))
                    && ty.is_some_and(|t| t.kind_id() == ids.type_identifier || t.kind_id() == ids.qualified_identifier)
            })
    }

    /// Does the declarator declare a function or a pointer to one?
    fn declares_function(&self, mut n: Node<'t>) -> bool {
        for _ in 0..32 {
            if n.kind_id() == self.ids.function_declarator {
                return true;
            }
            match n.child_by_field_id(self.ids.f_declarator.get()).or_else(|| n.named_child(0)) {
                Some(inner) => n = inner,
                None => break,
            }
        }
        false
    }

    /// Through parentheses, `*`, `&`, casts and subscripts to what is called
    /// or assigned.
    fn strip(&self, mut n: Node<'t>) -> Node<'t> {
        let ids = self.ids;
        for _ in 0..16 {
            let k = n.kind_id();
            let inner = if k == ids.parenthesized_expression {
                n.named_child(0)
            } else if k == ids.pointer_expression || k == ids.subscript_expression {
                n.child_by_field_id(ids.f_argument)
            } else if k == ids.cast_expression {
                n.child_by_field_id(ids.f_value)
            } else {
                None
            };
            match inner {
                Some(i) => n = i,
                None => break,
            }
        }
        n
    }

    /// Names in `n` that may be functions whose address is taken.
    fn function_names(&self, n: Node<'t>, out: &mut Vec<Node<'t>>) {
        let n = self.strip(n);
        let k = n.kind_id();
        if k == self.ids.identifier {
            if !(self.in_fn() && self.locals.contains(self.text(n))) {
                out.push(n);
            }
        } else if k == self.ids.conditional_expression {
            for f in [self.ids.f_consequence, self.ids.f_alternative] {
                if let Some(c) = n.child_by_field_id(f) {
                    self.function_names(c, out);
                }
            }
        } else if k == self.ids.initializer_list {
            let mut c = n.walk();
            for ch in n.named_children(&mut c) {
                self.function_names(ch, out);
            }
        }
    }

    fn bind(&mut self, value: Node<'t>, slot: u32, index: u32, var: bool, scope: u32, hint: u32) {
        // Stored in something without a name (a declarator the parser made
        // up past an error): nothing can be called through that.
        if slot == NONE {
            return;
        }
        let mut fns = Vec::new();
        self.function_names(value, &mut fns);
        for f in fns {
            let func = self.intern(self.text(f));
            let line = f.start_position().row as u32 + 1;
            self.out.binds.push(Bind { slot, index, func, line, scope, var, unit: NONE, hint });
        }
    }

    /// The members of a struct body in order, each with its type's name
    /// (see `type_name_of`). The members of an unnamed struct or union
    /// member are members of this one.
    fn member_names(&mut self, body: Node<'t>, names: &mut Vec<u32>, types: &mut Vec<u32>, elems: &mut Vec<(u32, u32, u32)>) {
        let ids = self.ids;
        let mut c = body.walk();
        // (A C++ class's methods are members too, declared or defined in it,
        // templates or not.)
        let cpp = self.cpp;
        let decls: Vec<Node<'t>> = body
            .named_children(&mut c)
            .map(|d| if cpp && d.kind() == "template_declaration" { d.named_child(d.named_child_count().saturating_sub(1)).unwrap_or(d) } else { d })
            .filter(|d| {
                let k = d.kind_id();
                k == ids.field_declaration || (cpp && (k == ids.function_definition || k == ids.declaration))
            })
            .collect();
        for decl in decls {
            let ds = self.declarators(decl);
            if ds.is_empty() {
                let inner = decl.child_by_field_id(ids.f_type).and_then(|t| t.child_by_field_id(ids.f_body));
                match inner {
                    Some(inner) => self.member_names(inner, names, types, elems),
                    None => {
                        names.push(NONE);
                        types.push(NONE);
                        elems.push((NONE, NONE, NONE));
                    }
                }
                continue;
            }
            let t = self.type_name_of(decl);
            let (e, key, arg) = match decl.child_by_field_id(ids.f_type).filter(|_| cpp) {
                Some(ty) => (self.element_type(ty), self.key_type(ty), self.first_template_arg(ty)),
                None => (NONE, NONE, NONE),
            };
            for d in ds {
                // A member that is a function pointer has no struct type,
                // whatever its return type is.
                let (name, t) = match self.decl_name(d) {
                    // (A C++ method's: what it returns, for `x.Get ().f ()`.)
                    Some((name, is_fn)) => (self.def_name(name), if self.declares_function(d) && !(self.cpp && is_fn) { NONE } else { t }),
                    None => (NONE, NONE),
                };
                names.push(name);
                types.push(t);
                elems.push(if self.declares_function(d) { (NONE, NONE, NONE) } else { (e, key, arg) });
            }
        }
    }

    fn push_record(&mut self, name: u32, node: Node<'t>, body: Node<'t>) {
        let (mut fields, mut types, mut elems) = (Vec::new(), Vec::new(), Vec::new());
        self.member_names(body, &mut fields, &mut types, &mut elems);
        let defs = vec![NONE; fields.len()];
        let record = self.out.records.len() as u32;
        for (at, &(e, key, arg)) in elems.iter().enumerate() {
            if e != NONE {
                self.out.cxx_mut().elems.push((record, at as u32, e));
            }
            if arg != NONE {
                self.out.cxx_mut().targs.push((record, at as u32, arg));
            }
            if key != NONE {
                self.out.cxx_mut().keys.push((record, at as u32, key));
            }
        }
        self.open_records.push((node.end_byte(), self.out.records.len()));
        self.out.records.push(Record { name, fields, types, defs });
    }

    /// A braced initializer of an object of struct type `type_name` (or of
    /// an array of them): bind the functions in it to the members they
    /// initialize.
    fn init_list(&mut self, list: Node<'t>, type_name: u32, array: bool, var: u32, scope: u32) {
        let ids = self.ids;
        let mut index = 0u32;
        let mut c = list.walk();
        let elements: Vec<Node<'t>> = list.named_children(&mut c).collect();
        for el in elements {
            let k = el.kind_id();
            if k == ids.initializer_pair {
                let Some(value) = el.child_by_field_id(ids.f_value) else { continue };
                let mut dc = el.walk();
                let field = el
                    .children_by_field_id(ids.f_designator, &mut dc)
                    .filter(|d| d.kind_id() == ids.field_designator)
                    .last()
                    .and_then(|d| d.named_child(0));
                match field {
                    Some(f) => {
                        let slot = self.intern(self.text(f));
                        // The initialized object's type says whose member it is.
                        let hint = if type_name == NONE { NONE } else { self.push_hint(root::TYPE, type_name, &[]) };
                        self.bind(value, slot, NONE, false, scope, hint);
                    }
                    // `[i] = { ... }`: one element of the array.
                    None if value.kind_id() == ids.initializer_list => self.init_list(value, type_name, false, var, scope),
                    None => self.bind(value, var, NONE, true, scope, NONE),
                }
            } else if array {
                if k == ids.initializer_list {
                    self.init_list(el, type_name, false, var, scope);
                } else if var != NONE {
                    // An array of function pointers.
                    self.bind(el, var, NONE, true, scope, NONE);
                }
            } else if !el.is_extra() {
                if type_name != NONE {
                    self.bind(el, type_name, index, false, scope, NONE);
                }
                index += 1;
            }
        }
    }

    /// The declared objects of a declaration and what they are initialized
    /// with.
    fn initializers(&mut self, decl: Node<'t>, scope: u32) {
        let ids = self.ids;
        let type_name = match decl.child_by_field_id(ids.f_type) {
            Some(t) if t.kind_id() == ids.type_identifier => self.intern(self.text(t)),
            Some(t) if t.kind_id() == ids.struct_specifier || t.kind_id() == ids.union_specifier => {
                t.child_by_field_id(ids.f_name).map_or(NONE, |n| self.intern(self.text(n)))
            }
            _ => NONE,
        };
        for d in self.declarators(decl) {
            if d.kind_id() != ids.init_declarator {
                continue;
            }
            let (Some(value), Some(inner)) = (d.child_by_field_id(ids.f_value), d.child_by_field_id(ids.f_declarator.get()))
            else {
                continue;
            };
            let var = match self.decl_name(inner) {
                Some((name, _)) if !self.in_fn() => self.intern(self.text(name)),
                _ => NONE,
            };
            if value.kind_id() != ids.initializer_list {
                if var != NONE {
                    self.bind(value, var, NONE, true, scope, NONE);
                }
                continue;
            }
            let array = inner.kind_id() == ids.array_declarator;
            let (start, end) = (value.start_byte(), value.end_byte());
            let at = self.site_starts.partition_point(|&p| p < start);
            let directive = || {
                self.orig[start..end]
                    .split(|&b| b == b'\n')
                    .any(|l| l.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'#'))
            };
            if self.site_starts.get(at).is_some_and(|&p| p < end) || directive() {
                // Macros or `#if` inside: positions are only known after
                // expansion, one branch at a time. A long table (more
                // than an expansion holds in one go) is read row by row
                // instead: rows under an `#if` are rows all the same, each
                // read on its own, and an `#if` inside a row goes with it.
                let by_row = array && end - start > ROWS_APART;
                if !by_row && end - start <= MAX_INIT_TEXT {
                    let text = String::from_utf8_lossy(&self.orig[start..end]);
                    let text = self.out.push_text(&text);
                    let line = value.start_position().row as u32 + 1;
                    self.out.inits.push(InitSite { type_name, text, line, scope, array });
                } else if by_row {
                    // A table too long to expand as one text (a generated
                    // list of commands): row by row, each a struct of its own.
                    let mut c = value.walk();
                    let rows: Vec<Node<'t>> = value.named_children(&mut c).filter(|r| !r.is_extra()).collect();
                    for row in rows {
                        let (a, e) = (row.start_byte(), row.end_byte());
                        let at = self.site_starts.partition_point(|&p| p < a);
                        let split = self.orig[a..e].split(|&b| b == b'\n').any(|l| l.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'#'));
                        if split || self.site_starts.get(at).is_some_and(|&p| p < e) {
                            // A row with a macro in it, or that is one
                            // (`ROW (a, b)` for `{ a, b }`), or `[i] = ...`:
                            // an array of that one element.
                            if e - a <= MAX_INIT_TEXT {
                                let text = format!("{{{}}}", String::from_utf8_lossy(&self.orig[a..e]));
                                let text = self.out.push_text(&text);
                                let line = row.start_position().row as u32 + 1;
                                self.out.inits.push(InitSite { type_name, text, line, scope, array: true });
                            }
                        } else if row.kind_id() == ids.initializer_list {
                            self.init_list(row, type_name, false, var, scope);
                        } else if row.kind_id() == ids.initializer_pair {
                            // `[i] = { ... }` or `[i] = f`.
                            match row.child_by_field_id(ids.f_value) {
                                Some(v) if v.kind_id() == ids.initializer_list => self.init_list(v, type_name, false, var, scope),
                                Some(v) if var != NONE => self.bind(v, var, NONE, true, scope, NONE),
                                _ => {}
                            }
                        } else if var != NONE {
                            // An array of function pointers.
                            self.bind(row, var, NONE, true, scope, NONE);
                        }
                    }
                }
            } else {
                self.init_list(value, type_name, array, var, scope);
            }
        }
    }

    /// Returns whether to descend into the node's children.
    fn visit(&mut self, node: Node<'t>) -> bool {
        self.flush_events(node.start_byte());
        self.leave_scope(node.start_byte());
        let k = node.kind_id();
        let ids = self.ids;

        if self.cpp {
            if k == ids.namespace_definition {
                // (What it spans is read from the text: `namespace_spans`.)
                if let Some(name) = node.child_by_field_id(ids.f_name).filter(|n| n.kind() == "namespace_identifier" && !n.byte_range().is_empty()) {
                    self.push_def(name, node, kind::NAMESPACE, 0);
                }
                return true;
            }
            if k == ids.friend_declaration {
                self.friend_end = node.end_byte();
                return true;
            }
        }

        if k == ids.identifier {
            let taken = self.skip.remove(&node.start_byte());
            if self.in_fn() && self.locals.contains(self.text(node)) {
                // (Its own declaration is not a use of it.)
                if !taken {
                    self.use_local(node);
                }
            } else if !taken {
                self.push_ref(node, refkind::USE);
            }
            return false;
        }
        if k == ids.type_identifier {
            // C++: the macro a definition is made with, which the parser takes
            // for its type (`TEST_F f(...) {`, the use masked), is not used
            // by what it defines: what the macro's text calls (registering
            // a test) is not called from the body.
            let defining = self.cpp && self.defining_macros.contains(&node.start_byte());
            if !self.skip.remove(&node.start_byte()) && !defining {
                self.push_ref(node, refkind::USE);
            }
            return false;
        }
        if k == ids.field_identifier {
            let hint = self.pending_hints.remove(&node.start_byte());
            if !self.skip.remove(&node.start_byte()) {
                self.push_ref(node, refkind::MEMBER);
                if let Some(h) = hint.filter(|&h| h != NONE) {
                    self.out.ref_hints.push((self.out.refs.len() as u32 - 1, h));
                }
            }
            return false;
        }
        if k == ids.field_expression {
            // Whose member it is, for when the walk reaches the name.
            if let Some(member) = node.child_by_field_id(ids.f_field) {
                if !self.skip.contains(&member.start_byte()) {
                    let hint = self.member_hint(node);
                    self.pending_hints.insert(member.start_byte(), hint);
                }
            }
            return true;
        }
        if k == ids.call_expression {
            match node.child_by_field_id(ids.f_function) {
                Some(f) if f.kind_id() == ids.identifier => {
                    self.skip.insert(f.start_byte());
                    if self.in_fn() && self.locals.contains(self.text(f)) {
                        if self.cpp && (self.local_types.contains_key(self.text(f)) || self.local_roots.contains_key(self.text(f))) {
                            // A local object called: its `operator()`.
                            self.push_operator(b"()", f, None, node);
                        }
                        self.out.indirect_calls += 1;
                        self.local_calls.push((self.text(f), f.start_position().row as u32 + 1));
                    } else {
                        self.push_ref(f, refkind::CALL);
                        if self.cpp {
                            self.note_args(node.child_by_field_id(ids.f_arguments));
                        }
                        // A macro's arguments it does not use: not compiled.
                        let discards = self.sets.discards(self.text(f));
                        let pasted = self.sets.pasted(self.text(f));
                        let asked = self.intern(self.text(f));
                        self.out.discards_asked.push((asked, discards, pasted));
                        if discards | pasted != 0 {
                            if let Some(args) = node.child_by_field_id(ids.f_arguments) {
                                let mut c = args.walk();
                                let list: Vec<Node<'t>> = args.named_children(&mut c).filter(|a| !a.is_extra()).collect();
                                for (i, a) in list.into_iter().enumerate().filter(|(i, _)| *i < 64) {
                                    if discards & 1 << i != 0 {
                                        self.quiet.push((a.start_byte(), a.end_byte()));
                                    } else if pasted & 1 << i != 0 {
                                        // (Its first name is part of another.)
                                        let mut first = a;
                                        while let Some(c) = first.child(0) {
                                            first = c;
                                        }
                                        if first.kind_id() == ids.identifier {
                                            self.quiet.push((first.start_byte(), first.end_byte()));
                                        }
                                    }
                                }
                            }
                        }
                        // Names handed over: callbacks, if the callee calls them.
                        let callee = self.intern(self.text(f));
                        if let Some(args) = node.child_by_field_id(ids.f_arguments) {
                            let mut c = args.walk();
                            let list: Vec<Node<'t>> = args.named_children(&mut c).filter(|a| !a.is_extra()).collect();
                            for (i, a) in list.into_iter().enumerate() {
                                let a = self.strip(a);
                                if a.kind_id() == ids.identifier && !(self.in_fn() && self.locals.contains(self.text(a))) {
                                    let func = self.intern(self.text(a));
                                    self.out.arg_passes.push(ArgPass { callee, index: i as u32, func });
                                }
                            }
                        }
                    }
                }
                Some(f) if self.cpp && (f.kind_id() == ids.qualified_identifier || f.kind_id() == ids.template_function) => {
                    // `X::f (...)`, `f<T> (...)`: the name, and what it is
                    // qualified with.
                    let qualified = f.kind_id() == ids.qualified_identifier;
                    let (parts, name) = self.qualified_parts(f);
                    if name.kind_id() == ids.identifier {
                        self.skip.insert(name.start_byte());
                        self.push_ref(name, refkind::CALL);
                        self.note_args(node.child_by_field_id(ids.f_arguments));
                        if let Some(q) = parts.last().filter(|_| qualified) {
                            let (q, at) = (self.intern(q), self.out.refs.len() as u32 - 1);
                            self.out.cxx_mut().ref_quals.push((at, q));
                        }
                    }
                }
                Some(f) => {
                    self.out.indirect_calls += 1;
                    let target = self.strip(f);
                    let tk = target.kind_id();
                    if tk == ids.field_expression {
                        // `p->member (...)`: through whatever is stored there
                        // (in C++, mostly a method).
                        if let Some(field) = target.child_by_field_id(ids.f_field) {
                            let (parts, member) = if self.cpp { self.qualified_parts(field) } else { (Vec::new(), field) };
                            if member.kind_id() == ids.field_identifier || member.kind_id() == ids.identifier {
                                self.skip.insert(member.start_byte());
                                self.push_ref(member, refkind::ICALL);
                                if self.cpp {
                                    self.note_args(node.child_by_field_id(ids.f_arguments));
                                }
                                let at = self.out.refs.len() as u32 - 1;
                                let hint = self.member_hint(target);
                                if hint != NONE {
                                    self.out.ref_hints.push((at, hint));
                                }
                                // `p->Base::f ()`.
                                if let Some(q) = parts.last().filter(|q| !q.is_empty()) {
                                    let q = self.intern(q);
                                    self.out.cxx_mut().ref_quals.push((at, q));
                                }
                            }
                        }
                    } else if tk == ids.identifier && !(self.in_fn() && self.locals.contains(self.text(target))) {
                        // `(*hook) (...)` through a variable that is not local.
                        self.skip.insert(target.start_byte());
                        self.push_ref(target, refkind::CALL);
                    } else if tk == ids.identifier {
                        self.local_calls.push((self.text(target), target.start_position().row as u32 + 1));
                    } else if self.cpp && tk == ids.call_expression {
                        // `make (m) (x)`: the `operator()` of what `make` returns.
                        self.push_operator(b"()", target, None, node);
                    }
                }
                None => self.out.indirect_calls += 1,
            }
            return true;
        }
        if k == ids.assignment_expression {
            if self.cpp && self.in_fn() {
                if let (Some(l), Some(op), Some(r)) =
                    (node.child_by_field_id(ids.f_left), node.child_by_field_id(ids.f_operator), node.child_by_field_id(ids.f_right))
                {
                    let op = self.text(op);
                    if ASSIGNMENT_OPERATORS.contains(&op) {
                        self.push_operator(op, l, Some(r), node);
                    }
                }
            }
            let plain = node.child_by_field_id(ids.f_operator).is_some_and(|o| self.text(o) == b"=");
            if let (true, Some(left), Some(right)) = (plain, node.child_by_field_id(ids.f_left), node.child_by_field_id(ids.f_right)) {
                let left = self.strip(left);
                let scope = self.scope.map_or(0, |s| s.def);
                if left.kind_id() == ids.field_expression {
                    if let Some(member) = left.child_by_field_id(ids.f_field) {
                        let slot = self.intern(self.text(member));
                        let hint = self.member_hint(left);
                        self.bind(right, slot, NONE, false, scope, hint);
                    }
                } else if left.kind_id() == ids.identifier && !(self.in_fn() && self.locals.contains(self.text(left))) {
                    let slot = self.intern(self.text(left));
                    self.bind(right, slot, NONE, true, scope, NONE);
                } else if left.kind_id() == ids.identifier {
                    self.local_source(self.text(left), right);
                }
            }
            return true;
        }
        if k == ids.field_declaration {
            let typedef_name = node.child_by_field_id(ids.f_type).filter(|t| t.kind_id() == ids.type_identifier);
            while self.open_records.last().is_some_and(|r| node.start_byte() >= r.0) {
                self.open_records.pop();
            }
            for d in self.declarators(node) {
                if let Some((name, is_fn)) = self.decl_name(d) {
                    let def = if self.cpp && is_fn {
                        // A method, declared.
                        let class = self.class_at(node.start_byte());
                        let mut flags = flag::DECL | if self.is_virtual(node) { flag::VIRTUAL } else { 0 };
                        if self.storage_flags(node) & flag::STATIC != 0 {
                            flags |= flag::CLASS_STATIC;
                        }
                        let def = self.push_def(name, node, kind::FUNCTION, flags);
                        let scope = self.declared_scope(d);
                        self.note_owner(def, &scope, class, node.start_byte());
                        if self.text(name).starts_with(b"operator") {
                            self.note_operator_params(def, Some(d));
                        }
                        def
                    } else if self.declares_function(d) {
                        self.push_def(name, node, kind::FIELD, flag::CALLABLE)
                    } else {
                        let def = self.push_def(name, node, kind::FIELD, 0);
                        if let Some(t) = typedef_name {
                            let t = self.intern(self.text(t));
                            self.out.field_types.push((def, t));
                        }
                        def
                    };
                    // Its place in the struct being read.
                    if let Some(&(_, r)) = self.open_records.last() {
                        let id = self.out.defs[def as usize].name;
                        let record = &mut self.out.records[r];
                        if let Some(at) = (0..record.fields.len()).find(|&i| record.fields[i] == id && record.defs[i] == NONE) {
                            record.defs[at] = def;
                        }
                    }
                }
            }
            return true;
        }
        if k == ids.function_definition {
            let named = node.child_by_field_id(ids.f_declarator.get()).and_then(|d| self.decl_name(d));
            if let Some((name, _)) = named {
                if self.in_fn() {
                    // C has no nested functions: `FOR_EACH (x) { ... }` inside a
                    // body is a macro followed by a block.
                    self.skip.insert(name.start_byte());
                    self.push_ref(name, refkind::CALL);
                    return true;
                }
                let mut flags = self.storage_flags(node);
                let (class, scope) = if self.cpp {
                    let class = self.class_at(node.start_byte());
                    let scope = node.child_by_field_id(ids.f_declarator.get()).map(|d| self.declared_scope(d)).unwrap_or_default();
                    (class, scope)
                } else {
                    (None, Vec::new())
                };
                if class.is_some() || !scope.is_empty() {
                    // A member: `static` says nothing of where it is seen.
                    if flags & flag::STATIC != 0 {
                        flags = flags & !flag::STATIC | flag::CLASS_STATIC;
                    }
                    if self.is_virtual(node) {
                        flags |= flag::VIRTUAL;
                    }
                } else if self.anonymous(node.start_byte()) {
                    flags |= flag::STATIC;
                }
                if self.cpp {
                    // `= default`, `= delete`: no body written, nothing to go to.
                    let mut c = node.walk();
                    if node.children(&mut c).any(|ch| matches!(ch.kind(), "default_method_clause" | "delete_method_clause")) {
                        flags |= flag::DECL;
                    }
                }
                let def = self.push_def(name, node, kind::FUNCTION, flags);
                self.method_class = NONE;
                if self.cpp {
                    if self.text(name).starts_with(b"operator") {
                        self.note_operator_params(def, node.child_by_field_id(ids.f_declarator.get()));
                    }
                    if let Some((test, method, base)) = self.test_case(name, node) {
                        // A test of a framework the corpus does not define
                        // the macros of: named as they name it.
                        self.out.defs[def as usize].name = self.intern_str(method);
                        let own = self.note_owner(def, &[test.as_bytes()], None, node.start_byte());
                        self.method_class = own;
                        if let Some(base) = base {
                            let base = self.intern_str(&base);
                            self.out.cxx_mut().bases.push((own, base));
                        }
                        if method == "test_method" {
                            // Boost.Test runs the case from a function the
                            // macro makes of it, `name_invoker`.
                            let line = self.out.defs[def as usize].line;
                            let name = self.intern_str(&format!("{test}_invoker"));
                            self.out.defs.push(Def { name, line, end_line: line, kind: kind::FUNCTION, flags: flag::STATIC });
                            let invoker = self.out.defs.len() as u32 - 1;
                            self.note_owner(invoker, &[], None, node.start_byte());
                            let called = self.intern_str(method);
                            self.out.refs.push(Ref { name: called, line, scope: invoker, kind: refkind::CALL });
                            let r = self.out.refs.len() as u32 - 1;
                            let test = self.intern_str(&test);
                            self.out.cxx_mut().ref_quals.push((r, test));
                        }
                    } else {
                        self.method_class = self.note_owner(def, &scope, class, node.start_byte());
                        if class.is_some() {
                            self.place_in_record(def, node.start_byte());
                        }
                    }
                }
                let returns = self.type_name_of(node);
                if returns != NONE {
                    self.out.typed.push((def, returns));
                }
                // (Not of `T* f ()`, `T& f ()` nor `std::unique_ptr<T> f ()`.)
                let by_value = node.child_by_field_id(ids.f_declarator.get()).is_some_and(|d| d.kind_id() == ids.function_declarator);
                self.returns_object = match node.child_by_field_id(ids.f_type) {
                    Some(t) if self.cpp && by_value => self.constructed_class(t),
                    _ => NONE,
                };
                self.locals.clear();
                self.strings.clear();
                self.local_ids.clear();
                if let Some(d) = node.child_by_field_id(ids.f_declarator.get()) {
                    self.note_params(d);
                }
                // Error recovery can end the node early; the body's matching
                // brace is where the function really ends.
                let mut end = node.end_byte();
                if let Some(body) = node.child_by_field_id(ids.f_body) {
                    let at = self.blocks.partition_point(|b| (b.0 as usize) < body.start_byte());
                    if let Some(&(start, block_end, last_line)) = self.blocks.get(at) {
                        if start as usize == body.start_byte() && block_end as usize > end {
                            end = block_end as usize;
                            self.out.defs[def as usize].end_line = last_line;
                        }
                    }
                }
                self.scope = Some(Scope { def, end, is_fn: true });
            }
            return true;
        }
        if k == ids.declaration {
            if self.in_fn() {
                let declared = self.type_name_of(node);
                for d in self.declarators(node) {
                    if let Some(names) = self.binding_names(d).filter(|_| self.cpp) {
                        // `auto [a, b] = expr;`: the members of what it is, in order.
                        let value = d.child_by_field_id(ids.f_value).filter(|_| d.kind_id() == ids.init_declarator);
                        let whole = value.and_then(|v| self.value_root(v));
                        for (i, n) in names.into_iter().enumerate() {
                            self.skip.insert(n.start_byte());
                            let text = self.text(n);
                            self.local_types.remove(text);
                            match self.binding_root(whole.clone(), i) {
                                Some(root) => self.local_roots.insert(text, root),
                                None => self.local_roots.remove(text),
                            };
                            self.note_local(n, false);
                        }
                        continue;
                    }
                    if let Some((name, _)) = self.decl_name(d) {
                        self.skip.insert(name.start_byte());
                        self.local_roots.remove(self.text(name));
                        self.local_elems.remove(self.text(name));
                        self.local_keys.remove(self.text(name));
                        let object = self.cpp && self.object_not_function(d);
                        let declares_function = !object && self.declares_function(d);
                        if self.cpp && !declares_function {
                            if let Some(ty) = node.child_by_field_id(ids.f_type) {
                                let (e, key) = (self.element_type(ty), self.key_type(ty));
                                if e != NONE {
                                    self.local_elems.insert(self.text(name), e);
                                }
                                if key != NONE {
                                    self.local_keys.insert(self.text(name), key);
                                }
                            }
                        }
                        if declared != NONE && !declares_function {
                            self.local_types.insert(self.text(name), declared);
                        } else {
                            self.local_types.remove(self.text(name));
                            // C++: `auto x = ...`: of what it is made from.
                            let value = d.child_by_field_id(ids.f_value).filter(|_| self.cpp && d.kind_id() == ids.init_declarator);
                            if let Some(root) = value.and_then(|v| self.value_root(v)) {
                                self.local_roots.insert(self.text(name), root);
                            }
                        }
                        self.note_local(name, false);
                        if self.cpp && !declares_function && self.is_string(node.child_by_field_id(ids.f_type), Some(d)) {
                            self.strings.insert(self.text(name));
                        } else {
                            self.strings.remove(self.text(name));
                        }
                        if self.cpp && !declares_function {
                            // `T x;`, `T x (a);`, `T x {a};` (not `T x = y;`,
                            // a copy, nor a pointer or a reference) make a T.
                            let (inner, value) = if d.kind_id() == ids.init_declarator {
                                (d.child_by_field_id(ids.f_declarator.get()), d.child_by_field_id(ids.f_value))
                            } else if object {
                                (d.child_by_field_id(ids.f_declarator.get()), d.child_by_field_id(ids.f_parameters))
                            } else {
                                (Some(d), None)
                            };
                            let plain = inner.is_some_and(|i| i.kind_id() == ids.identifier);
                            let made_one = plain || inner.is_some_and(|i| i.kind_id() == ids.array_declarator);
                            let made = object || value.is_none_or(|v| matches!(v.kind(), "argument_list" | "initializer_list"));
                            let class = match node.child_by_field_id(ids.f_type) {
                                Some(t) if made_one && made => self.constructed_class(t),
                                _ => NONE,
                            };
                            if class != NONE {
                                // (`T x;`: none; `T x {a}` may take a list.)
                                let args = if value.is_none() && plain { Some(0) } else { self.count_args(value) };
                                self.push_construction(class, d, NONE, args);
                                self.note_arg_types(value);
                            }
                        }
                        if let Some(value) = d.child_by_field_id(ids.f_value).filter(|_| d.kind_id() == ids.init_declarator) {
                            self.local_source(self.text(name), value);
                        }
                    }
                }
                self.initializers(node, self.scope.map_or(0, |s| s.def));
            } else {
                let storage = self.storage_flags(node);
                let declared = self.type_name_of(node);
                let mut first = None;
                for d in self.declarators(node) {
                    if let Some((name, is_fn)) = self.decl_name(d) {
                        let (kind, mut flags) =
                            if is_fn { (kind::FUNCTION, storage | flag::DECL) } else { (kind::GLOBAL, storage) };
                        // C++: a member of the class being read (a
                        // constructor, say), or of the one it is qualified
                        // with (`int Cache::hits = 0;`).
                        let (class, scope) = if self.cpp { (self.class_at(node.start_byte()), self.declared_scope(d)) } else { (None, Vec::new()) };
                        let member = class.is_some() || !scope.is_empty();
                        if member {
                            if flags & flag::STATIC != 0 {
                                flags = flags & !flag::STATIC | flag::CLASS_STATIC;
                            }
                            if is_fn && self.is_virtual(node) {
                                flags |= flag::VIRTUAL;
                            }
                        } else if self.cpp && self.anonymous(node.start_byte()) {
                            flags |= flag::STATIC;
                        }
                        let def = self.push_def(name, node, kind, flags);
                        if self.cpp && is_fn && self.text(name).starts_with(b"operator") {
                            self.note_operator_params(def, Some(d));
                        }
                        if self.cpp {
                            // (Of a function in a namespace, the namespace.)
                            self.note_owner(def, &scope, class, node.start_byte());
                            if class.is_some() {
                                self.place_in_record(def, node.start_byte());
                            }
                        }
                        // A function's return type, or a variable's own
                        // (a pointer to a function has neither to offer).
                        if declared != NONE && (is_fn || !self.declares_function(d)) {
                            self.out.typed.push((def, declared));
                        }
                        first.get_or_insert(def);
                    }
                }
                if let Some(def) = first {
                    self.scope = Some(Scope { def, end: node.end_byte(), is_fn: false });
                    self.initializers(node, def);
                }
            }
            return true;
        }
        if k == ids.parameter_declaration {
            let named = node.child_by_field_id(ids.f_declarator.get()).and_then(|d| self.decl_name(d));
            if let Some((name, _)) = named {
                self.skip.insert(name.start_byte());
                if self.in_fn() {
                    if self.cpp {
                        if let Some(ty) = node.child_by_field_id(ids.f_type) {
                            let (e, key) = (self.element_type(ty), self.key_type(ty));
                            if e != NONE {
                                self.local_elems.insert(self.text(name), e);
                            }
                            if key != NONE {
                                self.local_keys.insert(self.text(name), key);
                            }
                        }
                    }
                    // (A lambda's: its function's are typed already.)
                    if self.cpp && !self.local_types.contains_key(self.text(name)) {
                        let t = self.type_name_of(node);
                        if t != NONE {
                            self.local_types.insert(self.text(name), t);
                        }
                    }
                    self.note_local(name, true);
                    if self.cpp && self.is_string(node.child_by_field_id(ids.f_type), node.child_by_field_id(ids.f_declarator.get())) {
                        self.strings.insert(self.text(name));
                    }
                }
            }
            return true;
        }
        if self.cpp && self.in_fn() {
            match node.kind() {
                "return_statement" if self.returns_object != NONE => {
                    // `return s;` of a function returning a `Status`: one
                    // made from `s`, by the class's copy or move constructor.
                    // (Not in a lambda, which returns its own.)
                    let named = node.named_child(0).is_some_and(|v| v.kind_id() == ids.identifier || v.kind_id() == ids.field_expression);
                    let mut up = node.parent();
                    while let Some(p) = up.filter(|p| p.kind_id() != ids.function_definition) {
                        if p.kind() == "lambda_expression" {
                            break;
                        }
                        up = p.parent();
                    }
                    if named && up.is_some_and(|p| p.kind_id() == ids.function_definition) {
                        self.push_construction(self.returns_object, node, NONE, Some(1));
                        let r = self.out.refs.len() as u32 - 1;
                        self.out.cxx_mut().copies.push(r);
                    }
                }
                "binary_expression" => {
                    if let (Some(l), Some(op), Some(r)) =
                        (node.child_by_field_id(ids.f_left), node.child_by_field_id(ids.f_operator), node.child_by_field_id(ids.f_right))
                    {
                        let op = self.text(op);
                        if BINARY_OPERATORS.contains(&op) {
                            self.push_operator(op, l, Some(r), node);
                        }
                    }
                }
                "unary_expression" | "pointer_expression" => {
                    if let (Some(op), Some(arg)) = (node.child_by_field_id(ids.f_operator), node.child_by_field_id(ids.f_argument)) {
                        let op = self.text(op);
                        if matches!(op, b"!" | b"~" | b"-" | b"+" | b"*") {
                            self.push_operator(op, arg, None, node);
                        }
                    }
                }
                "update_expression" => {
                    if let (Some(op), Some(arg)) = (node.child_by_field_id(ids.f_operator), node.child_by_field_id(ids.f_argument)) {
                        let op = self.text(op);
                        self.push_operator(op, arg, None, node);
                    }
                }
                "subscript_expression" => {
                    if let Some(arg) = node.child_by_field_id(ids.f_argument) {
                        self.push_operator(b"[]", arg, None, node);
                    }
                }
                "user_defined_literal" => {
                    // `"ff"_hex`: `operator""_hex`.
                    let mut c = node.walk();
                    let suffix = node.named_children(&mut c).find(|n| n.kind() == "literal_suffix");
                    if let Some(suffix) = suffix {
                        let name = self.intern_str(&format!("operator\"\"{}", String::from_utf8_lossy(self.text(suffix))));
                        let scope = self.scope.map_or(0, |s| s.def);
                        self.out.refs.push(Ref { name, line: node.start_position().row as u32 + 1, scope, kind: refkind::CALL });
                    }
                }
                _ => {}
            }
        }
        if self.cpp && self.in_fn() && matches!(node.kind(), "new_expression" | "compound_literal_expression") {
            // `new T (a)`, `T {a}`: a T made.
            if let Some(t) = node.child_by_field_id(ids.f_type) {
                let class = self.constructed_class(t);
                if class != NONE {
                    let list = node.child_by_field_id(ids.f_arguments);
                    let args = if list.is_none() && node.kind() == "new_expression" { Some(0) } else { self.count_args(list) };
                    self.push_construction(class, node, NONE, args);
                    self.note_arg_types(list);
                }
            }
            return true;
        }
        if self.cpp && node.kind() == "field_initializer" {
            // `: Base (a)`, `: member_ (a)`: a base, or a member of the
            // class, made (which, the link tells).
            if let Some(first) = node.named_child(0) {
                let (_, name) = self.qualified_parts(first);
                if matches!(name.kind(), "field_identifier" | "type_identifier" | "identifier") {
                    let class = self.intern(self.text(name));
                    let hint = if self.method_class != NONE { self.push_hint(root::TYPE, self.method_class, &[]) } else { NONE };
                    let mut c = node.walk();
                    let args = node.named_children(&mut c).find(|n| n.kind() == "argument_list");
                    self.push_construction(class, node, hint, self.count_args(args));
                    self.note_arg_types(args);
                }
            }
            return true;
        }
        if self.cpp && self.in_fn() && node.kind() == "for_range_loop" {
            // `for (const auto& tx : block.vtx)`: an element of the container
            // (of a type written, that type). (Not `auto& [k, v]`.)
            let binding = node.child_by_field_id(ids.f_declarator.get()).and_then(|d| self.binding_names(d));
            if let (Some(names), Some(right)) = (binding, node.child_by_field_id(ids.f_right)) {
                // `for (auto& [k, v] : m)`: of a map, its key and its value;
                // of another container, the members of its element, in order.
                let local = Some(self.strip(right)).filter(|r| r.kind_id() == ids.identifier && self.locals.contains(self.text(*r))).map(|r| self.text(r));
                let element = match local {
                    Some(l) => self.local_elems.get(l).map(|&e| (root::TYPE, e, Vec::new())),
                    None => self.element_of(right),
                };
                let key = local.and_then(|l| self.local_keys.get(l).copied());
                for (i, n) in names.into_iter().enumerate() {
                    self.skip.insert(n.start_byte());
                    let text = self.text(n);
                    self.local_types.remove(text);
                    let root = match (key, element.clone()) {
                        (Some(k), _) if i == 0 => Some((root::TYPE, k, Vec::new())),
                        (Some(_), e) if i == 1 => e,
                        (_, e) => self.binding_root(e, i),
                    };
                    match root {
                        Some(root) => self.local_roots.insert(text, root),
                        None => self.local_roots.remove(text),
                    };
                    self.note_local(n, false);
                }
                return true;
            }
            let declared = node.child_by_field_id(ids.f_declarator.get()).and_then(|d| self.decl_name(d));
            if let (Some((name, _)), Some(right)) = (declared, node.child_by_field_id(ids.f_right)) {
                let text = self.text(name);
                self.skip.insert(name.start_byte());
                self.local_roots.remove(text);
                let t = self.type_name_of(node);
                if t != NONE {
                    self.local_types.insert(text, t);
                } else {
                    self.local_types.remove(text);
                    if let Some(root) = self.element_of(right) {
                        self.local_roots.insert(text, root);
                    }
                }
                self.note_local(name, false);
            }
            return true;
        }
        if self.cpp && node.kind() == "alias_declaration" {
            // `using Base = prevector<36, unsigned char>;`: another name.
            if let (Some(name), Some(t)) = (node.child_by_field_id(ids.f_name), node.child_by_field_id(ids.f_type).and_then(|d| d.child_by_field_id(ids.f_type))) {
                self.push_def(name, node, kind::TYPEDEF, 0);
                let (alias, tag) = (self.intern(self.text(name)), self.type_name(t));
                if tag != NONE && tag != alias {
                    self.out.aliases.push((alias, tag));
                }
            }
            return true;
        }
        if k == ids.type_definition {
            // (C++: `typedef prevector<36, unsigned char> CScriptBase;` names a
            // class as well.)
            let record = node.child_by_field_id(ids.f_type).filter(|t| {
                t.kind_id() == ids.struct_specifier
                    || t.kind_id() == ids.union_specifier
                    || (self.cpp && (t.kind_id() == ids.template_type || t.kind_id() == ids.type_identifier || t.kind_id() == ids.qualified_identifier))
            });
            for d in self.declarators(node) {
                if let Some((name, _)) = self.decl_name(d) {
                    let callable = if self.declares_function(d) { flag::CALLABLE } else { 0 };
                    self.push_def(name, node, kind::TYPEDEF, callable);
                    // `typedef struct tag name;`, `typedef struct { ... } name;`
                    // and pointers to them: the name stands for the struct.
                    let Some(r) = record.filter(|_| !self.declares_function(d)) else { continue };
                    let alias = self.intern(self.text(name));
                    let tag = self.type_name(r);
                    if tag != NONE {
                        self.out.aliases.push((alias, tag));
                    }
                }
            }
            return true;
        }
        let is_class = self.cpp && k == ids.class_specifier;
        if k == ids.struct_specifier || k == ids.union_specifier || k == ids.enum_specifier || is_class {
            if node.child_by_field_id(ids.f_body).is_some() {
                // (`class Foo<int> { ... }`, a specialization: `Foo`;
                // `struct List<T>::Node { ... }`, a member defined apart:
                // `Node`, in `List`.)
                let mut scope = Vec::new();
                let named = node
                    .child_by_field_id(ids.f_name)
                    .map(|n| {
                        let n = if self.cpp && n.kind_id() == ids.qualified_identifier {
                            let (parts, name) = self.qualified_parts(n);
                            scope = parts;
                            name
                        } else {
                            n
                        };
                        if self.cpp && n.kind_id() == ids.template_type { n.child_by_field_id(ids.f_name).unwrap_or(n) } else { n }
                    })
                    .filter(|n| !n.byte_range().is_empty());
                if let Some(name) = named {
                    let kind = if is_class {
                        kind::CLASS
                    } else if k == ids.struct_specifier {
                        kind::STRUCT
                    } else if k == ids.union_specifier {
                        kind::UNION
                    } else {
                        kind::ENUM
                    };
                    self.push_def(name, node, kind, 0);
                    if k != ids.enum_specifier {
                        let record = self.intern(self.text(name));
                        let body = node.child_by_field_id(ids.f_body).unwrap();
                        let outer = if self.cpp { self.class_at(node.start_byte()) } else { None };
                        // (A class in a class is meant by its short name only
                        // inside the outer one.)
                        let nested = outer.is_some() || scope.iter().any(|p| !p.is_empty());
                        self.push_record(record, node, body);
                        if nested {
                            let r = self.out.records.len() as u32 - 1;
                            self.out.cxx_mut().nested.push(r);
                        }
                        if self.cpp {
                            // What its members are members of, and what it
                            // derives from.
                            let mut chain = outer.map(|c| self.classes[c].chain.clone()).unwrap_or_default();
                            for part in scope.iter().filter(|p| !p.is_empty() && **p != b"?").chain(std::iter::once(&self.text(name))) {
                                if !chain.is_empty() {
                                    chain.push_str("::");
                                }
                                chain.push_str(&String::from_utf8_lossy(part));
                            }
                            let mut c = node.walk();
                            let clause: Vec<Node<'t>> = node.children(&mut c).filter(|ch| ch.kind_id() == ids.base_class_clause).collect();
                            // (A class template's own parameters, which a base may be.)
                            let params: Vec<&[u8]> = node
                                .parent()
                                .filter(|p| p.kind() == "template_declaration")
                                .and_then(|p| p.child_by_field_id(ids.f_parameters))
                                .map(|list| {
                                    let mut c = list.walk();
                                    list.named_children(&mut c)
                                        .filter(|p| p.kind() == "type_parameter_declaration")
                                        .filter_map(|p| {
                                            let mut c = p.walk();
                                            let name = p.named_children(&mut c).find(|n| n.kind() == "type_identifier");
                                            name.map(|n| self.text(n))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            for clause in clause {
                                let mut c = clause.walk();
                                let named: Vec<Node<'t>> = clause.named_children(&mut c).collect();
                                for b in named {
                                    if params.contains(&self.text(b)) && !self.out.cxx().param_bases.contains(&record) {
                                        self.out.cxx_mut().param_bases.push(record);
                                    }
                                    let base = self.type_name(b);
                                    if base != NONE {
                                        self.out.cxx_mut().bases.push((record, base));
                                    }
                                }
                            }
                            // Where the body's brace is closed: error recovery
                            // can make the node end elsewhere (past what a
                            // macro hid, to the end of the file).
                            let at = self.blocks.partition_point(|b| (b.0 as usize) < body.start_byte());
                            let end = match self.blocks.get(at) {
                                Some(&(start, block_end, _)) if start as usize == body.start_byte() => block_end as usize,
                                _ => body.end_byte(),
                            };
                            self.classes.push(OpenClass { end, chain });
                        }
                    }
                } else if k != ids.enum_specifier {
                    let record = self.unnamed(node);
                    self.push_record(record, node, node.child_by_field_id(ids.f_body).unwrap());
                }
            }
            return true;
        }
        if k == ids.enumerator {
            if let Some(name) = node.child_by_field_id(ids.f_name).filter(|n| !n.byte_range().is_empty()) {
                self.push_def(name, node, kind::ENUM_CONST, 0);
            }
            return true;
        }
        true
    }
}

/// The lines of `src` that the byte ranges `skipped` cover whole: from the
/// line after the directive a range starts in, to the line before the one
/// it ends at (the directive that closes the branch).
pub(crate) fn inactive_lines(src: &[u8], skipped: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let (mut line, mut at) = (1u32, 0usize);
    let mut line_of = |pos: usize| {
        // The ranges come in order: the count goes on from the last one.
        line += src[at..pos].iter().filter(|&&b| b == b'\n').count() as u32;
        at = pos;
        line
    };
    for &(from, to) in skipped {
        let (from, to) = (from as usize, (to as usize).min(src.len()));
        // A range starts in the opening directive's line, at its newline, or
        // just past it.
        let starts_line = from == 0 || src[from - 1] == b'\n';
        let first = line_of(from) + u32::from(!starts_line);
        let ends_file = to == src.len();
        let last = line_of(to) - u32::from(!ends_file);
        if first <= last {
            out.push((first, last));
        }
    }
    out
}

/// The same for the branches that may or may not be compiled, each with
/// the line of the directive that opens it; in order of where they start.
pub(crate) fn undecided_lines(src: &[u8], undecided: &[(u32, u32, u32, u64)]) -> Vec<(u32, u32, u32, u64)> {
    let breaks: Vec<usize> = src.iter().enumerate().filter(|(_, &b)| b == b'\n').map(|(i, _)| i).collect();
    // From 1: the number of line breaks before `pos`, and one.
    let line_of = |pos: usize| breaks.partition_point(|&b| b < pos) as u32 + 1;
    let mut out: Vec<(u32, u32, u32, u64)> = undecided
        .iter()
        .filter_map(|&(from, to, directive, key)| {
            let (from, to) = (from as usize, (to as usize).min(src.len()));
            let starts_line = from == 0 || src[from - 1] == b'\n';
            let first = line_of(from) + u32::from(!starts_line);
            let last = line_of(to) - u32::from(to != src.len());
            (first <= last).then_some((first, last, line_of(directive as usize), key))
        })
        .collect();
    out.sort_unstable();
    out
}

/// C++: the namespaces of a file as its text opens them, `namespace a {`,
/// `namespace a::b {`, `inline namespace v1 {`, `namespace {` (named ""),
/// each closed where bracket matching closes its brace: (start, end, name),
/// in order of where they start. The parser's tree can lose one to error
/// recovery (in a header full of macros and `#if`s); the text does not.
/// The least size of a piece the parser reads alone (`NORN_CHUNK`, bytes;
/// 0, the default, reads every file whole). An experiment: on the Linux
/// kernel, pieces of 16 KiB neither parse faster nor lower the peak memory,
/// which the facts and the graph decide rather than the trees.
fn chunk_min() -> usize {
    static MIN: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| std::env::var("NORN_CHUNK").ok().and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Where the masked text can be cut into pieces the parser reads alone, one
/// after another: at the end of a line that ends a top-level declaration
/// (a `;` at depth 0) or a function body (a `}` back to depth 0 whose `{`
/// followed a `)`), outside every `#if` group. Pieces are joined until each
/// is at least `min` bytes; `min == 0` gives the whole text.
fn pieces(src: &[u8], min: usize) -> Vec<Range> {
    let n = src.len();
    let whole = |start: usize, row: usize| Range {
        start_byte: start,
        end_byte: n,
        start_point: Point { row, column: 0 },
        end_point: Point { row: usize::MAX, column: usize::MAX },
    };
    if min == 0 || n < 2 * min {
        return vec![whole(0, 0)];
    }
    let mut out = Vec::new();
    let (mut start, mut start_row) = (0, 0);
    let (mut depth, mut pp) = (0usize, 0usize);
    let (mut last, mut body, mut line_start) = (0u8, false, true);
    let mut i = 0;
    while i < n {
        let c = src[i];
        if line_start {
            let j = i + src[i..].iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
            if src.get(j) == Some(&b'#') {
                // A directive: to the end of its (continued) line.
                let mut e = j;
                while e < n && !(src[e] == b'\n' && src[e - 1] != b'\\') {
                    e += 1;
                }
                let word: Vec<u8> = src[j + 1..e].iter().copied().skip_while(|b| b.is_ascii_whitespace()).take_while(|b| b.is_ascii_alphabetic()).collect();
                match word.as_slice() {
                    b"if" | b"ifdef" | b"ifndef" => pp += 1,
                    b"endif" => pp = pp.saturating_sub(1),
                    _ => {}
                }
                i = e;
                continue;
            }
        }
        line_start = false;
        match c {
            b'\n' => line_start = true,
            b'/' if src.get(i + 1) == Some(&b'*') => {
                let e = src[i + 2..].windows(2).position(|w| w == b"*/").map_or(n, |p| i + 4 + p);
                i = e;
                continue;
            }
            b'/' if src.get(i + 1) == Some(&b'/') => {
                i += src[i..].iter().position(|&b| b == b'\n').unwrap_or(n - i);
                continue;
            }
            b'"' | b'\'' => {
                let mut j = i + 1;
                while j < n && src[j] != c && src[j] != b'\n' {
                    j += if src[j] == b'\\' { 2 } else { 1 };
                }
                i = (j + 1).min(n);
                last = c;
                continue;
            }
            b'(' | b'[' => depth += 1,
            b'{' => {
                if depth == 0 {
                    body = last == b')';
                }
                depth += 1;
            }
            b')' | b']' => depth = depth.saturating_sub(1),
            b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if !c.is_ascii_whitespace() {
            last = c;
        }
        let ends = depth == 0 && pp == 0 && (c == b';' || (c == b'}' && body));
        i += 1;
        if ends {
            // Only blanks or a line comment may follow on the line.
            let rest = src[i..].iter().position(|&b| b == b'\n').map_or(n, |p| i + p);
            let tail = src[i..rest].trim_ascii_start();
            if (tail.is_empty() || tail.starts_with(b"//")) && rest < n && rest + 1 - start >= min && n - (rest + 1) >= min {
                let row = start_row + src[start..=rest].iter().filter(|&&b| b == b'\n').count();
                out.push(Range {
                    start_byte: start,
                    end_byte: rest + 1,
                    start_point: Point { row: start_row, column: 0 },
                    end_point: Point { row, column: 0 },
                });
                start = rest + 1;
                start_row = row;
                i = rest + 1;
                line_start = true;
            }
        }
    }
    out.push(whole(start, start_row));
    out
}

fn namespace_spans(src: &[u8], blocks: &[(u32, u32, u32)]) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let n = src.len();
    let mut i = 0;
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let skip_space = |mut i: usize| {
        while i < n && src[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    while i < n {
        let b = src[i];
        if b == b'/' && src.get(i + 1) == Some(&b'/') {
            while i < n && src[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && src.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < n && !(src[i] == b'*' && src[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if b == b'"' || b == b'\'' {
            i += 1;
            while i < n && src[i] != b && src[i] != b'\n' {
                if src[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if !ident(b) {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && ident(src[i]) {
            i += 1;
        }
        if &src[start..i] != b"namespace" || (start > 0 && ident(src[start - 1])) {
            continue;
        }
        // The name, `a::b`, or none; then the brace.
        let mut j = skip_space(i);
        let name_start = j;
        while j < n && (ident(src[j]) || src[j] == b':') {
            j += 1;
        }
        let name: String = String::from_utf8_lossy(&src[name_start..j]).into_owned();
        let brace = skip_space(j);
        if src.get(brace) != Some(&b'{') || name.starts_with(':') || name.ends_with(':') {
            continue;
        }
        let at = blocks.partition_point(|b| (b.0 as usize) < brace);
        let end = match blocks.get(at) {
            Some(&(s, e, _)) if s as usize == brace => e as usize,
            _ => continue,
        };
        out.push((start, end, name));
        i = brace + 1;
    }
    out
}

/// C++: `namespace tfm = tinyformat;`, as (alias, the namespace's own name).
fn namespace_aliases(src: &[u8]) -> Vec<(String, String)> {
    const WORD: &[u8] = b"namespace";
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(at) = src[i..].windows(WORD.len()).position(|w| w == WORD) {
        let start = i + at;
        i = start + WORD.len();
        if (start > 0 && ident(src[start - 1])) || src.get(i).is_some_and(|&b| ident(b)) {
            continue;
        }
        let skip = |mut j: usize| {
            while j < src.len() && matches!(src[j], b' ' | b'\t') {
                j += 1;
            }
            j
        };
        let a = skip(i);
        let mut e = a;
        while e < src.len() && ident(src[e]) {
            e += 1;
        }
        let eq = skip(e);
        if e == a || src.get(eq) != Some(&b'=') {
            continue;
        }
        let t = skip(eq + 1);
        let mut te = t;
        while te < src.len() && (ident(src[te]) || src[te] == b':') {
            te += 1;
        }
        let target = String::from_utf8_lossy(&src[t..te]);
        let last = target.rsplit("::").next().unwrap_or("");
        if !last.is_empty() && src.get(skip(te)) == Some(&b';') {
            out.push((String::from_utf8_lossy(&src[a..e]).into_owned(), last.to_string()));
        }
    }
    out
}

/// C++: Boost.Test's suites, `BOOST_FIXTURE_TEST_SUITE (name, Fixture)` or
/// `BOOST_AUTO_TEST_SUITE (name)` to `BOOST_AUTO_TEST_SUITE_END ()`, as
/// (start, end, fixture), by where they start.
fn test_suites(src: &[u8]) -> Vec<(usize, usize, Option<String>)> {
    const OPEN: &[u8] = b"_TEST_SUITE";
    if !src.windows(OPEN.len()).any(|w| w == OPEN) {
        return Vec::new();
    }
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let (mut out, mut open) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < src.len() {
        if !ident(src[i]) || (i > 0 && ident(src[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < src.len() && ident(src[i]) {
            i += 1;
        }
        let word = &src[start..i];
        let args = || {
            let open = i + src[i..].iter().position(|b| !b.is_ascii_whitespace())?;
            (src.get(open) == Some(&b'(')).then_some(())?;
            let close = open + src[open..].iter().position(|&b| b == b')')?;
            Some(String::from_utf8_lossy(&src[open + 1..close]).into_owned())
        };
        match word {
            b"BOOST_FIXTURE_TEST_SUITE" | b"BOOST_AUTO_TEST_SUITE" => {
                if let Some(a) = args() {
                    let fixture = a.split(',').nth(1).map(|f| f.trim().rsplit("::").next().unwrap_or("").to_string()).filter(|f| !f.is_empty());
                    open.push((start, fixture));
                }
            }
            b"BOOST_AUTO_TEST_SUITE_END" => {
                if let Some((s, fixture)) = open.pop() {
                    out.push((s, start, fixture));
                }
            }
            _ => {}
        }
    }
    out.extend(open.into_iter().map(|(s, fixture)| (s, src.len(), fixture)));
    out.sort_unstable_by_key(|s| s.0);
    out
}

/// Where the parser sees the name of the definition a macro use makes.
fn site_def_at(s: &mask::SiteTok) -> usize {
    if s.def_at != 0 { s.def_at as usize } else { s.name.start as usize }
}

#[cfg(test)]
mod tests {
    use super::is_cpp;

    #[test]
    fn a_source_is_cpp_by_its_name() {
        assert!(is_cpp("a.cc", b"int x;"));
        assert!(is_cpp("a.cpp", b""));
        assert!(is_cpp("a.hpp", b""));
        assert!(!is_cpp("a.c", b"namespace x {"));
    }

    #[test]
    fn a_header_is_cpp_by_what_it_says() {
        assert!(is_cpp("a.h", b"namespace leveldb {\nclass DB;\n}"));
        assert!(is_cpp("a.h", b"template <typename T> T max(T a, T b);"));
        assert!(is_cpp("a.h", b"class Writer : public Base {\n public:\n  void f();\n};"));
        assert!(is_cpp("a.h", b"bool parse(const std::string& s);"));
        assert!(is_cpp("a.h", b"extern \"C++\" { int f(); }"));
        assert!(!is_cpp("a.h", b"struct point { int x, y; };\nint f(struct point *p);"));
    }

    #[test]
    fn what_a_c_header_says_only_for_cpp_does_not_make_it_cpp() {
        // nedmalloc.h: a namespace for C++ users.
        assert!(!is_cpp("a.h", b"#ifdef __cplusplus\nnamespace nedalloc {\n#endif\nvoid *nedmalloc(size_t);\n#ifdef __cplusplus\n}\n#endif\n"));
        assert!(!is_cpp("a.h", b"#if defined(__cplusplus)\nextern \"C\" {\nnamespace x {}\n#endif\nint f(void);\n"));
        // ... but the other branch counts, and so does a class for C++.
        assert!(is_cpp("a.h", b"#ifndef __cplusplus\nint f(void);\n#else\n#endif\nnamespace y {}\n"));
        assert!(is_cpp("a.h", b"#ifdef __cplusplus\nextern \"C\" {\n#endif\nint f(void);\n#ifdef __cplusplus\n}\nclass W {\n public:\n  int g();\n};\n#endif\n"));
    }

    #[test]
    fn c_words_that_are_cpp_keywords_are_not_cpp() {
        // The kernel's driver model, and a bit field called `public`.
        assert!(!is_cpp("a.h", b"struct class {\n\tconst char *name;\n};\nextern struct class *class_create(const char *name);\n"));
        assert!(!is_cpp("a.h", b"struct sym {\n  unsigned int public:      1;\n  unsigned int new: 1;\n};\n"));
        // An `asm` with `::` in it.
        assert!(!is_cpp("a.h", b"static inline void barrier(void) { asm volatile(\"\" ::: \"memory\"); }\n"));
    }

    #[test]
    fn code_in_a_comment_does_not_count() {
        // A comment opened on a directive's line goes on past it (hp-symtab.h).
        assert!(!is_cpp("a.h", b"#define X 1 /* for example:\n    class c {\n      template <class T> class q;\n */\nint f(void);\n"));
        assert!(!is_cpp("a.h", b"/* namespace n { */\n// class c { };\nint g;\n"));
    }

    #[test]
    fn a_file_for_including_is_judged_by_what_it_says() {
        // arch/arm/nwfpe/fpa11.inl is C.
        assert!(!is_cpp("fpa11.inl", b"static inline unsigned int readFPSR(void) { return 0; }\n"));
        assert!(is_cpp("impl.inl", b"template <class T> void List<T>::push(T x) {}\n"));
    }
}

#[cfg(test)]
mod namespace_tests {
    #[test]
    #[ignore]
    fn print_namespace_spans() {
        let path = std::env::var("SI_FILE").unwrap();
        let src = std::fs::read(&path).unwrap();
        let lexed = crate::mask::lex_file(&src, &crate::mask::MacroSets::default(), Some(&path));
        let line = |p: usize| src[..p.min(src.len())].iter().filter(|&&b| b == b'\n').count() + 1;
        for (a, e, name) in super::namespace_spans(&lexed.masked, &lexed.blocks) {
            println!("{name:?} {}..{}", line(a), line(e));
        }
        let opens = src.windows(10).enumerate().filter(|(_, w)| w.starts_with(b"namespace ")).map(|(i, _)| line(i)).collect::<Vec<_>>();
        println!("namespace words at {opens:?}");
    }
}
