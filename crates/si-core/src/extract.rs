//! Single-file extraction: tree-sitter parse + one preorder walk, O(tokens).

use crate::model::{flag, kind, refkind, Def, FileFacts, Ref};
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
    preproc_def: u16,
    preproc_function_def: u16,
    call_expression: u16,
    function_declarator: u16,
    storage_class_specifier: u16,
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
            preproc_def: k("preproc_def"),
            preproc_function_def: k("preproc_function_def"),
            call_expression: k("call_expression"),
            function_declarator: k("function_declarator"),
            storage_class_specifier: k("storage_class_specifier"),
            f_declarator: f("declarator"),
            f_name: f("name").get(),
            f_body: f("body").get(),
            f_function: f("function").get(),
        }
    }
}

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
    src: &'a [u8],
    ids: &'a Ids,
    out: FileFacts,
    names: FxHashMap<&'a [u8], u32>,
    /// Start bytes of identifier nodes already consumed as a def name or call target.
    skip: FxHashSet<usize>,
    locals: FxHashSet<&'a [u8]>,
    scope: Option<Scope>,
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

    pub fn extract(&mut self, path: &str, src: &[u8]) -> FileFacts {
        let mut out = FileFacts::new(path.to_string());
        out.hash = xxhash_rust::xxh3::xxh3_64(src);
        out.bytes = src.len() as u32;
        let file_name = out.push_name(path);
        out.defs.push(Def { name: file_name, line: 1, end_line: 0, kind: kind::FILE, flags: 0 });

        let t = std::time::Instant::now();
        let Some(tree) = self.parser.parse(src, None) else {
            out.has_error = true;
            return out;
        };
        out.parse_ns = t.elapsed().as_nanos() as u64;
        let t = std::time::Instant::now();
        let root = tree.root_node();
        out.has_error = root.has_error();
        out.defs[0].end_line = root.end_position().row as u32 + 1;

        let mut w = Walk {
            src,
            ids: &self.ids,
            out,
            names: FxHashMap::default(),
            skip: FxHashSet::default(),
            locals: FxHashSet::default(),
            scope: None,
        };
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
        w.out.walk_ns = t.elapsed().as_nanos() as u64;
        w.out
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
            if k == self.ids.identifier || k == self.ids.type_identifier {
                return Some((n, is_fn));
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

    /// Returns whether to descend into the node's children.
    fn visit(&mut self, node: Node<'a>) -> bool {
        if let Some(s) = self.scope {
            if node.start_byte() >= s.end {
                self.scope = None;
                self.locals.clear();
            }
        }
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
        if k == ids.call_expression {
            match node.child_by_field_id(ids.f_function) {
                Some(f) if f.kind_id() == ids.identifier => {
                    self.skip.insert(f.start_byte());
                    if self.in_fn() && self.locals.contains(self.text(f)) {
                        self.out.indirect_calls += 1;
                    } else {
                        self.push_ref(f, refkind::CALL);
                    }
                }
                _ => self.out.indirect_calls += 1,
            }
            return true;
        }
        if k == ids.function_definition {
            let named = node.child_by_field_id(ids.f_declarator.get()).and_then(|d| self.decl_name(d));
            if let Some((name, _)) = named {
                let flags = self.storage_flags(node);
                let def = self.push_def(name, node, kind::FUNCTION, flags);
                self.locals.clear();
                self.scope = Some(Scope { def, end: node.end_byte(), is_fn: true });
            }
            return true;
        }
        if k == ids.declaration {
            if self.in_fn() {
                for d in self.declarators(node) {
                    if let Some((name, _)) = self.decl_name(d) {
                        self.skip.insert(name.start_byte());
                        self.locals.insert(self.text(name));
                    }
                }
            } else {
                let storage = self.storage_flags(node);
                let mut first = None;
                for d in self.declarators(node) {
                    if let Some((name, is_fn)) = self.decl_name(d) {
                        let (kind, flags) =
                            if is_fn { (kind::FUNCTION, storage | flag::DECL) } else { (kind::GLOBAL, storage) };
                        let def = self.push_def(name, node, kind, flags);
                        first.get_or_insert(def);
                    }
                }
                if let Some(def) = first {
                    self.scope = Some(Scope { def, end: node.end_byte(), is_fn: false });
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
            for d in self.declarators(node) {
                if let Some((name, _)) = self.decl_name(d) {
                    self.push_def(name, node, kind::TYPEDEF, 0);
                }
            }
            return true;
        }
        if k == ids.struct_specifier || k == ids.union_specifier || k == ids.enum_specifier {
            if node.child_by_field_id(ids.f_body).is_some() {
                if let Some(name) = node.child_by_field_id(ids.f_name) {
                    let kind = if k == ids.struct_specifier {
                        kind::STRUCT
                    } else if k == ids.union_specifier {
                        kind::UNION
                    } else {
                        kind::ENUM
                    };
                    self.push_def(name, node, kind, 0);
                }
            }
            return true;
        }
        if k == ids.enumerator {
            if let Some(name) = node.child_by_field_id(ids.f_name) {
                self.push_def(name, node, kind::ENUM_CONST, 0);
            }
            return true;
        }
        if k == ids.preproc_def || k == ids.preproc_function_def {
            if let Some(name) = node.child_by_field_id(ids.f_name) {
                self.push_def(name, node, kind::MACRO, 0);
                self.skip.remove(&name.start_byte());
            }
            // The macro body is an opaque token run to tree-sitter; nothing to walk.
            return false;
        }
        true
    }
}
