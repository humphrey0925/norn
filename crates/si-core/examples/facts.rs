//! What extraction recorded for one file: `facts <root> <relative path> [pattern]`.
use si_core::model::NONE;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().expect("root"));
    let rel = args.next().expect("file");
    let pat = args.next().unwrap_or_default();
    let files = si_core::scan(&root);
    let sets = si_core::prescan(&files);
    let mut ex = si_core::extract::Extractor::new();
    let f = si_core::extract_file(&mut ex, &root, &root.join(&rel), &sets);
    let name = |i: u32| if i == NONE { "-" } else { f.name(i as usize) };
    println!("{} defs, {} refs, {} sites, {} binds, {} inits, {} records", f.defs.len(), f.refs.len(), f.sites.len(), f.binds.len(), f.inits.len(), f.records.len());
    for s in f.sites.iter().filter(|s| name(s.name).contains(&pat)).take(12) {
        println!("site {}:{} {} flags {:#x} def {}", s.line, name(s.name), name(s.args).replace('\n', " "), s.flags, s.def as i32);
    }
    for b in f.binds.iter().filter(|b| name(b.func).contains(&pat) || name(b.slot).contains(&pat)).take(20) {
        println!("bind {}: {}[{}] <- {} var {}", b.line, name(b.slot), b.index as i32, name(b.func), b.var);
    }
    for i in f.inits.iter().take(8) {
        println!("init {}: type {} array {} {} bytes", i.line, name(i.type_name), i.array, name(i.text).len());
    }
    for r in f.records.iter().filter(|r| name(r.name).contains(&pat)).take(4) {
        println!("record {} {} fields: {:?}", name(r.name), r.fields.len(), r.fields.iter().take(12).map(|&x| name(x)).collect::<Vec<_>>());
    }
    for m in f.macros.iter().filter(|m| name(m.name).contains(&pat)).take(6) {
        println!("macro {} {}", name(m.name), name(m.text).replace('\n', " "));
    }
    println!("expanding: {}", sets.is_expanding(pat.as_bytes()));
}
