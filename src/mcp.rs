//! `avp mcp`: the verbs as MCP tools over stdio (JSON-RPC 2.0, one message per line), for an
//! agent that runs in a view directory. Every tool syncs the directory first when it matters.

use crate::client::Client;
use crate::engine::Engine;
use crate::model::*;
use crate::offline;
use crate::view;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write as _};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Mcp {
    /// The session's view: the directory the agent started in, or the folder avp_open laid out
    /// last. A call for one of several open tasks acts on that task's folder instead (`CALL_DIR`).
    dir: Mutex<PathBuf>,
    client: Client,
    /// This connection: written on every journal line and on the tasks it opens.
    session: String,
    /// The agent on the other end, as its client names itself (claude-code, codex-mcp-client...).
    client_name: Mutex<Option<String>>,
    /// The behaviour journal, one JSON line per session start, tool call and session end:
    /// whether agents reach for AVP on their own, in what order, and where they fail.
    /// $AVP_MCP_LOG (empty: off), default ~/.local/state/avp/mcp.jsonl.
    journal: Option<PathBuf>,
    /// The folders this session's calls act on, each with its lock, its watcher and what it took:
    /// a lead's subagents share one `avp mcp`, each with a task and a folder of its own (28/10).
    slots: Mutex<BTreeMap<PathBuf, Slot>>,
    /// The folder the agent started in, which names its conversation's folder for Claude Code.
    started_in: PathBuf,
    /// Held by avp_open: two opens at once would take the same free folder.
    opening: Mutex<()>,
}

/// A folder this session acts on.
#[derive(Clone, Default)]
struct Slot {
    /// Held while a tool runs on this folder: its watcher syncs only between calls, and two calls
    /// on one task go one after the other while calls on other tasks run beside them.
    busy: Arc<Mutex<()>>,
    /// Stops the folder's watcher, when one runs.
    stop: Option<Arc<AtomicBool>>,
    /// Paths the task wrote outside its scope for the first time, most of them sent by the folder's
    /// watcher, whose history the next avp_status or avp_check tells.
    taken: Arc<Mutex<Vec<String>>>,
}

thread_local! {
    /// The folder of the tool call running on this thread: each call has a thread of its own.
    static CALL_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// The progress token of the tool call running on this thread, when the host gave one.
    static PROGRESS: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// The tools that act on one task, which must know which when this session has several open.
fn on_a_task(name: &str, args: &Value) -> bool {
    match name {
        "avp_status" | "avp_check" | "avp_land" | "avp_refresh" | "avp_claim" | "avp_release" | "avp_undo" | "avp_save" | "avp_abandon" | "avp_goal" | "avp_pick" | "avp_wait" | "avp_events" | "avp_outbox" => true,
        "avp_diff" => args["from"].is_null(),
        "avp_proof" => args["id"].is_null(),
        _ => false,
    }
}

/// What `task` says to a tool, given to every tool but avp_open.
const TASK_ARG: &str = "The task this call is for: its id, or the start of one. Needed only when this session has several tasks open (a lead whose subagents each opened one); with one, it is that one.";

/// What a land or a check says when the server holds its proof back: it drains (`POST /drain`)
/// before it takes a new version, and starts no proof until then.
pub const DRAINED: &str = "the server is updating to a new version: your proof starts in a moment, once the proofs already running end (nothing to do, this waits)";
/// What it says when the update restarts the server under it.
pub const RESTARTED: &str = "the server restarts on its new version: asking again once it answers";

/// Runs `ask`, a land or a check of `task`, and says once through `say` when the server holds its
/// proof for an update (`GET /drain?task=`, after 2 s, then every 10 s), so that an agent does not
/// wait in silence. The update ends in a restart, which cuts the answer: once the server answers
/// again (5 min at most), `ask` goes again, and the proof starts there. Without a drain, an error
/// is the error.
pub fn through_drain<T>(client: &Client, task: &str, say: Arc<dyn Fn(&str) + Send + Sync>, ask: impl Fn() -> Result<T>) -> Result<T> {
    loop {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let held = Arc::new(AtomicBool::new(false));
        // False once `ask` is over: nothing is said after its answer.
        let live = Arc::new(Mutex::new(true));
        {
            let (client, path, held, live, say) = (client.clone(), format!("/drain?task={task}"), held.clone(), live.clone(), say.clone());
            std::thread::spawn(move || {
                let mut every = Duration::from_secs(2);
                while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(every) {
                    every = Duration::from_secs(10);
                    // A server from before the drain, or one that restarts: nothing more to say.
                    let Ok(d) = client.get::<Value>(&path) else { return };
                    let live = live.lock().unwrap_or_else(|e| e.into_inner());
                    if *live && d["held"] == true && !held.swap(true, Ordering::Relaxed) {
                        say(DRAINED);
                    }
                }
            });
        }
        let out = ask();
        *live.lock().unwrap_or_else(|e| e.into_inner()) = false;
        drop(stop);
        match out {
            Err(e) if held.load(Ordering::Relaxed) && e.chain().any(|c| c.is::<reqwest::Error>()) => {
                say(RESTARTED);
                let back = std::time::Instant::now() + Duration::from_secs(300);
                while client.get::<Value>("/work").is_err() {
                    anyhow::ensure!(std::time::Instant::now() < back, "the server did not answer again within 5 minutes of its update: {e:#}");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
            out => return out,
        }
    }
}

/// The file a conversation is being written to: the most recently written `.jsonl` under `dir`
/// (`depth` levels down), if one was written in the last ten minutes.
fn newest_jsonl(dir: &Path, depth: usize) -> Option<PathBuf> {
    let recent = std::time::SystemTime::now() - Duration::from_secs(600);
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((d, level)) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() && level < depth {
                stack.push((p, level + 1));
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(t) = meta.modified() {
                    if t > recent && best.as_ref().is_none_or(|(b, _)| t > *b) {
                        best = Some((t, p));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Where the agent on the other end keeps this conversation, so that `avp why` can point a person
/// at how a line came to be. Claude Code writes ~/.claude/projects/<its folder, every other
/// character a dash>/<session>.jsonl; Codex ~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl. The
/// one being written right now is this one.
fn transcript_of(client: Option<&str>, started_in: &Path) -> Option<String> {
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let found = match client {
        Some(c) if c.contains("claude") => {
            let folder: String = started_in.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
            newest_jsonl(&home.join(".claude/projects").join(folder), 0)
        }
        Some(c) if c.contains("codex") => newest_jsonl(&home.join(".codex/sessions"), 3),
        _ => None,
    };
    found.map(|p| p.display().to_string())
}

/// The agent's name on the positions it lands (`avp log`, the stats, AVP Live), from the name its
/// MCP client gives, as `avp agents` names it: each client's own, read in its source (October
/// 2026). Cursor's editor says `cursor-vscode`, its CLI `Cursor`; VS Code its product name, its MCP
/// client being Copilot's. A client not known here keeps its own name (Devin Desktop sends the
/// Rust SDK's default, `rmcp`).
pub fn agent_of(client: &str) -> String {
    const KNOWN: [(&str, &str); 16] = [
        ("claude-code", "claude-code"),
        // Claude Code in Claude's desktop app.
        ("local-agent-mode-", "claude-code"),
        ("codex-mcp-client", "codex"),
        ("cursor", "cursor"),
        ("gemini-cli-mcp-client", "gemini-cli"),
        ("qwen-cli-mcp-client", "qwen-code"),
        ("qwen-code-mcp-client", "qwen-code"),
        ("visual studio code", "copilot"),
        ("github-copilot-developer", "copilot"),
        ("opencode", "opencode"),
        ("kiro", "kiro"),
        ("zed", "zed"),
        ("amp-mcp-client", "amp"),
        ("windsurf", "windsurf"),
        ("cline", "cline"),
        ("@cline/core", "cline"),
    ];
    let c = client.to_lowercase();
    KNOWN.iter().find(|(start, _)| c.starts_with(start)).map(|(_, name)| name.to_string()).unwrap_or_else(|| client.to_string())
}

/// How long the task's files stay untouched before their tree goes to be proven.
const QUIET: Duration = Duration::from_secs(3);

/// Watches a task folder: once its files have been quiet for a moment, they go to the server as
/// writes and the server proves that tree in the background, while the agent runs its own tests.
/// `avp_land` then finds the proof done. $AVP_SPECULATE=0 turns it off.
fn watch(client: Client, dir: PathBuf, busy: Arc<Mutex<()>>, stop: Arc<AtomicBool>, taken: Arc<Mutex<Vec<String>>>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            // AVP's own state and git's: not the agent's work.
            let ours = |p: &Path| p.components().any(|c| matches!(c.as_os_str().to_str(), Some(".avp" | ".trame" | ".git")));
            if ev.paths.iter().any(|p| !ours(p)) {
                let _ = tx.send(());
            }
        }
    });
    let Ok(mut watcher) = watcher else { return };
    if notify::Watcher::watch(&mut watcher, &dir, notify::RecursiveMode::Recursive).is_err() {
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return,
        }
        while rx.recv_timeout(QUIET).is_ok() {}
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // A tool is running: it syncs, and landing proves, by itself.
        let Ok(_idle) = busy.try_lock() else { continue };
        let Ok(mut s) = view::load(&dir) else { continue };
        let Some(task) = s.task.clone() else { continue };
        // Queued writes go first, sent by the outbox in order; a task opened offline is not on the server yet.
        if offline::is_local(&task) || offline::pending(&dir) > 0 {
            continue;
        }
        if let Ok(r) = view::sync(&client, &dir, &mut s) {
            taken.lock().unwrap_or_else(|e| e.into_inner()).extend(crate::history::outside_scope(r.puts.iter().chain(r.deletes.iter())));
            if !r.puts.is_empty() || !r.deletes.is_empty() {
                let _: Result<Value> = client.post(&format!("/repos/{}/tasks/{task}/speculate", s.repo), &json!({}));
            }
        }
    }
}

/// A diff as an agent reads it: the files with their lines added and removed, then each one's hunks,
/// cut past 400 lines with the way to ask for the rest.
fn diff_compact(files: &[crate::engine::Diffed], what: &str) -> String {
    if files.is_empty() {
        return format!("no change {what}");
    }
    let large: Vec<bool> = files.iter().map(|d| d.large).collect();
    let files: Vec<&FileDiff> = files.iter().map(|d| &d.file).collect();
    let counts: Vec<(usize, usize)> = files.iter().map(|f| crate::client::diff_counts(f)).collect();
    let (added, removed) = counts.iter().fold((0, 0), |(x, y), (a, d)| (x + a, y + d));
    let mut out = format!("{} files changed {what}, +{added} -{removed}", files.len());
    let (mut left, mut cut) = (400usize, Vec::new());
    for ((f, (a, d)), large) in files.iter().zip(&counts).zip(&large) {
        out.push_str(&format!("\n\n{} ({}, +{a} -{d})", f.path, f.change));
        let lines = f.patch.lines().count();
        if f.binary || *large {
            out.push_str(if f.binary { "\nbinary" } else { "\ntoo large to show: read the file" });
        } else if lines > left {
            cut.push(f.path.as_str());
        } else {
            left -= lines;
            out.push('\n');
            out.push_str(&f.patch);
        }
    }
    if !cut.is_empty() {
        out.push_str(&format!("\n\nNot shown, to keep this short: {}. avp_diff with `paths` shows them.", cut.join(", ")));
    }
    out
}

/// At most `n` characters of `s`, on one line.
fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}...", one.chars().take(n).collect::<String>()) }
}

/// What every agent connected to this server is told, so nobody has to repeat it.
const INSTRUCTIONS: &str = "This repository is delivered through AVP (agent versioning protocol), not through git: never git commit, git push or open a pull request on it. For every change: avp_open first, with a one-line goal, before reading the code (it lays the head of the line out in a folder of your own, tells you where, and lists the other open tasks); read and edit in that folder, run its tests there if useful, then avp_land: it proves the change and lands it. On red, fix and land again; a conflict comes back as markers in your files: settle them and land again. Small tasks land often. When avp_open lists other tasks, avp_tasks and avp_events say who works on what. avp_open's answer ends with what the line remembers of your scope (the last tasks that changed each path, their goals and agents, and who holds it now), and avp_context says the same of any path: read it before exploring the code. To split work among subagents: each subagent calls avp_open itself (a task and a folder of its own) and passes its `task` to every other avp_ tool; never two subagents in one task.";

/// `Fix the dock: fold to a pill` -> `fix-the-dock-fold-to-a-pill`, at most 40 characters.
/// Takes `dir` for this session: `.avp/holder` names the process that has it. A holder that is
/// gone (its agent ended) frees it. False when a live session holds it.
fn hold(dir: &Path) -> bool {
    let holder = dir.join(".avp").join("holder");
    let me = std::process::id().to_string();
    if let Ok(pid) = std::fs::read_to_string(&holder) {
        let pid = pid.trim();
        if pid == me {
            return true;
        }
        let alive = Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        if alive {
            return false;
        }
        let _ = std::fs::remove_file(&holder);
    }
    let _ = std::fs::create_dir_all(dir.join(".avp"));
    // create_new: of two sessions taking the same folder at once, one gets it.
    std::fs::OpenOptions::new().write(true).create_new(true).open(&holder).and_then(|mut f| std::io::Write::write_all(&mut f, me.as_bytes())).is_ok()
}

/// A view of `repo` under `base` that no live session holds and whose task is closed: the one used
/// last, so that the fewest files differ from the head. It is held for this session when returned.
fn free_view(client: &Client, base: &Path, repo: &str) -> Option<(PathBuf, view::ViewState)> {
    let mut views: Vec<(std::time::SystemTime, PathBuf, view::ViewState)> = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let v = view::load(&e.path()).ok().filter(|v| v.repo == repo && v.server == client.base)?;
            let used = std::fs::metadata(view::state_path(&e.path())).and_then(|m| m.modified()).ok()?;
            Some((used, e.path(), v))
        })
        .collect();
    views.sort_by(|a, b| b.0.cmp(&a.0));
    views.into_iter().find_map(|(_, dir, v)| {
        let closed = match &v.task {
            Some(t) => client.get::<TaskStatus>(&format!("/repos/{repo}/tasks/{t}")).is_ok_and(|st| st.task.state != TaskState::Open),
            None => true,
        };
        (closed && hold(&dir)).then_some((dir, v))
    })
}

fn slug(goal: &str) -> String {
    let mut out = String::new();
    for c in goal.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "task".into() } else { out }
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({ "name": name, "description": description, "inputSchema": { "type": "object", "properties": props, "required": required } })
}

fn tools() -> Vec<Value> {
    vec![
        tool("avp_open", "This repository is delivered through AVP, not git: no commit, push or pull request; every change is an AVP task (avp_open, edit, avp_land). Open a task on the head of the line before writing anything. If this directory is not an AVP view, a folder of your own holding the head of the line is laid out and its path returned: work there. It is the folder a session left free, with its install, when there is one; else a new one, cloned from the head with the install of a folder whose lockfiles are the same. `scope` lists the paths you intend to change; leases are taken on them now, and the answer says what the line remembers of them. Subagents: each one calls avp_open itself and gets a task and a folder of its own, beside the others (an open task is never taken over); with several open, every other avp_ tool takes `task`.", json!({ "goal": { "type": "string", "description": "What the task will do, one line, as a commit title." }, "repo": { "type": "string", "description": "AVP repo name, when this directory is not a view yet (default: $AVP_REPO)." }, "transcript": { "type": "string", "description": "Where your conversation lives (path or URL), if you know it: `avp why` shows it to whoever reads this code later." }, "scope": { "type": "array", "items": { "type": "string" }, "description": "Paths (files or directories) this task will write." }, "on": { "type": "string", "description": "An open task to stack this one on (its id, or the start of one): you build on its work before it lands, avp_refresh brings its latest changes, and this task lands after it (avp_land with `stack` lands both)." } }), &["goal"]),
        tool("avp_status", "The open task: live writes, leases, head position, last proof. Also sends the directory's changes as writes first.", json!({}), &[]),
        tool("avp_check", "See the proof without landing: typecheck and the related tests on the candidate tree (base + this task's writes), per avp.toml. avp_land runs it itself (and reuses a green one), so there is no need to check before landing. Returns the verdict, steps, blamed files and who holds them. Syncs first.", json!({}), &[]),
        tool("avp_wait", "Wait until other tasks have landed (or been dropped) and paths are on the line: when a red proof comes from teammates' work not landed yet, wait here instead of polling, then avp_land again. Blocks up to timeout_s (default 600). To build on another task's work before it lands, open your task on it instead (avp_open with `on`).", json!({ "tasks": { "type": "array", "items": { "type": "string" }, "description": "Task ids (or their start)." }, "paths": { "type": "array", "items": { "type": "string" }, "description": "Paths that must be on the head of the line." }, "timeout_s": { "type": "number" } }), &[]),
        tool("avp_land", "Deliver the task: this is how a change reaches the repository (never git commit or push). Land the task on the line. Syncs, replays if the line moved, verifies, then compare-and-swaps. On `conflict`, files with markers are laid out here: settle them and land again. On `red`, fix and land again. On `blocked`, another task holds a failing file: wait for it. On `stacked`, the task this one is stacked on has not landed: `stack` lands it first, then this one.", json!({ "stack": { "type": "boolean", "description": "Land the open tasks this one is stacked on first, the bottom one first, then this one." } }), &[]),
        tool("avp_refresh", "Bring into your folder the work this task builds on, your changes kept on top: what landed on the line since, or for a task stacked on another (avp_open with `on`), that task's latest changes. Syncs first; conflict markers, if any, are laid out to settle before avp_land.", json!({}), &[]),
        tool("avp_claim", "Take leases on paths you are about to write outside your scope. Returns the previous holder if someone else had it.", json!({ "paths": { "type": "array", "items": { "type": "string" } } }), &["paths"]),
        tool("avp_release", "Give leases back on paths you are done with (all of them when `paths` is empty).", json!({ "paths": { "type": "array", "items": { "type": "string" } } }), &[]),
        tool("avp_undo", "Undo writes of this task and lay the result out here. With `save`: the task goes to that save point (avp_save), back or forward, nothing lost. With `path`: only that file goes back to the base. Otherwise: every write after sequence number `to` (0 undoes everything).", json!({ "save": { "type": "string", "description": "A save point: its number or its message." }, "to": { "type": "integer" }, "path": { "type": "string" } }), &[]),
        tool("avp_save", "Name the task's state now, a save point, before trying something you may want to take back: avp_undo with `save` returns to it. Kept with the task, never landed; avp_status lists them.", json!({ "message": { "type": "string", "description": "What this state is, a few words: `parser done, tests green`." } }), &["message"]),
        tool("avp_diff", "What this task changes against the position it stands on, file by file, as a compact unified diff: review your change before avp_land. `head`: against the head of the line instead (what landed since shows too). `from` (and `to`): between positions of the line, `from` alone up to this task. `paths`: only these files or folders. Syncs first.", json!({ "head": { "type": "boolean" }, "from": { "type": "integer" }, "to": { "type": "integer" }, "paths": { "type": "array", "items": { "type": "string" } } }), &[]),
        tool("avp_events", "What happened that concerns this task since cursor `since`: writes by others on your files, leases taken over, tasks opened and landed, line verdicts, positions AVP reverted because they broke the full suite (`line.reverted`, with the failing output). Read it at the start of a turn.", json!({ "since": { "type": "integer", "description": "Last event id you saw (0 for all)." } }), &[]),
        tool("avp_log", "The line: landed tasks, newest first, with their full-suite verdict.", json!({ "limit": { "type": "integer" } }), &[]),
        tool("avp_bisect", "Find the position that broke something: `check` is a shell command that exits 0 when a position is good (a test, a build). The positions between `good` (default: the newest whose full suite is green) and `bad` (default: the head) are halved, each laid out in a scratch folder and checked, down to the first bad one, named with its goal, agent, proof and the check's last lines. Each position checked runs the command once: keep it to what tells.", json!({ "check": { "type": "string", "description": "Run by `sh -c` in the position's folder: `cargo test parser`, `pnpm vitest run src/price.test.ts`." }, "good": { "type": "integer", "description": "A position where the check passes." }, "bad": { "type": "integer", "description": "A position where it fails." } }), &["check"]),
        tool("avp_pick", "Bring a position's change into your open task, three ways, as a replay merges it: how a change AVP reverted (`line.reverted`) comes back on the head with its fix. Files with conflict markers are laid out here to settle; then avp_land proves the whole.", json!({ "position": { "type": "integer" } }), &["position"]),
        tool("avp_why", "Why a file, or one line of it, is what it is: the task that wrote it, its goal, the person and the agent, the proof, and the conversation when it is known. `path` or `path:line`.", json!({ "path": { "type": "string" } }), &["path"]),
        tool("avp_context", "What the line remembers of paths, before you read them: for each (a directory by its most changed files, 6 paths at most), the last 3 positions that changed it (position, age, agent, goal) and the open task holding it. `path:line` also says why that line is what it is. Cheaper than reading a big file or grepping for its story.", json!({ "paths": { "type": "array", "items": { "type": "string" }, "description": "Files or directories; `path:line` for one line." } }), &["paths"]),
        tool("avp_tasks", "The repo's open tasks: who is working on what right now.", json!({}), &[]),
        tool("avp_stats", "How the line is doing, as JSON: landings, task opened to landed and to each environment (median and p90 seconds), proof time, machine minutes, red proofs, replays and conflicts, red and flaky full suites, reverts, landings per agent and per person.", json!({ "since": { "type": "string", "description": "From when: 48h, 7d, a date or an RFC3339 time (default: 7d)." }, "until": { "type": "string", "description": "Until when, as since (default: now)." } }), &[]),
        tool("avp_proof", "Read a red proof where it fails: the failing step's log around its first failing line (a compiler's error, a runner's FAIL), with the steps and their durations, in few lines rather than the whole log. Then fix and avp_land again. `id`: a proof (default: this task's last), `step`: another step, `lines`: how many (default 60). Names the files a step kept (coverage, traces) for a person to download.", json!({ "id": { "type": "string", "description": "A proof id (proof_...), as avp_land, avp_events or avp_log name it." }, "step": { "type": "string" }, "lines": { "type": "integer" } }), &[]),
        tool("avp_secrets", "The repo's secrets for tests: names and when each was set, never a value. When a red step says a secret it names is not set, tell a person which one: they set it with `avp secret set NAME` (the value on stdin). An agent never handles a value.", json!({}), &[]),
        tool("avp_migrate_report", "The last `avp migrate` report of this repo: what moving the team to AVP did (server, line, mirror on avp/line, agents, avp.toml from the CI), each proof step and shard of the whole suite green or red on the head with what to change, and the plan of what is left, numbered like docs/MIGRATE.md. Read it when asked to finish a move to AVP: fix what is red in avp.toml, then ask a person to run `avp migrate` again; tell a person what is theirs (a secret to set, main to protect).", json!({ "repo": { "type": "string", "description": "AVP repo name, when this directory is neither a view nor its checkout (default: $AVP_REPO)." } }), &[]),
        tool("avp_abandon", "Drop the open task and its leases. Nothing lands.", json!({}), &[]),
        tool("avp_goal", "Say the open task's goal better before it lands: what it changes, in one sentence. Once landed, it is the line's record of the change.", json!({ "goal": { "type": "string" } }), &["goal"]),
        tool("avp_outbox", "What waits in this folder's outbox (what changed something while the server did not answer), sent in order: each entry, when it was queued, why its last send failed. When an answer says the outbox is stuck on an entry the server refused, `retry` sends it again, `drop` takes entries out by number (`avp outbox` for a person).", json!({ "retry": { "type": "boolean" }, "drop": { "type": "array", "items": { "type": "integer" }, "description": "Entry numbers, as listed." } }), &[]),
    ]
    .into_iter()
    .map(|mut t| {
        if !matches!(t["name"].as_str(), Some("avp_open" | "avp_migrate_report")) {
            t["inputSchema"]["properties"]["task"] = json!({ "type": "string", "description": TASK_ARG });
        }
        t
    })
    .collect()
}

impl Mcp {
    pub fn new(dir: &Path, client: Client) -> Self {
        let journal = match std::env::var("AVP_MCP_LOG") {
            Ok(p) if p.is_empty() => None,
            Ok(p) => Some(PathBuf::from(p)),
            Err(_) => std::env::var("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .ok()
                .map(|d| d.join("avp/mcp.jsonl")),
        };
        let session = std::env::var("AVP_SESSION").unwrap_or_else(|_| new_id("s"));
        Self { dir: Mutex::new(dir.to_path_buf()), client, session, client_name: Mutex::new(None), journal, slots: Mutex::default(), started_in: dir.to_path_buf(), opening: Mutex::default() }
    }

    /// The folder's slot, made on first use.
    fn slot(&self, dir: &Path) -> Slot {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).entry(dir.to_path_buf()).or_default().clone()
    }

    /// The tasks open in this session's folders, each with its folder.
    fn open_tasks(&self) -> Vec<(PathBuf, String, String)> {
        let mut dirs: Vec<PathBuf> = self.slots.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
        let session = self.dir.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !dirs.contains(&session) {
            dirs.push(session);
        }
        dirs.into_iter()
            .filter_map(|d| {
                let s = view::load(&d).ok()?;
                let t = s.task?;
                let st: TaskStatus = self.client.get(&format!("/repos/{}/tasks/{t}", s.repo)).ok()?;
                (st.task.state == TaskState::Open).then(|| (d, t, st.task.goal))
            })
            .collect()
    }

    /// The folder a call acts on: the one of the task `task` names; with no `task`, the session's
    /// when it has at most one task open. Of several, it never guesses: it says which there are.
    fn folder_for(&self, name: &str, args: &Value) -> Result<PathBuf> {
        let session = self.dir.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if name == "avp_open" {
            return Ok(session);
        }
        let want = args["task"].as_str().map(str::trim).filter(|t| !t.is_empty());
        if want.is_none() && !on_a_task(name, args) {
            return Ok(session);
        }
        let open = self.open_tasks();
        let list = || open.iter().map(|(d, t, g)| format!("  {t}  {g}  (folder {})", d.display())).collect::<Vec<_>>().join("\n");
        if let Some(want) = want {
            let hits: Vec<&(PathBuf, String, String)> = open.iter().filter(|(_, t, _)| t.starts_with(want)).collect();
            return match hits.as_slice() {
                [(d, _, _)] => Ok(d.clone()),
                // A read needs the repo only: a task landed since is no reason to refuse it (28/10, a
                // subagent asked avp_context with the task it had just landed).
                _ if !on_a_task(name, args) => Ok(session),
                [] if open.is_empty() => anyhow::bail!("no task open in this session: avp_open first"),
                [] => anyhow::bail!("no open task {want} in this session. Its open tasks:\n{}", list()),
                _ => anyhow::bail!("{want} is the start of several open tasks: give more of the id.\n{}", list()),
            };
        }
        match open.as_slice() {
            [(d, _, _)] => Ok(d.clone()),
            [] => Ok(session),
            _ => anyhow::bail!("{} tasks are open in this session (a lead's subagents each opened one): pass `task`, one of\n{}", open.len(), list()),
        }
    }

    /// What a land or a check says while the server drains (see through_drain): a progress
    /// notification when the host gave a token for one, and the lines said, for the tool's answer,
    /// which is what the agent reads.
    #[allow(clippy::type_complexity)]
    fn drain_say(&self) -> (Arc<dyn Fn(&str) + Send + Sync>, Arc<Mutex<Vec<String>>>) {
        let said: Arc<Mutex<Vec<String>>> = Arc::default();
        let (token, lines) = (PROGRESS.with(|p| p.borrow().clone()), said.clone());
        let say = move |line: &str| {
            let mut lines = lines.lock().unwrap_or_else(|e| e.into_inner());
            lines.push(line.to_string());
            if let Some(token) = &token {
                let note = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": { "progressToken": token, "progress": lines.len(), "message": line } });
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{note}");
                let _ = out.flush();
            }
        };
        (Arc::new(say), said)
    }

    /// Appends one line to the journal. Never fails the call it describes.
    fn log(&self, kind: &str, mut entry: Value) {
        let Some(path) = &self.journal else { return };
        entry["at"] = json!(now_ms());
        entry["kind"] = json!(kind);
        entry["session"] = json!(self.session);
        entry["client"] = json!(self.client_name.lock().unwrap_or_else(|e| e.into_inner()).clone());
        let _ = path.parent().map(std::fs::create_dir_all);
        // One write per line: agents running side by side append to the same journal, and a line
        // written in pieces gets interleaved with another's.
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(format!("{entry}\n").as_bytes());
        }
    }

    fn dir(&self) -> PathBuf {
        CALL_DIR.with(|d| d.borrow().clone()).unwrap_or_else(|| self.dir.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    /// From now on, this call and the session act on `dir`.
    fn act_in(&self, dir: &Path) {
        *self.dir.lock().unwrap_or_else(|e| e.into_inner()) = dir.to_path_buf();
        CALL_DIR.with(|d| *d.borrow_mut() = Some(dir.to_path_buf()));
    }

    fn state(&self) -> Result<view::ViewState> {
        view::load(&self.dir()).context("no AVP task here yet: call avp_open first")
    }

    /// Opens a task. A view whose task is closed is reused; any other directory (the agent's own
    /// checkout, a folder that is not a view) gets a folder under $AVP_VIEWS (default ~/avp): one
    /// a session left free, with its install, or else a new one. The head of the line is laid out
    /// there, and the tools act on that folder from now on.
    fn open(&self, args: &Value) -> Result<String> {
        let goal = args["goal"].as_str().context("goal")?.to_string();
        let scope: Vec<String> = args["scope"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        // The repo: named, or AVP_REPO, or the one `avp up` made from the git checkout the agent is in.
        let wanted = args["repo"].as_str().map(String::from).or_else(|| std::env::var("AVP_REPO").ok()).or_else(|| crate::up::repo_name(&self.dir()));
        // Two subagents opening at once would take the same free folder: one open at a time.
        let _one = self.opening.lock().unwrap_or_else(|e| e.into_inner());
        let here = view::load(&self.dir()).ok();
        // A folder whose task is open stays that task's: the new one gets a folder of its own (a
        // lead's subagents, each with a task, share this server).
        let taken = match here.as_ref().and_then(|h| h.task.as_ref().map(|t| (h, t))) {
            Some((h, t)) => self.client.get::<TaskStatus>(&format!("/repos/{}/tasks/{t}", h.repo))?.task.state == TaskState::Open,
            None => false,
        };
        let wanted = wanted.or_else(|| here.as_ref().filter(|_| taken).map(|h| h.repo.clone()));
        // The folder this session is in, unless another live session has taken it meanwhile.
        let reuse = !taken
            && match (&here, &wanted) {
                (Some(_), None) => true,
                (Some(h), Some(w)) => self.client.get::<Repo>(&format!("/repos/{w}")).map(|r| r.id == h.repo).unwrap_or(false),
                (None, _) => false,
            }
            && hold(&self.dir());
        let base = crate::up::views();
        // The clean git checkout the agent stands in, when its host gives each agent one of its own
        // (AVP_VIEW_IN_CHECKOUT): the task is laid out there, with the install it already has,
        // unless a task of this session is open there.
        let in_place = if reuse { None } else { crate::checkout::in_place_target(&self.started_in).filter(|d| hold(d) && view::load(d).ok().and_then(|v| v.task).is_none()) };
        let (dir, mut s) = if reuse {
            (self.dir(), here.unwrap())
        } else if let Some(top) = in_place.clone() {
            let key = wanted.clone().context("this folder is not an AVP view: pass `repo` (or set AVP_REPO)")?;
            let r: Repo = self.client.get(&format!("/repos/{key}"))?;
            (top, view::ViewState { server: self.client.base.clone(), repo: r.id, task: None, known: BTreeMap::new(), ..Default::default() })
        } else {
            let key = wanted.context("this folder is not an AVP view: pass `repo` (or set AVP_REPO)")?;
            let r: Repo = self.client.get(&format!("/repos/{key}"))?;
            // A folder another session left, with its install: the agent does not start from nothing.
            match free_view(&self.client, &base, &r.id) {
                Some(found) => found,
                None => {
                    let name = slug(&r.name);
                    let dir = (1..).map(|n| base.join(format!("{name}-{n}"))).find(|d| !d.exists() && hold(d)).unwrap();
                    (dir, view::ViewState { server: self.client.base.clone(), repo: r.id, task: None, known: BTreeMap::new(), ..Default::default() })
                }
            }
        };
        let repo = s.repo.clone();
        let actor = self.actor();
        let transcript = self.transcript(args);
        let on = args["on"].as_str().map(str::trim).filter(|o| !o.is_empty());
        anyhow::ensure!(on.iter().all(|o| o.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')), "on: a task id, or the start of one (avp_tasks)");
        let at = on.map(|o| format!("?on={o}")).unwrap_or_default();
        let opened: crate::blame::Opened = self.client.post(&format!("/repos/{repo}/tasks{at}"), &OpenTask { goal, actor, scope, transcript })?;
        let task = &opened.task;
        s.task = Some(task.id.clone());
        s.base = Some((task.base_position, task.base_tree.clone()));
        self.slot(&dir).taken.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // A stacked task stands on the tree of the task under it, not on a position.
        let tree: Tree = match &task.on {
            Some(_) => self.client.get(&format!("/repos/{repo}/tasks/{}/tree", task.id))?,
            None => self.client.get(&format!("/repos/{repo}/tree/{}", task.base_position))?,
        };
        if dir.starts_with(&base) || in_place.is_some() {
            view::reset(&dir, &tree, &mut s)?;
        }
        // Files the agent's checkout or another view already holds are copied, not downloaded.
        let mut seeds: Vec<PathBuf> = vec![self.started_in.clone()];
        seeds.extend(std::fs::read_dir(&base).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| *p != dir && view::load(p).is_ok_and(|v| v.repo == repo)));
        if in_place.is_some() {
            seeds.extend(crate::checkout::worktrees(&dir));
        }
        let n = view::lay_out(&self.client, &tree, &dir, &mut s, &seeds)?;
        // The install of a folder whose lockfiles are the same, cloned: the install has nothing to do.
        let deps = view::reuse_deps(&tree, &dir, &seeds);
        if in_place.is_some() {
            crate::checkout::align(&self.client, &dir, &repo, task.base_position);
        }
        self.act_in(&dir);
        offline::refresh(&self.client, &repo, false);
        if std::env::var("AVP_SPECULATE").map_or(true, |v| v != "0") {
            let stop = Arc::new(AtomicBool::new(false));
            let slot = {
                let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
                let slot = slots.entry(dir.clone()).or_default();
                if let Some(old) = slot.stop.replace(stop.clone()) {
                    old.store(true, Ordering::Relaxed);
                }
                slot.clone()
            };
            let (client, d) = (self.client.clone(), dir.clone());
            std::thread::spawn(move || watch(client, d, slot.busy, stop, slot.taken));
        }
        let install = if deps.is_empty() {
            "a JS project needs its install there once".to_string()
        } else {
            let what: Vec<String> = deps.iter().map(|d| format!("{} cloned from a folder with the same {}", d.folders.join(", "), d.lockfile)).collect();
            format!("{}: the install has nothing to do", what.join("; "))
        };
        let mut out = format!(
            "task {} opened on position {} ({} files laid out).\nYour folder: {}\nWork there, with absolute paths: read and edit files, run tests if useful ({install}), then avp_land: it proves the change, then lands it. No git commit, push or pull request on this repository.",
            task.id, task.base_position, n, dir.display()
        );
        let mine = self.open_tasks().len();
        if mine > 1 {
            out.push_str(&format!("\nThis session has {mine} tasks open, one per subagent: pass task \"{}\" to every other avp_ tool for this one.", task.id));
        }
        if task.on.is_some() {
            let st: TaskStatus = self.client.get(&format!("/repos/{repo}/tasks/{}", task.id))?;
            if let Some(w) = st.stack {
                out.push_str(&format!("\nIts folder holds that task's work as it is now. {w}"));
            }
        }
        // Who else works on this repo, and how to wait for their work, so an agent needs no
        // avp_tasks, and no loop on avp_log, to build on it.
        let open: Vec<Task> = self.client.get(&format!("/repos/{repo}/tasks?state=open")).unwrap_or_default();
        out.push_str(&format!("\n{}", crate::wait::open_hint(&open, &task.id).unwrap_or_else(|| "No other open task on this repo.".into())));
        if !task.similar.is_empty() {
            out.push_str("\nOpen tasks with a close goal (someone may already be doing this: check before duplicating, or abandon):");
            for o in &task.similar {
                out.push_str(&format!("\n  {o}"));
            }
        }
        // A position AVP reverted on these paths, or of this agent: why, before building on them.
        let reverted: Vec<Event> = self.client.get(&format!("/repos/{repo}/tasks/{}/reverted", task.id)).unwrap_or_default();
        for e in &reverted {
            out.push_str(&format!("\n{}", crate::engine::revert_hint(e)));
        }
        // What the line remembers of the scope, so the agent does not read and grep to rebuild it.
        let c = crate::blame::context_text(&opened.context, now_ms());
        if !c.is_empty() {
            out.push_str(&format!("\n{c}"));
        }
        Ok(out)
    }

    fn task(&self, s: &view::ViewState) -> Result<String> {
        s.task.clone().context("no open task in this directory: call avp_open first")
    }

    fn sync(&self, s: &mut view::ViewState) -> Result<String> {
        let r = view::sync(&self.client, &self.dir(), s)?;
        self.slot(&self.dir()).taken.lock().unwrap_or_else(|e| e.into_inner()).extend(crate::history::outside_scope(r.puts.iter().chain(r.deletes.iter())));
        let mut lines = Vec::new();
        for w in r.puts.iter().chain(r.deletes.iter()) {
            lines.push(format!("{} {} [{}{}]", if w.write.kind == WriteKind::Delete { "-" } else { "+" }, w.write.path, w.lease, w.previous_holder.as_ref().map(|h| format!(" from {h}")).unwrap_or_default()));
        }
        Ok(if lines.is_empty() { "no new writes".into() } else { lines.join("\n") })
    }

    /// What the line remembers of the paths the task first wrote outside its scope since the last
    /// answer that told it, on a line of its own; each path is told once.
    fn first_writes(&self, repo: &str, task: &str) -> String {
        let paths = std::mem::take(&mut *self.slot(&self.dir()).taken.lock().unwrap_or_else(|e| e.into_inner()));
        let c = crate::history::first_writes(&self.client, repo, task, &paths);
        if c.is_empty() { c } else { format!("\n{c}") }
    }

    fn proof_text(p: &Proof) -> String {
        let mut s = format!("verdict: {:?} (base position {})", p.verdict, p.base_position);
        for st in &p.steps {
            s.push_str(&format!("\n{} {} ({:.1}s)", if st.exit == 0 { "✓" } else { "✗" }, st.name, st.seconds));
        }
        if p.verdict != Verdict::Green {
            for (f, h) in &p.blamed {
                s.push_str(&format!("\nblamed: {f}{}", h.as_ref().map(|h| format!(" (held by {h})")).unwrap_or_default()));
            }
            if let Some(st) = p.steps.iter().find(|s| s.exit != 0) {
                let tail: Vec<&str> = crate::check::red_part(&st.output).lines().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect();
                s.push_str("\n--- output (tail) ---\n");
                s.push_str(&tail.join("\n"));
                s.push_str(&format!("\n--- avp_proof (id {}) reads the log where it first fails", p.id));
            }
        }
        s
    }

    /// avp_proof: the steps, then `n` lines of one step's log from just before where it first fails
    /// (the red step's, or `step`'s), and what the steps kept of their files.
    fn proof_read(&self, repo: &str, id: &str, step: Option<&str>, n: usize) -> Result<String> {
        let raw: Value = self.client.get(&format!("/repos/{repo}/proofs/{id}"))?;
        let p: Proof = serde_json::from_value(raw.clone())?;
        let mut s = format!("{id}: {:?} (base position {})", p.verdict, p.base_position);
        for st in &p.steps {
            s.push_str(&format!("\n{} {} ({:.1}s)", if st.exit == 0 { "✓" } else { "✗" }, st.name, st.seconds));
        }
        for (f, h) in &p.blamed {
            s.push_str(&format!("\nblamed: {f}{}", h.as_ref().map(|h| format!(" (held by {h})")).unwrap_or_default()));
        }
        let red = p.steps.iter().find(|st| st.exit != 0 && st.exit != crate::check::CANCELLED);
        let Some(st) = step.map(|n| p.steps.iter().find(|st| st.name == n).with_context(|| format!("no step {n:?} in {id}"))).transpose()?.or(red) else {
            return Ok(format!("{s}\nnothing failed: `step` reads a step's last lines"));
        };
        // A server from before keeps no log: the end of the output it has.
        let log: Value = match self.client.get(&format!("/repos/{repo}/proofs/{id}/log?{}&window={n}", crate::check::step_query(&st.name))) {
            Ok(l) => l,
            Err(_) => return Ok(format!("{s}\n--- {} (the end of its output)\n{}", st.name, crate::check::red_part(&st.output).lines().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))),
        };
        let lines: Vec<&str> = log["lines"].as_array().into_iter().flatten().filter_map(|l| l[1].as_str()).collect();
        let from = log["from"].as_u64().unwrap_or(0);
        let total = log["total"].as_u64().unwrap_or(0);
        let at = match log["first"].as_u64() {
            Some(f) => format!(", it first fails at line {}", f + 1),
            None => String::new(),
        };
        s.push_str(&format!("\n--- {}: lines {}-{} of {total}{at}", st.name, from + 1, from + lines.len() as u64));
        s.push_str(&format!("\n{}", lines.join("\n")));
        let kept: Vec<String> = p.steps.iter().zip(raw["steps"].as_array().into_iter().flatten()).filter_map(|(st, v)| v["artifacts"]["files"].as_u64().filter(|n| *n > 0).map(|n| format!("{} ({n} file{})", st.name, if n == 1 { "" } else { "s" }))).collect();
        if !kept.is_empty() {
            s.push_str(&format!("\nartifacts kept: {}. A person downloads them with `avp proof {id} --artifacts <dir>`", kept.join(", ")));
        }
        Ok(s)
    }

    fn call(&self, name: &str, args: &Value) -> Result<String> {
        // trame_*: the tools' names before AVP was named AVP, for an agent that listed them earlier.
        let name = name.strip_prefix("trame_").map(|n| format!("avp_{n}")).unwrap_or_else(|| name.to_string());
        let name = name.as_str();
        if name == "avp_open" {
            return self.open(args);
        }
        // The tools that only read work before a task is open, on the repo of this checkout: the
        // instructions send an agent to avp_tasks and avp_events first.
        let reads = matches!(name, "avp_events" | "avp_log" | "avp_why" | "avp_context" | "avp_tasks" | "avp_stats" | "avp_secrets" | "avp_diff" | "avp_bisect" | "avp_proof");
        let mut s = match self.state() {
            Ok(s) => s,
            Err(e) if reads => {
                let repo = args["repo"].as_str().map(String::from).or_else(|| std::env::var("AVP_REPO").ok()).or_else(|| crate::up::repo_name(&self.dir()));
                view::ViewState { repo: repo.ok_or(e)?, ..Default::default() }
            }
            Err(e) => return Err(e),
        };
        let repo = s.repo.clone();
        match name {
            "avp_status" => {
                let synced = self.sync(&mut s)?;
                let t = self.task(&s)?;
                let st: TaskStatus = self.client.get(&format!("/repos/{repo}/tasks/{t}"))?;
                let live = Engine::effective_writes(&st.writes);
                let mut out = format!("sync: {synced}\nfolder: {}\ntask {} ({:?}) on position {} (head {}): {}\nlive writes: {}", self.dir().display(), st.task.id, st.task.state, st.task.base_position, st.head_position, st.task.goal, live.len());
                if let Some(w) = &st.stack {
                    out.push_str(&format!("\n{w}"));
                }
                for (p, w) in &live {
                    out.push_str(&format!("\n  {} {p}", match w.kind { WriteKind::Put => "+", WriteKind::Delete => "-", WriteKind::Conflict => "! (unsettled conflict)" }));
                }
                out.push_str(&format!("\nleases: {}", st.leases.iter().map(|l| l.path.clone()).collect::<Vec<_>>().join(", ")));
                if let Some(p) = &st.last_proof {
                    out.push_str(&format!("\nlast proof: {}", Self::proof_text(p)));
                }
                let saves: Vec<crate::engine::Save> = self.client.get(&format!("/repos/{repo}/tasks/{t}/saves")).unwrap_or_default();
                if !saves.is_empty() {
                    out.push_str(&format!("\nsave points (avp_undo with `save` goes to one): {}", saves.iter().map(|s| format!("{} {:?} (write {})", s.n, s.message, s.seq)).collect::<Vec<_>>().join(", ")));
                }
                out.push_str(&format!("\nonline; {}", offline::outbox_text(&self.dir())));
                out.push_str(&self.first_writes(&repo, &t));
                offline::refresh(&self.client, &repo, false);
                Ok(out)
            }
            "avp_check" => {
                let synced = self.sync(&mut s)?;
                let t = self.task(&s)?;
                let (say, said) = self.drain_say();
                let p: Proof = through_drain(&self.client, &t, say, || self.client.check(&repo, &t))?;
                let waited: String = said.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|l| format!("{l}\n")).collect();
                Ok(format!("{waited}sync: {synced}\n{}{}", Self::proof_text(&p), self.first_writes(&repo, &t)))
            }
            "avp_land" => {
                let t = self.task(&s)?;
                // Landed already: a person approved it, and the approval landed it.
                let st: TaskStatus = self.client.get(&format!("/repos/{repo}/tasks/{t}"))?;
                if st.task.state == TaskState::Landed {
                    let position = st.task.landed_position.unwrap_or(st.head_position);
                    let tree: Tree = self.client.get(&format!("/repos/{repo}/tree/{position}"))?;
                    s.task = None;
                    view::materialize(&self.client, &tree, &self.dir(), &mut s)?;
                    view::save(&self.dir(), &s)?;
                    return Ok(format!("landed at position {position}{}", st.approval.as_ref().map(|a| format!(", approved by {}", a.person)).unwrap_or_default()));
                }
                let synced = self.sync(&mut s)?;
                let (say, said) = self.drain_say();
                let stack = args["stack"].as_bool() == Some(true);
                let landed = through_drain(&self.client, &t, say, || self.client.land_as(&repo, &t, stack))?;
                // What the agent waited for, when the server drained under it, then the answer.
                let waited: String = said.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|l| format!("{l}\n")).collect();
                match landed {
                    Ok(l) => {
                        let tree: Tree = self.client.get(&format!("/repos/{repo}/tree/{}", l.position))?;
                        s.task = None;
                        s.base = Some((l.position, l.tree.clone()));
                        view::materialize(&self.client, &tree, &self.dir(), &mut s)?;
                        offline::refresh(&self.client, &repo, true);
                        // A view in the agent's git checkout: git follows the landed position once the
                        // mirror wrote its commit, without holding the answer.
                        if crate::checkout::is_checkout_view(&self.dir()) {
                            let (client, dir, repo, position) = (self.client.clone(), self.dir(), repo.clone(), l.position);
                            std::thread::spawn(move || crate::checkout::align_when_written(&client, &dir, &repo, position, std::time::Duration::from_secs(120)));
                        }
                        let under: String = l.stack.iter().map(|b| format!("task {} landed at position {}\n", b.task, b.position)).collect();
                        Ok(format!("{waited}sync: {synced}\n{under}landed at position {}{} (proof {}). The task is closed; open a new one for the next piece of work.", l.position, if l.replayed { ", replayed on the moved line" } else { "" }, l.proof))
                    }
                    Err(n) => {
                        let under: String = n.stack.iter().map(|b| format!("task {} landed at position {}\n", b.task, b.position)).collect();
                        let mut out = format!("{waited}sync: {synced}\n{under}not landed: {}", n.reason);
                        if let Some(note) = &n.note {
                            out.push_str(&format!("\n{note}"));
                        }
                        if !n.conflicts.is_empty() {
                            let tree: Tree = self.client.get(&format!("/repos/{repo}/tasks/{t}/tree"))?;
                            view::materialize(&self.client, &tree, &self.dir(), &mut s)?;
                            out.push_str("\nconflict markers (<<<<<<< ours / ||||||| original / ======= / >>>>>>> theirs) are now in:");
                            for c in &n.conflicts {
                                out.push_str(&format!("\n  {c}"));
                            }
                            out.push_str("\nSettle each file (keep both intents when they are compatible), then avp_land again.");
                        }
                        if let Some(p) = &n.proof {
                            out.push_str(&format!("\n{}", Self::proof_text(p)));
                        }
                        if !n.waits_for.is_empty() && n.note.is_none() {
                            out.push_str(&format!("\nwaits for: {}", n.waits_for.join(", ")));
                        }
                        if n.reason == "red" {
                            if let Some(h) = crate::wait::others_hint(&self.client, &repo, &t) {
                                out.push_str(&format!("\n{h}"));
                            }
                        }
                        if n.reason == "empty" {
                            out.push_str("\nNothing to land: this task has no write. Write files first, or avp_abandon.");
                        }
                        if n.reason == "already" {
                            out.push_str("\nNothing to land: the line already holds this change, another task landed it. avp_abandon this one.");
                        }
                        if n.reason == "approval" && !n.guarded.is_empty() {
                            out.push_str(&format!("\nThese paths are guarded: {}.", n.guarded.join(", ")));
                        }
                        if n.reason == "approval" {
                            out.push_str("\nYour proof is green; this repo needs a person's approval before landing. Say so in your final message and stop: a person will approve or refuse, and you will be asked to land again.");
                        }
                        if let Some(w) = &n.word {
                            out.push_str(&format!("\nRefused by {}: {}\nAddress the reason, then avp_land again.", w.person, w.reason));
                        }
                        Ok(out)
                    }
                }
            }
            "avp_claim" => {
                let t = self.task(&s)?;
                let paths: Vec<String> = args["paths"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                let r: Vec<(String, Option<String>)> = self.client.post(&format!("/repos/{repo}/tasks/{t}/claim"), &json!({ "paths": paths }))?;
                let mut out = r.iter().map(|(p, h)| format!("{p}: {}", h.as_ref().map(|h| format!("taken from {h}")).unwrap_or_else(|| "taken".into()))).collect::<Vec<_>>().join("\n");
                // About to write outside the scope: what the line remembers of those paths.
                let c = crate::history::first_writes(&self.client, &repo, &t, &paths);
                if !c.is_empty() {
                    out.push_str(&format!("\n{c}"));
                }
                Ok(out)
            }
            "avp_release" => {
                let t = self.task(&s)?;
                let paths: Vec<String> = args["paths"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                let r: Value = self.client.post(&format!("/repos/{repo}/tasks/{t}/release"), &json!({ "paths": if paths.is_empty() { None } else { Some(paths) } }))?;
                Ok(format!("released {}", r["released"]))
            }
            "avp_undo" if args["save"].is_string() || args["save"].is_number() => {
                self.sync(&mut s)?;
                let t = self.task(&s)?;
                let to = args["save"].as_str().map(String::from).unwrap_or_else(|| args["save"].to_string());
                let r: crate::engine::Restored = self.client.post(&format!("/repos/{repo}/tasks/{t}/restore"), &json!({ "to": to }))?;
                let tree: Tree = self.client.get(&format!("/repos/{repo}/tasks/{t}/tree"))?;
                let n = view::materialize(&self.client, &tree, &self.dir(), &mut s)?;
                let mut out = format!("at save point {} ({}): {} files changed, {n} laid out in {}", r.save.n, r.save.message, r.files.len(), self.dir().display());
                if let Some(p) = r.replayed_on {
                    out.push_str(&format!("\nThe save was made on position {}; the task is on position {p} now, and the line's changes since are kept.", r.save.position));
                }
                if !r.conflicts.is_empty() {
                    out.push_str(&format!("\nConflict markers to settle before avp_land: {}", r.conflicts.join(", ")));
                }
                Ok(out)
            }
            "avp_save" => {
                self.sync(&mut s)?;
                let t = self.task(&s)?;
                let message = args["message"].as_str().context("message: what this state is, a few words")?;
                let sp: crate::engine::Save = self.client.post(&format!("/repos/{repo}/tasks/{t}/saves"), &json!({ "message": message }))?;
                Ok(format!("save point {} ({}): write {}, {} files changed. avp_undo with save {} comes back to it.", sp.n, sp.message, sp.seq, sp.files, sp.n))
            }
            "avp_diff" => {
                let list: Vec<String> = args["paths"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                let (from, to) = (args["from"].as_i64(), args["to"].as_i64());
                let task = s.task.clone().filter(|_| to.is_none());
                let (from, to) = match (from, to, &task) {
                    (Some(a), Some(b), _) => (a.to_string(), b.to_string()),
                    (Some(a), None, Some(_)) => (a.to_string(), "task".to_string()),
                    (Some(a), None, None) => (a.to_string(), "head".to_string()),
                    (None, _, Some(_)) => ((if args["head"].as_bool() == Some(true) { "head" } else { "base" }).to_string(), "task".to_string()),
                    (None, _, None) => anyhow::bail!("no open task here: avp_open first, or give `from` (and `to`) positions to compare"),
                };
                if to == "task" {
                    self.sync(&mut s)?;
                }
                let files: Vec<crate::engine::Diffed> = self.client.post(&format!("/repos/{repo}/diff"), &json!({ "task": task, "from": from, "to": to, "paths": list, "context": 1 }))?;
                let what = match (from.as_str(), to.as_str()) {
                    ("base", _) => "against the position the task stands on".to_string(),
                    ("head", _) => "against the head of the line".to_string(),
                    (a, "task") => format!("from position {a} to this task"),
                    (a, b) => format!("from position {a} to {b}"),
                };
                Ok(diff_compact(&files, &what))
            }
            "avp_undo" => {
                let t = self.task(&s)?;
                let to = args["to"].as_i64().unwrap_or(0);
                let path = args["path"].as_str();
                let r: Value = self.client.post(&format!("/repos/{repo}/tasks/{t}/undo"), &json!({ "to": to, "path": path }))?;
                let tree: Tree = self.client.get(&format!("/repos/{repo}/tasks/{t}/tree"))?;
                let n = view::materialize(&self.client, &tree, &self.dir(), &mut s)?;
                Ok(format!("undone {} writes, {} files laid out", r["undone"], n))
            }
            "avp_events" => {
                let since = args["since"].as_i64().unwrap_or(0);
                let task = s.task.as_ref().map(|t| format!("&task={t}")).unwrap_or_default();
                let mut evs: Vec<Event> = self.client.get(&format!("/repos/{repo}/events?since={since}&limit=1000{task}"))?;
                if s.task.is_none() && since == 0 {
                    // No task yet: what happened lately on the repo, not its whole history.
                    evs.retain(|e| e.kind != "line.imported" && e.kind != "write");
                    let n = evs.len().saturating_sub(15);
                    evs.drain(..n);
                }
                if evs.is_empty() {
                    return Ok("nothing new".into());
                }
                Ok(evs.iter().map(|e| format!("#{} {} {}{}{}", e.id, e.kind, e.task.as_deref().unwrap_or(""), e.path.as_ref().map(|p| format!(" {p}")).unwrap_or_default(), if e.data.is_null() || e.data == json!({}) { String::new() } else { format!(" {}", e.data) })).collect::<Vec<_>>().join("\n"))
            }
            "avp_log" => {
                let limit = args["limit"].as_i64().unwrap_or(20);
                let line: Vec<LineEntry> = self.client.get(&format!("/repos/{repo}/line?limit={limit}"))?;
                let tags: Vec<crate::db::Tag> = self.client.get(&format!("/repos/{repo}/tags")).unwrap_or_default();
                Ok(line
                    .iter()
                    .map(|e| {
                        let named: Vec<&str> = tags.iter().filter(|t| t.position == e.position).map(|t| t.name.as_str()).collect();
                        let decor = if named.is_empty() { String::new() } else { format!("(tag: {}) ", named.join(", tag: ")) };
                        format!("{:>5}  {}  {}  {decor}{}", e.position, e.full.map(|v| format!("{v:?}")).unwrap_or_else(|| "pending".into()), e.task.as_deref().unwrap_or("-"), e.note)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "avp_bisect" => {
                let check = args["check"].as_str().context("check: a shell command that exits 0 on a good position")?;
                let b = crate::history::bisect(&self.client, &repo, check, args["good"].as_i64(), args["bad"].as_i64(), &|_| {})?;
                Ok(crate::history::bisect_text(&b))
            }
            "avp_pick" => {
                let position = args["position"].as_i64().context("position: the position whose change comes into the task")?;
                let p = crate::history::pick(&self.client, &self.dir(), &mut s, position)?;
                Ok(crate::history::picked_text(&p))
            }
            "avp_why" => crate::history::why(&self.client, &repo, args["path"].as_str().context("path")?),
            "avp_context" => {
                let paths: Vec<String> = args["paths"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
                anyhow::ensure!(!paths.is_empty(), "paths: the files or directories to ask about (`path:line` for one line)");
                Ok(crate::history::context_text(&crate::history::context(&self.client, &repo, &paths, s.task.as_deref())?))
            }
            "avp_stats" => {
                let (since, until) = crate::stats::window(Some(args["since"].as_str().unwrap_or("7d")), args["until"].as_str(), now_ms())?;
                let v: Value = self.client.get(&format!("/repos/{repo}/stats?since={}&until={}", crate::stats::rfc3339(since), crate::stats::rfc3339(until)))?;
                Ok(v.to_string())
            }
            "avp_tasks" => {
                let ts: Vec<Task> = self.client.get(&format!("/repos/{repo}/tasks?state=open"))?;
                if ts.is_empty() {
                    return Ok("no open task".into());
                }
                Ok(crate::engine::stack_order(&ts).into_iter().map(|(depth, t)| format!("{}{}  {}  {} via {}  {}{}", "  ".repeat(depth), t.id, crate::engine::stands_on(t, depth), t.actor.person, t.actor.agent, t.goal, if s.task.as_deref() == Some(&t.id) { "  (this one is yours)" } else { "" })).collect::<Vec<_>>().join("\n"))
            }
            "avp_secrets" => {
                let kept: Vec<crate::check::SecretInfo> = self.client.get(&format!("/repos/{repo}/secrets"))?;
                let set = kept.iter().map(|s| format!("{}  set {} UTC", s.name, crate::blame::utc(s.set_at))).collect::<Vec<_>>().join("\n");
                let next = "a step that names another is red until a person sets it: `avp secret set NAME`, the value on stdin";
                Ok(if kept.is_empty() { format!("no secret set for this repo: {next}") } else { format!("{set}\n{next}") })
            }
            "avp_goal" => {
                let t = self.task(&s)?;
                let goal = args.get("goal").and_then(Value::as_str).unwrap_or_default();
                let _: Value = self.client.post(&format!("/repos/{repo}/tasks/{t}/goal"), &json!({ "goal": goal }))?;
                Ok(format!("goal of {t}: {}", goal.trim()))
            }
            "avp_wait" => {
                let list = |k: &str| args[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
                let secs = args["timeout_s"].as_u64().unwrap_or(600);
                let said = crate::wait::wait(&self.client, &repo, s.task.as_deref(), &list("tasks"), &list("paths"), std::time::Duration::from_secs(secs))?;
                // Ready, with a task open here: what was waited for comes into the folder, to build on.
                if said.starts_with("ready") && s.task.is_some() {
                    let moved = view::catch_up(&self.client, &self.dir(), &mut s)?;
                    view::save(&self.dir(), &s)?;
                    return Ok(format!("{said}\n{moved}"));
                }
                Ok(said)
            }
            "avp_proof" => {
                let id = match args["id"].as_str() {
                    Some(id) => id.to_string(),
                    None => {
                        let t = self.task(&s)?;
                        let st: TaskStatus = self.client.get(&format!("/repos/{repo}/tasks/{t}"))?;
                        st.last_proof.context("this task has no proof yet: avp_land (or avp_check) runs one")?.id
                    }
                };
                self.proof_read(&repo, &id, args["step"].as_str(), args["lines"].as_u64().unwrap_or(60).clamp(5, 400) as usize)
            }
            "avp_refresh" => {
                self.task(&s)?;
                let moved = view::catch_up(&self.client, &self.dir(), &mut s)?;
                view::save(&self.dir(), &s)?;
                Ok(moved)
            }
            "avp_abandon" => {
                let t = self.task(&s)?;
                let r: Value = self.client.post(&format!("/repos/{repo}/tasks/{t}/abandon"), &json!({}))?;
                s.task = None;
                view::save(&self.dir(), &s)?;
                let mut out = format!("abandoned {t}");
                for above in r["stacked"].as_array().into_iter().flatten().filter_map(|t| t.as_str()) {
                    out.push_str(&format!("\n{above}, stacked on it, stays open: it lands its own changes without this task's (its agent hears of it)"));
                }
                Ok(out)
            }
            other => Err(anyhow::anyhow!("unknown tool {other}")),
        }
    }

    fn actor(&self) -> Actor {
        Actor { person: std::env::var("AVP_PERSON").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "someone".into()), agent: std::env::var("AVP_AGENT").ok().or_else(|| self.client_name.lock().unwrap_or_else(|e| e.into_inner()).as_deref().map(agent_of)).unwrap_or_else(|| "mcp".into()), session: self.session.clone() }
    }

    fn transcript(&self, args: &Value) -> Option<String> {
        args["transcript"].as_str().map(String::from).or_else(|| std::env::var("AVP_TRANSCRIPT").ok()).or_else(|| transcript_of(self.client_name.lock().unwrap_or_else(|e| e.into_inner()).as_deref(), &self.started_in))
    }

    /// A tool call: the outbox first (what was queued while the server did not answer goes to it in
    /// order), then the tool, on the server, or on this machine when the server does not answer.
    /// What sending the outbox said comes before the tool's answer.
    fn answer(&self, name: &str, args: &Value) -> Result<String> {
        // The folder this call acts on, and its lock: calls on other tasks run beside this one.
        let dir = self.folder_for(name, args)?;
        CALL_DIR.with(|d| *d.borrow_mut() = Some(dir.clone()));
        let busy = self.slot(&dir).busy;
        let _busy = busy.lock().unwrap_or_else(|e| e.into_inner());
        // The way out of a stuck outbox: a drop must not send first what it drops.
        if name == "avp_outbox" {
            let drop: Vec<u64> = args["drop"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
            return Ok(match (drop.is_empty(), args["retry"].as_bool() == Some(true)) {
                (false, _) => offline::drop_entries(&self.dir(), &drop)?.1,
                (true, true) => offline::retry(&self.client, &self.dir())?.1,
                (true, false) => offline::list(&self.dir())?.1,
            });
        }
        // Read on this machine, where `avp migrate` kept it: no task, no server needed.
        if name == "avp_migrate_report" {
            let key = args["repo"].as_str().map(String::from).or_else(|| view::load(&self.dir()).ok().map(|s| s.repo)).or_else(|| std::env::var("AVP_REPO").ok()).or_else(|| crate::up::repo_name(&self.dir()));
            return Ok(match crate::migrate::last_report(&self.dir(), key.as_deref()) {
                Some(r) => crate::migrate::text(&r),
                None => "no `avp migrate` report for this repo on this machine: a person runs `avp migrate` in a checkout of it (`--json` for an agent), then this tool returns what it found and what is left".into(),
            });
        }
        let link = if view::load(&self.dir()).is_ok() { offline::flush(&self.client, &self.dir(), true) } else { offline::Link::default() };
        let task = view::load(&self.dir()).ok().and_then(|s| s.task);
        let out = match offline::ask(link.down, task.as_deref(), || self.call(name, args)) {
            Err(e) if crate::client::unreachable(&e) => self.offline(name, args, link.stuck),
            r => r,
        };
        if link.said.is_empty() {
            return out;
        }
        let said = link.said.join("\n");
        out.map(|t| format!("{said}\n{t}")).map_err(|e| e.context(said))
    }

    /// A tool while the server does not answer: in one line, it says so, then does what it can on
    /// this machine, and the agent keeps working. Nothing here tells it to stop. `stuck`: it answers,
    /// but the outbox waits on an entry it refused.
    fn offline(&self, name: &str, args: &Value, stuck: bool) -> Result<String> {
        let name = name.strip_prefix("trame_").map(|n| format!("avp_{n}")).unwrap_or_else(|| name.to_string());
        let (dir, server) = (self.dir(), self.client.base.clone());
        if name == "avp_open" {
            return self.open_offline(args);
        }
        let keep = "Keep working: edits are queued, avp_check and avp_land prove on this machine, and everything goes to the server, in order, once it answers.";
        let mut s = match self.state() {
            Ok(s) => s,
            Err(e) => {
                let repo = args["repo"].as_str().map(String::from).or_else(|| std::env::var("AVP_REPO").ok()).or_else(|| crate::up::repo_name(&dir)).ok_or(e)?;
                view::ViewState { repo, ..Default::default() }
            }
        };
        let notice = offline::notice_of(&server, &s.repo);
        let list = |k: &str| args[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
        match name.as_str() {
            "avp_status" => {
                if s.task.is_some() {
                    offline::sync(&server, &dir, &mut s)?;
                }
                Ok(format!("{}\nfolder: {}\n{keep}", offline::status(&server, &dir, &s, stuck)?.1, dir.display()))
            }
            "avp_check" => {
                let (lines, n) = offline::sync(&server, &dir, &mut s)?;
                let p = offline::prove(&server, &dir, &s)?;
                Ok(format!("{notice}\nsync: {} ({n} waiting)\nproven on this machine (the server proves it again when the land reaches it): {}", if lines.is_empty() { "no new writes".into() } else { lines.join(", ") }, Self::proof_text(&p)))
            }
            "avp_land" => match offline::land(&server, &dir, &mut s)? {
                offline::Landing::Queued(p, n) => Ok(format!(
                    "{notice}\ngreen on this machine ({}). The land is queued ({n} waiting) and lands when the server answers: it proves it again there, replayed on what landed meanwhile. The task is closed in this folder; avp_open opens the next one, offline too.",
                    if p.steps.is_empty() { "avp.toml names no step".into() } else { p.steps.iter().map(|s| format!("✓ {}", s.name)).collect::<Vec<_>>().join(", ") }
                )),
                offline::Landing::Empty => Ok(format!("{notice}\nNothing to land: this task has no write. Write files first, or avp_abandon.")),
                offline::Landing::Red(p) => Ok(format!("{notice}\nnot landed: red on this machine, nothing queued. Fix it, then avp_land again.\n{}", Self::proof_text(&p))),
            },
            "avp_claim" => Ok(offline::queued_text(offline::queue(&dir, &s, offline::Op::Claim { paths: list("paths") })?)),
            "avp_release" => {
                let paths = Some(list("paths")).filter(|p| !p.is_empty());
                Ok(offline::queued_text(offline::queue(&dir, &s, offline::Op::Release { paths })?))
            }
            "avp_goal" => {
                let goal = args.get("goal").and_then(Value::as_str).unwrap_or_default().trim().to_string();
                let n = offline::queue(&dir, &s, offline::Op::Goal { goal: goal.clone() })?;
                Ok(format!("goal of {}: {goal}\n{}", self.task(&s)?, offline::queued_text(n)))
            }
            // A save point's number belongs to the server: never taken for `to`, which would undo all.
            "avp_undo" if !args["save"].is_null() => Ok(format!("{notice}\nGoing to a save point needs the server, which keeps them: ask again once it answers. {keep}")),
            "avp_undo" => offline::undo(&server, &dir, &mut s, args["to"].as_i64().unwrap_or(0), args["path"].as_str().map(String::from)),
            "avp_abandon" => offline::abandon(&dir, &mut s),
            "avp_log" => {
                let r = offline::replica(&server, &s.repo)?;
                let line = r.log(args["limit"].as_i64().unwrap_or(20));
                Ok(format!("{notice}\n{}", line.iter().map(|e| format!("{:>5}  {}  {}  {}", e.position, e.full.map(|v| format!("{v:?}")).unwrap_or_else(|| "pending".into()), e.task.as_deref().unwrap_or("-"), e.note)).collect::<Vec<_>>().join("\n")))
            }
            "avp_why" => {
                let r = offline::replica(&server, &s.repo)?;
                Ok(format!("{notice}\n{}", crate::history::why_text(&r.why(args["path"].as_str().context("path")?)?)))
            }
            "avp_context" => {
                let paths = list("paths");
                anyhow::ensure!(!paths.is_empty(), "paths: the files or directories to ask about (`path:line` for one line)");
                let r = offline::replica(&server, &s.repo)?;
                Ok(format!("{notice}\n{}", crate::history::context_text(&r.context(&paths, s.task.as_deref())?)))
            }
            "avp_tasks" => {
                let ts = offline::replica(&server, &s.repo)?.tasks();
                Ok(format!("{notice}\n{}", if ts.is_empty() { "no open task".into() } else { ts.iter().map(|t| format!("{}  pos {}  {} via {}  {}", t.id, t.base_position, t.actor.person, t.actor.agent, t.goal)).collect::<Vec<_>>().join("\n") }))
            }
            other => Ok(format!("{notice}\n{other} needs the server: ask again once it answers. {keep}")),
        }
    }

    /// avp_open while the server does not answer: a task of this machine's in this folder when it is
    /// a view free of an open task, else in a new folder laid out from the replica.
    fn open_offline(&self, args: &Value) -> Result<String> {
        let goal = args["goal"].as_str().context("goal")?.to_string();
        anyhow::ensure!(args["on"].as_str().is_none_or(|o| o.trim().is_empty()), "{}\nA stacked task opens on the server, which holds the writes of the task under it: avp_open with `on` once it answers, or without `on` now.", offline::notice_of(&self.client.base, args["repo"].as_str().unwrap_or("")));
        let scope: Vec<String> = args["scope"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let server = self.client.base.clone();
        let wanted = args["repo"].as_str().map(String::from).or_else(|| std::env::var("AVP_REPO").ok()).or_else(|| crate::up::repo_name(&self.dir()));
        let here = view::load(&self.dir()).ok();
        if let Some(t) = here.as_ref().and_then(|h| h.task.clone()) {
            anyhow::bail!("task {t} is still open in {}: land or abandon it first", self.dir().display());
        }
        let same = |h: &view::ViewState| wanted.as_ref().is_none_or(|w| offline::Replica::find(&server, w).is_some_and(|r| r.line().repo == h.repo || *w == h.repo));
        let (dir, mut s) = match here {
            Some(h) if same(&h) && hold(&self.dir()) => (self.dir(), h),
            _ => {
                let key = wanted.context("this folder is not an AVP view: pass `repo` (or set AVP_REPO)")?;
                let line = offline::replica(&server, &key)?.line();
                let name = slug(if line.name.is_empty() { &key } else { &line.name });
                let base = crate::up::views();
                let dir = (1..).map(|n| base.join(format!("{name}-{n}"))).find(|d| !d.exists() && hold(d)).unwrap();
                (dir, view::ViewState { server: server.clone(), repo: if line.repo.is_empty() { key } else { line.repo }, ..Default::default() })
            }
        };
        let said = offline::open(&server, &dir, &mut s, goal, scope, self.actor(), self.transcript(args))?;
        self.slot(&dir).taken.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.act_in(&dir);
        Ok(format!("{said}\nYour folder: {}\nWork there, with absolute paths, then avp_land: it proves the change on this machine and queues it; it lands when the server answers. No git commit, push or pull request on this repository.", dir.display()))
    }

    pub fn serve(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let started = std::time::Instant::now();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let done = AtomicBool::new(false);
        std::thread::scope(|sc| -> Result<()> {
            // Every few seconds, between tool calls, the outbox of each folder this session works in
            // goes to the server once it answers again: work done offline lands without the agent asking.
            sc.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    for _ in 0..5 {
                        std::thread::sleep(Duration::from_secs(1));
                        if done.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                    let mut dirs: Vec<PathBuf> = self.slots.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
                    let session = self.dir.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if !dirs.contains(&session) {
                        dirs.push(session);
                    }
                    for dir in dirs {
                        if offline::pending(&dir) == 0 {
                            continue;
                        }
                        if let Ok(_idle) = self.slot(&dir).busy.try_lock() {
                            offline::flush(&self.client, &dir, false);
                        }
                    }
                }
            });
            for line in stdin.lock().lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("avp mcp: bad json: {e}");
                        continue;
                    }
                };
                let id = msg.get("id").cloned();
                let method = msg["method"].as_str().unwrap_or("");
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                // From protocol 2026-07-28, a client names itself on every request instead of in
                // `initialize` (Kiro and Devin do).
                if let Some(name) = params["_meta"]["io.modelcontextprotocol/clientInfo"]["name"].as_str() {
                    self.client_name.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| name.to_string());
                }
                let result: Result<Value> = match method {
                    "initialize" => {
                        let v = params["protocolVersion"].as_str().unwrap_or("2025-06-18").to_string();
                        let c = &params["clientInfo"];
                        *self.client_name.lock().unwrap_or_else(|e| e.into_inner()) = c["name"].as_str().map(String::from);
                        let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
                        self.log("start", json!({ "client_version": c["version"], "protocol": v, "cwd": cwd, "view": view::load(&self.dir()).is_ok(), "repo": std::env::var("AVP_REPO").ok(), "avp": env!("CARGO_PKG_VERSION") }));
                        Ok(json!({ "protocolVersion": v, "capabilities": { "tools": {} }, "serverInfo": { "name": "avp", "version": env!("CARGO_PKG_VERSION") }, "instructions": INSTRUCTIONS }))
                    }
                    "ping" => Ok(json!({})),
                    "tools/list" => Ok(json!({ "tools": tools() })),
                    "tools/call" => {
                        // A thread per call: a land that proves for minutes holds neither the other
                        // subagents' calls nor this session's reads.
                        let calls = &calls;
                        sc.spawn(move || {
                            let name = params["name"].as_str().unwrap_or("");
                            let args = params.get("arguments").cloned().unwrap_or(json!({}));
                            let t0 = std::time::Instant::now();
                            PROGRESS.with(|p| *p.borrow_mut() = params["_meta"].get("progressToken").cloned());
                            let r = self.answer(name, &args);
                            calls.fetch_add(1, Ordering::Relaxed);
                            let mut a = serde_json::Map::new();
                            for (k, v) in args.as_object().into_iter().flatten() {
                                a.insert(k.clone(), v.as_str().map(|s| json!(clip(s, 200))).unwrap_or_else(|| v.clone()));
                            }
                            let task = view::load(&self.dir()).ok().and_then(|s| s.task);
                            let (ok, text) = match &r { Ok(t) => (true, t.clone()), Err(e) => (false, format!("{e:#}")) };
                            self.log("call", json!({ "tool": name, "args": a, "ms": t0.elapsed().as_millis() as u64, "ok": ok, "out": clip(&text, if ok { 300 } else { 600 }), "dir": self.dir().display().to_string(), "task": task }));
                            if let Some(id) = id {
                                let _ = reply(id, Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": !ok })));
                            }
                        });
                        continue;
                    }
                    m if m.starts_with("notifications/") => continue,
                    _ => Err(anyhow::anyhow!("method not found")),
                };
                if let Some(id) = id {
                    reply(id, result)?;
                }
            }
            done.store(true, Ordering::Relaxed);
            Ok(())
        })?;
        self.log("end", json!({ "calls": calls.load(Ordering::Relaxed), "seconds": started.elapsed().as_secs() }));
        Ok(())
    }
}

/// One answer, written whole under stdout's lock: calls end on their own threads, and a progress
/// notification may come from another.
fn reply(id: Value, result: Result<Value>) -> Result<()> {
    let reply = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("{e:#}") } }),
    };
    let mut out = std::io::stdout().lock();
    out.write_all(format!("{reply}\n").as_bytes())?;
    out.flush()?;
    Ok(())
}
