//! `si`: thin CLI over si-core. Every command prints one JSON object so the
//! benchmark harness can consume it directly.

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use si_core::graph::Graph;
use si_core::model::{flag, kind, refkind, EdgeHalf, FileFacts};
use si_core::packed::PackedGraph;
use si_core::sqlite::{self, Flavor, SqliteStore};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

mod lsp;
mod mem;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "norn", about = "Norn: a Source-Insight-style code index for C")]
struct Cli {
    /// Worker threads (0 = all cores).
    #[arg(long, global = true, default_value_t = 0)]
    threads: usize,
    /// Benchmark knob: let tree-sitter use the platform heap instead of mimalloc.
    #[arg(long, global = true)]
    system_parser_alloc: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index a source tree and write the selected stores.
    Index {
        root: PathBuf,
        #[arg(long, default_value = "out")]
        out: PathBuf,
        /// Comma-separated: csr, csr-packed, sqlite-naive, sqlite-tuned, none.
        #[arg(long, default_value = "csr")]
        stores: String,
        /// Index only every k-th file (for scaling measurements).
        #[arg(long, default_value_t = 1)]
        stride: usize,
        /// `FROM=TO`: an `#include` of a file named FROM reads TO instead,
        /// for a header the build generates from a template. Repeatable.
        #[arg(long, value_name = "FROM=TO")]
        include_alias: Vec<String>,
        /// Also write `binds.tsv`: where each function is stored in a member
        /// or variable (for accuracy checks).
        #[arg(long)]
        dump_binds: bool,
    },
    /// One-shot query against a CSR index: find | callers | callees | refs | tree.
    Query {
        dir: PathBuf,
        op: String,
        name: String,
        #[arg(long, default_value_t = 3)]
        depth: u32,
    },
    /// In-process query latency for each store present in `dir`.
    BenchQuery {
        dir: PathBuf,
        #[arg(long, default_value_t = 2000)]
        n: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// In-process incremental update cost: re-extract one file, relink.
    BenchIncr {
        root: PathBuf,
        #[arg(long, default_value_t = 30)]
        n: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Names of randomly sampled function definitions (benchmark workload).
    Sample {
        dir: PathBuf,
        #[arg(long, default_value_t = 200)]
        n: usize,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Line-delimited JSON query server on stdin/stdout.
    Serve { dir: PathBuf },
    /// The same over the sources themselves, kept up to date: besides the
    /// queries, `{"op":"update","files":[...]}` after files change.
    Live {
        root: PathBuf,
        #[arg(long, value_name = "FROM=TO")]
        include_alias: Vec<String>,
        #[command(flatten)]
        keep: Keep,
    },
    /// Language Server over stdin/stdout, for editors. The root is the
    /// workspace the editor names unless one is given here.
    Lsp {
        root: Option<PathBuf>,
        #[arg(long, value_name = "FROM=TO")]
        include_alias: Vec<String>,
        #[command(flatten)]
        keep: Keep,
    },
    /// Every symbol as `file<TAB>line<TAB>kind<TAB>decl|def<TAB>name` (for accuracy checks).
    Symbols { dir: PathBuf },
    /// Every call edge, tab-separated (for accuracy checks).
    Calls { dir: PathBuf },
}

/// Whether and where a live index is kept between runs.
#[derive(clap::Args, Clone)]
struct Keep {
    /// Where to keep the index between runs (default: `.si/cache` under
    /// the root).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Index from scratch every time and keep nothing.
    #[arg(long)]
    no_cache: bool,
    /// Throw away what is kept, index from scratch, and keep that.
    #[arg(long)]
    rebuild: bool,
}

impl Keep {
    fn open(&self, root: &Path, options: &si_core::link::Options) -> (si_core::session::Session, si_core::session::Update) {
        use si_core::session::{Session, Update};
        if self.no_cache {
            let s = Session::open(root, options.clone());
            let extracted = s.files();
            return (s, Update { extracted, ..Update::default() });
        }
        let dir = self.cache_dir.clone().unwrap_or_else(|| si_core::store::Store::default_dir(root));
        if self.rebuild {
            si_core::store::Store::new(dir.clone()).invalidate();
        }
        Session::open_cached(root, options.clone(), dir)
    }
}

impl Keep {
    /// What the last session saved to answer with at once.
    fn early(&self, root: &Path) -> Option<si_core::session::Session> {
        if self.no_cache || self.rebuild {
            return None;
        }
        let dir = self.cache_dir.clone().unwrap_or_else(|| si_core::store::Store::default_dir(root));
        si_core::session::Session::early(root, dir)
    }

    /// As `open`, with the saving left to be done after (`save_some`).
    fn open_deferred(&self, root: &Path, options: &si_core::link::Options, have: Option<u64>) -> (si_core::session::Session, si_core::session::Update) {
        use si_core::session::Session;
        if self.no_cache {
            return self.open(root, options);
        }
        let dir = self.cache_dir.clone().unwrap_or_else(|| si_core::store::Store::default_dir(root));
        if self.rebuild {
            si_core::store::Store::new(dir.clone()).invalidate();
        }
        Session::open_deferred(root, options.clone(), dir, have)
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// xorshift64*: deterministic sampling without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn pctl(sorted: &[u64], p: f64) -> u64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn summary(mut ns: Vec<u64>) -> Value {
    ns.sort_unstable();
    let mean = ns.iter().sum::<u64>() as f64 / ns.len() as f64;
    json!({ "n": ns.len(), "p50_ns": pctl(&ns, 0.50), "p99_ns": pctl(&ns, 0.99), "max_ns": ns[ns.len() - 1], "mean_ns": mean })
}

fn is_call(e: &&EdgeHalf) -> bool {
    e.kind() == refkind::CALL
}

fn sym_json(g: &Graph, id: u32) -> Value {
    let s = &g.syms[id as usize];
    json!({ "name": g.sym_name(id), "kind": kind::name(s.kind), "file": g.file_path(s.file), "line": s.line,
            "decl": s.flags & flag::DECL != 0 })
}

fn index(root: &Path, out: &Path, stores: &str, stride: usize, options: &si_core::link::Options, dump_binds: bool) -> Value {
    let total = Instant::now();
    let t = Instant::now();
    // The project's own settings, if it has any: which folders, and what
    // the build defines.
    let scope = si_core::scope::Scope::load(root);
    let files: Vec<PathBuf> = si_core::scan_scope(root, &scope).into_iter().step_by(stride.max(1)).collect();
    let mut conditions = si_core::conditions_of(root, &files);
    let scan_ms = ms(t);

    let t = Instant::now();
    let transparent = si_core::prescan_under(root, &files, &mut conditions);
    let prescan_ms = ms(t);

    let t = Instant::now();
    let facts = si_core::extract_all(root, &files, &transparent);
    let extract_ms = ms(t);
    let facts_bytes: usize = facts.iter().map(FileFacts::heap_bytes).sum();

    let t = Instant::now();
    let (g, ls) = si_core::link::link_with(&facts, options);
    let link_ms = ms(t);

    std::fs::create_dir_all(out).expect("create output dir");
    if dump_binds {
        let mut w = std::io::BufWriter::new(std::fs::File::create(out.join("binds.tsv")).expect("create binds.tsv"));
        for &[file, line, slot, func] in &ls.bind_sites {
            let f = &g.syms[func as usize];
            let what = kind::name(g.syms[slot as usize].kind);
            writeln!(w, "{}\t{line}\t{}\t{}\t{}\t{what}", g.file_path(file), g.sym_name(slot), g.sym_name(func), g.file_path(f.file))
                .expect("write binds.tsv");
        }
    }
    let mut store_stats = serde_json::Map::new();
    for store in stores.split(',').filter(|s| !s.is_empty() && *s != "none") {
        let t = Instant::now();
        let path = match store {
            "csr" => {
                let p = out.join("graph.si");
                g.save(&p).expect("write csr");
                p
            }
            "csr-packed" => {
                let p = out.join("graph.sip");
                PackedGraph::from_graph(&g).save(&p).expect("write packed csr");
                p
            }
            "sqlite-naive" | "sqlite-tuned" => {
                let p = out.join(format!("{store}.db"));
                let flavor = if store == "sqlite-naive" { Flavor::Naive } else { Flavor::Tuned };
                sqlite::write(&g, &p, flavor).expect("write sqlite");
                p
            }
            other => panic!("unknown store {other}"),
        };
        store_stats.insert(store.to_string(), json!({ "write_ms": ms(t), "bytes": file_len(&path) }));
    }

    let slowest = facts.iter().max_by_key(|f| f.parse_ns + f.walk_ns).expect("no source files");
    let (rss, peak_rss) = mem::rss();
    json!({
        "files": facts.len(),
        "bytes": facts.iter().map(|f| f.bytes as u64).sum::<u64>(),
        "lines": facts.iter().map(|f| f.defs[0].end_line as u64).sum::<u64>(),
        "files_with_parse_errors": facts.iter().filter(|f| f.has_error).count(),
        "threads": rayon::current_num_threads(),
        "transparent_macros": transparent.len(),
        "conditions_known": conditions.len(),
        "prescan_steps": si_core::PRESCAN_STEPS.lock().unwrap().iter().map(|(what, ms)| json!([what, ms])).collect::<Vec<_>>(),
        "prescan_slowest": si_core::PRESCAN_SLOWEST.lock().unwrap().iter().map(|(what, path, ms, bytes)| json!([what, path, ms, bytes])).collect::<Vec<_>>(),
        "expanding_macros": transparent.expanding_len(),
        "expansion": { "sites": ls.sites, "defs_added": ls.expanded_defs, "refs_added": ls.expanded_refs },
        "names": g.n_names(),
        "symbols": g.syms.len(),
        "edges": ls.edges,
        "refs": { "total": ls.refs, "resolved": ls.resolved, "ambiguous": ls.ambiguous,
                  "unresolved": ls.unresolved, "capped": ls.capped,
                  "indirect_calls": facts.iter().map(|f| f.indirect_calls as u64).sum::<u64>() },
        "includes": { "found": ls.includes_found, "missing": ls.includes_missing },
        "ms": { "scan": scan_ms, "prescan": prescan_ms, "extract": extract_ms, "link": link_ms,
                "link_intern": ls.intern_ms, "link_includes": ls.include_ms, "link_expand": ls.expand_ms, "link_syms": ls.syms_ms, "link_resolve": ls.resolve_ms, "link_csr": ls.csr_ms,
                "total": ms(total),
                "cpu_mask": facts.iter().map(|f| f.mask_ns).sum::<u64>() as f64 / 1e6,
                "cpu_parse": facts.iter().map(|f| f.parse_ns).sum::<u64>() as f64 / 1e6,
                "cpu_walk": facts.iter().map(|f| f.walk_ns).sum::<u64>() as f64 / 1e6,
                "slowest_file": (slowest.parse_ns + slowest.walk_ns) as f64 / 1e6 },
        "slowest_file": slowest.path,
        "mem": { "facts_heap_bytes": facts_bytes, "graph_heap_bytes": g.heap_bytes(), "rss_bytes": rss, "peak_rss_bytes": peak_rss },
        "stores": store_stats,
    })
}

fn query(dir: &Path, op: &str, name: &str, depth: u32) -> Value {
    let g = Graph::load(&dir.join("graph.si")).expect("load graph.si");
    run_query(&g, op, name, depth)
}

fn run_query(g: &Graph, op: &str, name: &str, depth: u32) -> Value {
    let syms = g.find(name);
    match op {
        "find" => json!({ "results": syms.iter().map(|&s| sym_json(g, s)).collect::<Vec<_>>() }),
        // Fragments in any order, initials, letters in order: see si_core::search.
        "search" => {
            let t = Instant::now();
            let hits = si_core::search::search(g, name, 50);
            let results: Vec<Value> = hits
                .iter()
                .map(|&(score, s)| {
                    let mut v = sym_json(g, s);
                    v["score"] = json!(score);
                    v
                })
                .collect();
            json!({ "count": results.len(), "ms": ms(t), "results": results })
        }
        "callers" => {
            let mut results = Vec::new();
            for &s in syms {
                for c in si_core::callers_through_macros(g, s) {
                    let mut v = sym_json(g, c.caller);
                    v["at_line"] = json!(c.line);
                    v["certain"] = json!(!c.ambiguous);
                    if let Some(m) = c.via {
                        v["via_macro"] = json!(g.sym_name(m));
                    }
                    if let Some(slot) = c.slot {
                        v["through"] = json!(g.sym_name(slot));
                    }
                    results.push(v);
                }
            }
            json!({ "count": results.len(), "results": results })
        }
        "callees" => {
            let mut results = Vec::new();
            for &s in syms {
                for c in si_core::callees_through_slots(g, s) {
                    let mut v = sym_json(g, c.caller);
                    v["at_line"] = json!(c.line);
                    v["certain"] = json!(!c.ambiguous);
                    if let Some(slot) = c.slot {
                        v["through"] = json!(g.sym_name(slot));
                    }
                    results.push(v);
                }
            }
            json!({ "count": results.len(), "results": results })
        }
        "refs" => {
            let mut results = Vec::new();
            for &s in syms {
                let edges: Vec<&EdgeHalf> = g.refs_to(s).iter().collect();
                for e in edges {
                    let mut v = sym_json(g, e.other);
                    v["at_line"] = json!(e.line());
                    v["certain"] = json!(!e.ambiguous());
                    results.push(v);
                }
            }
            json!({ "count": results.len(), "results": results })
        }
        "tree" => {
            let reached: usize = syms
                .iter()
                .map(|&s| si_core::bfs(s, depth, |x, out| out.extend(si_core::callees_through_slots(g, x).iter().map(|c| c.caller))))
                .sum();
            json!({ "reached": reached })
        }
        other => json!({ "error": format!("unknown op {other}") }),
    }
}

/// `n` function definitions drawn uniformly (with replacement), plus the population size.
fn sample_functions(g: &Graph, n: usize, seed: u64) -> (Vec<u32>, usize) {
    let fns: Vec<u32> = (0..g.syms.len() as u32)
        .filter(|&s| g.syms[s as usize].kind == kind::FUNCTION && g.syms[s as usize].flags & flag::DECL == 0)
        .collect();
    let mut rng = Rng(seed | 1);
    ((0..n).map(|_| fns[rng.below(fns.len())]).collect(), fns.len())
}

fn sample(dir: &Path, n: usize, seed: u64) -> Value {
    let g = Graph::load(&dir.join("graph.si")).expect("load graph.si");
    let (sample, _) = sample_functions(&g, n, seed);
    json!(sample.iter().map(|&s| g.sym_name(s)).collect::<Vec<_>>())
}

fn bench_query(dir: &Path, n: usize, seed: u64) -> Value {
    let t = Instant::now();
    let g = Graph::load(&dir.join("graph.si")).expect("load graph.si");
    let load_ms = ms(t);
    let (sample, n_fns) = sample_functions(&g, n, seed);

    let mut out = serde_json::Map::new();
    out.insert("functions".into(), json!(n_fns));

    // Each op is timed per query; `sink` keeps the work observable.
    let mut sink = 0usize;
    macro_rules! time_op {
        ($body:expr) => {{
            let mut ns = Vec::with_capacity(sample.len());
            for &s in &sample {
                let t = Instant::now();
                sink += $body(s);
                ns.push(t.elapsed().as_nanos() as u64);
            }
            summary(ns)
        }};
    }

    let csr = json!({
        "load_ms": load_ms,
        "heap_bytes": g.heap_bytes(),
        "find": time_op!(|s: u32| g.find(g.sym_name(s)).len()),
        "callers": time_op!(|s: u32| g.refs_to(s).iter().filter(is_call).count()),
        "callees": time_op!(|s: u32| g.refs_from(s).iter().filter(is_call).count()),
        "refs": time_op!(|s: u32| g.refs_to(s).len()),
        "tree3": time_op!(|s: u32| si_core::bfs(s, 3, |x, o| o.extend(g.refs_from(x).iter().filter(is_call).map(|e| e.other)))),
        "symbol_at": time_op!(|s: u32| {
            let sym = &g.syms[s as usize];
            g.symbol_at(sym.file, (sym.line + sym.end_line) / 2).is_some() as usize
        }),
    });
    out.insert("csr".into(), csr);

    let packed = dir.join("graph.sip");
    if packed.exists() {
        let t = Instant::now();
        let pg = PackedGraph::load(&packed).expect("load graph.sip");
        let load_ms = ms(t);
        let call = |e: &EdgeHalf| e.kind() == refkind::CALL;
        let v = json!({
            "load_ms": load_ms,
            "heap_bytes": pg.heap_bytes(),
            "find": time_op!(|s: u32| pg.base.find(pg.base.sym_name(s)).len()),
            "callers": time_op!(|s: u32| pg.inc.iter(s).filter(call).count()),
            "callees": time_op!(|s: u32| pg.out.iter(s).filter(call).count()),
            "refs": time_op!(|s: u32| pg.inc.iter(s).count()),
            "tree3": time_op!(|s: u32| si_core::bfs(s, 3, |x, o| o.extend(pg.out.iter(x).filter(call).map(|e| e.other)))),
            "symbol_at": time_op!(|s: u32| {
                let sym = &pg.base.syms[s as usize];
                pg.base.symbol_at(sym.file, (sym.line + sym.end_line) / 2).is_some() as usize
            }),
        });
        out.insert("csr-packed".into(), v);
    }

    for (name, flavor) in [("sqlite-naive", Flavor::Naive), ("sqlite-tuned", Flavor::Tuned)] {
        let path = dir.join(format!("{name}.db"));
        if !path.exists() {
            continue;
        }
        let t = Instant::now();
        let db = SqliteStore::open(&path, flavor).expect("open sqlite");
        let open_ms = ms(t);
        let mut buf = Vec::new();
        let v = json!({
            "load_ms": open_ms,
            "find": time_op!(|s: u32| { buf.clear(); db.find(g.sym_name(s), &mut buf).unwrap(); buf.len() }),
            "callers": time_op!(|s: u32| { buf.clear(); db.refs_to(s, true, &mut buf).unwrap(); buf.len() }),
            "callees": time_op!(|s: u32| { buf.clear(); db.refs_from(s, true, &mut buf).unwrap(); buf.len() }),
            "refs": time_op!(|s: u32| { buf.clear(); db.refs_to(s, false, &mut buf).unwrap(); buf.len() }),
            "tree3": time_op!(|s: u32| si_core::bfs(s, 3, |x, o| db.refs_from(x, true, o).unwrap())),
            "symbol_at": time_op!(|s: u32| {
                let sym = &g.syms[s as usize];
                db.symbol_at(sym.file, g.file_path(sym.file), (sym.line + sym.end_line) / 2).unwrap().is_some() as usize
            }),
        });
        out.insert(name.into(), v);
    }
    out.insert("checksum".into(), json!(sink));
    Value::Object(out)
}

fn bench_incr(root: &Path, n: usize, seed: u64) -> Value {
    let files = si_core::scan(root);
    let transparent = si_core::prescan(&files);
    let mut facts = si_core::extract_all(root, &files, &transparent);
    let options = si_core::link::Options::default();
    let mut cache = si_core::expand::Cache::default();
    // The graph is held from one link to the next, as a session holds it:
    // the memory reported at the end is that of an index in use.
    let (mut g, _) = si_core::link::link_cached(&facts, &options, &mut cache);
    let edges = g.out.len();

    let mut ex = si_core::extract::Extractor::new();
    let mut rng = Rng(seed | 1);
    let (mut extract_ns, mut link_ns, mut total_ns) = (Vec::new(), Vec::new(), Vec::new());
    // Where the last relink spent its time, in milliseconds.
    let mut steps = Value::Null;
    for _ in 0..n {
        let i = rng.below(files.len());
        let t0 = Instant::now();
        facts[i] = si_core::extract_file(&mut ex, root, &files[i], &transparent);
        let t1 = Instant::now();
        // The content is the same, but an edit would not be: expand it again.
        cache.forget(i);
        drop(std::mem::take(&mut g));
        let stats;
        (g, stats) = si_core::link::link_cached(&facts, &options, &mut cache);
        let t2 = Instant::now();
        steps = json!({ "includes": stats.include_ms, "expand": stats.expand_ms, "intern": stats.intern_ms, "syms": stats.syms_ms,
                        "resolve": stats.resolve_ms, "csr": stats.csr_ms });
        assert_eq!(g.out.len(), edges, "relink of unchanged content must be stable");
        extract_ns.push((t1 - t0).as_nanos() as u64);
        link_ns.push((t2 - t1).as_nanos() as u64);
        total_ns.push((t2 - t0).as_nanos() as u64);
    }
    let (rss, peak_rss) = mem::rss();
    json!({
        "files": files.len(), "edges": edges, "threads": rayon::current_num_threads(),
        "reextract_one_file": summary(extract_ns), "full_relink": summary(link_ns), "total": summary(total_ns),
        "mem": { "rss_bytes": rss, "peak_rss_bytes": peak_rss }, "last_relink_ms": steps,
    })
}

fn aliases(given: &[String]) -> Vec<(String, String)> {
    given.iter().map(|a| a.split_once('=').map(|(from, to)| (from.to_string(), to.to_string())).expect("FROM=TO")).collect()
}

/// `{"op":"definition","file":...,"line":...,"name":...}`: what the name
/// written there refers to.
fn definition(g: &Graph, req: &Value) -> Value {
    let (file, line, name) = (req["file"].as_str().unwrap_or(""), req["line"].as_u64().unwrap_or(0), req["name"].as_str().unwrap_or(""));
    let found = si_core::definition_at(g, &file.replace('\\', "/"), line as u32, name);
    json!({ "results": found.iter().map(|&s| sym_json(g, s)).collect::<Vec<_>>() })
}

fn answer(g: &Graph, req: &Value) -> Value {
    let op = req["op"].as_str().unwrap_or("");
    if op == "definition" {
        return definition(g, req);
    }
    run_query(g, op, req["name"].as_str().unwrap_or(""), req["depth"].as_u64().unwrap_or(3) as u32)
}

fn live(root: &Path, keep: &Keep, options: &si_core::link::Options) {
    let t = Instant::now();
    let (mut session, opened) = keep.open(root, options);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let ready = json!({ "ready": true, "files": session.files(), "symbols": session.graph.syms.len(), "ms": ms(t),
                        "loaded": opened.loaded, "extracted": opened.extracted, "link_ms": opened.link_ms,
                        "cache": session.cache_dir().map(|d| d.to_string_lossy().into_owned()) });
    writeln!(out, "{ready}").and_then(|_| out.flush()).expect("write response");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let resp = if req["op"] == "sync" || req["op"] == "scope" {
            // `scope` with `include`/`exclude` sets the project's folders
            // (kept in .si/config.json); both bring the index in line with
            // the disk.
            let list = |key: &str| -> Vec<String> {
                req[key].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_string).collect()
            };
            let up = if req["op"] == "scope" {
                session.set_scope(si_core::scope::Scope { include: list("include"), exclude: list("exclude") }).unwrap_or_default()
            } else {
                session.sync()
            };
            json!({ "added": up.added, "removed": up.removed, "changed": up.changed, "extracted": up.extracted,
                    "files": session.files(), "symbols": session.graph.syms.len(),
                    "include": session.scope().include, "exclude": session.scope().exclude,
                    "extract_ms": up.extract_ms, "link_ms": up.link_ms })
        } else if req["op"] == "update" {
            let files: Vec<PathBuf> = req["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).map(PathBuf::from).collect();
            let up = session.update(&files);
            json!({ "changed": up.changed, "added": up.added, "removed": up.removed, "extracted": up.extracted,
                    "macros_reclassified": up.macros_reclassified, "extract_ms": up.extract_ms, "link_ms": up.link_ms,
                    "symbols": session.graph.syms.len() })
        } else {
            answer(&session.graph, &req)
        };
        writeln!(out, "{resp}").and_then(|_| out.flush()).expect("write response");
    }
}

fn serve(dir: &Path) {
    let g = Graph::load(&dir.join("graph.si")).expect("load graph.si");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let resp = answer(&g, &req);
        writeln!(out, "{resp}").and_then(|_| out.flush()).expect("write response");
    }
}

fn symbols(dir: &Path) {
    let g = Graph::load(&dir.join("graph.si")).expect("load graph.si");
    let q = si_core::graph::Qualifier::new(&g);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for (id, s) in g.syms.iter().enumerate().filter(|(_, s)| s.kind != kind::FILE) {
        let decl = if s.flags & flag::DECL != 0 { "decl" } else { "def" };
        writeln!(out, "{}	{}	{}	{decl}	{}", g.file_path(s.file), s.line, kind::name(s.kind), q.name(&g, id as u32))
            .expect("write symbols");
    }
}

/// Every call the index knows, one per line: call site file and line, then
/// the calling and the called function as `name`, `file`, `line`, then
/// `certain|ambiguous`, `decl|def` for the callee, the macro, member or
/// variable the call was made through (or `-`), and `direct|indirect`.
fn calls(dir: &Path) {
    let g = Graph::load(&dir.join("graph.si")).expect("load graph.si");
    let q = si_core::graph::Qualifier::new(&g);
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for (id, s) in g.syms.iter().enumerate().filter(|(_, s)| s.kind == kind::FUNCTION) {
        let decl = if s.flags & flag::DECL != 0 { "decl" } else { "def" };
        for c in si_core::callers_through_macros(&g, id as u32) {
            let from = &g.syms[c.caller as usize];
            if from.kind != kind::FUNCTION {
                continue;
            }
            let certain = if c.ambiguous { "ambiguous" } else { "certain" };
            let via = c.slot.or(c.via).map_or(if c.pointer { "*" } else { "-" }, |m| g.sym_name(m));
            let how = if c.indirect() { "indirect" } else { "direct" };
            writeln!(out, "{}	{}	{}	{}	{}	{}	{}	{}	{certain}	{decl}	{via}	{how}",
                     g.file_path(from.file), c.line, q.name(&g, c.caller), g.file_path(from.file), from.line,
                     q.name(&g, id as u32), g.file_path(s.file), s.line)
                .expect("write calls");
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if cli.threads > 0 {
        rayon::ThreadPoolBuilder::new().num_threads(cli.threads).build_global().expect("thread pool");
    }
    if !cli.system_parser_alloc {
        si_core::use_mimalloc_for_parser();
    }
    let result = match &cli.cmd {
        Cmd::Index { root, out, stores, stride, include_alias, dump_binds } => {
            index(root, out, stores, *stride, &si_core::link::Options { include_aliases: aliases(include_alias) }, *dump_binds)
        }
        Cmd::Query { dir, op, name, depth } => query(dir, op, name, *depth),
        Cmd::BenchQuery { dir, n, seed } => bench_query(dir, *n, *seed),
        Cmd::BenchIncr { root, n, seed } => bench_incr(root, *n, *seed),
        Cmd::Sample { dir, n, seed } => sample(dir, *n, *seed),
        Cmd::Serve { dir } => return serve(dir),
        Cmd::Lsp { root, include_alias, keep } => {
            let options = si_core::link::Options { include_aliases: aliases(include_alias) };
            let keep = keep.clone();
            let early = keep.clone();
            return lsp::run(root.as_deref(), move |dir: &Path| early.early(dir), move |dir: &Path, have| keep.open_deferred(dir, &options, have));
        }
        Cmd::Live { root, include_alias, keep } => {
            return live(root, keep, &si_core::link::Options { include_aliases: aliases(include_alias) })
        }
        Cmd::Symbols { dir } => return symbols(dir),
        Cmd::Calls { dir } => return calls(dir),
    };
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
}
