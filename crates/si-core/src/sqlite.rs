//! SQLite stores, used as comparison points for the CSR store.
//!
//! `Naive` is the obvious schema (text columns, indexes maintained during
//! insert). `Tuned` interns strings, clusters edges with WITHOUT ROWID tables
//! and builds secondary indexes after the bulk load.

use crate::graph::Graph;
use rusqlite::{params, Connection, Result};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Naive,
    Tuned,
}

pub fn write(g: &Graph, path: &Path, flavor: Flavor) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let mut db = Connection::open(path)?;
    match flavor {
        Flavor::Naive => write_naive(g, &mut db),
        Flavor::Tuned => write_tuned(g, &mut db),
    }
}

fn write_naive(g: &Graph, db: &mut Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE symbols(id INTEGER PRIMARY KEY, name TEXT, kind INTEGER, flags INTEGER,
                              file TEXT, line INTEGER, end_line INTEGER);
         CREATE INDEX symbols_name ON symbols(name);
         CREATE INDEX symbols_file ON symbols(file, line);
         CREATE TABLE edges(src INTEGER, dst INTEGER, meta INTEGER);
         CREATE INDEX edges_src ON edges(src);
         CREATE INDEX edges_dst ON edges(dst);",
    )?;
    let tx = db.transaction()?;
    {
        let mut ins = tx.prepare("INSERT INTO symbols VALUES(?,?,?,?,?,?,?)")?;
        for (id, s) in g.syms.iter().enumerate() {
            ins.execute(params![id as u32, g.name(s.name), s.kind, s.flags, g.file_path(s.file), s.line, s.end_line])?;
        }
        let mut ins = tx.prepare("INSERT INTO edges VALUES(?,?,?)")?;
        for src in 0..g.syms.len() as u32 {
            for e in g.refs_from(src) {
                ins.execute(params![src, e.other, e.meta])?;
            }
        }
    }
    tx.commit()
}

fn write_tuned(g: &Graph, db: &mut Connection) -> Result<()> {
    db.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA locking_mode=EXCLUSIVE;
         PRAGMA cache_size=-262144;
         CREATE TABLE names(id INTEGER PRIMARY KEY, name TEXT);
         CREATE TABLE symbols(id INTEGER PRIMARY KEY, name INTEGER, kind INTEGER, flags INTEGER,
                              file INTEGER, line INTEGER, end_line INTEGER);
         CREATE TABLE out_edges(src INTEGER, dst INTEGER, meta INTEGER, PRIMARY KEY(src, dst, meta)) WITHOUT ROWID;
         CREATE TABLE in_edges(dst INTEGER, src INTEGER, meta INTEGER, PRIMARY KEY(dst, src, meta)) WITHOUT ROWID;",
    )?;
    let tx = db.transaction()?;
    {
        let mut ins = tx.prepare("INSERT INTO names VALUES(?,?)")?;
        for id in 0..g.n_names() as u32 {
            ins.execute(params![id, g.name(id)])?;
        }
        let mut ins = tx.prepare("INSERT INTO symbols VALUES(?,?,?,?,?,?,?)")?;
        for (id, s) in g.syms.iter().enumerate() {
            ins.execute(params![id as u32, s.name, s.kind, s.flags, s.file, s.line, s.end_line])?;
        }
        let mut ins = tx.prepare("INSERT OR IGNORE INTO out_edges VALUES(?,?,?)")?;
        for src in 0..g.syms.len() as u32 {
            for e in g.refs_from(src) {
                ins.execute(params![src, e.other, e.meta])?;
            }
        }
        let mut ins = tx.prepare("INSERT OR IGNORE INTO in_edges VALUES(?,?,?)")?;
        for dst in 0..g.syms.len() as u32 {
            for e in g.refs_to(dst) {
                ins.execute(params![dst, e.other, e.meta])?;
            }
        }
    }
    tx.commit()?;
    db.execute_batch(
        "CREATE UNIQUE INDEX names_name ON names(name);
         CREATE INDEX symbols_name ON symbols(name);
         CREATE INDEX symbols_file ON symbols(file, line);",
    )
}

/// Read side, mirroring the `Graph` query API.
pub struct SqliteStore {
    db: Connection,
    flavor: Flavor,
}

impl SqliteStore {
    pub fn open(path: &Path, flavor: Flavor) -> Result<Self> {
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.set_prepared_statement_cache_capacity(16);
        Ok(SqliteStore { db, flavor })
    }

    fn ids(&self, sql: &str, key: impl rusqlite::ToSql, out: &mut Vec<u32>) -> Result<()> {
        let mut st = self.db.prepare_cached(sql)?;
        let rows = st.query_map([key], |r| r.get::<_, u32>(0))?;
        for id in rows {
            out.push(id?);
        }
        Ok(())
    }

    pub fn find(&self, name: &str, out: &mut Vec<u32>) -> Result<()> {
        let sql = match self.flavor {
            Flavor::Naive => "SELECT id FROM symbols WHERE name=?",
            Flavor::Tuned => "SELECT s.id FROM names n JOIN symbols s ON s.name=n.id WHERE n.name=?",
        };
        self.ids(sql, name, out)
    }

    /// References to `sym`; with `calls_only`, just its callers.
    pub fn refs_to(&self, sym: u32, calls_only: bool, out: &mut Vec<u32>) -> Result<()> {
        let sql = match (self.flavor, calls_only) {
            (Flavor::Naive, false) => "SELECT src FROM edges WHERE dst=?",
            (Flavor::Naive, true) => "SELECT src FROM edges WHERE dst=? AND (meta&7)=0",
            (Flavor::Tuned, false) => "SELECT src FROM in_edges WHERE dst=?",
            (Flavor::Tuned, true) => "SELECT src FROM in_edges WHERE dst=? AND (meta&7)=0",
        };
        self.ids(sql, sym, out)
    }

    /// References made by `sym`; with `calls_only`, just its callees.
    pub fn refs_from(&self, sym: u32, calls_only: bool, out: &mut Vec<u32>) -> Result<()> {
        let sql = match (self.flavor, calls_only) {
            (Flavor::Naive, false) => "SELECT dst FROM edges WHERE src=?",
            (Flavor::Naive, true) => "SELECT dst FROM edges WHERE src=? AND (meta&7)=0",
            (Flavor::Tuned, false) => "SELECT dst FROM out_edges WHERE src=?",
            (Flavor::Tuned, true) => "SELECT dst FROM out_edges WHERE src=? AND (meta&7)=0",
        };
        self.ids(sql, sym, out)
    }

    /// Innermost symbol enclosing `line`; `file` is a path (naive) or file id (tuned).
    pub fn symbol_at(&self, file_id: u32, file_path: &str, line: u32) -> Result<Option<u32>> {
        let sql = "SELECT id FROM symbols WHERE file=?1 AND line<=?2 AND end_line>=?2 AND kind<>0
                   ORDER BY line DESC LIMIT 1";
        let mut st = self.db.prepare_cached(sql)?;
        let mut rows = match self.flavor {
            Flavor::Naive => st.query(params![file_path, line])?,
            Flavor::Tuned => st.query(params![file_id, line])?,
        };
        Ok(match rows.next()? {
            Some(r) => Some(r.get(0)?),
            None => None,
        })
    }
}
