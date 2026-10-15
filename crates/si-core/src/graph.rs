//! The linked graph in CSR form, its on-disk format, and O(degree) queries.

use crate::model::{EdgeHalf, FileRec, Sym};
use bytemuck::Pod;
use std::io::{self, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"SIGRAPH2";

/// All arrays are flat and integer-only. Names are sorted, so a name id is
/// also its lexicographic rank: lookup is a binary search with no hash table,
/// and prefix search is a contiguous range.
#[derive(Default)]
pub struct Graph {
    pub str_bytes: Vec<u8>,
    pub str_off: Vec<u32>,
    pub files: Vec<FileRec>,
    pub syms: Vec<Sym>,
    /// name id -> symbol ids (CSR).
    pub by_name_off: Vec<u32>,
    pub by_name: Vec<u32>,
    /// symbol -> outgoing references (CSR).
    pub out_off: Vec<u32>,
    pub out: Vec<EdgeHalf>,
    /// symbol -> incoming references (CSR).
    pub in_off: Vec<u32>,
    pub inc: Vec<EdgeHalf>,
    /// C++: (symbol, the scope it is in as written, a name id), of those
    /// that are in one, by symbol. (A few among millions in a C tree with
    /// a C++ file or two: kept only for those.)
    pub owner: Vec<[u32; 2]>,
}

/// Names a C++ symbol with the classes it is in, not the namespaces:
/// `DBImpl::Get`, `Writer::Rep::~Rep`.
pub struct Qualifier {
    /// Names that are namespaces and no class.
    namespaces: rustc_hash::FxHashSet<u32>,
}

impl Qualifier {
    pub fn new(g: &Graph) -> Self {
        use crate::model::kind;
        let mut namespaces = rustc_hash::FxHashSet::default();
        if !g.owner.is_empty() {
            for s in g.syms.iter().filter(|s| s.kind == kind::NAMESPACE) {
                namespaces.insert(s.name);
            }
            namespaces.retain(|&n| !g.syms_named(n).iter().any(|&c| matches!(g.syms[c as usize].kind, kind::CLASS | kind::STRUCT | kind::UNION)));
        }
        Qualifier { namespaces }
    }

    /// The classes of a scope as written (`leveldb::DBImpl` → `DBImpl`).
    pub fn classes<'g>(&self, g: &'g Graph, scope: u32) -> &'g str {
        let mut s = g.name(scope);
        while let Some((first, rest)) = s.split_once("::") {
            match g.name_id(first) {
                Some(id) if self.namespaces.contains(&id) => s = rest,
                _ => break,
            }
        }
        // (A scope all namespaces: `ns::f`, no member.)
        match g.name_id(s) {
            Some(id) if self.namespaces.contains(&id) && !s.contains("::") => "",
            _ => s,
        }
    }

    pub fn name<'g>(&self, g: &'g Graph, sym: u32) -> std::borrow::Cow<'g, str> {
        let name = g.sym_name(sym);
        match g.owner_of(sym) {
            Some(o) => match self.classes(g, o) {
                "" => name.into(),
                classes => format!("{classes}::{name}").into(),
            },
            None => name.into(),
        }
    }
}

pub(crate) fn write_vec<T: Pod, W: Write>(w: &mut W, v: &[T]) -> io::Result<()> {
    let bytes: &[u8] = bytemuck::cast_slice(v);
    w.write_all(&(bytes.len() as u64).to_le_bytes())?;
    w.write_all(bytes)
}

pub(crate) fn read_vec<T: Pod, R: Read>(r: &mut R) -> io::Result<Vec<T>> {
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let n = u64::from_le_bytes(len) as usize / std::mem::size_of::<T>();
    let mut v = vec![T::zeroed(); n];
    r.read_exact(bytemuck::cast_slice_mut(&mut v))?;
    Ok(v)
}

impl Graph {
    pub fn n_names(&self) -> usize {
        self.str_off.len().saturating_sub(1)
    }

    pub fn name(&self, id: u32) -> &str {
        let (a, b) = (self.str_off[id as usize] as usize, self.str_off[id as usize + 1] as usize);
        // Names come from `String`s, so the bytes are valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(&self.str_bytes[a..b]) }
    }

    pub fn sym_name(&self, sym: u32) -> &str {
        self.name(self.syms[sym as usize].name)
    }

    /// C++: the scope `sym` is in, as written (`leveldb::DBImpl`).
    pub fn owner_of(&self, sym: u32) -> Option<u32> {
        let at = self.owner.binary_search_by_key(&sym, |o| o[0]).ok()?;
        Some(self.owner[at][1])
    }

    pub fn file_path(&self, file: u32) -> &str {
        self.name(self.files[file as usize].path)
    }

    /// First name id whose string is >= `s`.
    fn lower_bound(&self, s: &str) -> u32 {
        let (mut lo, mut hi) = (0u32, self.n_names() as u32);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.name(mid) < s {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub fn name_id(&self, s: &str) -> Option<u32> {
        let id = self.lower_bound(s);
        ((id as usize) < self.n_names() && self.name(id) == s).then_some(id)
    }

    /// Symbols with exactly this name. O(log names).
    pub fn find(&self, s: &str) -> &[u32] {
        match self.name_id(s) {
            Some(id) => self.syms_named(id),
            None => &[],
        }
    }

    pub fn syms_named(&self, name: u32) -> &[u32] {
        &self.by_name[self.by_name_off[name as usize] as usize..self.by_name_off[name as usize + 1] as usize]
    }

    /// Name ids starting with `prefix`, as a contiguous range.
    pub fn prefix(&self, prefix: &str) -> std::ops::Range<u32> {
        let lo = self.lower_bound(prefix);
        let mut hi = lo;
        while (hi as usize) < self.n_names() && self.name(hi).starts_with(prefix) {
            hi += 1;
        }
        lo..hi
    }

    pub fn refs_from(&self, sym: u32) -> &[EdgeHalf] {
        &self.out[self.out_off[sym as usize] as usize..self.out_off[sym as usize + 1] as usize]
    }

    pub fn refs_to(&self, sym: u32) -> &[EdgeHalf] {
        &self.inc[self.in_off[sym as usize] as usize..self.in_off[sym as usize + 1] as usize]
    }

    /// The symbols of a file, the file itself first, in line order.
    pub fn file_syms(&self, file: u32) -> std::ops::Range<u32> {
        let start = self.files[file as usize].sym_start;
        let end = self.files.get(file as usize + 1).map_or(self.syms.len() as u32, |f| f.sym_start);
        start..end
    }

    /// Innermost symbol of `file` enclosing `line`. O(log symbols-in-file).
    pub fn symbol_at(&self, file: u32, line: u32) -> Option<u32> {
        let r = self.file_syms(file);
        let syms = &self.syms[r.start as usize..r.end as usize];
        let mut i = syms.partition_point(|s| s.line <= line);
        while i > 1 {
            i -= 1;
            if syms[i].end_line >= line {
                return Some(r.start + i as u32);
            }
        }
        None
    }

    pub fn heap_bytes(&self) -> usize {
        self.str_bytes.len()
            + (self.str_off.len() + self.by_name_off.len() + self.by_name.len() + self.out_off.len() + self.in_off.len())
                * 4
            + self.files.len() * std::mem::size_of::<FileRec>()
            + self.syms.len() * std::mem::size_of::<Sym>()
            + (self.out.len() + self.inc.len()) * std::mem::size_of::<EdgeHalf>()
            + self.owner.len() * 8
    }

    /// The tables shared by every graph layout: names, files, symbols, name index.
    pub(crate) fn write_tables<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_vec(w, &self.str_bytes)?;
        write_vec(w, &self.str_off)?;
        write_vec(w, &self.files)?;
        write_vec(w, &self.syms)?;
        write_vec(w, &self.by_name_off)?;
        write_vec(w, &self.by_name)?;
        write_vec(w, &self.owner)
    }

    pub(crate) fn read_tables<R: Read>(r: &mut R) -> io::Result<Graph> {
        Ok(Graph {
            str_bytes: read_vec(r)?,
            str_off: read_vec(r)?,
            files: read_vec(r)?,
            syms: read_vec(r)?,
            by_name_off: read_vec(r)?,
            by_name: read_vec(r)?,
            owner: read_vec(r)?,
            ..Default::default()
        })
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut w = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        w.write_all(MAGIC)?;
        self.write_tables(&mut w)?;
        write_vec(&mut w, &self.out_off)?;
        write_vec(&mut w, &self.out)?;
        write_vec(&mut w, &self.in_off)?;
        write_vec(&mut w, &self.inc)?;
        w.flush()
    }

    pub fn load(path: &Path) -> io::Result<Graph> {
        let mut r = io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a si graph file"));
        }
        let mut g = Graph::read_tables(&mut r)?;
        g.out_off = read_vec(&mut r)?;
        g.out = read_vec(&mut r)?;
        g.in_off = read_vec(&mut r)?;
        g.inc = read_vec(&mut r)?;
        Ok(g)
    }
}
