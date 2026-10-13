//! Single-file extraction: lexical mask, tree-sitter parse, one preorder walk. O(tokens).

use crate::mask::{self, Event, Tok, MacroSets};
use crate::model::{
    flag, kind, refkind, root, site, ArgPass, Bind, Def, FileFacts, InitSite, MacroBody, MemberHint, ParamCall, Record,
    Ref, Site, NONE,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::num::NonZeroU16;
use tree_sitter::{Language, Node, Parser};

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
        }
    }
}

/// Longest initializer kept as text for expansion.
const MAX_INIT_TEXT: usize = 256 << 10;
/// An array initializer longer than this is expanded a row at a time.
const ROWS_APART: usize = 8 << 10;

/// Reusable per-thread extractor (a tree-sitter parser is not `Sync`).
pub struct Extractor {
    parser: Parser,
    ids: Ids,
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
    out: FileFacts,
    names: FxHashMap<&'a [u8], u32>,
    /// Start bytes of identifier nodes already consumed as a def name or call target.
    skip: FxHashSet<usize>,
    locals: FxHashSet<&'a [u8]>,
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
    /// Hints worked out at a member access, for the reference its member
    /// name is about to become (by the name's position).
    pending_hints: FxHashMap<usize, u32>,
    /// Struct bodies being read: (end, index in `out.records`).
    open_records: Vec<(usize, usize)>,
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
        Extractor { parser, ids: Ids::new(&lang) }
    }

    /// The facts depend on the file's content and on `transparent`.
    pub fn extract(&mut self, path: &str, src: &[u8], transparent: &MacroSets) -> FileFacts {
        let mut out = FileFacts::new(path.to_string());
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
        let t = std::time::Instant::now();
        let lexed = mask::lex_file(src, transparent, Some(path));
        for &(a, e, conditional) in &lexed.includes {
            let id = out.push_name(&String::from_utf8_lossy(&src[a as usize..e as usize]));
            out.includes.push(id);
            out.include_conditional.push(conditional);
        }
        out.mask_ns = t.elapsed().as_nanos() as u64;
        let t = std::time::Instant::now();
        let Some(tree) = self.parser.parse(&lexed.masked, None) else {
            out.has_error = true;
            return out;
        };
        out.parse_ns = t.elapsed().as_nanos() as u64;
        let t = std::time::Instant::now();
        let root = tree.root_node();
        out.has_error = root.has_error();
        out.defs[0].end_line = root.end_position().row as u32 + 1;

        let mut events = lexed.events.iter();
        let mut w = Walk {
            src: &lexed.masked,
            orig: src,
            next_event: events.next(),
            events,
            blocks: &lexed.blocks,
            ids: &self.ids,
            out,
            names: FxHashMap::default(),
            skip: FxHashSet::default(),
            locals: FxHashSet::default(),
            scope: None,
            site_defs: lexed
                .events
                .iter()
                .filter_map(|e| match e {
                    Event::Site(s) => Some((s.name.start as usize, NONE)),
                    _ => None,
                })
                .collect(),
            site_starts: Vec::new(),
            local_sources: FxHashMap::default(),
            local_calls: Vec::new(),
            params: FxHashMap::default(),
            local_types: FxHashMap::default(),
            pending_hints: FxHashMap::default(),
            open_records: Vec::new(),
        };
        w.site_starts = w.site_defs.keys().copied().collect();
        w.site_starts.sort_unstable();
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
        w.flush_events(usize::MAX);
        w.flush_local_calls();
        w.out.walk_ns = t.elapsed().as_nanos() as u64;
        let mut out = w.out;
        out.shrink();
        out
    }
}

impl<'a> Walk<'a> {
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

    fn push_def(&mut self, name: Node, span: Node, kind: u8, flags: u8) -> u32 {
        self.skip.insert(name.start_byte());
        if let Some(slot) = self.site_defs.get_mut(&name.start_byte()) {
            *slot = self.out.defs.len() as u32;
        }
        let name = self.intern(self.text(name));
        self.out.defs.push(Def {
            name,
            line: span.start_position().row as u32 + 1,
            end_line: span.end_position().row as u32 + 1,
            kind,
            flags,
        });
        (self.out.defs.len() - 1) as u32
    }

    fn leave_scope(&mut self, pos: usize) {
        if self.scope.is_some_and(|s| pos >= s.end) {
            self.flush_local_calls();
            self.scope = None;
            self.locals.clear();
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
    }

    /// The name of a declaration's type when it is a struct, a union or a
    /// typedef: what a member access on the declared thing is looked up in.
    fn type_name_of(&mut self, decl: Node<'a>) -> u32 {
        match decl.child_by_field_id(self.ids.f_type) {
            Some(t) => self.type_name(t),
            None => NONE,
        }
    }

    fn type_name(&mut self, t: Node<'a>) -> u32 {
        let ids = self.ids;
        let k = t.kind_id();
        if k == ids.type_identifier {
            self.intern(self.text(t))
        } else if k == ids.struct_specifier || k == ids.union_specifier {
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
    fn unnamed(&mut self, t: Node<'a>) -> u32 {
        let name = format!("@{}:{}", self.out.name(0), t.start_byte());
        self.out.push_name(&name)
    }

    /// What `object` in `object.member` is: its root (a type, a variable or
    /// a call) and the members followed from there, appended to `path`.
    fn object_root(&mut self, mut n: Node<'a>, path: &mut Vec<u32>) -> Option<(u8, u32)> {
        let ids = self.ids;
        for _ in 0..32 {
            let k = n.kind_id();
            if k == ids.parenthesized_expression {
                n = n.named_child(0)?;
            } else if k == ids.pointer_expression || k == ids.subscript_expression {
                n = n.child_by_field_id(ids.f_argument)?;
            } else {
                break;
            }
        }
        let k = n.kind_id();
        if k == ids.identifier {
            let text = self.text(n);
            if self.in_fn() && self.locals.contains(text) {
                return self.local_types.get(text).map(|&t| (root::TYPE, t));
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
            let f = n.child_by_field_id(ids.f_function).filter(|f| f.kind_id() == ids.identifier)?;
            return Some((root::FUNC, self.intern(self.text(f))));
        }
        if k == ids.cast_expression {
            let t = n.child_by_field_id(ids.f_type).and_then(|d| d.child_by_field_id(ids.f_type))?;
            let name = self.type_name(t);
            return (name != NONE).then_some((root::TYPE, name));
        }
        None
    }

    /// The hint for the member of a member access, or `NONE`.
    fn member_hint(&mut self, access: Node<'a>) -> u32 {
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
    fn note_params(&mut self, declarator: Node<'a>) {
        let ids = self.ids;
        // Down to the declarator that holds the parameter list of the
        // function itself: the one whose own declarator is the name.
        let mut n = declarator;
        for _ in 0..32 {
            let Some(inner) = n.child_by_field_id(ids.f_declarator.get()).or_else(|| n.named_child(0)) else { return };
            if n.kind_id() == ids.function_declarator && inner.kind_id() == ids.identifier {
                break;
            }
            n = inner;
        }
        let Some(list) = n.child_by_field_id(ids.f_parameters) else { return };
        let mut c = list.walk();
        let params: Vec<Node<'a>> = list.named_children(&mut c).filter(|p| p.kind_id() == ids.parameter_declaration).collect();
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
    fn local_source(&mut self, local: &'a [u8], value: Node<'a>) {
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
                    self.out.defs.push(Def { name, line: m.name.line, end_line: m.end_line, kind: kind::MACRO, flags: if m.conditional { flag::CONDITIONAL } else { 0 } });
                    let scope = (self.out.defs.len() - 1) as u32;
                    for t in &m.refs {
                        self.push_tok_ref(t, scope);
                    }
                    if m.keep {
                        let text = String::from_utf8_lossy(&self.orig[m.name.end as usize..m.end as usize]);
                        let text = self.out.push_name(&text);
                        let guard = if m.guard.is_empty() { NONE } else { self.out.push_name(&m.guard) };
                        self.out.macros.push(MacroBody { name, text, function_like: m.name.call, guard, conditional: m.conditional, fallback: m.fallback });
                    }
                }
                Event::Site(s) => {
                    let name = self.intern(self.tok_text(&s.name));
                    let args = if s.args_end == 0 {
                        NONE
                    } else {
                        let text = String::from_utf8_lossy(&self.orig[s.name.end as usize..s.args_end as usize]);
                        self.out.push_name(&text)
                    };
                    // Outside every bracket is outside every function, whatever
                    // the parser made of the lines before.
                    let top = s.flags & site::TOP != 0;
                    let flags = s.flags | if self.in_fn() && !top { site::IN_FN } else { 0 };
                    let def = self.site_defs[&(s.name.start as usize)];
                    let scope = if top && def == NONE { 0 } else { self.scope.map_or(0, |s| s.def) };
                    self.out.sites.push(Site { name, args, line: s.name.line, scope, def, flags });
                    let (open, close) = (s.init.0 as usize, s.init.1 as usize);
                    if close > open && close - open <= MAX_INIT_TEXT {
                        let type_name = self.intern(&self.orig[s.init_type.0 as usize..s.init_type.1 as usize]);
                        let text = String::from_utf8_lossy(&self.orig[open..close]);
                        let text = self.out.push_name(&text);
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
    fn decl_name(&self, mut n: Node<'a>) -> Option<(Node<'a>, bool)> {
        let mut is_fn = false;
        for _ in 0..32 {
            let k = n.kind_id();
            if k == self.ids.identifier || k == self.ids.type_identifier || k == self.ids.field_identifier {
                // A name the parser made up to get past an error (where a
                // macro stood, say) has no text, and names nothing.
                return (!n.byte_range().is_empty()).then_some((n, is_fn));
            }
            let inner = n.child_by_field_id(self.ids.f_declarator.get()).or_else(|| n.named_child(0))?;
            if k == self.ids.function_declarator {
                is_fn = inner.kind_id() == self.ids.identifier;
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

    fn declarators(&self, n: Node<'a>) -> Vec<Node<'a>> {
        let mut c = n.walk();
        n.children_by_field_id(self.ids.f_declarator, &mut c).collect()
    }

    /// Does the declarator declare a function or a pointer to one?
    fn declares_function(&self, mut n: Node<'a>) -> bool {
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
    fn strip(&self, mut n: Node<'a>) -> Node<'a> {
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
    fn function_names(&self, n: Node<'a>, out: &mut Vec<Node<'a>>) {
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

    fn bind(&mut self, value: Node<'a>, slot: u32, index: u32, var: bool, scope: u32, hint: u32) {
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
    fn member_names(&mut self, body: Node<'a>, names: &mut Vec<u32>, types: &mut Vec<u32>) {
        let ids = self.ids;
        let mut c = body.walk();
        let decls: Vec<Node<'a>> = body.named_children(&mut c).filter(|d| d.kind_id() == ids.field_declaration).collect();
        for decl in decls {
            let ds = self.declarators(decl);
            if ds.is_empty() {
                let inner = decl.child_by_field_id(ids.f_type).and_then(|t| t.child_by_field_id(ids.f_body));
                match inner {
                    Some(inner) => self.member_names(inner, names, types),
                    None => {
                        names.push(NONE);
                        types.push(NONE);
                    }
                }
                continue;
            }
            let t = self.type_name_of(decl);
            for d in ds {
                // A member that is a function pointer has no struct type,
                // whatever its return type is.
                let (name, t) = match self.decl_name(d) {
                    Some((name, _)) => (self.intern(self.text(name)), if self.declares_function(d) { NONE } else { t }),
                    None => (NONE, NONE),
                };
                names.push(name);
                types.push(t);
            }
        }
    }

    fn push_record(&mut self, name: u32, node: Node<'a>, body: Node<'a>) {
        let (mut fields, mut types) = (Vec::new(), Vec::new());
        self.member_names(body, &mut fields, &mut types);
        let defs = vec![NONE; fields.len()];
        self.open_records.push((node.end_byte(), self.out.records.len()));
        self.out.records.push(Record { name, fields, types, defs });
    }

    /// A braced initializer of an object of struct type `type_name` (or of
    /// an array of them): bind the functions in it to the members they
    /// initialize.
    fn init_list(&mut self, list: Node<'a>, type_name: u32, array: bool, var: u32, scope: u32) {
        let ids = self.ids;
        let mut index = 0u32;
        let mut c = list.walk();
        let elements: Vec<Node<'a>> = list.named_children(&mut c).collect();
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
    fn initializers(&mut self, decl: Node<'a>, scope: u32) {
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
                    let text = self.out.push_name(&text);
                    let line = value.start_position().row as u32 + 1;
                    self.out.inits.push(InitSite { type_name, text, line, scope, array });
                } else if by_row {
                    // A table too long to expand as one text (a generated
                    // list of commands): row by row, each a struct of its own.
                    let mut c = value.walk();
                    let rows: Vec<Node<'a>> = value.named_children(&mut c).filter(|r| !r.is_extra()).collect();
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
                                let text = self.out.push_name(&text);
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
    fn visit(&mut self, node: Node<'a>) -> bool {
        self.flush_events(node.start_byte());
        self.leave_scope(node.start_byte());
        let k = node.kind_id();
        let ids = self.ids;

        if k == ids.identifier {
            if !self.skip.remove(&node.start_byte()) && !(self.in_fn() && self.locals.contains(self.text(node))) {
                self.push_ref(node, refkind::USE);
            }
            return false;
        }
        if k == ids.type_identifier {
            if !self.skip.remove(&node.start_byte()) {
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
                        self.out.indirect_calls += 1;
                        self.local_calls.push((self.text(f), f.start_position().row as u32 + 1));
                    } else {
                        self.push_ref(f, refkind::CALL);
                        // Names handed over: callbacks, if the callee calls them.
                        let callee = self.intern(self.text(f));
                        if let Some(args) = node.child_by_field_id(ids.f_arguments) {
                            let mut c = args.walk();
                            let list: Vec<Node<'a>> = args.named_children(&mut c).filter(|a| !a.is_extra()).collect();
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
                Some(f) => {
                    self.out.indirect_calls += 1;
                    let target = self.strip(f);
                    let tk = target.kind_id();
                    if tk == ids.field_expression {
                        // `p->member (...)`: through whatever is stored there.
                        if let Some(member) = target.child_by_field_id(ids.f_field) {
                            self.skip.insert(member.start_byte());
                            self.push_ref(member, refkind::ICALL);
                            let hint = self.member_hint(target);
                            if hint != NONE {
                                self.out.ref_hints.push((self.out.refs.len() as u32 - 1, hint));
                            }
                        }
                    } else if tk == ids.identifier && !(self.in_fn() && self.locals.contains(self.text(target))) {
                        // `(*hook) (...)` through a variable that is not local.
                        self.skip.insert(target.start_byte());
                        self.push_ref(target, refkind::CALL);
                    } else if tk == ids.identifier {
                        self.local_calls.push((self.text(target), target.start_position().row as u32 + 1));
                    }
                }
                None => self.out.indirect_calls += 1,
            }
            return true;
        }
        if k == ids.assignment_expression {
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
                if let Some((name, _)) = self.decl_name(d) {
                    let def = if self.declares_function(d) {
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
                let flags = self.storage_flags(node);
                let def = self.push_def(name, node, kind::FUNCTION, flags);
                let returns = self.type_name_of(node);
                if returns != NONE {
                    self.out.typed.push((def, returns));
                }
                self.locals.clear();
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
                    if let Some((name, _)) = self.decl_name(d) {
                        self.skip.insert(name.start_byte());
                        self.locals.insert(self.text(name));
                        if declared != NONE && !self.declares_function(d) {
                            self.local_types.insert(self.text(name), declared);
                        } else {
                            self.local_types.remove(self.text(name));
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
                        let (kind, flags) =
                            if is_fn { (kind::FUNCTION, storage | flag::DECL) } else { (kind::GLOBAL, storage) };
                        let def = self.push_def(name, node, kind, flags);
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
                    self.locals.insert(self.text(name));
                }
            }
            return true;
        }
        if k == ids.type_definition {
            let record = node
                .child_by_field_id(ids.f_type)
                .filter(|t| t.kind_id() == ids.struct_specifier || t.kind_id() == ids.union_specifier);
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
        if k == ids.struct_specifier || k == ids.union_specifier || k == ids.enum_specifier {
            if node.child_by_field_id(ids.f_body).is_some() {
                if let Some(name) = node.child_by_field_id(ids.f_name).filter(|n| !n.byte_range().is_empty()) {
                    let kind = if k == ids.struct_specifier {
                        kind::STRUCT
                    } else if k == ids.union_specifier {
                        kind::UNION
                    } else {
                        kind::ENUM
                    };
                    self.push_def(name, node, kind, 0);
                    if k != ids.enum_specifier {
                        let record = self.intern(self.text(name));
                        self.push_record(record, node, node.child_by_field_id(ids.f_body).unwrap());
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
