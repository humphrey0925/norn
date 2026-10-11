//! Space-optimized adjacency: each list sorted by target, stored as
//! delta-encoded LEB128 varints. Trades sequential decode for ~half the bytes.

use crate::graph::{read_vec, write_vec, Graph};
use crate::model::EdgeHalf;
use std::io::{self, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"SIPACK01";

#[derive(Default)]
pub struct PackedAdj {
    /// Byte offset of each node's list; `off[n]` is the total length.
    off: Vec<u32>,
    bytes: Vec<u8>,
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(bytes: &[u8], pos: &mut usize) -> u32 {
    let (mut v, mut shift) = (0u32, 0);
    loop {
        let b = bytes[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

impl PackedAdj {
    pub fn from_csr(off: &[u32], adj: &[EdgeHalf]) -> Self {
        let mut p = PackedAdj { off: Vec::with_capacity(off.len()), bytes: Vec::with_capacity(adj.len() * 4) };
        let mut list: Vec<EdgeHalf> = Vec::new();
        for w in off.windows(2) {
            p.off.push(p.bytes.len() as u32);
            list.clear();
            list.extend_from_slice(&adj[w[0] as usize..w[1] as usize]);
            list.sort_unstable_by_key(|e| (e.other, e.meta));
            let mut prev = 0;
            for e in &list {
                put_varint(&mut p.bytes, e.other - prev);
                put_varint(&mut p.bytes, e.meta);
                prev = e.other;
            }
        }
        p.off.push(p.bytes.len() as u32);
        p
    }

    pub fn iter(&self, node: u32) -> impl Iterator<Item = EdgeHalf> + '_ {
        let end = self.off[node as usize + 1] as usize;
        let mut pos = self.off[node as usize] as usize;
        let mut prev = 0;
        std::iter::from_fn(move || {
            if pos >= end {
                return None;
            }
            prev += get_varint(&self.bytes, &mut pos);
            let meta = get_varint(&self.bytes, &mut pos);
            Some(EdgeHalf { other: prev, meta })
        })
    }

    pub fn heap_bytes(&self) -> usize {
        self.off.len() * 4 + self.bytes.len()
    }
}

/// A graph whose two adjacency lists are packed; names, files and symbols are
/// shared with the plain layout (`base.out` / `base.inc` are left empty).
#[derive(Default)]
pub struct PackedGraph {
    pub base: Graph,
    pub out: PackedAdj,
    pub inc: PackedAdj,
}

impl PackedGraph {
    pub fn from_graph(g: &Graph) -> Self {
        let (out, inc) =
            rayon::join(|| PackedAdj::from_csr(&g.out_off, &g.out), || PackedAdj::from_csr(&g.in_off, &g.inc));
        let base = Graph {
            str_bytes: g.str_bytes.clone(),
            str_off: g.str_off.clone(),
            files: g.files.clone(),
            syms: g.syms.clone(),
            by_name_off: g.by_name_off.clone(),
            by_name: g.by_name.clone(),
            ..Default::default()
        };
        PackedGraph { base, out, inc }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut w = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        w.write_all(MAGIC)?;
        self.base.write_tables(&mut w)?;
        for adj in [&self.out, &self.inc] {
            write_vec(&mut w, &adj.off)?;
            write_vec(&mut w, &adj.bytes)?;
        }
        w.flush()
    }

    pub fn load(path: &Path) -> io::Result<PackedGraph> {
        let mut r = io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a si packed graph file"));
        }
        let base = Graph::read_tables(&mut r)?;
        let mut adj = || -> io::Result<PackedAdj> { Ok(PackedAdj { off: read_vec(&mut r)?, bytes: read_vec(&mut r)? }) };
        let out = adj()?;
        let inc = adj()?;
        Ok(PackedGraph { base, out, inc })
    }

    pub fn heap_bytes(&self) -> usize {
        self.base.heap_bytes() + self.out.heap_bytes() + self.inc.heap_bytes()
    }
}
