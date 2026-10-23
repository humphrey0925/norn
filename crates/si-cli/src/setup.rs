//! `norn setup`: the index's tools for the agents on this machine, each set
//! up where it reads its MCP servers. One server for all of them, at a
//! path that does not move: `~/.norn/bin/norn`, a copy of this program
//! that a newer one (an update of the VS Code extension) keeps up to date.
//!
//! Every agent starts it as `norn mcp`, with no folder: which project is
//! the one the agent works in (see `crate::mcp`).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The agents set up, as they are named on the command line.
pub const AGENTS: &[&str] = &["claude", "codex", "opencode"];

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Where the copy every agent starts is kept.
pub fn installed() -> PathBuf {
    home().join(".norn").join("bin").join(if cfg!(windows) { "norn.exe" } else { "norn" })
}

/// The version a `norn` says it is (`norn 0.25.0`), if it runs.
fn version_of(exe: &Path) -> Option<Vec<u64>> {
    let out = Command::new(exe).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let word = text.split_whitespace().nth(1)?;
    word.split('.').map(|n| n.parse().ok()).collect()
}

/// Put this program at `installed()`, unless one as new or newer is there:
/// an older copy of the extension's server does not replace a newer one.
/// Says what it did.
pub fn install() -> std::io::Result<String> {
    let to = installed();
    let me = std::env::current_exe()?;
    if std::fs::canonicalize(&me).ok() == std::fs::canonicalize(&to).ok() {
        return Ok(format!("{} is this program", to.display()));
    }
    let mine: Vec<u64> = env!("CARGO_PKG_VERSION").split('.').filter_map(|n| n.parse().ok()).collect();
    if let Some(there) = version_of(&to) {
        if there >= mine {
            return Ok(format!("{} is there already ({})", to.display(), there.iter().map(u64::to_string).collect::<Vec<_>>().join(".")));
        }
    }
    std::fs::create_dir_all(to.parent().unwrap())?;
    // An agent may be running the copy there: a running program can be
    // renamed (on Windows too), not written over.
    let old = to.with_extension("old");
    let _ = std::fs::remove_file(&old);
    if to.exists() {
        std::fs::rename(&to, &old)?;
    }
    let tmp = to.with_extension("tmp");
    std::fs::copy(&me, &tmp)?;
    std::fs::rename(&tmp, &to)?;
    let _ = std::fs::remove_file(&old);
    Ok(format!("{} is version {}", to.display(), env!("CARGO_PKG_VERSION")))
}

/// A program on the PATH, as the shell would find it.
fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| exts.iter().map(move |x| dir.join(format!("{name}{x}"))))
        .find(|p| p.is_file())
}

/// Run a program found on the PATH (a `.cmd` through the shell, as npm
/// installs them on Windows); its output, or why it failed.
fn run(exe: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = if exe.extension().is_some_and(|x| x == "cmd" || x == "bat") {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(exe);
        c
    } else {
        Command::new(exe)
    };
    let out = command.args(args).output().map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).trim().to_string();
    if out.status.success() { Ok(text) } else { Err(text) }
}

/// The skill that tells an agent how to go about C and C++ with Norn:
/// the build configuration first, then the tools rather than grep.
pub const SKILL: &str = include_str!("../../../skills/norn/SKILL.md");

/// Where the skill is put for `agent`: Claude Code reads `~/.claude/skills`;
/// Codex, OpenCode and VS Code's Copilot read `~/.agents/skills` (OpenCode
/// and Copilot read both, and take a skill of one name once).
fn skill_dir(agent: &str) -> PathBuf {
    let base = if agent == "claude" { home().join(".claude") } else { home().join(".agents") };
    base.join("skills").join("norn")
}

/// Whether the skill at `dir` is Norn's own, of this version or another
/// (not one of the user's that happens to have its name): by its title,
/// which every version keeps.
fn skill_is_ours(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("SKILL.md")).is_ok_and(|t| t.starts_with("---\nname: norn\n") && t.contains("\n# Navigating C and C++ with Norn\n"))
}

/// Put the skill at `dir`, or take it out (`remove`). Says what it did.
fn skill(dir: &Path, remove: bool) -> Done {
    let file = dir.join("SKILL.md");
    let at = file.display().to_string();
    let there = std::fs::read_to_string(&file).ok();
    if remove {
        if !skill_is_ours(dir) {
            return Done::NotThere;
        }
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(dir);
        return Done::Removed(at);
    }
    match there {
        Some(t) if t == SKILL => Done::Already(at),
        Some(_) if !skill_is_ours(dir) => Done::ByHand(format!("{at} is a skill of another named norn; left as it is")),
        _ => match std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&file, SKILL)) {
            Ok(()) => Done::Set(at),
            Err(e) => Done::ByHand(format!("write {at} ({e})")),
        },
    }
}

/// What became of one agent.
pub enum Done {
    Set(String),
    Already(String),
    Removed(String),
    NotThere,
    /// Not done, and what to do instead.
    ByHand(String),
}

fn codex_config() -> PathBuf {
    std::env::var_os("CODEX_HOME").map_or_else(|| home().join(".codex"), PathBuf::from).join("config.toml")
}

fn opencode_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home().join(".config"), PathBuf::from).join("opencode")
}

/// Whether an agent is on this machine: its program, or its settings.
pub fn present(agent: &str) -> bool {
    which(agent).is_some()
        || match agent {
            "claude" => home().join(".claude.json").exists() || home().join(".claude").is_dir(),
            "codex" => codex_config().parent().is_some_and(Path::is_dir),
            "opencode" => opencode_dir().is_dir(),
            _ => false,
        }
}

/// Set Norn up for `agent`, or take it out (`remove`).
pub fn agent(agent: &str, exe: &Path, remove: bool) -> Done {
    match agent {
        "claude" => claude(exe, remove),
        "codex" => codex(exe, remove),
        "opencode" => opencode(exe, remove),
        _ => Done::NotThere,
    }
}

/// Claude Code (in a terminal, in VS Code, in the desktop app: one set of
/// servers for all), through its own command: its settings file is one it
/// writes itself while it runs.
fn claude(exe: &Path, remove: bool) -> Done {
    let command = format!("claude mcp add --scope user norn -- \"{}\" mcp", exe.display());
    let Some(claude) = which("claude") else {
        return if remove { Done::ByHand("claude mcp remove --scope user norn".into()) } else { Done::ByHand(command) };
    };
    let had = run(&claude, &["mcp", "remove", "--scope", "user", "norn"]).is_ok();
    if remove {
        return if had { Done::Removed("claude mcp remove --scope user norn".into()) } else { Done::NotThere };
    }
    match run(&claude, &["mcp", "add", "--scope", "user", "norn", "--", &exe.to_string_lossy(), "mcp"]) {
        Ok(_) => Done::Set("for every project (claude mcp add --scope user)".into()),
        Err(e) => Done::ByHand(format!("{command}\n    ({e})")),
    }
}

/// A TOML string of `s`: literal where it can be (Windows paths keep their
/// backslashes as they are).
fn toml_string(s: &str) -> String {
    if s.contains('\'') || s.contains('\n') { format!("{s:?}") } else { format!("'{s}'") }
}

/// `text` without the `[mcp_servers.norn]` table (and its own sub-tables),
/// and whether it had one.
fn without_codex_table(text: &str) -> (String, bool) {
    let mut out = String::new();
    let (mut inside, mut had) = (false, false);
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            let name = t.trim_start_matches('[').trim_end_matches(']').trim();
            inside = name == "mcp_servers.norn" || name.starts_with("mcp_servers.norn.");
            had |= inside;
        }
        if !inside {
            out.push_str(line);
        }
    }
    (out, had)
}

/// Codex (its command line, its VS Code extension, and the ChatGPT desktop
/// app's Codex): one `config.toml` for all.
fn codex(exe: &Path, remove: bool) -> Done {
    let path = codex_config();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let (rest, had) = without_codex_table(&text);
    let table = format!("[mcp_servers.norn]\ncommand = {}\nargs = [\"mcp\"]\n", toml_string(&exe.to_string_lossy()));
    if remove {
        if !had {
            return Done::NotThere;
        }
        let rest = if rest.trim().is_empty() { String::new() } else { format!("{}\n", rest.trim_end()) };
        return match std::fs::write(&path, rest) {
            Ok(()) => Done::Removed(path.display().to_string()),
            Err(e) => Done::ByHand(format!("take [mcp_servers.norn] out of {} ({e})", path.display())),
        };
    }
    if had && text.contains(&table) {
        return Done::Already(path.display().to_string());
    }
    let mut new = rest.trim_end().to_string();
    if !new.is_empty() {
        new.push_str("\n\n");
    }
    new.push_str(&table);
    let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, new));
    match written {
        Ok(()) => Done::Set(path.display().to_string()),
        Err(e) => Done::ByHand(format!("add to {} ({e}):\n{table}", path.display())),
    }
}

/// Where in `text` (JSON, comments allowed) the value of `key` begins, the
/// key being one of the object that begins at `from`.
fn json_member(text: &str, from: usize, key: &str) -> Option<usize> {
    let b = text.as_bytes();
    let quoted = format!("\"{key}\"");
    let mut depth = 0;
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = string_end(b, i);
                if depth == 1 && text[i..end] == quoted {
                    let colon = end + text[end..].find(':')? + 1;
                    return Some(colon + text[colon..].len() - text[colon..].trim_start().len());
                }
                i = end;
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => i += text[i..].find('\n').unwrap_or(text.len() - i),
            b'/' if b.get(i + 1) == Some(&b'*') => i += text[i..].find("*/").map_or(text.len() - i, |e| e + 2),
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return None;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Just after the string that begins at `i`.
fn string_end(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() && b[j] != b'"' {
        j += if b[j] == b'\\' { 2 } else { 1 };
    }
    (j + 1).min(b.len())
}

/// Just after the value (an object, array, string or word) at `i`.
fn value_end(text: &str, i: usize) -> usize {
    let b = text.as_bytes();
    match b.get(i) {
        Some(b'"') => string_end(b, i),
        Some(b'{' | b'[') => {
            let mut depth = 0;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return j + 1;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            b.len()
        }
        _ => i + text[i..].find([',', '}', '\n']).unwrap_or(text.len() - i),
    }
}

/// OpenCode: its own settings (`~/.config/opencode/opencode.json`), changed
/// as text: what else is in them, comments included, stays as it was.
fn opencode(exe: &Path, remove: bool) -> Done {
    let dir = opencode_dir();
    let path = ["opencode.jsonc", "opencode.json"].iter().map(|n| dir.join(n)).find(|p| p.exists()).unwrap_or_else(|| dir.join("opencode.json"));
    let entry = format!("{{ \"type\": \"local\", \"command\": [{}, \"mcp\"], \"enabled\": true }}", serde_json::to_string(&exe.to_string_lossy()).unwrap());
    let by_hand = |why: String| Done::ByHand(format!("in {}, under \"mcp\": \"norn\": {entry} ({why})", path.display()));
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let Some(open) = text.find('{').or(text.trim().is_empty().then_some(usize::MAX)) else {
        return by_hand("it is not a JSON object".into());
    };
    let mcp = (open != usize::MAX).then(|| json_member(&text, open, "mcp")).flatten();
    let norn = mcp.filter(|&m| text.as_bytes()[m] == b'{').and_then(|m| json_member(&text, m, "norn"));
    let new = match (remove, mcp, norn) {
        (true, _, None) => return Done::NotThere,
        (true, _, Some(at)) => {
            // From the key to the end of the value, and a comma on either side.
            let key = text[..at].rfind("\"norn\"").unwrap();
            let end = value_end(&text, at);
            let after = &text[end..];
            let (start, end) = match after.trim_start().strip_prefix(',') {
                // Not the last: the key's line, if it is alone on it, and the comma after.
                Some(rest) => {
                    let line = text[..key].rfind('\n').map_or(0, |n| n + 1);
                    let mut end = text.len() - rest.len();
                    if text[line..key].trim().is_empty() {
                        let blank = rest.len() - rest.trim_start_matches([' ', '\t', '\r']).len();
                        if rest[blank..].starts_with('\n') {
                            end += blank + 1;
                        }
                        (line, end)
                    } else {
                        (key, end)
                    }
                }
                // The last: the comma before.
                None => (text[..key].trim_end().strip_suffix(',').map_or(key, str::len), end),
            };
            format!("{}{}", &text[..start], &text[end..])
        }
        (false, _, Some(at)) => {
            if text[at..value_end(&text, at)] == entry {
                return Done::Already(path.display().to_string());
            }
            format!("{}{entry}{}", &text[..at], &text[value_end(&text, at)..])
        }
        (false, Some(m), None) if text.as_bytes()[m] == b'{' => {
            let inner = &text[m + 1..value_end(&text, m) - 1];
            let comma = if inner.trim().is_empty() { "" } else { "," };
            format!("{}\n    \"norn\": {entry}{comma}{}", &text[..m + 1], &text[m + 1..])
        }
        (false, Some(_), None) => return by_hand("its \"mcp\" is not an object".into()),
        (false, None, None) if open == usize::MAX => {
            format!("{{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {{\n    \"norn\": {entry}\n  }}\n}}\n")
        }
        (false, None, None) => {
            let inner = &text[open + 1..value_end(&text, open) - 1];
            let comma = if inner.trim().is_empty() { "" } else { "," };
            format!("{}\n  \"mcp\": {{\n    \"norn\": {entry}\n  }}{comma}{}", &text[..open + 1], &text[open + 1..])
        }
    };
    let written = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, new));
    match (written, remove) {
        (Ok(()), true) => Done::Removed(path.display().to_string()),
        (Ok(()), false) => Done::Set(path.display().to_string()),
        (Err(e), _) => by_hand(e.to_string()),
    }
}

/// `norn setup`: put this program where it stays, and set up each agent
/// asked for (each one on this machine, if none is named).
pub fn run_setup(asked: &[String], remove: bool, print: bool, refresh: bool) -> i32 {
    // An update of the extension: the copy brought up to date, if it is there.
    if refresh {
        if installed().exists() {
            match install() {
                Ok(what) => println!("{what}"),
                Err(e) => {
                    eprintln!("norn: cannot update {}: {e}", installed().display());
                    return 1;
                }
            }
        }
        // And the skills put there before, as this version has them.
        for dir in [skill_dir("claude"), skill_dir("codex")] {
            if skill_is_ours(&dir) {
                if let Done::Set(at) = skill(&dir, false) {
                    println!("{at} brought up to date");
                }
            }
        }
        return 0;
    }
    if let Some(other) = asked.iter().find(|a| !AGENTS.contains(&a.as_str())) {
        eprintln!("norn: no agent named {other}; there are {}", AGENTS.join(", "));
        return 2;
    }
    let exe = installed();
    if print {
        println!("Claude Code:  claude mcp add --scope user norn -- \"{}\" mcp", exe.display());
        println!("Codex ({}):\n[mcp_servers.norn]\ncommand = {}\nargs = [\"mcp\"]", codex_config().display(), toml_string(&exe.to_string_lossy()));
        println!(
            "OpenCode ({}), under \"mcp\":\n\"norn\": {{ \"type\": \"local\", \"command\": [{}, \"mcp\"], \"enabled\": true }}",
            opencode_dir().join("opencode.json").display(),
            serde_json::to_string(&exe.to_string_lossy()).unwrap()
        );
        return 0;
    }
    if !remove {
        match install() {
            Ok(what) => println!("norn:      {what}"),
            Err(e) => {
                eprintln!("norn: cannot put the server at {}: {e}", exe.display());
                return 1;
            }
        }
    }
    let agents: Vec<&str> = if asked.is_empty() { AGENTS.to_vec() } else { asked.iter().map(String::as_str).collect() };
    let mut status = 0;
    // The skill's folders, for the agents set up; and with none named, the
    // shared one, which VS Code's Copilot reads too.
    let mut skills: Vec<PathBuf> = Vec::new();
    if asked.is_empty() {
        skills.push(skill_dir("codex"));
    }
    for name in agents {
        if asked.is_empty() && !present(name) {
            println!("{name:<10} not on this machine");
            continue;
        }
        if !skills.contains(&skill_dir(name)) {
            skills.push(skill_dir(name));
        }
        match agent(name, &exe, remove) {
            Done::Set(where_) => println!("{name:<10} set up: {where_}"),
            Done::Already(where_) => println!("{name:<10} already set up: {where_}"),
            Done::Removed(where_) => println!("{name:<10} taken out: {where_}"),
            Done::NotThere => println!("{name:<10} {}", if remove { "was not set up" } else { "not on this machine" }),
            Done::ByHand(what) => {
                println!("{name:<10} to do by hand: {what}");
                status = 1;
            }
        }
    }
    for dir in skills {
        match skill(&dir, remove) {
            Done::Set(at) => println!("{:<10} written: {at}", "skill"),
            Done::Already(at) => println!("{:<10} already there: {at}", "skill"),
            Done::Removed(at) => println!("{:<10} taken out: {at}", "skill"),
            Done::NotThere => {}
            Done::ByHand(what) => {
                println!("{:<10} not written: {what}", "skill");
                status = 1;
            }
        }
    }
    if !remove {
        println!("\nAn agent started from now on has Norn's tools, and the skill that says to set the build configuration first and use them rather than grep; one running now, once it starts again.");
    }
    status
}

#[cfg(test)]
mod tests {
    /// The skill's description is what an agent decides by whether to load
    /// it: whole, quoted (an unquoted ` #if` would end it, YAML taking the
    /// rest for a comment), and short: Claude Code (2.1.309) lists a skill
    /// whose description is longer than about 125 characters by its name
    /// alone.
    #[test]
    fn the_skills_description_is_read_whole() {
        let head = super::SKILL.strip_prefix("---\n").and_then(|s| s.split_once("\n---\n")).expect("frontmatter").0;
        let line = head.lines().find_map(|l| l.strip_prefix("description: ")).expect("a description");
        let text = line.strip_prefix('"').and_then(|l| l.strip_suffix('"')).expect("quoted");
        assert!(!text.contains('"') && !text.contains('\\') && text.len() <= 120, "{text}");
        assert!(head.lines().any(|l| l == "name: norn"));
    }
}
