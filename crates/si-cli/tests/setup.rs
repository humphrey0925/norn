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

    // Again: nothing more.
    let (said, ok) = setup(&home, path, &["codex", "opencode"]);
    assert!(ok && said.matches("already set up").count() == 2, "{said}");
    assert_eq!(std::fs::read_to_string(&codex).unwrap(), toml);

    // Claude Code, with no `claude` to run: by hand.
    let (said, ok) = setup(&home, path, &["claude"]);
    assert!(!ok && said.contains("claude mcp add --scope user norn --"), "{said}");

    // Taken out: as they were.
    let (said, ok) = setup(&home, path, &["--remove", "codex", "opencode"]);
    assert!(ok, "{said}");
    assert_eq!(std::fs::read_to_string(&codex).unwrap(), codex_before);
    assert_eq!(std::fs::read_to_string(&opencode).unwrap(), opencode_before);
    let _ = std::fs::remove_dir_all(&home);
}
