//! The member hints extraction recorded for one file: `hints <root> <file>`.
use si_core::model::NONE;
fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(args.next().expect("root"));
    let rel = args.next().expect("file");
    let files = si_core::scan(&root);
    let sets = si_core::prescan(&files);
    let mut ex = si_core::extract::Extractor::new();
    let f = si_core::extract_file(&mut ex, &root, &root.join(&rel), &sets);
    let name = |i: u32| if i == NONE { "-" } else { f.name(i as usize) };
    let refs: Vec<_> = f.refs.iter().collect();
    for &(r, h) in &f.ref_hints {
        let (r, h) = (&refs[r as usize], &f.hints[h as usize]);
        let path: Vec<&str> = f.hint_path[h.path as usize..(h.path + h.len) as usize].iter().map(|&m| name(m)).collect();
        println!("ref {}:{} kind {} <- root {} {} path {:?}", r.line, name(r.name), r.kind, h.root_kind, name(h.root), path);
    }
    for b in &f.binds {
        println!("bind {}: {} <- {} hint {}", b.line, name(b.slot), name(b.func), b.hint as i32);
    }
    for &(d, t) in &f.typed {
        println!("typed {} : {}", name(f.defs[d as usize].name), name(t));
    }
    for r in &f.records {
        println!("record {} fields {:?} types {:?} defs {:?}", name(r.name), r.fields.iter().map(|&x| name(x)).collect::<Vec<_>>(), r.types.iter().map(|&x| name(x)).collect::<Vec<_>>(), r.defs);
    }
}
