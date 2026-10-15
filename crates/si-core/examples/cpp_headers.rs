//! The headers under a folder that are read as C++: `cpp_headers <dir>`.
fn main() {
    let dir = std::env::args().nth(1).expect("dir");
    for p in si_core::scan(std::path::Path::new(&dir)) {
        let name = p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
        if si_core::is_header(&name) && si_core::extract::is_cpp(&name, &std::fs::read(&p).unwrap_or_default()) {
            println!("{name}");
        }
    }
}
