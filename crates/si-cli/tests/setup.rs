//! `norn setup` in a home folder of its own: the program put where it
//! stays, and each agent's settings given it, and taken out, with what else
//! is in them left as it was.

use std::path::Path;
use std::process::Command;

fn setup(home: &Path, path: &str, args: &[&str]) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_norn"))
        .arg("setup")
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("CODEX_HOME")
        .env("PATH", path)
        .output()
        .expect("norn setup");
    (format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)), out.status.success())
}

#[test]
fn setup_gives_each_agent_the_server_and_takes_it_out_again() {
    let home = std::env::temp_dir().join(format!("norn-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let (codex, opencode) = (home.join(".codex").join("config.toml"), home.join(".config").join("opencode").join("opencode.json"));
    std::fs::create_dir_all(codex.parent().unwrap()).unwrap();
    std::fs::create_dir_all(opencode.parent().unwrap()).unwrap();
    let codex_before = "model = \"o4\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
    let opencode_before = "{\n  // mine\n  \"theme\": \"dark\",\n  \"mcp\": {\n    \"other\": { \"type\": \"local\", \"command\": [\"x\"] }\n  }\n}\n";
    std::fs::write(&codex, codex_before).unwrap();
    std::fs::write(&opencode, opencode_before).unwrap();
    // No `claude` on the PATH: it is said what to type.
    let empty = home.join("bin");
    std::fs::create_dir_all(&empty).unwrap();
    let path = empty.to_str().unwrap();

    let (said, _) = setup(&home, path, &["codex", "opencode"]);
    let exe = home.join(".norn").join("bin").join(if cfg!(windows) { "norn.exe" } else { "norn" });
    assert!(exe.is_file(), "{said}");
    let out = Command::new(&exe).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("norn "), "the copy runs");

    let toml = std::fs::read_to_string(&codex).unwrap();
    assert!(toml.starts_with(codex_before) && toml.contains("[mcp_servers.norn]\ncommand = '") && toml.contains("args = [\"mcp\"]"), "{toml}");
    let json = std::fs::read_to_string(&opencode).unwrap();
    assert!(json.contains("// mine") && json.contains("\"other\"") && json.contains("\"norn\": { \"type\": \"local\""), "{json}");
    // The skill, where Codex and OpenCode read it.
    let shared = home.join(".agents").join("skills").join("norn").join("SKILL.md");
    let skill = std::fs::read_to_string(&shared).unwrap_or_default();
    assert!(skill.starts_with("---\nname: norn\n") && skill.contains("list_configurations"), "{said}");

    // Again: nothing more.
    let (said, ok) = setup(&home, path, &["codex", "opencode"]);
    assert!(ok && said.matches("already set up").count() == 2 && said.contains("already there"), "{said}");
    assert_eq!(std::fs::read_to_string(&codex).unwrap(), toml);

    // Claude Code, with no `claude` to run: by hand.
    let (said, ok) = setup(&home, path, &["claude"]);
    assert!(!ok && said.contains("claude mcp add --scope user norn --"), "{said}");
    // Its skill, where Claude Code reads it.
    let claude_skill = home.join(".claude").join("skills").join("norn").join("SKILL.md");
    assert_eq!(std::fs::read_to_string(&claude_skill).unwrap_or_default(), skill, "{said}");

    // Taken out: as they were.
    let (said, ok) = setup(&home, path, &["--remove", "codex", "opencode"]);
    assert!(ok, "{said}");
    assert_eq!(std::fs::read_to_string(&codex).unwrap(), codex_before);
    assert_eq!(std::fs::read_to_string(&opencode).unwrap(), opencode_before);
    assert!(!shared.exists() && claude_skill.exists(), "{said}");

    // A skill of the user's that happens to be named norn is left alone.
    std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
    std::fs::write(&shared, "---\nname: norn\ndescription: the user's own\n---\nmine\n").unwrap();
    let _ = setup(&home, path, &["codex"]);
    assert!(std::fs::read_to_string(&shared).unwrap().contains("the user's own"));
    let _ = setup(&home, path, &["--remove", "codex"]);
    assert!(shared.exists());
    let _ = std::fs::remove_dir_all(&home);
}
