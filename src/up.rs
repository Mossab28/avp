//! `avp up`: AVP on a git checkout, without touching it. A local server when none answers, the
//! repo imported from its history, the checks its proofs run (guessed from the project and landed
//! on the line), a mirror that writes every landed task as a commit on a branch of its own, and each
//! coding agent installed here wired for this folder. `avp down` stops what `avp up` started.

use crate::client::Client;
use crate::gitbridge;
use crate::model::*;
use crate::view;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The server `avp up` starts when none is given: this machine only.
pub const LOCAL: &str = "http://127.0.0.1:7117";

/// Where `avp up` keeps what it starts: the local server's data, logs, pid files.
pub fn home() -> PathBuf {
    std::env::var("AVP_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".avp"))
}

/// Where tasks opened from MCP lay their folders out.
pub fn views() -> PathBuf {
    std::env::var("AVP_VIEWS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("avp"))
}

pub fn git_top(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// The repo name a checkout gets on the server: its folder's name, lowercase, `[a-z0-9._-]`.
pub fn repo_name(dir: &Path) -> Option<String> {
    let top = git_top(dir)?;
    let n: String = top.file_name()?.to_string_lossy().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    (!n.is_empty()).then_some(n)
}

fn alive(client: &Client) -> bool {
    client.get::<Value>("/repos").is_ok()
}

fn pid_alive(pid_file: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()?;
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().ok()?.success().then_some(pid)
}

/// Starts `exe args...` on its own, outside the terminal's process group, output in `log`.
fn spawn(exe: &Path, args: &[String], cwd: &Path, log: &Path, pid_file: &Path) -> Result<u32> {
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let child = Command::new(exe).args(args).current_dir(cwd).stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out).process_group(0).spawn()?;
    std::fs::write(pid_file, child.id().to_string())?;
    Ok(child.id())
}

/// Whether a Python project lists `package` as a dependency it installs: in a requirements file, or
/// in pyproject.toml (project dependencies and extras, dependency groups, poetry's and uv's lists).
fn python_declares(top: &Path, package: &str) -> bool {
    let named = |spec: &str| {
        let s = spec.trim().to_ascii_lowercase();
        s.strip_prefix(package).is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| "<>=~![ ;".contains(c)))
    };
    for req in ["requirements.txt", "requirements-dev.txt", "requirements-test.txt", "dev-requirements.txt"] {
        if std::fs::read_to_string(top.join(req)).is_ok_and(|t| t.lines().any(named)) {
            return true;
        }
    }
    let Some(py) = std::fs::read_to_string(top.join("pyproject.toml")).ok().and_then(|t| t.parse::<toml::Value>().ok()) else { return false };
    let list = |v: Option<&toml::Value>| v.and_then(|v| v.as_array()).is_some_and(|a| a.iter().filter_map(|x| x.as_str()).any(named));
    let lists = |v: Option<&toml::Value>| v.and_then(|v| v.as_table()).is_some_and(|t| t.values().any(|x| list(Some(x))));
    let keys = |v: Option<&toml::Value>| v.and_then(|v| v.as_table()).is_some_and(|t| t.keys().any(|k| k.eq_ignore_ascii_case(package)));
    let poetry = py.get("tool").and_then(|t| t.get("poetry"));
    list(py.get("project").and_then(|p| p.get("dependencies")))
        || lists(py.get("project").and_then(|p| p.get("optional-dependencies")))
        || lists(py.get("dependency-groups"))
        || list(py.get("tool").and_then(|t| t.get("uv")).and_then(|u| u.get("dev-dependencies")))
        || keys(poetry.and_then(|p| p.get("dependencies")))
        || keys(poetry.and_then(|p| p.get("dev-dependencies")))
        || poetry.and_then(|p| p.get("group")).and_then(|g| g.as_table()).is_some_and(|g| g.values().any(|grp| keys(grp.get("dependencies"))))
}

/// The checks a proof runs, guessed from the files at the root of the project (Node, Python,
/// Rust, Go, Java with Maven or Gradle, Ruby). None when nothing is recognised: the line then lands without proof steps until an
/// avp.toml says what to run.
pub fn guess_checks(top: &Path) -> Option<(String, Vec<String>)> {
    let has = |p: &str| top.join(p).exists();
    let mut steps: Vec<(String, String, bool)> = Vec::new();
    let mut setup: Option<(String, Vec<String>)> = None;
    let mut links: Vec<String> = vec!["node_modules".into()];
    let mut keep: Vec<String> = vec![];
    if has("package.json") {
        let pkg: Value = std::fs::read_to_string(top.join("package.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        let dep = |n: &str| ["dependencies", "devDependencies"].iter().any(|k| pkg[*k].get(n).is_some());
        let (install, lock) = if has("pnpm-lock.yaml") {
            ("pnpm install --frozen-lockfile --prefer-offline", "pnpm-lock.yaml")
        } else if has("yarn.lock") {
            ("yarn install --frozen-lockfile", "yarn.lock")
        } else if has("bun.lockb") || has("bun.lock") {
            ("bun install --frozen-lockfile", if has("bun.lock") { "bun.lock" } else { "bun.lockb" })
        } else if has("package-lock.json") {
            ("npm ci", "package-lock.json")
        } else {
            ("npm install", "package.json")
        };
        let mut keys = vec!["package.json".to_string(), lock.to_string()];
        if has("pnpm-workspace.yaml") {
            keys.push("pnpm-workspace.yaml".into());
        }
        keys.dedup();
        setup = Some((install.into(), keys));
        // The project's own typecheck first: in a monorepo, the root tsconfig often covers nothing
        // of the packages, while its `typecheck` script (turbo, nx, a loop) covers them all.
        let pm = match install.split(' ').next() { Some(pm) => pm, None => "npm" };
        let workspaces = has("pnpm-workspace.yaml") || pkg.get("workspaces").is_some();
        if pkg["scripts"]["typecheck"].is_string() {
            steps.push(("typecheck".into(), format!("{pm} run typecheck"), false));
        } else if workspaces && pm == "pnpm" {
            steps.push(("typecheck of every package".into(), "pnpm -r --if-present run typecheck".into(), false));
        } else if workspaces && pm == "npm" {
            steps.push(("typecheck of every package".into(), "npm run typecheck --workspaces --if-present".into(), false));
        } else if has("tsconfig.json") && dep("typescript") {
            steps.push(("typecheck".into(), "node_modules/.bin/tsc --noEmit -p .".into(), false));
        }
        if dep("vitest") {
            steps.push(("tests related to the task's files".into(), "node_modules/.bin/vitest related --run --passWithNoTests {files}".into(), true));
        } else if dep("jest") {
            steps.push(("tests related to the task's files".into(), "node_modules/.bin/jest --findRelatedTests --passWithNoTests {files}".into(), true));
        } else if pkg["scripts"]["test"].as_str().is_some_and(|t| !t.contains("no test specified")) {
            // Another runner (node --test, mocha, ava, uvu...): the project's own test script.
            let run = match install.split(' ').next() { Some("bun") => "bun run", Some(pm) => pm, None => "npm" };
            steps.push(("tests".into(), format!("{run} test"), false));
        }
    } else if has("Cargo.toml") {
        // Cargo knows what in `target` is stale: kept from one proof to the next, a proof compiles
        // what changed instead of the whole crate.
        keep.push("target".into());
        steps.push(("cargo check".into(), "cargo check --all-targets --quiet".into(), false));
        steps.push(("cargo test".into(), "cargo test --quiet".into(), false));
    } else if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        let text = |p: &str| std::fs::read_to_string(top.join(p)).unwrap_or_default();
        let declared = ["pyproject.toml", "setup.cfg", "requirements.txt", "requirements-dev.txt", "requirements-test.txt", "tox.ini"].iter().map(|p| text(p)).collect::<String>();
        let pytest = declared.contains("pytest") || has("tests") || has("test") || has("conftest.py");
        // Configured is not installed: many projects have a [tool.ruff] section and run ruff
        // through pre-commit, so the step would only find no ruff in the virtualenv.
        let ruff = python_declares(top, "ruff");
        // One virtualenv, built once per lockfile from the dependencies alone and kept between
        // proofs; the project itself installs in each candidate, from the candidate's own sources
        // (a build backend needs its README and its package, which the setup does not have).
        let packaged = has("pyproject.toml") || has("setup.py");
        let (install, keys, run, root): (String, Vec<&str>, &str, Option<&str>) = if has("uv.lock") {
            ("uv sync --frozen --all-extras --no-install-project".into(), vec!["pyproject.toml", "uv.lock"], "uv run --frozen", None)
        } else if has("poetry.lock") {
            ("POETRY_VIRTUALENVS_IN_PROJECT=true poetry install --no-interaction --all-extras --no-root".into(), vec!["pyproject.toml", "poetry.lock"], "poetry run", Some("poetry install --no-interaction --only-root -q"))
        } else if has("requirements.txt") {
            let mut i = String::from("python3 -m venv .venv && .venv/bin/pip install -q -r requirements.txt");
            for extra in ["requirements-dev.txt", "requirements-test.txt"] {
                if has(extra) {
                    i.push_str(&format!(" -r {extra}"));
                }
            }
            (i, vec!["requirements.txt", "requirements-dev.txt", "requirements-test.txt"], ".venv/bin/python -m", packaged.then_some(".venv/bin/pip install -q --no-deps -e ."))
        } else {
            // No lockfile: the dependencies come with the project, in each candidate (pip's cache
            // makes it quick after the first proof).
            ("python3 -m venv .venv".into(), vec!["pyproject.toml", "setup.py", "setup.cfg"], ".venv/bin/python -m", Some("(.venv/bin/pip install -q -e '.[test,dev]' || .venv/bin/pip install -q -e .)"))
        };
        setup = Some((install, keys.into_iter().filter(|k| has(k)).map(String::from).collect()));
        links = vec![".venv".into()];
        if ruff {
            steps.push(("ruff".into(), format!("{run} ruff check ."), false));
        }
        if pytest {
            let pre = root.map(|r| format!("{r} && ")).unwrap_or_default();
            steps.push(("pytest".into(), format!("{pre}{run} pytest -q"), false));
        }
    } else if has("go.mod") {
        steps.push(("go vet".into(), "go vet ./...".into(), false));
        steps.push(("go test".into(), "go test ./...".into(), false));
    } else if has("pom.xml") {
        // Dependencies land in ~/.m2 once, then every proof reads them from there.
        let mvn = if has("mvnw") { "sh mvnw" } else { "mvn" };
        steps.push(("maven test".into(), format!("{mvn} -q -B test"), false));
    } else if has("build.gradle") || has("build.gradle.kts") || has("settings.gradle") || has("settings.gradle.kts") {
        let gradle = if has("gradlew") { "sh gradlew" } else { "gradle" };
        steps.push(("gradle test".into(), format!("{gradle} test --quiet"), false));
    } else if has("Gemfile") {
        // Gems install once in a folder of AVP's (a Homebrew or system Ruby's own gem folder is
        // often not writable), then `bundle install` only checks. It runs in the candidate: a
        // Gemfile's `gemspec` reads lib/.
        let text = |p: &str| std::fs::read_to_string(top.join(p)).unwrap_or_default();
        // A gem declares its test tools in its gemspec, which the Gemfile pulls in with `gemspec`.
        let mut declared = text("Gemfile") + &text("Gemfile.lock");
        for e in std::fs::read_dir(top).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().ends_with(".gemspec") {
                declared.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
            }
        }
        let tests = if has("spec") && (declared.contains("rspec") || has(".rspec")) {
            Some("bundle exec rspec")
        } else if has("test") && has("Rakefile") {
            Some("bundle exec rake test")
        } else {
            None
        };
        if let Some(t) = tests {
            steps.push(("tests".into(), format!("export BUNDLE_PATH=\"${{BUNDLE_PATH:-$HOME/.avp/bundle}}\"; bundle install --quiet && {t}"), false));
        }
    }
    if steps.is_empty() {
        return None;
    }
    let mut t = String::from("# What a task must prove before it lands on this repo's line: AVP runs each step on the\n# candidate tree (the head of the line plus the task's files). Guessed by `avp up`: edit freely.\n[check]\n");
    if let Some((install, keys)) = &setup {
        t.push_str(&format!("setup = {:?}\nsetup_keys = {:?}\nsetup_links = {:?}\n", install, keys, links));
    }
    if !keep.is_empty() {
        t.push_str(&format!("keep = {keep:?}\n"));
    }
    let mut names = Vec::new();
    for (name, run, needs) in &steps {
        t.push_str(&format!("\n[[check.steps]]\nname = {name:?}\nrun = {run:?}\n"));
        if *needs {
            t.push_str("needs_files = true\n");
        }
        names.push(name.clone());
    }
    Some((t, names))
}

/// Lands `files` on the line as one task, from a folder of its own under the AVP home.
fn land_files(client: &Client, repo: &Repo, goal: &str, files: &[(&str, &str)]) -> Result<std::result::Result<Landed, NotLanded>> {
    let person = std::env::var("AVP_PERSON").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "someone".into());
    let actor = Actor { person: person.clone(), agent: "avp-up".into(), session: new_id("s") };
    let task: Task = client.post(&format!("/repos/{}/tasks", repo.id), &OpenTask { goal: goal.into(), actor, scope: vec![], transcript: None })?;
    let dir = home().join("views").join(&task.id);
    std::fs::create_dir_all(&dir)?;
    let mut s = view::ViewState { server: client.base.clone(), repo: repo.id.clone(), task: Some(task.id.clone()), known: BTreeMap::new(), ..Default::default() };
    let tree: Tree = client.get(&format!("/repos/{}/tree/{}", repo.id, task.base_position))?;
    // Laid out from the ready copy of the head, which it leaves made: the first task opens at once.
    view::lay_out(client, &tree, &dir, &mut s, &[])?;
    for (path, text) in files {
        std::fs::write(dir.join(path), text)?;
    }
    view::sync(client, &dir, &mut s)?;
    let mut r = client.land(&repo.id, &task.id)?;
    // A change to the checks always waits for a person's word. Here the person is at the terminal,
    // asking for these checks: their run says yes. A repo where every landing waits for a person
    // keeps waiting for one.
    if matches!(&r, Err(n) if n.reason == "approval") && repo.landing == LandingRule::Proof {
        let _: Approval = client.post(&format!("/repos/{}/tasks/{}/approve", repo.id, task.id), &json!({ "person": person, "approved": true, "reason": "" }))?;
        eprintln!("a change to the checks waits for a person's word: approved as {person}, who ran avp up");
        r = client.land(&repo.id, &task.id)?;
    }
    if r.is_err() {
        // Fixed and landed by a person as a task of their own: this one would stay open for good.
        let _: Result<Value> = client.post(&format!("/repos/{}/tasks/{}/abandon", repo.id, task.id), &json!({}));
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(r)
}

/// One checkout `avp up` wires the agents for.
pub struct Here {
    pub top: PathBuf,
    /// The repo's name on the server.
    pub repo: String,
    /// The `avp` the agents start.
    pub exe: PathBuf,
}

/// The shape of the `avp` entry in an agent's MCP config.
#[derive(Clone, Copy)]
pub enum Shape {
    /// `{"command": ..., "args": [...]}`
    Plain,
    /// `{"type": "stdio", "command": ..., "args": [...]}` (VS Code, Cursor)
    Stdio,
    /// `{"type": "local", "command": [command, args...], "enabled": true}` (OpenCode)
    Local,
    /// `{"command": ..., "args": [...], "tools": ["*"]}` (Copilot CLI)
    Tools,
}

/// How `avp up` puts its MCP server in an agent's reach.
enum Wire {
    /// `claude mcp add-json --scope local`: Claude Code keeps it outside the checkout.
    Claude,
    /// A JSON file of the checkout (the first of `files` there, else the first): `"avp"` under the
    /// keys `at`.
    Json { files: &'static [&'static str], at: &'static [&'static str], shape: Shape },
    /// A TOML file of the checkout: `[<table>.avp]` (Codex).
    Toml { file: &'static str, table: &'static str },
    /// The person's own file, read for every project (`~/` is their home folder): the line to add
    /// is printed, the file is never written.
    Global { file: &'static str, at: &'static [&'static str], shape: Shape },
    /// No MCP (Aider): the rule, in its CLI form, is the whole wiring.
    Shell,
}

/// Where an agent reads the rule that tells its model to deliver through AVP.
enum Rule {
    /// The MCP server's own instructions reach the model (Claude Code).
    Served,
    /// AGENTS.md.
    Agents,
    /// AGENTS.md, unless the checkout has one of `before`: the agent reads the first of those
    /// instead (Zed reads one rules file only).
    First { before: &'static [&'static str] },
    /// Its own file, unless one of its settings files (the checkout's first, then the person's)
    /// names AGENTS.md among its context files.
    Own { file: &'static str, settings: &'static [&'static str] },
    /// The rule in CLI form, in a file `config` makes it read (Aider).
    Shell { file: &'static str, config: &'static str },
}

pub struct Agent {
    /// Its name in `avp agents --json`, and on the positions it lands.
    pub id: &'static str,
    pub name: &'static str,
    /// Its programs: one on PATH says it is installed.
    bins: &'static [&'static str],
    /// Folders it makes under the home folder: one there says it is installed too (an editor
    /// opened from its application has nothing on PATH). A last part ending in `*` is a prefix.
    homes: &'static [&'static str],
    wire: Wire,
    rule: Rule,
    /// What the agent asks of a person before it loads a project's MCP config, if anything.
    note: &'static str,
}

/// The agents `avp up` knows, each config where its documentation puts it (October 2026). Windsurf
/// is Devin Desktop now: its default agent reads the project's .devin/mcp_config.json.
pub const AGENTS: &[Agent] = &[
    Agent { id: "claude-code", name: "Claude Code", bins: &["claude"], homes: &[], wire: Wire::Claude, rule: Rule::Served, note: "" },
    Agent { id: "codex", name: "Codex", bins: &["codex"], homes: &[".codex"], wire: Wire::Toml { file: ".codex/config.toml", table: "mcp_servers" }, rule: Rule::Agents, note: "Codex loads it once it trusts this folder" },
    Agent { id: "cursor", name: "Cursor", bins: &["cursor", "cursor-agent"], homes: &[".cursor"], wire: Wire::Json { files: &[".cursor/mcp.json"], at: &["mcpServers"], shape: Shape::Stdio }, rule: Rule::Agents, note: "" },
    Agent { id: "gemini-cli", name: "Gemini CLI", bins: &["gemini"], homes: &[".gemini"], wire: Wire::Json { files: &[".gemini/settings.json"], at: &["mcpServers"], shape: Shape::Plain }, rule: Rule::Own { file: "GEMINI.md", settings: &[".gemini/settings.json", "~/.gemini/settings.json"] }, note: "loaded in a trusted folder" },
    Agent { id: "copilot", name: "Copilot (VS Code)", bins: &["code", "code-insiders"], homes: &[".vscode", ".vscode-insiders"], wire: Wire::Json { files: &[".vscode/mcp.json"], at: &["servers"], shape: Shape::Stdio }, rule: Rule::Agents, note: "loaded in a trusted workspace" },
    Agent { id: "copilot-cli", name: "Copilot CLI", bins: &["copilot"], homes: &[".copilot"], wire: Wire::Json { files: &[".github/mcp.json"], at: &["mcpServers"], shape: Shape::Tools }, rule: Rule::Agents, note: "loaded once you trust the folder" },
    Agent { id: "opencode", name: "OpenCode", bins: &["opencode"], homes: &[".config/opencode"], wire: Wire::Json { files: &["opencode.json", "opencode.jsonc"], at: &["mcp"], shape: Shape::Local }, rule: Rule::Agents, note: "" },
    Agent { id: "qwen-code", name: "Qwen Code", bins: &["qwen"], homes: &[".qwen"], wire: Wire::Json { files: &[".qwen/settings.json"], at: &["mcpServers"], shape: Shape::Plain }, rule: Rule::Agents, note: "" },
    Agent { id: "kiro", name: "Kiro", bins: &["kiro-cli"], homes: &[".kiro"], wire: Wire::Json { files: &[".kiro/settings/mcp.json"], at: &["mcpServers"], shape: Shape::Plain }, rule: Rule::Agents, note: "with MCP support on in Kiro's settings" },
    Agent { id: "zed", name: "Zed", bins: &["zed"], homes: &[".config/zed"], wire: Wire::Json { files: &[".zed/settings.json"], at: &["context_servers"], shape: Shape::Plain }, rule: Rule::First { before: &[".rules", ".cursorrules", ".windsurfrules", ".clinerules", ".github/copilot-instructions.md", "AGENT.md"] }, note: "loaded once the worktree is trusted" },
    Agent { id: "amp", name: "Amp", bins: &["amp"], homes: &[".config/amp"], wire: Wire::Json { files: &[".amp/settings.json", ".amp/settings.jsonc"], at: &["amp.mcpServers"], shape: Shape::Plain }, rule: Rule::Agents, note: "approve it once: `amp mcp approve avp`" },
    Agent { id: "windsurf", name: "Windsurf", bins: &["windsurf", "devin"], homes: &[".codeium/windsurf", ".config/devin"], wire: Wire::Json { files: &[".devin/mcp_config.json"], at: &["mcpServers"], shape: Shape::Plain }, rule: Rule::Agents, note: "" },
    Agent { id: "cline", name: "Cline", bins: &["cline"], homes: &[".cline", ".vscode/extensions/saoudrizwan.claude-dev-*", ".cursor/extensions/saoudrizwan.claude-dev-*"], wire: Wire::Global { file: "~/.cline/data/settings/cline_mcp_settings.json", at: &["mcpServers"], shape: Shape::Plain }, rule: Rule::Agents, note: "" },
    Agent { id: "aider", name: "Aider", bins: &["aider"], homes: &[], wire: Wire::Shell, rule: Rule::Shell { file: ".aider.avp.md", config: ".aider.conf.yml" }, note: "" },
];

/// The rule, for agents whose model does not get the MCP server's instructions: without it, Codex
/// agents left their changes in the checkout.
pub const AGENTS_RULE: &str = "# Delivering changes

This repository is delivered through AVP, not git: no commit, push or pull request. For every
change, use the `avp` MCP tools: `avp_open` first, with a one-line goal (it lays out a folder of your
own: read and work there), edit, then `avp_land`, which proves the change and lands it. On red, fix and
land again. Landing is delivering. To split work among subagents, each subagent calls `avp_open`
itself (a task and a folder of its own) and passes its `task` to the other `avp_` tools.
";

/// The rule for an agent without MCP: the same verbs, from its shell.
pub fn shell_rule(repo: &str) -> String {
    format!(
        "# Delivering changes

This repository is delivered through AVP, not git: no commit, push or pull request. For every
change, from the shell: `avp open --repo {repo} --goal \"<one line>\"` first (it opens a task and lays
the head of the line out in this folder), edit, then `avp land`, which proves the change and lands
it. On red, fix and run `avp land` again; on a conflict, settle the markers in the files it names
and land again. Landing is delivering.
"
    )
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

pub fn installed(a: &Agent) -> bool {
    a.bins.iter().any(|b| on_path(b))
        || a.homes.iter().any(|h| match h.strip_suffix('*') {
            Some(prefix) => {
                let p = home_dir().join(prefix);
                let (Some(dir), Some(start)) = (p.parent(), p.file_name()) else { return false };
                std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| e.file_name().to_string_lossy().starts_with(start.to_string_lossy().as_ref()))
            }
            None => home_dir().join(h).is_dir(),
        })
}

/// A file of the table: `~/` is the home folder, anything else is in the checkout.
fn place(here: &Here, file: &str) -> PathBuf {
    match file.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None => here.top.join(file),
    }
}

/// The `avp` entry of a config. One in the checkout starts `avp mcp` on the checkout, wherever the
/// agent starts it from; a person's global one, wherever the agent runs. Never the server nor a
/// secret: `avp mcp` finds them in its environment, or in what `avp up` kept (see `keep`).
pub fn entry(shape: Shape, exe: &Path, top: Option<&Path>) -> Value {
    let exe = exe.display().to_string();
    let mut args: Vec<String> = top.map(|t| vec!["-C".to_string(), t.display().to_string()]).unwrap_or_default();
    args.push("mcp".into());
    match shape {
        Shape::Plain => json!({ "command": exe, "args": args }),
        Shape::Stdio => json!({ "type": "stdio", "command": exe, "args": args }),
        Shape::Local => json!({ "type": "local", "command": std::iter::once(exe).chain(args).collect::<Vec<_>>(), "enabled": true }),
        Shape::Tools => json!({ "command": exe, "args": args, "tools": ["*"] }),
    }
}

/// The agent's JSON file in this checkout: the first of `files` there, else the first.
fn json_file(here: &Here, files: &[&'static str]) -> &'static str {
    files.iter().copied().find(|f| here.top.join(f).exists()).unwrap_or(files[0])
}

/// Whether Codex trusts this checkout (`projects.<path>.trust_level` in its own config): it loads
/// a project's .codex/config.toml only then, and asks the first time it starts there.
fn codex_trusts(top: &Path) -> bool {
    let codex = std::env::var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|_| home_dir().join(".codex"));
    let config = std::fs::read_to_string(codex.join("config.toml")).ok().and_then(|t| t.parse::<toml::Table>().ok());
    config.is_some_and(|c| c.get("projects").and_then(|p| p.get(top.to_string_lossy().as_ref())).and_then(|p| p.get("trust_level")).and_then(|v| v.as_str()) == Some("trusted"))
}

fn codex_entry(here: &Here) -> toml::Table {
    let mut t = toml::Table::new();
    t.insert("command".into(), here.exe.display().to_string().into());
    t.insert("args".into(), toml::Value::Array(vec!["-C".into(), here.top.display().to_string().into(), "mcp".into()]));
    t
}

/// What adds the entry by hand: `"avp": {...} under "mcpServers" in .cursor/mcp.json`.
fn by_hand(file: &str, at: &[&str], entry: &Value) -> String {
    let under = at.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(" > ");
    format!("add \"avp\": {entry} under {under} in {file}")
}

/// Registers the `avp` MCP server for Claude Code in this folder (local scope: nothing written in
/// the repo's tracked files) and lets it write in the folders AVP lays tasks out in.
fn claude_mcp(here: &Here, server: &str) -> Result<()> {
    let top = &here.top;
    let present = Command::new("claude").args(["mcp", "get", "avp"]).current_dir(top).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
    if !present {
        // The folders tasks lay out in go with it: the agent works where it is allowed to write.
        let cfg = json!({ "type": "stdio", "command": here.exe.display().to_string(), "args": ["mcp"], "env": { "AVP_SERVER": server, "AVP_REPO": here.repo, "AVP_VIEWS": views().display().to_string() } });
        let out = Command::new("claude").args(["mcp", "add-json", "--scope", "local", "avp", &cfg.to_string()]).current_dir(top).output()?;
        if !out.status.success() {
            bail!("claude mcp add-json failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    // Writing in ~/avp/... is writing outside the project: without this, every edit asks.
    let settings = top.join(".claude/settings.local.json");
    let mut v: Value = std::fs::read_to_string(&settings).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
    let dir = views().display().to_string();
    let mut dirs = v["permissions"]["additionalDirectories"].as_array().cloned().unwrap_or_default();
    if !dirs.iter().any(|x| x.as_str() == Some(dir.as_str())) {
        dirs.push(json!(dir));
        v["permissions"]["additionalDirectories"] = json!(dirs);
        std::fs::create_dir_all(settings.parent().unwrap())?;
        std::fs::write(&settings, serde_json::to_string_pretty(&v)?)?;
        exclude(top, ".claude/settings.local.json");
    }
    Ok(())
}

/// Wires `a` for this checkout: its MCP config, then its rule. What it cannot write (a config git
/// tracks, a file it cannot read) it leaves alone: `agents_of` says what to add by hand.
pub fn wire(a: &Agent, here: &Here, server: &str) -> Result<()> {
    match &a.wire {
        Wire::Claude => claude_mcp(here, server)?,
        Wire::Json { files, at, shape } => {
            let file = json_file(here, files);
            anyhow::ensure!(!tracked(&here.top, file), "{file} is tracked by git: AVP leaves it as it is");
            if merge_json(&here.top.join(file), at, &entry(*shape, &here.exe, Some(&here.top)))? {
                exclude(&here.top, file);
            }
        }
        Wire::Toml { file, table } => {
            anyhow::ensure!(!tracked(&here.top, file), "{file} is tracked by git: AVP leaves it as it is");
            if merge_toml(&here.top.join(file), table, &codex_entry(here))? {
                exclude(&here.top, file);
            }
        }
        Wire::Global { .. } | Wire::Shell => {}
    }
    let Some((file, mark)) = rule_file(a, here) else { return Ok(()) };
    if let Rule::Shell { config, .. } = &a.rule {
        write_rule(here, file, &shell_rule(&here.repo), mark)?;
        // `avp open` makes the checkout a view: its state stays out of git status too.
        exclude(&here.top, ".avp/");
        let p = here.top.join(config);
        if !p.exists() {
            // Aider commits every edit to git by itself: here, landing delivers.
            std::fs::write(&p, format!("# Written by `avp up`: this repository is delivered through AVP.\nread: {file}\nauto-commits: false\n"))?;
            exclude(&here.top, config);
        }
        return Ok(());
    }
    write_rule(here, file, AGENTS_RULE, mark)
}

/// The file `a` reads the rule in, in this checkout, and what in it says the rule is there. None
/// when the MCP server's instructions are enough.
fn rule_file(a: &Agent, here: &Here) -> Option<(&'static str, &'static str)> {
    match &a.rule {
        Rule::Served => None,
        Rule::Agents => Some(("AGENTS.md", "avp_land")),
        Rule::First { before } => Some((before.iter().copied().find(|f| here.top.join(f).exists()).unwrap_or("AGENTS.md"), "avp_land")),
        Rule::Own { file, settings } => Some((if reads_agents_md(here, settings) { "AGENTS.md" } else { file }, "avp_land")),
        Rule::Shell { file, .. } => Some((file, "avp land")),
    }
}

/// Where the rule waits when the repo has its own `file`: in AVP's home, for someone to add it.
fn aside(here: &Here, file: &str) -> PathBuf {
    home().join(format!("{}.{}", here.repo, file.trim_start_matches('.').replace('/', "-")))
}

/// Writes `file` with the rule when the repo has none, kept out of `git status`. A repo that has
/// its own keeps it as it is: the rule waits beside it.
fn write_rule(here: &Here, file: &str, rule: &str, mark: &str) -> Result<()> {
    let p = here.top.join(file);
    match std::fs::read_to_string(&p) {
        Ok(s) if s.contains(mark) => {}
        Ok(_) => std::fs::write(aside(here, file), rule)?,
        Err(_) => {
            std::fs::write(&p, rule)?;
            exclude(&here.top, file);
        }
    }
    Ok(())
}

/// Whether the first of `settings` that names context files (`context.fileName`, or the former
/// `contextFileName`: a name or a list) names AGENTS.md.
fn reads_agents_md(here: &Here, settings: &[&str]) -> bool {
    for s in settings {
        let Some(v) = read_jsonc(&place(here, s)) else { continue };
        let names = v.get("context").and_then(|c| c.get("fileName")).or_else(|| v.get("contextFileName"));
        if let Some(n) = names {
            return n.as_str() == Some("AGENTS.md") || n.as_array().is_some_and(|a| a.iter().any(|x| x.as_str() == Some("AGENTS.md")));
        }
    }
    false
}

/// An agent as `avp agents` shows it.
#[derive(serde::Serialize)]
pub struct Seen {
    pub agent: &'static str,
    pub name: &'static str,
    /// `wired`, `found` (installed, not wired) or `missing` (not installed).
    pub state: &'static str,
    /// Where it is wired.
    pub wired: Vec<String>,
    /// What the agent asks of a person before it loads what is wired, if anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
    /// What wires it, when it is found and not wired.
    pub todo: Vec<String>,
}

fn read_jsonc(p: &Path) -> Option<Value> {
    serde_json::from_str(&blank_jsonc(&std::fs::read_to_string(p).ok()?)).ok()
}

fn json_has(p: &Path, at: &[&str]) -> bool {
    read_jsonc(p).is_some_and(|v| at.iter().try_fold(&v, |v, k| v.get(*k)).and_then(|o| o.get("avp")).is_some())
}

/// Each agent of the table, for this checkout. `failed`: what `avp up` could not wire, and why.
pub fn agents_of(here: &Here, failed: &BTreeMap<&str, String>) -> Vec<Seen> {
    let up = "run `avp up`".to_string();
    AGENTS
        .iter()
        .map(|a| {
            let (mut wired, mut todo) = (vec![], vec![]);
            if !installed(a) {
                return Seen { agent: a.id, name: a.name, state: "missing", wired, note: None, todo };
            }
            if let Some(e) = failed.get(a.id) {
                todo.push(e.clone());
            }
            match &a.wire {
                Wire::Claude => {
                    let ok = Command::new("claude").args(["mcp", "get", "avp"]).current_dir(&here.top).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
                    if ok { wired.push("MCP server for this folder".into()) } else { todo.push(up.clone()) }
                }
                Wire::Json { files, at, shape } => {
                    let file = json_file(here, files);
                    let p = here.top.join(file);
                    if json_has(&p, at) {
                        wired.push(file.to_string());
                    } else if tracked(&here.top, file) || (p.exists() && read_jsonc(&p).is_none()) {
                        todo.push(by_hand(file, at, &entry(*shape, &here.exe, Some(&here.top))));
                    } else {
                        todo.push(up.clone());
                    }
                }
                Wire::Toml { file, table } => {
                    let p = here.top.join(file);
                    let kept = std::fs::read_to_string(&p).ok().and_then(|t| t.parse::<toml::Table>().ok());
                    if kept.as_ref().is_some_and(|t| t.get(*table).and_then(|m| m.get("avp")).is_some()) {
                        wired.push(file.to_string());
                    } else if tracked(&here.top, file) || (p.exists() && kept.is_none()) {
                        let e = codex_entry(here);
                        todo.push(format!("add [{table}.avp] with command = {}, args = {} to {file}", e["command"], e["args"]));
                    } else {
                        todo.push(up.clone());
                    }
                }
                Wire::Global { file, at, shape } => {
                    if json_has(&place(here, file), at) {
                        wired.push(file.to_string());
                    } else {
                        todo.push(by_hand(file, at, &entry(*shape, &here.exe, None)));
                    }
                }
                Wire::Shell => {}
            }
            if let Some((file, mark)) = rule_file(a, here) {
                match std::fs::read_to_string(here.top.join(file)) {
                    Ok(s) if s.contains(mark) => wired.push(format!("rule in {file}")),
                    Ok(_) => todo.push(format!("add the AVP rule to {file} (`avp up` writes it in {})", aside(here, file).display())),
                    Err(_) => todo.push(up.clone()),
                }
            }
            if let Rule::Shell { file, config } = &a.rule {
                match std::fs::read_to_string(here.top.join(config)) {
                    Ok(s) if s.contains(file) => {}
                    Ok(_) => todo.push(format!("add `read: {file}` to {config}, or start it with `aider --read {file}`")),
                    Err(_) => todo.push(up.clone()),
                }
            }
            let mut once = std::collections::HashSet::new();
            todo.retain(|t| once.insert(t.clone()));
            let state = if todo.is_empty() { "wired" } else { "found" };
            let note = (!a.note.is_empty() && !(a.id == "codex" && codex_trusts(&here.top))).then_some(a.note);
            Seen { agent: a.id, name: a.name, state, wired, note, todo }
        })
        .collect()
}

/// The short list `avp up` and `avp agents` print: one line per agent found, one for the others.
pub fn listed(seen: &[Seen]) -> Vec<String> {
    let w = seen.iter().filter(|s| s.state != "missing").map(|s| s.name.len()).max().unwrap_or(0);
    let mut out: Vec<String> = seen
        .iter()
        .filter(|s| s.state != "missing")
        .map(|s| match s.state {
            "wired" => format!("{:w$}  wired: {}{}", s.name, s.wired.join(", "), s.note.map(|n| format!(" ({n})")).unwrap_or_default()),
            _ => format!("{:w$}  found, not wired: {}", s.name, s.todo.join("; ")),
        })
        .collect();
    let missing: Vec<&str> = seen.iter().filter(|s| s.state == "missing").map(|s| s.name).collect();
    if !missing.is_empty() {
        out.push(format!("not installed: {}", missing.join(", ")));
    }
    out
}

/// `avp agents`: the list, for the checkout `dir` is in.
pub fn agents(dir: &Path, as_json: bool) -> Result<()> {
    let top = git_top(dir).context("not inside a git checkout: run `avp agents` from your repository")?;
    let kept: Option<Value> = std::fs::read_to_string(kept_path(&top)).ok().and_then(|t| serde_json::from_str(&t).ok());
    let repo = kept.as_ref().and_then(|k| k["env"]["AVP_REPO"].as_str().map(String::from)).or_else(|| repo_name(&top)).context("no name for this repo")?;
    let here = Here { top, repo, exe: std::env::current_exe()? };
    let seen = agents_of(&here, &BTreeMap::new());
    if as_json {
        println!("{}", serde_json::to_string_pretty(&seen)?);
    } else {
        for line in listed(&seen) {
            println!("{line}");
        }
    }
    Ok(())
}

fn tracked(top: &Path, rel: &str) -> bool {
    Command::new("git").arg("-C").arg(top).args(["ls-files", "--error-unmatch", rel]).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Keeps a file AVP wrote out of `git status`, in .git/info/exclude (local to this clone). In a view
/// git does not hold (`avp clone`), out of the task's writes, in .avp/exclude.
fn exclude(top: &Path, rel: &str) {
    let p = if view::state_path(top).exists() && !top.join(".git").exists() {
        top.join(".avp").join("exclude")
    } else {
        if tracked(top, rel) {
            return;
        }
        let Ok(out) = Command::new("git").arg("-C").arg(top).args(["rev-parse", "--git-path", "info/exclude"]).output() else { return };
        if !out.status.success() {
            return;
        }
        top.join(String::from_utf8_lossy(&out.stdout).trim())
    };
    let cur = std::fs::read_to_string(&p).unwrap_or_default();
    if !cur.lines().any(|l| l.trim() == rel) {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(&p, format!("{cur}{}{rel}\n", if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" }));
    }
}

/// `text` with its comments and trailing commas turned to spaces: what a strict JSON parser reads,
/// at the same byte offsets. VS Code and Zed write their settings as JSON with comments.
fn blank_jsonc(text: &str) -> String {
    let mut b = text.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1)) {
            (b'"', _) => i = string_end(&b, i),
            (b'/', Some(b'/')) => {
                while i < b.len() && b[i] != b'\n' {
                    b[i] = b' ';
                    i += 1;
                }
            }
            (b'/', Some(b'*')) => {
                let start = i;
                i += 2;
                while i < b.len() && !(b[i - 1] == b'*' && b[i] == b'/' && i > start + 2) {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                for c in &mut b[start..i] {
                    if *c != b'\n' {
                        *c = b' ';
                    }
                }
            }
            _ => i += 1,
        }
    }
    // A comma whose next mark is a closing bracket, once the comments are gone.
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => i = string_end(&b, i),
            b',' => {
                if matches!(b.get(ws(&b, i + 1)), Some(b'}' | b']')) {
                    b[i] = b' ';
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    // Only ASCII was replaced, by ASCII: still UTF-8.
    String::from_utf8(b).unwrap_or_default()
}

fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Just past the closing quote of the string whose opening quote is at `i`.
fn string_end(b: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < b.len() && b[i] != b'"' {
        i += if b[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(b.len())
}

/// Just past the JSON value that starts at `i`.
fn value_end(b: &[u8], i: usize) -> usize {
    match b.get(i) {
        Some(b'"') => string_end(b, i),
        Some(b'{' | b'[') => {
            let (mut depth, mut j) = (0, i);
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
            j
        }
        _ => {
            let mut j = i;
            while j < b.len() && !b",}] \t\r\n".contains(&b[j]) {
                j += 1;
            }
            j
        }
    }
}

/// The value of member `key` in the object whose `{` is at `open`: where it starts.
fn member(b: &[u8], open: usize, key: &str) -> Option<usize> {
    let mut i = ws(b, open + 1);
    while b.get(i) == Some(&b'"') {
        let end = string_end(b, i);
        let name: String = serde_json::from_slice(&b[i..end]).ok()?;
        let v = ws(b, ws(b, end) + 1);
        if name == key {
            return Some(v);
        }
        i = ws(b, value_end(b, v));
        if b.get(i) == Some(&b',') {
            i = ws(b, i + 1);
        }
    }
    None
}

/// Puts `"avp": entry` under the keys `at` of the JSON file `p` (comments allowed), creating the
/// file or the keys it lacks. Every byte the file had stays as it was: the entry is written in, the
/// file is never written over. False when an `avp` entry was already there.
pub fn merge_json(p: &Path, at: &[&str], entry: &Value) -> Result<bool> {
    let mut nested = json!({ "avp": entry });
    for k in at.iter().rev() {
        nested = json!({ *k: nested });
    }
    let Some(text) = std::fs::read_to_string(p).ok().filter(|t| !t.trim().is_empty()) else {
        std::fs::create_dir_all(p.parent().context("no folder")?)?;
        std::fs::write(p, serde_json::to_string_pretty(&nested)? + "\n")?;
        return Ok(true);
    };
    let blank = blank_jsonc(&text);
    let v: Value = serde_json::from_str(&blank).map_err(|e| anyhow::anyhow!("{} is not JSON AVP can read ({e})", p.display()))?;
    let pointer = |v: &Value| at.iter().try_fold(v, |v, k| v.get(*k)).and_then(|o| o.get("avp")).cloned();
    if pointer(&v).is_some() {
        return Ok(false);
    }
    let b = blank.as_bytes();
    let mut open = ws(b, 0);
    anyhow::ensure!(b.get(open) == Some(&b'{'), "{} does not hold a JSON object", p.display());
    // The deepest object on the way that is there: what it lacks goes in it.
    let mut depth = 0;
    while depth < at.len() {
        match member(b, open, at[depth]) {
            Some(v) if b.get(v) == Some(&b'{') => open = v,
            Some(_) => bail!("\"{}\" in {} is not an object", at[depth], p.display()),
            None => break,
        }
        depth += 1;
    }
    let mut rest = &nested;
    for k in &at[..depth] {
        rest = &rest[*k];
    }
    let (key, value) = rest.as_object().and_then(|o| o.iter().next()).context("nothing to add")?;
    // Indented one step deeper than the line the object opens on, in the file's own unit.
    let line = &text[text[..open].rfind('\n').map_or(0, |n| n + 1)..];
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let unit = text.lines().map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect::<String>()).filter(|s| !s.is_empty()).min_by_key(|s| s.len()).unwrap_or_else(|| "  ".into());
    let empty = b.get(ws(b, open + 1)) == Some(&b'}');
    let added = format!("\n{indent}{unit}{}: {}{}", json!(key), value, if empty { format!("\n{indent}") } else { ",".into() });
    let merged = format!("{}{added}{}", &text[..open + 1], &text[open + 1..]);
    let check: Value = serde_json::from_str(&blank_jsonc(&merged)).map_err(|e| anyhow::anyhow!("{} would not stay JSON ({e})", p.display()))?;
    anyhow::ensure!(pointer(&check).as_ref() == Some(entry), "{}: the entry did not land where it should", p.display());
    std::fs::write(p, merged)?;
    Ok(true)
}

/// Appends `[<table>.avp]` to the TOML file `p` (Codex's `.codex/config.toml`), or creates it. The
/// file's own lines and comments stay as they were. False when that table was already there.
pub fn merge_toml(p: &Path, table: &str, entry: &toml::Table) -> Result<bool> {
    let text = std::fs::read_to_string(p).unwrap_or_default();
    let v: toml::Table = text.parse().map_err(|e| anyhow::anyhow!("{} is not TOML AVP can read ({e})", p.display()))?;
    if v.get(table).and_then(|t| t.get("avp")).is_some() {
        return Ok(false);
    }
    let mut added = format!("[{table}.avp]\n");
    for (k, x) in entry {
        added.push_str(&format!("{k} = {x}\n"));
    }
    let merged = if text.is_empty() { added } else { format!("{text}{}\n{added}", if text.ends_with('\n') { "" } else { "\n" }) };
    let check: toml::Table = merged.parse().map_err(|e| anyhow::anyhow!("{} would not stay TOML ({e})", p.display()))?;
    anyhow::ensure!(check.get(table).and_then(|t| t.get("avp")) == Some(&toml::Value::Table(entry.clone())), "{}: the entry did not land where it should", p.display());
    std::fs::create_dir_all(p.parent().context("no folder")?)?;
    std::fs::write(p, merged)?;
    Ok(true)
}

/// Where `avp up` keeps, for one checkout, what `avp mcp` needs to find its server when the agent
/// that starts it passes no environment (an editor opened from the Dock): outside the checkout and
/// readable by the user only, since the server's address, or AVP_TOKEN, is a secret. The agents'
/// configs in the checkout never hold it.
fn kept_path(top: &Path) -> PathBuf {
    let key: String = top.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    home().join("checkouts").join(format!("{key}.json"))
}

const KEPT: [&str; 6] = ["AVP_SERVER", "AVP_REPO", "AVP_VIEWS", "AVP_TOKEN", "AVP_USER", "AVP_PERSON"];

pub fn keep(top: &Path, server: &str, repo: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut env = serde_json::Map::new();
    for k in KEPT {
        let v = match k {
            "AVP_SERVER" => Some(server.to_string()),
            "AVP_REPO" => Some(repo.to_string()),
            "AVP_VIEWS" => Some(views().display().to_string()),
            _ => std::env::var(k).ok().filter(|v| !v.is_empty()),
        };
        if let Some(v) = v {
            env.insert(k.into(), json!(v));
        }
    }
    let p = kept_path(top);
    std::fs::create_dir_all(p.parent().context("no folder")?)?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p)?;
    // A file made before with wider permissions keeps them through `mode`: narrowed first.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(serde_json::to_string_pretty(&json!({ "checkout": top, "env": env }))?.as_bytes())?;
    Ok(())
}

/// Sets each variable `avp up` kept for the checkout `dir` is in that the environment lacks, and
/// returns the server it names. For `avp mcp`, before anything reads the environment.
pub fn adopt(dir: &Path) -> Option<String> {
    // A view `avp clone` made is no git checkout: kept under its own folder.
    let top = git_top(dir).or_else(|| view::state_path(dir).exists().then(|| dir.canonicalize().ok()).flatten())?;
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(kept_path(&top)).ok()?).ok()?;
    if kept["checkout"].as_str() != Some(top.to_string_lossy().as_ref()) {
        return None;
    }
    for k in KEPT {
        if let (None, Some(v)) = (std::env::var_os(k), kept["env"][k].as_str()) {
            std::env::set_var(k, v);
        }
    }
    std::env::var("AVP_SERVER").ok()
}

pub struct Up {
    pub name: Option<String>,
    pub branch: String,
    pub push: bool,
    pub history: usize,
    pub checks: bool,
    /// Run a mirror from this checkout even though the server is a team's (it mirrors itself).
    pub mirror: bool,
}

/// Whether the server is a team's, anything but this machine: it mirrors its own line.
pub fn is_team(client: &Client) -> bool {
    !(client.base.starts_with("http://127.0.0.1:") || client.base.starts_with("http://localhost:"))
}

/// The server `client` talks to: running, or started here when it is this machine's and none
/// answers, its data in the AVP home.
pub fn serve_here(client: &Client) -> Result<String> {
    let h = home();
    std::fs::create_dir_all(&h)?;
    if alive(client) {
        return Ok(format!("{} (running)", client.shown()));
    }
    let Some(listen) = client.base.strip_prefix("http://").filter(|a| a.starts_with("127.0.0.1:") || a.starts_with("localhost:")) else {
        bail!("no AVP server answers at {}", client.shown());
    };
    spawn(&std::env::current_exe()?, &["serve".into(), "--listen".into(), listen.into(), "--data".into(), h.join("data").display().to_string()], &h, &h.join("server.log"), &h.join("server.pid"))?;
    let t0 = std::time::Instant::now();
    while !alive(client) {
        anyhow::ensure!(t0.elapsed().as_secs() < 15, "the local server did not start: see {}", h.join("server.log").display());
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(format!("{} (started, data in {})", client.base, h.join("data").display()))
}

/// The repo `name` on the server, imported from the checkout's history when it is not there yet.
/// The checkout stays a plain git checkout.
pub fn repo_here(client: &Client, top: &Path, name: &str, history: usize) -> Result<(Repo, String)> {
    match client.get::<Repo>(&format!("/repos/{name}")) {
        Ok(r) => {
            let head: LineEntry = client.get(&format!("/repos/{}/head", r.id))?;
            Ok((r, format!("{name}, already on AVP (head of the line: position {})", head.position)))
        }
        Err(_) => {
            let (r, last) = gitbridge::import_git(client, top, name, "advisory", history)?;
            let what = if last == 0 { "the commit at HEAD is position 0".to_string() } else { format!("the last {} commits of HEAD are positions 0 to {last}", last + 1) };
            Ok((r, format!("{name}, imported: {what}")))
        }
    }
}

/// The mirror that writes every landed task as a commit on `branch`, pushed to origin when `push`
/// and there is one: started from the checkout unless one runs. A team server mirrors its line
/// itself: none starts here unless `force`.
pub fn mirror_here(client: &Client, top: &Path, name: &str, branch: &str, push: bool, force: bool) -> Result<String> {
    anyhow::ensure!(branch != "main" && branch != "master", "the mirror writes its own branch; `avp mirror --branch {branch}` makes it a mirror on purpose");
    let h = home();
    let remote = push.then(|| Command::new("git").arg("-C").arg(top).args(["remote", "get-url", "origin"]).output().ok().filter(|x| x.status.success()).map(|_| "origin".to_string())).flatten();
    let pid_file = h.join(format!("mirror-{name}.pid"));
    if is_team(client) && !force {
        return Ok("none from this checkout: the team server mirrors its line (--mirror to run one here)".to_string());
    }
    if let Some(pid) = pid_alive(&pid_file) {
        return Ok(format!("already running (pid {pid}), writes {branch}"));
    }
    let mut args = vec!["--server".into(), client.base.clone(), "-C".into(), top.display().to_string(), "mirror".into(), "--repo".into(), name.to_string(), "--branch".into(), branch.to_string(), "--follow".into(), "15".into()];
    if let Some(r) = &remote {
        args.extend(["--remote".into(), r.clone()]);
    }
    spawn(&std::env::current_exe()?, &args, top, &h.join(format!("mirror-{name}.log")), &pid_file)?;
    Ok(match &remote {
        Some(r) => format!("every landed task becomes a commit on {r}/{branch}, pushed within 15 s"),
        None => format!("every landed task becomes a commit on the local branch {branch} (no push)"),
    })
}

/// Each coding agent installed here, wired through its own config in the checkout, and what is
/// left to wire by hand. The server and its secrets stay outside it, where `avp mcp` finds them.
pub fn wire_here(client: &Client, top: &Path, name: &str) -> Result<Vec<Seen>> {
    keep(top, &client.base, name)?;
    let here = Here { top: top.to_path_buf(), repo: name.to_string(), exe: std::env::current_exe()? };
    let failed: BTreeMap<&str, String> = AGENTS.iter().filter(|a| installed(a)).filter_map(|a| wire(a, &here, &client.base).err().map(|e| (a.id, format!("{e:#}")))).collect();
    Ok(agents_of(&here, &failed))
}

pub fn up(client: &Client, dir: &Path, o: Up) -> Result<()> {
    let top = git_top(dir).context("not inside a git checkout: run `avp up` from your repository")?;
    let name = o.name.or_else(|| repo_name(&top)).context("no name for this repo: pass --name")?;
    let h = home();
    anyhow::ensure!(o.branch != "main" && o.branch != "master", "the mirror writes its own branch; `avp mirror --branch {}` makes it a mirror on purpose", o.branch);

    // 1. The server.
    let server_line = serve_here(client)?;

    // 2. The repo, from the checkout's history.
    let (repo, repo_line) = repo_here(client, &top, &name, o.history)?;

    // 3. The checks its proofs run.
    let head: LineEntry = client.get(&format!("/repos/{}/head", repo.id))?;
    let tree: Tree = client.get(&format!("/repos/{}/tree/{}", repo.id, head.position))?;
    let checks_line = if tree.get("avp.toml").is_some() || tree.get("trame.toml").is_some() {
        "avp.toml is on the line".to_string()
    } else if !o.checks {
        "none (--no-checks): proofs pass without steps until an avp.toml says what to run".into()
    } else if let Some((toml, names)) = guess_checks(&top) {
        eprintln!("landing avp.toml ({}): its first proof installs the dependencies once...", names.join(", "));
        // A setup that fails is a guess to fix, not a reason to stop: the mirror and the agents
        // still come up.
        let landed = land_files(client, &repo, "AVP: the checks a task proves before it lands (avp.toml, guessed by avp up)", &[("avp.toml", &toml)]);
        match landed {
            Ok(Ok(l)) => format!("avp.toml landed at position {}: {}", l.position, names.join(", ")),
            Ok(Err(n)) => {
                let p = h.join(format!("{name}.avp.toml"));
                std::fs::write(&p, &toml)?;
                // The step that failed and its last words: `cargo: command not found` says what to do.
                let failed = n.proof.as_ref().and_then(|pr| pr.steps.iter().find(|s| s.exit != 0)).map(|s| {
                    let last = s.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                    format!(", {} failed: {}", s.name, last.chars().take(160).collect::<String>())
                });
                format!("the guessed checks did not pass on the head ({}{}): fix {} and land it as a task", n.reason, failed.unwrap_or_default(), p.display())
            }
            Err(e) => {
                let p = h.join(format!("{name}.avp.toml"));
                std::fs::write(&p, &toml)?;
                let why = format!("{e:#}");
                let last = why.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                format!("the guessed checks could not run on the head ({last}): fix {} and land it as a task", p.display())
            }
        }
    } else {
        "none guessed: write an avp.toml (see README) for proofs to run your typecheck and tests".into()
    };

    // 4. The mirror: every landed task becomes a commit on its own branch, pushed when there is a remote.
    // A team server (anything but this machine) mirrors its line itself: joining it starts none.
    let team = is_team(client);
    let mirror_line = mirror_here(client, &top, &name, &o.branch, o.push, o.mirror)?;

    // 5. The agents: each one installed here, wired through its own config in this checkout.
    let seen = wire_here(client, &top, &name)?;

    println!("AVP is on for {}", top.display());
    println!("  server   {server_line}");
    println!("  repo     {repo_line}");
    println!("  checks   {checks_line}");
    if !checks_line.starts_with("avp.toml is on the line") && !crate::importci::ci_files(&top).is_empty() {
        println!("           your CI has more to say: `avp import-ci` proposes avp.toml from its jobs, services and deploys");
    }
    println!("  mirror   {mirror_line}");
    for (i, line) in listed(&seen).iter().enumerate() {
        println!("  {}{line}", if i == 0 { "agents   " } else { "         " });
    }
    println!("  folders  agents work in {}/<repo>-<n>, one each at a time: taken again with its install, or cloned from the head with the install of a folder whose lockfiles are the same", views().display());
    let wired: Vec<&str> = seen.iter().filter(|s| s.state == "wired").map(|s| s.name).collect();
    let who = match wired.split_last() {
        None => "a coding agent (`avp agents` says how to wire one)".to_string(),
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    };
    println!("Your branches, main and the files here are untouched. Start {who} in this folder and ask for a change:");
    if team && !o.mirror {
        println!("it opens a task, proves it, and lands it on the team's line. `avp log` shows the line.");
    } else {
        println!("it opens a task, proves it, lands it, and the mirror writes it on {}. `avp log` shows the line, `avp down` stops the mirror.", o.branch);
    }
    Ok(())
}

/// `avp clone`: the head of `repo` laid out in a new folder (`into`, or the repo's name under `cwd`),
/// a view attached to the server `client` talks to, with no git. Each coding agent installed here is
/// wired for it as `up` wires a checkout, the files that takes kept out of the task's writes
/// (.avp/exclude), and what `avp mcp` needs kept as for a checkout (`keep`).
pub fn clone(client: &Client, repo: &str, into: Option<PathBuf>, cwd: &Path, as_json: bool) -> Result<()> {
    let r: Repo = client.get(&format!("/repos/{repo}")).with_context(|| format!("no repo {repo} on {}", client.shown()))?;
    let dir = into.unwrap_or_else(|| cwd.join(&r.name));
    anyhow::ensure!(std::fs::read_dir(&dir).map_or(true, |mut d| d.next().is_none()), "{} is not empty: clone into a new folder", dir.display());
    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    let head: LineEntry = client.get(&format!("/repos/{}/head", r.id))?;
    let tree: Tree = client.get(&format!("/repos/{}/tree/{}", r.id, head.position))?;
    let mut state = view::ViewState { server: client.base.clone(), repo: r.id.clone(), base: Some((head.position, head.tree.clone())), ..Default::default() };
    let n = view::lay_out(client, &tree, &dir, &mut state, &[])?;
    // The replica too: offline, the clone reads the line as any view does.
    crate::offline::refresh(client, &r.id, false);
    keep(&dir, &client.base, &r.name)?;
    let here = Here { top: dir.clone(), repo: r.name.clone(), exe: std::env::current_exe()? };
    let failed: BTreeMap<&str, String> = AGENTS.iter().filter(|a| installed(a)).filter_map(|a| wire(a, &here, &client.base).err().map(|e| (a.id, format!("{e:#}")))).collect();
    let seen = agents_of(&here, &failed);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&json!({ "repo": r.name, "position": head.position, "dir": dir, "files": n, "agents": seen }))?);
        return Ok(());
    }
    println!("{} cloned into {} at position {} ({n} files): a view of the line, no git", r.name, dir.display(), head.position);
    for (i, line) in listed(&seen).iter().enumerate() {
        println!("  {}{line}", if i == 0 { "agents   " } else { "         " });
    }
    println!("Start an agent there and ask for a change, or `avp open --goal \"<one line>\"` in it: a task on the head of the line, then `avp land`.");
    Ok(())
}

/// Stops this checkout's mirror and, once no mirror is left, the local server `avp up` started.
pub fn down(dir: &Path, name: Option<String>) -> Result<()> {
    let h = home();
    let name = name.or_else(|| repo_name(dir)).context("not inside a git checkout: pass --name")?;
    let stop = |pid_file: &Path, what: &str| {
        if let Some(pid) = pid_alive(pid_file) {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            println!("{what} stopped (pid {pid})");
        }
        let _ = std::fs::remove_file(pid_file);
    };
    stop(&h.join(format!("mirror-{name}.pid")), &format!("mirror of {name}"));
    let others = std::fs::read_dir(&h).map(|d| d.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with("mirror-") && e.file_name().to_string_lossy().ends_with(".pid")).any(|e| pid_alive(&e.path()).is_some())).unwrap_or(false);
    if !others {
        stop(&h.join("server.pid"), "local server");
    }
    println!("The line and its data stay in {}: `avp up` starts again where it was.", h.join("data").display());
    Ok(())
}
