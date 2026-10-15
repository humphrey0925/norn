//! Shared data model: per-file facts (extraction output) and the linked graph.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

pub mod kind {
    pub const FILE: u8 = 0;
    pub const FUNCTION: u8 = 1;
    pub const MACRO: u8 = 2;
    pub const STRUCT: u8 = 3;
    pub const UNION: u8 = 4;
    pub const ENUM: u8 = 5;
    pub const ENUM_CONST: u8 = 6;
    pub const TYPEDEF: u8 = 7;
    pub const GLOBAL: u8 = 8;
    /// A member of a struct or union.
    pub const FIELD: u8 = 9;
    /// C++.
    pub const CLASS: u8 = 10;
    pub const NAMESPACE: u8 = 11;

    pub fn name(k: u8) -> &'static str {
        match k {
            FILE => "file",
            FUNCTION => "function",
            MACRO => "macro",
            STRUCT => "struct",
            UNION => "union",
            ENUM => "enum",
            ENUM_CONST => "enum_const",
            TYPEDEF => "typedef",
            GLOBAL => "global",
            FIELD => "field",
            CLASS => "class",
            NAMESPACE => "namespace",
            _ => "?",
        }
    }
}

pub mod flag {
    /// `static` storage class: invisible outside its translation unit unless in a header.
    pub const STATIC: u8 = 1;
    /// Declaration only (prototype / `extern`), not a definition.
    pub const DECL: u8 = 2;
    /// Defined in a header file.
    pub const HEADER: u8 = 4;
    /// A typedef of a function or function pointer type, or a member that
    /// can hold a function.
    pub const CALLABLE: u8 = 8;
    /// A macro defined under an `#if`: it may not be defined at all.
    pub const CONDITIONAL: u8 = 16;
    /// A C++ method declared `virtual` (or `override`, `final`): a call of
    /// it may run an override.
    pub const VIRTUAL: u8 = 32;
}

pub mod refkind {
    pub const CALL: u8 = 0;
    pub const USE: u8 = 1;
    /// A member access: `x.name`, `x->name`, or `.name =` in an initializer.
    pub const MEMBER: u8 = 2;
    /// A call through a member: `x->name (...)`.
    pub const ICALL: u8 = 3;
    /// Not a reference in the source: an edge from a field or variable to a
    /// function stored in it.
    pub const BIND: u8 = 4;
    /// A call through a local pointer variable that the function assigns
    /// this name to: `fn = cond ? f : g; fn (x);`.
    pub const PCALL: u8 = 5;
    /// Flag on a reference in the facts (not in the graph): it comes from
    /// expanding a macro that is only defined in some configurations, so it
    /// is one reading of the line, not the reading.
    pub const MAYBE: u8 = 0x80;
}

pub const NONE: u32 = u32::MAX;

/// Where a use of an expanding macro stands.
pub mod site {
    /// The whole use is directly followed by `(`.
    pub const CALLED: u8 = 1;
    /// Inside a function body.
    pub const IN_FN: u8 = 2;
    /// Outside every bracket, lexically.
    pub const TOP: u8 = 4;
    /// The whole use is directly followed by `{`.
    pub const BRACE_FOLLOWS: u8 = 8;
    /// Inside an initializer's braces.
    pub const INIT: u8 = 16;
    /// The recorded arguments follow a name that takes none itself.
    pub const TRAILING: u8 = 32;
}

/// The text of a `#define` an expansion may need: `text` is everything after
/// the name, parameter list included.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MacroBody {
    pub name: u32,
    pub text: u32,
    pub function_like: bool,
    /// The `#if NAME == value` tests around it, as `NAME == 64;OTHER != 1`,
    /// or `NONE`.
    pub guard: u32,
    /// Defined only under some `#if`.
    pub conditional: bool,
    /// Defined under `#ifndef` of its own name.
    pub fallback: bool,
}

/// A use of an expanding macro, to be expanded at link time, when the
/// definitions in other files are known.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Site {
    pub name: u32,
    /// The argument list as written, parentheses included, or `NONE`.
    pub args: u32,
    pub line: u32,
    pub scope: u32,
    /// The definition this use names (`NAME (a, b) (void) { }`), or `NONE`.
    pub def: u32,
    pub flags: u8,
}

/// A function stored where a call can later find it: in a struct member or
/// in a variable. `func` is only a name; whether it is a function is
/// decided at link time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Bind {
    /// The member or variable name; or, when `index` is not `NONE`, the name
    /// of the struct type whose `index`-th member is meant (an initializer
    /// without designators).
    pub slot: u32,
    pub index: u32,
    pub func: u32,
    pub line: u32,
    pub scope: u32,
    /// The slot is a variable, not a member.
    pub var: bool,
    /// For a member: what it is a member of (an index into `hints`), or `NONE`.
    pub hint: u32,
    /// The translation unit (a file index, set at link time) in which the
    /// store reads this way, when macros decide that; else `NONE`.
    pub unit: u32,
}

/// A call a function makes through one of its parameters.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ParamCall {
    /// The function, as a definition of this file.
    pub scope: u32,
    /// The parameter's position.
    pub index: u32,
    pub line: u32,
}

/// A name passed as an argument in a call by name: if the callee calls
/// through that parameter and the name is a function, that is who it calls.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ArgPass {
    pub callee: u32,
    pub index: u32,
    pub func: u32,
}

/// A braced initializer with macro uses in it, kept as text: which member
/// each element initializes is only known once the macros are expanded.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct InitSite {
    /// The struct type's name, or `NONE`.
    pub type_name: u32,
    pub text: u32,
    pub line: u32,
    pub scope: u32,
    /// The initialized object is an array of the type.
    pub array: bool,
}

/// The members of a struct or union, in order (`NONE` for an unnamed one).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub name: u32,
    pub fields: Vec<u32>,
    /// For each member, the name of its type when that is a struct, a union
    /// or a typedef (else `NONE`), and its definition in this file.
    pub types: Vec<u32>,
    pub defs: Vec<u32>,
    /// C++: a class in a class, called by its short name only in there.
    pub nested: bool,
    /// C++: for each member that is a container (`std::vector<T> v`), the
    /// name of its elements' type (`T`; of a map, its values'), else `NONE`.
    /// Empty where none is.
    pub elems: Vec<u32>,
}

/// What a member access is made on, as far as the text says: something of a
/// known type, a variable or a call, then members followed from there. From
/// this the link finds the struct the member belongs to, so that two structs
/// with a member of the same name are not confused.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MemberHint {
    pub root_kind: u8,
    /// A type name, a variable's name or a function's, by `root_kind`.
    pub root: u32,
    /// The members followed from the root: `len` names in `hint_path`.
    pub path: u32,
    pub len: u32,
}

pub mod root {
    pub const TYPE: u8 = 0;
    pub const VAR: u8 = 1;
    pub const FUNC: u8 = 2;
}

/// A definition found in one file. `name` indexes the file-local name table.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Def {
    pub name: u32,
    pub line: u32,
    pub end_line: u32,
    pub kind: u8,
    pub flags: u8,
}

/// A reference found in one file. `scope` is the index of the enclosing def
/// in the same file (0 = the file itself).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Ref {
    pub name: u32,
    pub line: u32,
    pub scope: u32,
    pub kind: u8,
}

/// Everything extracted from a single file. Depends only on that file's
/// content and the transparent-macro set, so it can be produced in parallel
/// and cached by (content hash, set digest).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub path: String,
    pub hash: u64,
    pub bytes: u32,
    pub has_error: bool,
    pub indirect_calls: u32,
    /// Wall time spent in the lexical mask, the tree-sitter parse and the tree walk.
    pub mask_ns: u64,
    pub parse_ns: u64,
    pub walk_ns: u64,
    names: Names,
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    /// `#include` operands as written, as file-local name ids.
    pub includes: Vec<u32>,
    /// For each of them, whether it is under an `#if` (other than the
    /// file's include guard): the file may not be read at all.
    pub include_conditional: Vec<bool>,
    /// The lines the conditions in force switch off (first, last; from 1):
    /// not compiled, so not indexed, and nothing is to be found from them.
    pub inactive: Vec<(u32, u32)>,
    /// The lines that may or may not be compiled, the conditions in force
    /// cannot tell: (first, last, the line of the `#if` that decides).
    /// Indexed, and what is found in them is only as sure as that `#if`.
    pub undecided: Vec<(u32, u32, u32)>,
    pub macros: Vec<MacroBody>,
    pub sites: Vec<Site>,
    pub binds: Vec<Bind>,
    pub inits: Vec<InitSite>,
    pub records: Vec<Record>,
    /// `typedef struct tag name;` as (name, tag).
    pub aliases: Vec<(u32, u32)>,
    /// Members declared with a typedef name, as (definition, type name):
    /// whether they can hold a function depends on the typedef.
    pub field_types: Vec<(u32, u32)>,
    pub param_calls: Vec<ParamCall>,
    pub arg_passes: Vec<ArgPass>,
    pub hints: Vec<MemberHint>,
    pub hint_path: Vec<u32>,
    /// (reference, hint): which member references have one.
    pub ref_hints: Vec<(u32, u32)>,
    /// (definition, type name) of variables and functions at file scope
    /// whose type, or return type, is a struct, union or typedef.
    pub typed: Vec<(u32, u32)>,
    /// Every name the file `#define`s, in whatever branch.
    pub defined_names: Vec<u32>,
    /// The parameters and local variables of the file's functions, in the
    /// order they are declared. They are no part of the graph: a local is
    /// its function's alone, and is looked up in the file it is in.
    pub locals: Vec<Local>,
    /// Where each is used: (index into `locals`, line), in source order.
    pub local_uses: Vec<(u32, u32)>,
    /// Read as C++.
    pub cpp: bool,
    /// C++: the class a definition is a member of, as (definition, the
    /// scope as written, the class's own name): `DBImpl::Get` defined
    /// inside `namespace leveldb` or written `leveldb::DBImpl::Get` has
    /// the scope `DBImpl` or `leveldb::DBImpl`, and the class `DBImpl`.
    pub owners: Vec<(u32, u32, u32)>,
    /// C++: (class, base class), by their own names.
    pub bases: Vec<(u32, u32)>,
    /// C++: the names of the namespaces the file opens, or names another
    /// way: `namespace tfm = tinyformat;` as (`tfm`, `tinyformat`).
    pub namespaces: Vec<u32>,
    pub namespace_aliases: Vec<(u32, u32)>,
    /// C++: (reference, qualifier) of a name called as `X::f`: the last
    /// part of the scope written before it (`""` for `::f`).
    pub ref_quals: Vec<(u32, u32)>,
}

/// A parameter or a local variable.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Local {
    pub name: u32,
    /// The line of its declaration.
    pub line: u32,
    /// The function, as a definition of this file.
    pub scope: u32,
    pub param: bool,
    /// The name of its type when that is a struct, a union or a typedef, else `NONE`.
    pub type_name: u32,
}

/// A file's names: for each, its number among the names of all files
/// ([`crate::names`]), or, marked with [`OWN`], a place in the file's own
/// text. The file's own are what is kept to be expanded later (a macro's
/// body, the arguments of a use, an initializer): long, and written once.
#[derive(Clone, Debug)]
struct Names {
    ids: Vec<u32>,
    buf: String,
    off: Vec<u32>,
}

const OWN: u32 = crate::names::LIMIT;

impl Default for Names {
    fn default() -> Self {
        Names { ids: Vec::new(), buf: String::new(), off: vec![0] }
    }
}

/// On disk a file's names are all text, so that what is saved of a file
/// can be read by a process that has given the names other numbers.
#[derive(Serialize, Deserialize)]
struct SavedNames {
    buf: String,
    off: Vec<u32>,
    /// Which of them are the file's own.
    own: Vec<u32>,
}

impl Serialize for Names {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut saved = SavedNames { buf: String::new(), off: vec![0], own: Vec::new() };
        for (i, &id) in self.ids.iter().enumerate() {
            if id & OWN != 0 {
                saved.own.push(i as u32);
            }
            saved.buf.push_str(self.get(i));
            saved.off.push(saved.buf.len() as u32);
        }
        saved.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Names {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let saved = SavedNames::deserialize(d)?;
        let mut names = Names::default();
        let mut own = saved.own.iter().copied().peekable();
        for i in 0..saved.off.len().saturating_sub(1) {
            let text = saved.buf.get(saved.off[i] as usize..saved.off[i + 1] as usize).ok_or_else(|| serde::de::Error::custom("names out of bounds"))?;
            if own.next_if_eq(&(i as u32)).is_some() {
                names.push_own(text);
            } else {
                names.ids.push(crate::names::id(text));
            }
        }
        names.ids.shrink_to_fit();
        names.buf.shrink_to_fit();
        names.off.shrink_to_fit();
        Ok(names)
    }
}

impl Names {
    fn push_own(&mut self, s: &str) {
        self.ids.push(OWN | (self.off.len() - 1) as u32);
        self.buf.push_str(s);
        self.off.push(self.buf.len() as u32);
    }

    fn get(&self, i: usize) -> &str {
        let id = self.ids[i];
        if id & OWN == 0 {
            crate::names::text(id)
        } else {
            let at = (id & !OWN) as usize;
            &self.buf[self.off[at] as usize..self.off[at + 1] as usize]
        }
    }
}

impl FileFacts {
    pub fn new(path: String) -> Self {
        FileFacts { path, ..Default::default() }
    }

    /// A name: an identifier, a path, anything other files say too.
    pub fn push_name(&mut self, s: &str) -> u32 {
        self.names.ids.push(crate::names::id(s));
        (self.names.ids.len() - 1) as u32
    }

    /// Text kept under a name's number that is no name: only this file has it.
    pub fn push_text(&mut self, s: &str) -> u32 {
        self.names.push_own(s);
        (self.names.ids.len() - 1) as u32
    }

    pub fn n_names(&self) -> usize {
        self.names.ids.len()
    }

    pub fn name(&self, i: usize) -> &str {
        self.names.get(i)
    }

    /// For each name, its number among all files' names, or `NONE` for
    /// what is the file's own text.
    pub fn shared_names(&self) -> impl Iterator<Item = u32> + '_ {
        self.names.ids.iter().map(|&id| if id & OWN == 0 { id } else { NONE })
    }

    /// The bytes of the file's own text.
    pub fn own_text_bytes(&self) -> usize {
        self.names.buf.len()
    }

    /// Give back what the vectors hold beyond what is in them: they grew
    /// by doubling, and in a large tree the slack of every file adds up.
    ///
    /// Each is copied into a block of its size. Asking the allocator to
    /// make the block smaller (`shrink_to_fit`) is not enough: mimalloc
    /// leaves a block as it is unless less than half of it is wanted, so
    /// the vectors would go on holding a third more than is in them (in
    /// the kernel, 0.8 GB) while saying they do not.
    pub fn shrink(&mut self) {
        fn exact<T>(v: &mut Vec<T>) {
            if v.capacity() > v.len() {
                let mut fitting = Vec::with_capacity(v.len());
                fitting.append(v);
                *v = fitting;
            }
        }
        exact(&mut self.names.ids);
        if self.names.buf.capacity() > self.names.buf.len() {
            self.names.buf = self.names.buf.clone();
        }
        exact(&mut self.names.off);
        exact(&mut self.defs);
        exact(&mut self.refs);
        exact(&mut self.includes);
        exact(&mut self.include_conditional);
        exact(&mut self.inactive);
        exact(&mut self.undecided);
        exact(&mut self.macros);
        exact(&mut self.sites);
        exact(&mut self.binds);
        exact(&mut self.inits);
        for r in &mut self.records {
            exact(&mut r.fields);
            exact(&mut r.types);
            exact(&mut r.defs);
            exact(&mut r.elems);
        }
        exact(&mut self.records);
        exact(&mut self.aliases);
        exact(&mut self.field_types);
        exact(&mut self.owners);
        exact(&mut self.bases);
        exact(&mut self.namespaces);
        exact(&mut self.namespace_aliases);
        exact(&mut self.ref_quals);
        exact(&mut self.param_calls);
        exact(&mut self.arg_passes);
        exact(&mut self.hints);
        exact(&mut self.hint_path);
        exact(&mut self.ref_hints);
        exact(&mut self.typed);
        exact(&mut self.defined_names);
        exact(&mut self.locals);
        exact(&mut self.local_uses);
    }

    /// What the names (and the texts kept as names) take.
    pub fn names_bytes(&self) -> usize {
        self.names.buf.capacity() + (self.names.off.capacity() + self.names.ids.capacity()) * 4
    }

    /// What each part of the facts takes, in bytes, by name.
    pub fn parts(&self) -> [(&'static str, usize); 12] {
        fn of<T>(v: &Vec<T>) -> usize {
            v.capacity() * std::mem::size_of::<T>()
        }
        let records = of(&self.records) + self.records.iter().map(|r| of(&r.fields) + of(&r.types) + of(&r.defs)).sum::<usize>();
        [
            ("names", self.names_bytes()),
            ("refs", of(&self.refs)),
            ("defs", of(&self.defs)),
            ("sites", of(&self.sites) + of(&self.inits)),
            ("macros", of(&self.macros) + of(&self.defined_names)),
            ("binds", of(&self.binds)),
            ("records", records + of(&self.aliases) + of(&self.field_types) + of(&self.typed) + of(&self.owners) + of(&self.bases) + of(&self.namespaces)),
            ("hints", of(&self.hints) + of(&self.hint_path) + of(&self.ref_hints) + of(&self.ref_quals)),
            ("calls through", of(&self.param_calls) + of(&self.arg_passes)),
            ("locals", of(&self.locals)),
            ("local uses", of(&self.local_uses)),
            ("rest", self.path.capacity() + of(&self.includes) + of(&self.include_conditional) + of(&self.inactive) + of(&self.undecided) + std::mem::size_of::<FileFacts>()),
        ]
    }

    pub fn heap_bytes(&self) -> usize {
        self.parts().iter().map(|p| p.1).sum()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Sym {
    pub name: u32,
    pub file: u32,
    pub line: u32,
    pub end_line: u32,
    pub kind: u8,
    pub flags: u8,
    pub _pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct FileRec {
    pub hash: u64,
    /// Name id of the file path.
    pub path: u32,
    /// First symbol id of this file; its symbols are contiguous and sorted by line.
    pub sym_start: u32,
}

/// One half of an edge as stored in a CSR adjacency list.
/// `meta` = line << 4 | ambiguous << 3 | refkind.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct EdgeHalf {
    pub other: u32,
    pub meta: u32,
}

impl EdgeHalf {
    pub fn line(&self) -> u32 {
        self.meta >> 4
    }
    pub fn ambiguous(&self) -> bool {
        self.meta & 8 != 0
    }
    pub fn kind(&self) -> u8 {
        (self.meta & 7) as u8
    }
}

pub fn pack_meta(line: u32, ambiguous: bool, kind: u8) -> u32 {
    (line << 4) | ((ambiguous as u32) << 3) | kind as u32
}
