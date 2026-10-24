//! One index of a project, however many ask of it: the editor's language
//! server and every agent's `norn mcp`.
//!
//! The first to hold a project's lock (a file in `~/.norn/run`, locked by
//! the system, so let go of when its process ends however it ends) makes
//! the index and answers the others: it listens on this machine only, and
//! writes beside the lock where, with a word to give, made up at each start.
//! The others hand it the messages of the Model Context Protocol, one line
//! each way. A language server, which must have the index in its own
//! process, asks the holder to give it up (`yield`) and holds it from then.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// What another process asks of the holder.
pub enum Asked {
    /// A message, and where its answer goes (`null` for none).
    Message(Value, Sender<Value>),
    /// A language server wants the index: give it up.
    Yield,
}

/// Where the holder of a project answers.
#[derive(Clone)]
pub struct Endpoint {
    port: u16,
    word: String,
    pub pid: u32,
}

/// This process holds the project: kept as long as it does.
pub struct Held {
    _lock: std::fs::File,
    endpoint: PathBuf,
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

pub enum Claim {
    Held(Held, TcpListener, String),
    /// Another process holds it and answers there.
    Taken(Endpoint),
    /// Another holds it, and does not say where it answers (yet), or is of
    /// another version.
    Busy,
}

/// Where locks and endpoints are kept: one folder for every project of
/// this user (`NORN_RUN_DIR` for another).
fn run_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NORN_RUN_DIR") {
        return PathBuf::from(dir);
    }
    match std::env::home_dir() {
        Some(home) => home.join(".norn").join("run"),
        None => std::env::temp_dir().join("norn-run"),
    }
}

/// The name a project's files are kept under: of the folder as the system
/// names it, the same for every process however it was given.
fn key(root: &Path) -> String {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let text = real.to_string_lossy();
    let text = if cfg!(windows) { text.to_lowercase() } else { text.into_owned() };
    // FNV-1a: the same in every build, as the std's hashers are not said to be.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// A word no one else can guess, for the others to give.
fn word() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u32 {
        // (The std's hasher keys come from the system's randomness.)
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u32(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Hold the project at `root`, or say who does.
pub fn claim(root: &Path) -> std::io::Result<Claim> {
    let dir = run_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let key = key(root);
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!("{key}.lock")))?;
    let endpoint = dir.join(format!("{key}.json"));
    match lock.try_lock() {
        Ok(()) => {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let word = word();
            let info = json!({ "port": listener.local_addr()?.port(), "word": word, "pid": std::process::id(),
                               "version": env!("CARGO_PKG_VERSION"), "root": root.to_string_lossy() });
            let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
            write_private(&tmp, info.to_string().as_bytes())?;
            std::fs::rename(&tmp, &endpoint)?;
            Ok(Claim::Held(Held { _lock: lock, endpoint }, listener, word))
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let info: Value = std::fs::read(&endpoint).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
            match (info["port"].as_u64(), info["word"].as_str()) {
                (Some(port), Some(word)) if info["version"] == env!("CARGO_PKG_VERSION") => {
                    Ok(Claim::Taken(Endpoint { port: port as u16, word: word.to_string(), pid: info["pid"].as_u64().unwrap_or(0) as u32 }))
                }
                _ => Ok(Claim::Busy),
            }
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// Answer the others, each on a thread of its own: what they ask goes to
/// `asked`, to be answered where the index is.
pub fn serve(listener: TcpListener, word: String, asked: impl Fn(Asked) -> bool + Send + Sync + 'static) {
    let asked = std::sync::Arc::new(asked);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (asked, word) = (asked.clone(), word.clone());
            std::thread::spawn(move || {
                let _ = answer(stream, &word, &*asked);
            });
        }
    });
}

fn answer(stream: TcpStream, word: &str, asked: &dyn Fn(Asked) -> bool) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    let mut lines = BufReader::new(stream).lines();
    // First, the word: a process that does not know it is no one to answer.
    let hello: Value = serde_json::from_str(&lines.next().transpose()?.unwrap_or_default()).unwrap_or(Value::Null);
    if hello["word"] != word {
        return Ok(());
    }
    for line in lines {
        let msg: Value = serde_json::from_str(&line?).unwrap_or(Value::Null);
        if msg["yield"] == true {
            asked(Asked::Yield);
            return Ok(());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        if !asked(Asked::Message(msg, tx)) {
            return Ok(());
        }
        let answer = rx.recv().unwrap_or(Value::Null);
        writeln!(out, "{answer}")?;
        out.flush()?;
    }
    Ok(())
}

/// A line to the holder of a project.
pub struct Line {
    out: TcpStream,
    lines: std::io::Lines<BufReader<TcpStream>>,
}

impl Line {
    pub fn open(at: &Endpoint) -> std::io::Result<Line> {
        let stream = TcpStream::connect(("127.0.0.1", at.port))?;
        let mut out = stream.try_clone()?;
        writeln!(out, "{}", json!({ "word": at.word }))?;
        Ok(Line { out, lines: BufReader::new(stream).lines() })
    }

    /// The holder's answer to `msg` (`null` for none); an error if it is gone.
    pub fn ask(&mut self, msg: &Value) -> std::io::Result<Value> {
        writeln!(self.out, "{msg}")?;
        self.out.flush()?;
        let line = self.lines.next().transpose()?.ok_or(std::io::ErrorKind::UnexpectedEof)?;
        serde_json::from_str(&line).map_err(std::io::Error::other)
    }
}

/// Ask the holder of the project to give it up.
pub fn ask_to_yield(at: &Endpoint) {
    if let Ok(mut line) = Line::open(at) {
        let _ = writeln!(line.out, "{}", json!({ "yield": true }));
    }
}

/// The holder's line, once it says where it answers: waited for a while,
/// as one just started writes that a moment after it holds the lock.
/// `Err(true)`: no one holds the project (any more).
pub fn reach(root: &Path, within: Duration) -> Result<(Line, Endpoint), bool> {
    let end = Instant::now() + within;
    loop {
        match claim(root) {
            Ok(Claim::Taken(at)) => {
                if let Ok(line) = Line::open(&at) {
                    return Ok((line, at));
                }
            }
            // (Held: let go at once; the caller decides whether to hold it.)
            Ok(Claim::Held(..)) => return Err(true),
            Ok(Claim::Busy) | Err(_) => {}
        }
        if Instant::now() >= end {
            return Err(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
