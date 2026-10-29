//! What `avp up` writes in a checkout for the coding agents: merged into the files a person has,
//! never over them, kept out of `git status`, and never the server's password. Alone in its binary:
//! it sets HOME and AVP_HOME for the process.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

fn files(dir: &Path, base: &Path, out: &mut BTreeMap<String, String>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, base, out);
        } else {
            out.insert(p.strip_prefix(base).unwrap().display().to_string(), std::fs::read_to_string(&p).unwrap_or_default());
        }
    }
}

#[test]
fn agent_configs_are_merged_in_never_written_over_and_hold_no_secret() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("avp-agents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let top = root.join("shop");
    // A git checkout as git itself recognises one, so that the files land in its local excludes.
    for d in [".git/objects", ".git/refs", ".git/info", ".cursor", ".vscode", ".codex"] {
        std::fs::create_dir_all(top.join(d)).unwrap();
    }
    std::fs::write(top.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("AVP_HOME", root.join("avp-home"));
    std::fs::create_dir_all(root.join("avp-home")).unwrap();

    // The person's own files: a Cursor config with a server, comments and a trailing comma, VS Code's
    // without servers yet, Codex's with a comment, and the repo's own AGENTS.md.
    let cursor = "// mine\n{\n  \"mcpServers\": {\n    \"mine\": { \"command\": \"mine\" }, // keep it\n  },\n}\n";
    std::fs::write(top.join(".cursor/mcp.json"), cursor).unwrap();
    std::fs::write(top.join(".vscode/mcp.json"), "{\"inputs\": []}\n").unwrap();
    let codex = "# mine\nmodel = \"o3\"\n\n[mcp_servers.mine]\ncommand = \"mine\"\n";
    std::fs::write(top.join(".codex/config.toml"), codex).unwrap();
    std::fs::write(top.join("AGENTS.md"), "# Ours\n").unwrap();

    let here = avp::up::Here { top: top.clone(), repo: "shop".into(), exe: "/opt/avp/bin/avp".into() };
    let server = "https://alice:s3cret@avp.example.com";
    let wire_all = || {
        // Claude Code is wired through its own CLI, outside the checkout.
        for a in avp::up::AGENTS.iter().filter(|a| a.id != "claude-code") {
            avp::up::wire(a, &here, server).unwrap_or_else(|e| panic!("{}: {e:#}", a.id));
        }
    };
    wire_all();
    avp::up::keep(&top, server, "shop").unwrap();
    let mut first = BTreeMap::new();
    files(&top, &top, &mut first);
    // Again: nothing written twice, nothing written over.
    wire_all();
    let mut again = BTreeMap::new();
    files(&top, &top, &mut again);
    assert_eq!(first, again);

    // Every line of the person's Cursor config is still there, the entry beside them.
    let c = &first[".cursor/mcp.json"];
    for line in cursor.lines() {
        assert!(c.contains(line), "lost {line:?} in\n{c}");
    }
    assert!(c.contains(r#""avp": {"args":["-C","#) && c.contains(r#""type":"stdio""#), "{c}");
    let v: Value = serde_json::from_str(&first[".vscode/mcp.json"]).unwrap();
    assert_eq!(v["inputs"], json!([]));
    assert_eq!(v["servers"]["avp"], json!({ "type": "stdio", "command": "/opt/avp/bin/avp", "args": ["-C", top.display().to_string(), "mcp"] }));
    let t = &first[".codex/config.toml"];
    assert!(t.starts_with(codex), "{t}");
    let t: toml::Table = t.parse().unwrap();
    assert_eq!(t["mcp_servers"]["mine"]["command"].as_str(), Some("mine"));
    assert_eq!(t["mcp_servers"]["avp"]["command"].as_str(), Some("/opt/avp/bin/avp"));
    let o: Value = serde_json::from_str(&first["opencode.json"]).unwrap();
    assert_eq!(o["mcp"]["avp"]["command"], json!(["/opt/avp/bin/avp", "-C", top.display().to_string(), "mcp"]));
    // The repo's own AGENTS.md stays as it was: the rule waits outside the checkout.
    assert_eq!(first["AGENTS.md"], "# Ours\n");
    assert!(std::fs::read_to_string(root.join("avp-home/shop.AGENTS.md")).unwrap().contains("avp_land"));
    assert!(first["GEMINI.md"].contains("avp_land"));
    assert!(first[".aider.avp.md"].contains("avp open --repo shop"));

    // Out of `git status`, every file AVP wrote.
    let excluded = &first[".git/info/exclude"];
    for f in [".cursor/mcp.json", ".vscode/mcp.json", ".codex/config.toml", "opencode.json", ".zed/settings.json", "GEMINI.md", ".aider.conf.yml"] {
        assert!(excluded.lines().any(|l| l == f), "{f} not excluded:\n{excluded}");
    }

    // The password is nowhere in the checkout; `avp mcp` finds it in a file for the user alone.
    for (p, text) in &first {
        assert!(!text.contains("s3cret"), "{p} holds the server's password");
    }
    let kept: Vec<_> = std::fs::read_dir(root.join("avp-home/checkouts")).unwrap().flatten().map(|e| e.path()).collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(std::fs::metadata(&kept[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(std::fs::read_to_string(&kept[0]).unwrap().contains(server));
    let _ = std::fs::remove_dir_all(&root);
}

/// The name each MCP client gives in `initialize` (from its source) is the agent `avp log` shows.
#[test]
fn an_mcp_client_lands_under_its_agent_name() {
    for (client, agent) in [
        ("claude-code", "claude-code"),
        ("codex-mcp-client", "codex"),
        ("gemini-cli-mcp-client", "gemini-cli"),
        ("qwen-cli-mcp-client-avp", "qwen-code"),
        ("cursor-vscode", "cursor"),
        ("Cursor", "cursor"),
        ("Visual Studio Code", "copilot"),
        ("Visual Studio Code - Insiders", "copilot"),
        ("github-copilot-developer", "copilot"),
        ("opencode", "opencode"),
        ("kiro", "kiro"),
        ("Zed", "zed"),
        ("amp-mcp-client", "amp"),
        ("Cline", "cline"),
        ("@cline/core", "cline"),
        ("some-new-agent", "some-new-agent"),
    ] {
        assert_eq!(avp::mcp::agent_of(client), agent, "{client}");
    }
}
