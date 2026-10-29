//! Work without the server. Git is distributed: a laptop on a plane commits, logs, diffs, and pushes
//! later. AVP's server holds the truth, so offline is a mode of the client, for a person and an
//! agent alike:
//!
//! - The replica, outside the repo (`$AVP_HOME/<server>/<repo>/`): the objects of every tree a view
//!   laid out, and the line's last `KEEP` positions (goal, person, agent, proof, and what each one
//!   changed), refreshed whenever the server answers. Offline, `log`, `show`, `why`, `blame` and
//!   `context` answer from it, and say once that they are offline and as of which position.
//! - The outbox, in the view (`.avp/outbox.json`): what changed something while the server did not
//!   answer (writes, an open, a land...), in order. A task opened offline has an id of this
//!   machine's (`local_...`), mapped to the server's when it opens there.
//! - Local proofs: avp.toml's steps run on this machine as the server runs them. A red land is not
//!   queued, it is the agent's to fix; a green one is, and the server proves it again on return.
//! - The flush: the next command that reaches the server sends the outbox in order. A task opened
//!   offline opens there on the position the folder held, its writes as they were made, and its land
//!   replays it on what landed since. A server from before that opens it on the head: a write made on
//!   a file the line changed meanwhile is then merged three ways here, a conflict coming back with
//!   its markers instead of landing. An entry the server refuses stops those after it: `avp outbox`
//!   lists them, `retry` sends them again, `drop` takes one out.

use crate::blame::{Blame, BlameLine, Change, FileLog, PathContext};
use crate::client::{unreachable, Client, Unreachable};
use crate::history::Context;
use crate::model::*;
use crate::objects::{hash_bytes, Objects};
use crate::view::{self, ViewState};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

/// The positions of the line a replica keeps, newest first. Each costs one request once, when the
/// replica first sees it: what it changed never changes.
pub const KEEP: i64 = 100;

/// A task opened on this machine while the server did not answer.
pub fn is_local(task: &str) -> bool {
    task.starts_with("local_")
}

/// The error a command meets when the outbox already found the server down: it goes offline at
/// once, without waiting for a connection that will not come.
pub fn down() -> anyhow::Error {
    Unreachable("the server does not answer".into()).into()
}

/// `ask`, unless the server is known to be down or the task is one it does not know yet (opened
/// offline): then the error a server that does not answer gives.
pub fn ask<T>(down: bool, task: Option<&str>, ask: impl FnOnce() -> Result<T>) -> Result<T> {
    if down || task.is_some_and(is_local) {
        return Err(self::down());
    }
    ask()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// Written whole or not at all: a file cut by a crash would lose what it holds.
fn write_json(path: &Path, v: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// A server's folder under $AVP_HOME: its host, port and path, never its password.
fn server_key(base: &str) -> String {
    let rest = base.split_once("://").map_or(base, |(_, r)| r);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{host}/{path}").trim_end_matches('/').chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect()
}

fn folder_name(repo: &str) -> String {
    repo.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect()
}

/// What the replica remembers of the line.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Line {
    pub repo: String,
    pub name: String,
    /// The head when the server last answered, and when that was (ms).
    pub head: i64,
    pub refreshed_at: i64,
    /// Newest first.
    pub positions: Vec<Pos>,
    /// The repo's open tasks then.
    pub open: Vec<Task>,
}

/// A position as `log`, `show` and `why` read it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pos {
    pub entry: LineEntry,
    pub person: Option<String>,
    pub agent: Option<String>,
    pub opened_at: Option<i64>,
    pub transcript: Option<String>,
    /// The proof's verdict and steps, their output left on the server.
    pub verdict: Option<Verdict>,
    pub steps: Vec<StepHead>,
    pub base_position: Option<i64>,
}

#[derive(Clone, Deserialize)]
struct Who {
    person: String,
    agent: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepHead {
    pub name: String,
    pub exit: i32,
    pub seconds: f64,
}

/// A row of `GET /positions`.
#[derive(Deserialize)]
struct Row {
    position: i64,
    #[serde(default)]
    actor: Option<Who>,
    #[serde(default)]
    opened_at: Option<i64>,
    #[serde(default)]
    transcript: Option<String>,
    #[serde(default)]
    verdict: Option<Verdict>,
    #[serde(default)]
    base_position: Option<i64>,
    #[serde(default)]
    steps: Vec<StepHead>,
}

/// This machine's replica of a repo's line on a server.
pub struct Replica {
    pub root: PathBuf,
    pub objects: Objects,
}

impl Replica {
    pub fn open(server: &str, repo: &str) -> Result<Self> {
        Self::at(crate::up::home().join(server_key(server)).join(folder_name(repo)))
    }

    fn at(root: PathBuf) -> Result<Self> {
        Ok(Self { objects: Objects::open(&root.join("objects"))?, root })
    }

    /// The replica of `key`, a repo's id or its name, when this machine keeps one.
    pub fn find(server: &str, key: &str) -> Option<Self> {
        let base = crate::up::home().join(server_key(server));
        let direct = base.join(folder_name(key));
        if direct.is_dir() {
            return Self::at(direct).ok();
        }
        let named = |p: &PathBuf| read_json::<Line>(&p.join("line.json")).is_some_and(|l| l.name == key);
        Self::at(fs::read_dir(&base).ok()?.flatten().map(|e| e.path()).find(named)?).ok()
    }

    pub fn line(&self) -> Line {
        read_json(&self.root.join("line.json")).unwrap_or_default()
    }

    /// What `position` changed, when this machine keeps it.
    fn diff(&self, position: i64) -> Option<Vec<FileDiff>> {
        read_json(&self.root.join("diffs").join(format!("{position}.json")))
    }

    /// The positions this machine keeps with what each changed, newest first.
    fn diffs<'a>(&self, line: &'a Line) -> Vec<(&'a Pos, Vec<FileDiff>)> {
        line.positions.iter().filter_map(|p| Some((p, self.diff(p.entry.position)?))).collect()
    }

    /// The newest position whose tree and files this machine keeps whole: where a task opened
    /// offline in a folder that holds nothing starts.
    pub fn newest_whole(&self) -> Option<(i64, String, Tree)> {
        self.line().positions.iter().find_map(|p| {
            let tree = self.objects.get_tree(&p.entry.tree).ok()?;
            tree.values().all(|e| self.objects.has(&e.blob)).then(|| (p.entry.position, p.entry.tree.clone(), tree))
        })
    }

    /// A blob from the replica, else from the server, kept here once fetched.
    fn object(&self, client: &Client, repo: &str, hash: &str) -> Result<Vec<u8>> {
        if let Ok(b) = self.objects.get(hash) {
            return Ok(b);
        }
        let b = client.get_object(repo, hash)?;
        self.objects.put(&b)?;
        Ok(b)
    }
}

/// The replica of `key` for an offline answer, or why there is none.
pub fn replica(server: &str, key: &str) -> Result<Replica> {
    Replica::find(server, key).with_context(|| format!("offline: the server does not answer, and this machine keeps no copy of {key}'s line yet (a view makes one when it lays the repo out, online)"))
}

/// The one line an offline answer starts with.
pub fn notice(line: &Line) -> String {
    if line.refreshed_at == 0 {
        return "offline: the server does not answer (this machine has not seen the line yet)".into();
    }
    format!("offline: the server does not answer; as of position {}, {} UTC", line.head, crate::blame::utc(line.refreshed_at))
}

/// `notice` for the replica of `key`, kept or not.
pub fn notice_of(server: &str, key: &str) -> String {
    notice(&Replica::find(server, key).map(|r| r.line()).unwrap_or_default())
}

/// Brings the replica of `key` up to the server, which just answered: the head and the open tasks,
/// and when the line moved, its last `KEEP` positions with what each changed. At most every 10
/// seconds unless `force`d; it never fails the command that asks for it.
pub fn refresh(client: &Client, key: &str, force: bool) {
    let _ = try_refresh(client, key, force);
}

fn try_refresh(client: &Client, key: &str, force: bool) -> Result<()> {
    if key.is_empty() {
        return Ok(());
    }
    let r = match Replica::find(&client.base, key) {
        Some(r) => r,
        None => Replica::open(&client.base, &client.get::<Repo>(&format!("/repos/{key}"))?.id)?,
    };
    let mut line = r.line();
    if !force && now_ms() - line.refreshed_at < 10_000 {
        return Ok(());
    }
    if line.repo.is_empty() {
        let repo: Repo = client.get(&format!("/repos/{key}"))?;
        (line.repo, line.name) = (repo.id, repo.name);
    }
    let repo = line.repo.clone();
    let head: LineEntry = client.get(&format!("/repos/{repo}/head"))?;
    line.open = client.get(&format!("/repos/{repo}/tasks?state=open"))?;
    if head.position != line.head || line.positions.is_empty() {
        let entries: Vec<LineEntry> = client.get(&format!("/repos/{repo}/line?limit={KEEP}"))?;
        let rows: Vec<Row> = client.get(&format!("/repos/{repo}/positions?limit={KEEP}")).unwrap_or_default();
        line.positions = entries
            .into_iter()
            .map(|entry| {
                let row = rows.iter().find(|r| r.position == entry.position);
                let actor = row.and_then(|r| r.actor.clone());
                Pos {
                    person: actor.as_ref().map(|a| a.person.clone()),
                    agent: actor.map(|a| a.agent),
                    opened_at: row.and_then(|r| r.opened_at),
                    transcript: row.and_then(|r| r.transcript.clone()),
                    verdict: row.and_then(|r| r.verdict),
                    steps: row.map(|r| r.steps.clone()).unwrap_or_default(),
                    base_position: row.and_then(|r| r.base_position),
                    entry,
                }
            })
            .collect();
        let dir = r.root.join("diffs");
        let missing: Vec<i64> = line.positions.iter().map(|p| p.entry.position).filter(|p| !dir.join(format!("{p}.json")).is_file()).collect();
        fetch_diffs(client, &repo, &dir, &missing)?;
        let oldest = line.positions.last().map_or(0, |p| p.entry.position);
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().trim_end_matches(".json").parse::<i64>().is_ok_and(|p| p < oldest) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    line.head = head.position;
    line.refreshed_at = now_ms();
    write_json(&r.root.join("line.json"), &line)
}

/// What each of `positions` changed, four requests in flight: a first replica is a few round trips.
fn fetch_diffs(client: &Client, repo: &str, dir: &Path, positions: &[i64]) -> Result<()> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    while let Some(&p) = positions.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        // The line's start changed nothing: everything it holds was added there.
                        let diff: Vec<FileDiff> = if p == 0 {
                            client.get::<Tree>(&format!("/repos/{repo}/tree/0"))?.into_keys().map(|path| FileDiff { path, change: "added".into(), patch: String::new(), binary: false }).collect()
                        } else {
                            client.get(&format!("/repos/{repo}/line/{p}/diff"))?
                        };
                        write_json(&dir.join(format!("{p}.json")), &diff)?;
                    }
                    Ok(())
                })
            })
            .collect();
        for w in workers {
            w.join().map_err(|_| anyhow::anyhow!("a download stopped"))??;
        }
        Ok(())
    })
}

/// Keeps `tree` and its files in the replica: the folder `dir` just got them laid out, so they are
/// read there, once each. Never fails the layout.
pub fn keep_laid(server: &str, repo: &str, tree: &Tree, dir: &Path) {
    let Ok(r) = Replica::open(server, repo) else { return };
    let _ = r.objects.put_tree(tree);
    for (path, e) in tree {
        if r.objects.has(&e.blob) {
            continue;
        }
        if let Ok(bytes) = view::read_project(&dir.join(path)) {
            if hash_bytes(&bytes) == e.blob {
                let _ = r.objects.put(&bytes);
            }
        }
    }
}

// ---- answers from the replica ----

fn change(p: &Pos, kind: &str) -> Change {
    Change {
        position: p.entry.position,
        landed_at: p.entry.landed_at,
        goal: p.entry.note.clone(),
        task: p.entry.task.clone(),
        person: p.person.clone().filter(|x| !x.is_empty()),
        agent: p.agent.clone().filter(|x| !x.is_empty()),
        change: kind.into(),
    }
}

fn task_of(p: &Pos) -> Option<Task> {
    Some(Task {
        id: p.entry.task.clone()?,
        repo: String::new(),
        goal: p.entry.note.clone(),
        actor: Actor { person: p.person.clone().unwrap_or_default(), agent: p.agent.clone().unwrap_or_default(), session: String::new() },
        scope: vec![],
        base_position: p.base_position.unwrap_or(p.entry.position - 1),
        base_tree: String::new(),
        state: TaskState::Landed,
        created_at: p.opened_at.unwrap_or(p.entry.landed_at),
        landed_position: Some(p.entry.position),
        transcript: p.transcript.clone(),
        similar: vec![],
        on: None,
    })
}

fn proof_of(p: &Pos) -> Option<Proof> {
    Some(Proof {
        id: p.entry.proof.clone()?,
        task: p.entry.task.clone(),
        base_position: p.base_position.unwrap_or(p.entry.position - 1),
        candidate_tree: p.entry.tree.clone(),
        files: vec![],
        steps: p.steps.iter().map(|s| Step { name: s.name.clone(), command: String::new(), exit: s.exit, seconds: s.seconds, output: String::new() }).collect(),
        verdict: p.verdict.unwrap_or(Verdict::Green),
        blamed: vec![],
        created_at: p.entry.landed_at,
    })
}

/// `text` through a patch as the server stores it (`FileDiff::patch`, its header gone), or back
/// through it to the text before. None when it does not apply.
fn apply(text: &str, patch: &str, back: bool) -> Option<String> {
    let mut full = String::from("--- a\n+++ b\n");
    for l in patch.lines() {
        let l = match (back, l.strip_prefix("@@ -")) {
            (false, _) => l.to_string(),
            (true, Some(rest)) => {
                let (old, rest) = rest.split_once(" +")?;
                let (new, tail) = rest.split_once(" @@")?;
                format!("@@ -{new} +{old} @@{tail}")
            }
            (true, None) => match l.chars().next() {
                Some('+') => format!("-{}", &l[1..]),
                Some('-') => format!("+{}", &l[1..]),
                _ => l.to_string(),
            },
        };
        full.push_str(&l);
        full.push('\n');
    }
    diffy::apply(text, &diffy::Patch::from_str(&full).ok()?).ok()
}

impl Replica {
    /// The line, newest first, `limit` positions at most.
    pub fn log(&self, limit: i64) -> Vec<LineEntry> {
        self.line().positions.into_iter().take(limit.max(0) as usize).map(|p| p.entry).collect()
    }

    /// The positions this machine keeps that changed `path`, from `from` back, newest first.
    pub fn file_log(&self, path: &str, from: Option<i64>, limit: usize) -> FileLog {
        let line = self.line();
        let mut changes: Vec<Change> = self.diffs(&line).iter().filter(|(p, _)| from.is_none_or(|f| p.entry.position <= f)).filter_map(|(p, ds)| ds.iter().find(|d| d.path == path).map(|d| change(p, &d.change))).collect();
        let more = if changes.len() > limit.max(1) {
            let at = changes[limit.max(1)].position;
            changes.truncate(limit.max(1));
            Some(at)
        } else {
            // Older positions than these are on the server only.
            line.positions.last().map(|p| p.entry.position).filter(|p| *p > 0)
        };
        FileLog { path: path.to_string(), changes, more }
    }

    /// `avp show` from the replica: who, why, with which proof, then the diff file by file.
    pub fn show(&self, position: i64) -> Result<String> {
        anyhow::ensure!(position > 0, "position 0 is where the line starts: it changed nothing");
        let line = self.line();
        let kept = |l: &Line| format!("{}..{}", l.positions.last().map_or(0, |p| p.entry.position), l.head);
        let p = line.positions.iter().find(|p| p.entry.position == position).with_context(|| format!("position {position} is not among those this machine keeps ({})", kept(&line)))?;
        let diffs = self.diff(position).with_context(|| format!("what position {position} changed is not on this machine"))?;
        let e = &p.entry;
        let mut out = format!("position {position}  {}\n", e.note.trim());
        if let Some(t) = &e.task {
            out.push_str(&format!("  task {t}, by {} via {}\n", p.person.as_deref().unwrap_or("-"), p.agent.as_deref().unwrap_or("-")));
        }
        if let Some(proof) = proof_of(p) {
            out.push_str(&format!("  proof {}: {:?}, {}\n", proof.id, proof.verdict, proof.steps.iter().map(|s| format!("{} {}", if s.exit == 0 { "✓" } else { "✗" }, s.name)).collect::<Vec<_>>().join(", ")));
        }
        out.push_str(&format!("  full suite: {}", e.full.map(|v| format!("{v:?}")).unwrap_or_else(|| "not run yet".into())));
        if let Some(g) = &e.git {
            out.push_str(&format!(", git commit {}", &g[..g.len().min(12)]));
        }
        out.push('\n');
        if let Some(rv) = &e.reverted {
            out.push_str(&format!("  reverted by AVP at position {} (task {}): the full suite's shard {} went red twice on it: {}\n", rv.by, rv.task, rv.shard, rv.tests.join(" | ")));
        }
        for d in &diffs {
            out.push_str(&format!("\n--- a/{}\n+++ b/{}\n", d.path, d.path));
            out.push_str(match (d.binary, d.patch.is_empty()) {
                (true, _) => "binary file changed\n",
                (false, true) if d.change == "modified" => "too large to show\n",
                _ => "",
            });
            if !d.patch.is_empty() {
                out.push_str(&format!("{}\n", d.patch));
            }
        }
        Ok(out)
    }

    /// Each line of `path` with the position that last changed it: the file as the newest position
    /// this machine has the files of holds it, brought to the head through what each later position
    /// changed, then followed back through every change the replica keeps.
    pub fn blame(&self, path: &str) -> Result<Blame> {
        let line = self.line();
        let (start, mut text) = line
            .positions
            .iter()
            .find_map(|p| {
                let tree = self.objects.get_tree(&p.entry.tree).ok()?;
                let bytes = match tree.get(path) {
                    Some(e) => Some(String::from_utf8(self.objects.get(&e.blob).ok()?).ok()?),
                    None => None,
                };
                Some((p.entry.position, bytes))
            })
            .with_context(|| format!("{path}: no position this machine keeps the files of"))?;
        let changed = |p: &Pos| self.diff(p.entry.position).and_then(|ds| ds.into_iter().find(|d| d.path == path));
        let mut position = start;
        for p in line.positions.iter().rev().filter(|p| p.entry.position > start) {
            if let Some(d) = changed(p) {
                text = match d.change.as_str() {
                    "deleted" => None,
                    // A large or binary file has no patch to follow: the blame stays where it is.
                    _ if d.binary || (d.patch.is_empty() && d.change != "added") => break,
                    _ => match apply(text.as_deref().unwrap_or(""), &d.patch, false) {
                        Some(t) => Some(t),
                        None => break,
                    },
                };
            }
            position = p.entry.position;
        }
        let shown = text.with_context(|| format!("no {path} at position {position}"))?;
        let mut current = shown.clone();
        let mut open: Vec<(usize, usize)> = (0..shown.lines().count()).map(|l| (l, l)).collect();
        let mut owner: Vec<Option<i64>> = vec![None; open.len()];
        let mut found: BTreeMap<i64, &'static str> = BTreeMap::new();
        let mut reached = position;
        for p in line.positions.iter().filter(|p| p.entry.position <= position) {
            reached = p.entry.position;
            if open.is_empty() {
                break;
            }
            let Some(d) = changed(p) else { continue };
            if d.change == "added" {
                for (l, _) in open.drain(..) {
                    owner[l] = Some(reached);
                }
                found.insert(reached, "added");
                break;
            }
            // A large or binary file has no patch to follow back: its older lines are marked as such.
            let Some(old) = (!d.binary && !d.patch.is_empty()).then(|| apply(&current, &d.patch, true)).flatten() else { break };
            let map = crate::blame::origins(&old, &current);
            open.retain_mut(|(l, at)| match map.get(*at) {
                Some(Some(o)) => {
                    *at = *o;
                    true
                }
                _ => {
                    owner[*l] = Some(reached);
                    found.insert(reached, "modified");
                    false
                }
            });
            current = old;
        }
        let changes: Vec<Change> = found.into_iter().rev().filter_map(|(at, kind)| line.positions.iter().find(|p| p.entry.position == at).map(|p| change(p, kind))).collect();
        let person = |at: i64| changes.iter().find(|c| c.position == at).and_then(|c| c.person.clone());
        let lines = shown.lines().zip(owner).enumerate().map(|(i, (t, o))| BlameLine { line: i + 1, text: t.to_string(), position: o.unwrap_or(reached), person: o.and_then(person), older: o.is_none() }).collect();
        Ok(Blame { path: path.to_string(), position, lines, changes })
    }

    /// `avp why` from the replica: the position that last changed the file, or the line.
    pub fn why(&self, target: &str) -> Result<Why> {
        let (path, n) = crate::history::split_line(target);
        let line = self.line();
        let pos = |at: i64| line.positions.iter().find(|p| p.entry.position == at);
        if let Some(n) = n {
            anyhow::ensure!(n > 0, "lines count from 1");
            let b = self.blame(path)?;
            let l = b.lines.get(n - 1).with_context(|| format!("{path} has {} lines", b.lines.len()))?;
            let p = pos(l.position);
            return Ok(Why { path: path.into(), position: l.position, task: p.and_then(task_of), proof: p.and_then(proof_of), line: Some(n), text: Some(l.text.clone()) });
        }
        let (p, _) = self.diffs(&line).into_iter().find(|(_, ds)| ds.iter().any(|d| d.path == path)).with_context(|| format!("no position of the {} this machine keeps changed {path}", line.positions.len()))?;
        Ok(Why { path: path.into(), position: p.entry.position, task: task_of(p), proof: proof_of(p), line: None, text: None })
    }

    /// `avp context` from the replica: as the server answers it, the holders read from the open
    /// tasks' scopes as they were when the server last answered.
    pub fn context(&self, targets: &[String], task: Option<&str>) -> Result<Context> {
        let line = self.line();
        let mut found: BTreeMap<String, Vec<Change>> = BTreeMap::new();
        for (p, ds) in self.diffs(&line) {
            for d in ds {
                found.entry(d.path.clone()).or_default().push(change(p, &d.change));
            }
        }
        let mut asked: Vec<&str> = Vec::new();
        for t in targets.iter().map(|t| crate::history::split_line(t).0.trim_matches('/')).filter(|p| !p.is_empty() && *p != ".") {
            if !asked.contains(&t) {
                asked.push(t);
            }
        }
        let offers: Vec<Vec<&String>> = asked
            .iter()
            .map(|a| match found.get_key_value(*a) {
                Some((p, _)) => vec![p],
                None => {
                    let mut under: Vec<(&String, &Vec<Change>)> = found.iter().filter(|(p, c)| p.starts_with(&format!("{a}/")) && c[0].change != "deleted").collect();
                    under.sort_by_key(|(_, c)| std::cmp::Reverse((c.len(), c[0].position)));
                    under.into_iter().map(|(p, _)| p).collect()
                }
            })
            .collect();
        let mut picked: Vec<&String> = Vec::new();
        for round in 0..crate::blame::CONTEXT_PATHS {
            for p in offers.iter().filter_map(|o| o.get(round)) {
                if picked.len() < crate::blame::CONTEXT_PATHS && !picked.contains(p) {
                    picked.push(p);
                }
            }
        }
        let held = |p: &str| {
            line.open.iter().filter(|t| Some(t.id.as_str()) != task).find(|t| t.scope.iter().any(|s| s.trim_end_matches('/') == p || p.starts_with(&format!("{}/", s.trim_end_matches('/'))))).map(|t| crate::blame::Holder {
                task: t.id.clone(),
                goal: t.goal.clone(),
                agent: if t.actor.agent.is_empty() { t.actor.person.clone() } else { t.actor.agent.clone() },
            })
        };
        let paths = picked.into_iter().map(|p| PathContext { path: p.clone(), changes: found[p].iter().take(crate::blame::CONTEXT_CHANGES).cloned().collect(), held_by: held(p) }).collect();
        let why = targets.iter().filter(|t| crate::history::split_line(t).1.is_some()).map(|t| self.why(t)).collect::<Result<_>>()?;
        Ok(Context { paths, why })
    }

    /// The open tasks as they were when the server last answered.
    pub fn tasks(&self) -> Vec<Task> {
        self.line().open
    }
}

// ---- the outbox ----

/// What a view did while the server did not answer, in order (`.avp/outbox.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Outbox {
    pub queued: Vec<Queued>,
    /// The number the next entry takes.
    #[serde(default)]
    next: u64,
    /// Tasks opened offline (`local_...`), and the server's ids for them once opened there.
    #[serde(default)]
    ids: BTreeMap<String, String>,
    /// Those the server opened on the position the folder held (the server's ids): their writes go
    /// as they were made.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    exact: BTreeSet<String>,
    /// A queued land the server did not land (a conflict, a red proof): what was queued after it
    /// waits until that task lands or is abandoned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    held: Option<String>,
    /// The task the view held when one was handed back: its own again once the hold is over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Flush>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Queued {
    pub n: u64,
    pub at: i64,
    /// The task it acts on: the server's id, or one of this machine's.
    pub task: String,
    /// Its last send failed, the server answering (it refused it): what was queued after it waits
    /// until a send goes through, or `avp outbox drop` drops it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<Failure>,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Failure {
    pub at: i64,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// A task opened offline, on position `base`.
    Open { goal: String, scope: Vec<String>, actor: Actor, transcript: Option<String>, base: i64 },
    /// A file written or deleted. `was` is what the view held before: what the write is replayed
    /// from when the server's task holds something else by then.
    Write { path: String, kind: WriteKind, blob: Option<String>, mode: u32, was: Option<String> },
    Undo { to: i64, path: Option<String> },
    Goal { goal: String },
    Claim { paths: Vec<String> },
    Release { paths: Option<Vec<String>> },
    /// Proven green on this machine first (`proof` is that local proof).
    Land { proof: String },
    Abandon,
}

/// The last time the outbox was sent.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Flush {
    pub at: i64,
    /// Entries sent, and those still queued after it.
    pub sent: usize,
    pub left: usize,
    pub said: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Said already: a flush the MCP server made between two calls is told by the next one.
    #[serde(default)]
    pub told: bool,
}

fn outbox_file(dir: &Path) -> PathBuf {
    dir.join(".avp").join("outbox.json")
}

/// The view's outbox. One that cannot be read is an error, never an empty outbox: what it holds
/// must not be lost by the next write over it.
pub fn outbox(dir: &Path) -> Result<Outbox> {
    match fs::read(outbox_file(dir)) {
        Ok(b) => serde_json::from_slice(&b).context(".avp/outbox.json"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Outbox::default()),
        Err(e) => Err(e).context(".avp/outbox.json"),
    }
}

/// How many entries wait in the view's outbox.
pub fn pending(dir: &Path) -> usize {
    outbox(dir).map_or(0, |o| o.queued.len())
}

/// One process at a time reads and writes the outbox: the CLI, `avp watch`, the MCP server.
fn lock(dir: &Path) -> Result<fs::File> {
    fs::create_dir_all(dir.join(".avp"))?;
    let f = fs::File::create(dir.join(".avp").join("outbox.lock"))?;
    f.lock()?;
    Ok(f)
}

fn add(o: &mut Outbox, task: &str, ops: Vec<Op>) {
    for op in ops {
        o.next += 1;
        o.queued.push(Queued { n: o.next, at: now_ms(), task: task.to_string(), failed: None, op });
    }
}

fn push(dir: &Path, task: &str, ops: Vec<Op>) -> Result<usize> {
    let _one = lock(dir)?;
    let mut o = outbox(dir)?;
    add(&mut o, task, ops);
    write_json(&outbox_file(dir), &o)?;
    Ok(o.queued.len())
}

/// What a command that changes something answers offline.
pub fn queued_text(n: usize) -> String {
    format!("offline: queued ({n} waiting), lands when the server answers")
}

/// What the folder changed since the view last sent or queued it, as writes: each file kept in
/// the replica, the view's `known` moved on. The caller queues them, then saves the view.
fn scan_writes(server: &str, dir: &Path, state: &mut ViewState) -> Result<Vec<Op>> {
    let r = Replica::open(server, &state.repo)?;
    let now = view::scan(dir, &state.known, &mut state.stamps)?;
    let mut ops = Vec::new();
    for (path, (hash, mode)) in &now {
        let was = state.known.get(path).cloned();
        if was.as_ref() == Some(&(hash.clone(), *mode)) {
            continue;
        }
        let blob = r.objects.put(&view::read_project(&dir.join(path))?)?;
        ops.push(Op::Write { path: path.clone(), kind: WriteKind::Put, blob: Some(blob.clone()), mode: *mode, was: was.map(|w| w.0) });
        state.known.insert(path.clone(), (blob, *mode));
    }
    let gone: Vec<String> = state.known.keys().filter(|p| !now.contains_key(*p) && !view::is_host_path(p)).cloned().collect();
    for path in gone {
        let was = state.known.remove(&path).map(|w| w.0);
        ops.push(Op::Write { path, kind: WriteKind::Delete, blob: None, mode: 0o100644, was });
    }
    Ok(ops)
}

fn write_lines(ops: &[Op]) -> Vec<String> {
    ops.iter()
        .filter_map(|op| match op {
            Op::Write { path, kind: WriteKind::Delete, .. } => Some(format!("- {path}")),
            Op::Write { path, .. } => Some(format!("+ {path}")),
            _ => None,
        })
        .collect()
}

/// Queues `ops` (with what the folder changed before them), then saves the view: in that order, a
/// crash between the two sends a write twice, never loses one.
fn queue_after_scan(server: &str, dir: &Path, state: &mut ViewState, task: &str, then: Vec<Op>) -> Result<(Vec<String>, usize)> {
    let _one = lock(dir)?;
    let mut ops = scan_writes(server, dir, state)?;
    let lines = write_lines(&ops);
    ops.extend(then);
    let mut o = outbox(dir)?;
    add(&mut o, task, ops);
    write_json(&outbox_file(dir), &o)?;
    view::save(dir, state)?;
    Ok((lines, o.queued.len()))
}

/// `avp sync` offline: the folder's changes queued as writes (`+ path`, `- path`), and how many
/// entries wait.
pub fn sync(server: &str, dir: &Path, state: &mut ViewState) -> Result<(Vec<String>, usize)> {
    let task = state.task.clone().context("no open task in this view: avp open first")?;
    queue_after_scan(server, dir, state, &task, vec![])
}

/// `avp add` offline: these paths queued as writes whatever the ignore rules say.
pub fn add_paths(server: &str, dir: &Path, state: &mut ViewState, paths: &[String]) -> Result<(Vec<String>, usize)> {
    let task = state.task.clone().context("no task open here")?;
    let r = Replica::open(server, &state.repo)?;
    let mut ops = Vec::new();
    for p in paths {
        let full = dir.join(p);
        let meta = fs::metadata(&full).with_context(|| format!("{p} is not a file here"))?;
        let (blob, mode) = (r.objects.put(&view::read_project(&full)?)?, view::mode_of(&meta));
        let was = state.known.get(p).map(|w| w.0.clone());
        ops.push(Op::Write { path: p.clone(), kind: WriteKind::Put, blob: Some(blob.clone()), mode, was });
        state.known.insert(p.clone(), (blob, mode));
    }
    let lines = write_lines(&ops);
    let n = push(dir, &task, ops)?;
    view::save(dir, state)?;
    Ok((lines, n))
}

/// `avp open` offline: a task of this machine's (`local_...`) on the position the folder holds or,
/// in a folder that holds nothing yet, on the newest position the replica keeps whole. It opens on
/// the server, under the server's id, when the outbox reaches it.
pub fn open(server: &str, dir: &Path, state: &mut ViewState, goal: String, scope: Vec<String>, actor: Actor, transcript: Option<String>) -> Result<String> {
    let r = replica(server, &state.repo)?;
    let line = r.line();
    if !line.repo.is_empty() {
        state.repo = line.repo.clone();
    }
    if let Some(t) = &state.task {
        anyhow::bail!("task {t} is still open here: land or abandon it first");
    }
    let mut laid = 0;
    if state.known.is_empty() {
        let (position, hash, tree) = r.newest_whole().with_context(|| format!("offline, and this machine keeps no position of {} whole to lay out: open in a view of it", state.repo))?;
        laid = view::materialize_offline(&r.objects, &tree, dir, state)?;
        state.base = Some((position, hash));
    }
    if state.server.is_empty() {
        state.server = server.to_string();
    }
    let base = state.base.as_ref().map_or(line.head, |b| b.0);
    let tree: Tree = state.known.iter().map(|(p, (b, m))| (p.clone(), Entry { blob: b.clone(), mode: *m })).collect();
    let from = r.objects.put_tree(&tree)?;
    let id = new_id("local");
    let n = push(dir, &id, vec![Op::Open { goal, scope, actor, transcript, base }])?;
    state.task = Some(id.clone());
    state.base = Some((base, from));
    view::save(dir, state)?;
    Ok(format!(
        "{}\ntask {id} opened on this machine, on position {base} ({laid} files laid out): it opens on the server when it answers ({n} waiting). Write, then `avp land`: it proves here, then queues the landing.",
        notice(&line)
    ))
}

fn known_tree(state: &ViewState) -> Tree {
    state.known.iter().map(|(p, (b, m))| (p.clone(), Entry { blob: b.clone(), mode: *m })).collect()
}

/// The proof avp.toml's steps give the view's candidate on this machine, run as the server runs
/// them: laid out in a folder of the replica's (`work/`), with its setup and the folders it keeps.
/// It is advice: the server proves the task again before landing it. A step that needs a secret
/// the server keeps is red here.
pub fn prove(server: &str, dir: &Path, state: &ViewState) -> Result<Proof> {
    let task = state.task.clone().context("no task open here")?;
    let r = Replica::open(server, &state.repo)?;
    let tree = known_tree(state);
    for (path, e) in &tree {
        if !r.objects.has(&e.blob) {
            let bytes = view::read_project(&dir.join(path))?;
            anyhow::ensure!(hash_bytes(&bytes) == e.blob, "{path} changed while it was sent: prove again");
            r.objects.put(&bytes)?;
        }
    }
    let hash = r.objects.put_tree(&tree)?;
    let base = state.base.as_ref().and_then(|(_, h)| r.objects.get_tree(h).ok()).unwrap_or_default();
    let mut files: Vec<String> = tree.iter().filter(|(p, e)| base.get(*p) != Some(*e)).map(|(p, _)| p.clone()).collect();
    files.extend(base.keys().filter(|p| !tree.contains_key(*p)).cloned());
    // One proof at a time on this machine: they share the replica's work folder.
    let one = fs::File::create(r.root.join("work.lock"))?;
    one.lock()?;
    let (steps, _) = crate::check::Workspace::new(&r.root, "local").check(&r.objects, &tree, &hash, &task, &files, None, None)?;
    drop(one);
    let red = steps.iter().find(|s| s.exit != 0);
    let proof = Proof {
        id: new_id("local"),
        task: Some(task),
        base_position: state.base.as_ref().map_or(-1, |b| b.0),
        candidate_tree: hash,
        blamed: red.map(|s| crate::check::failed_files(&s.output, &tree).into_iter().map(|f| (f, None)).collect()).unwrap_or_default(),
        verdict: if red.is_some() { Verdict::Red } else { Verdict::Green },
        files,
        steps,
        created_at: now_ms(),
    };
    write_json(&dir.join(".avp").join("local-proof.json"), &proof)?;
    Ok(proof)
}

/// What an offline land did.
pub enum Landing {
    /// Red on this machine: nothing queued, the task stays open here to fix.
    Red(Proof),
    /// The task changes nothing.
    Empty,
    /// Green here, queued: the task is closed in this folder. How many entries wait.
    Queued(Proof, usize),
}

/// `avp land` offline: the folder's changes queued, the candidate proven on this machine, and the
/// landing queued when it is green.
pub fn land(server: &str, dir: &Path, state: &mut ViewState) -> Result<Landing> {
    sync(server, dir, state)?;
    let task = state.task.clone().context("no task open here")?;
    let hash = Replica::open(server, &state.repo)?.objects.put_tree(&known_tree(state))?;
    if state.base.as_ref().is_some_and(|b| b.1 == hash) {
        return Ok(Landing::Empty);
    }
    let p = prove(server, dir, state)?;
    if p.verdict != Verdict::Green {
        return Ok(Landing::Red(p));
    }
    let n = push(dir, &task, vec![Op::Land { proof: p.id.clone() }])?;
    state.task = None;
    view::save(dir, state)?;
    Ok(Landing::Queued(p, n))
}

/// `avp undo` offline: one path back to the task's base, or every write undone (to 0). Laid out
/// here from the replica, and queued for the server.
pub fn undo(server: &str, dir: &Path, state: &mut ViewState, to: i64, path: Option<String>) -> Result<String> {
    let task = state.task.clone().context("no task open here")?;
    anyhow::ensure!(to == 0, "offline, `undo {to}` needs the server's numbering of the writes: `avp undo --path <file>`, and `avp undo` (all of them), work offline");
    let r = Replica::open(server, &state.repo)?;
    let base_hash = state.base.clone().context("the tree this task started from is not on this machine: this undo needs the server")?.1;
    let base = r.objects.get_tree(&base_hash).context("the tree this task started from is not on this machine: this undo needs the server")?;
    queue_after_scan(server, dir, state, &task, vec![])?;
    let mut tree = known_tree(state);
    match &path {
        Some(p) => match base.get(p) {
            Some(e) => {
                tree.insert(p.clone(), e.clone());
            }
            None => {
                tree.remove(p);
            }
        },
        None => tree = base,
    }
    let n = view::materialize_offline(&r.objects, &tree, dir, state)?;
    let waiting = push(dir, &task, vec![Op::Undo { to, path }])?;
    Ok(format!("{}\nundone here, {n} files laid out; {}", notice(&r.line()), queued_text(waiting)))
}

/// `goal`, `claim`, `release` offline: queued as they are.
pub fn queue(dir: &Path, state: &ViewState, op: Op) -> Result<usize> {
    let task = state.task.clone().context("no task open here")?;
    push(dir, &task, vec![op])
}

/// `avp abandon` offline. A task that never reached the server takes its entries with it.
pub fn abandon(dir: &Path, state: &mut ViewState) -> Result<String> {
    let task = state.task.clone().context("no task open here")?;
    let one = lock(dir)?;
    let mut o = outbox(dir)?;
    let said = if is_local(&task) {
        o.queued.retain(|q| q.task != task);
        format!("abandoned {task}: it never reached the server, so nothing of it is sent ({} waiting)", o.queued.len())
    } else {
        add(&mut o, &task, vec![Op::Abandon]);
        format!("abandoned {task}; {}", queued_text(o.queued.len()))
    };
    write_json(&outbox_file(dir), &o)?;
    drop(one);
    state.task = None;
    view::save(dir, state)?;
    Ok(said)
}

fn describe(op: &Op) -> String {
    match op {
        Op::Open { goal, base, .. } => format!("open on position {base}: {goal}"),
        Op::Write { path, kind: WriteKind::Delete, .. } => format!("- {path}"),
        Op::Write { path, .. } => format!("+ {path}"),
        Op::Undo { path: Some(p), .. } => format!("undo {p}"),
        Op::Undo { to, .. } => format!("undo to {to}"),
        Op::Goal { goal } => format!("goal: {goal}"),
        Op::Claim { paths } => format!("claim {}", paths.join(", ")),
        Op::Release { paths } => format!("release {}", paths.as_ref().map_or("all".into(), |p| p.join(", "))),
        Op::Land { .. } => "land (green on this machine)".into(),
        Op::Abandon => "abandon".into(),
    }
}

/// The entry the outbox is stuck on (the server refused it, everything after it waits), and the
/// way out, in one line.
pub fn stuck(o: &Outbox) -> Option<String> {
    let q = o.queued.first()?;
    let f = q.failed.as_ref()?;
    let after = match o.queued.len() - 1 {
        0 => String::new(),
        1 => "; the entry after it waits".into(),
        n => format!("; the {n} after it wait"),
    };
    Some(format!("outbox stuck on #{} ({}), refused {} UTC: {}{after}. `avp outbox drop {}` drops it, `avp outbox retry` sends it again", q.n, describe(&q.op), crate::blame::utc(f.at), f.error, q.n))
}

/// The outbox and the last flush, as `avp status` shows them.
pub fn outbox_text(dir: &Path) -> String {
    match outbox(dir) {
        Ok(o) => text_of(&o, 20),
        Err(e) => format!("outbox: cannot be read ({e:#}): it is kept as it is"),
    }
}

/// `max` entries at most: number, when queued, task, what; then why the first one's last send
/// failed, and the way out.
fn text_of(o: &Outbox, max: usize) -> String {
    let mut s = if o.queued.is_empty() { "outbox: empty".to_string() } else { format!("outbox: {} waiting, sent in order when the server answers", o.queued.len()) };
    for q in o.queued.iter().take(max) {
        s.push_str(&format!("\n  #{} {} UTC  {} {}", q.n, crate::blame::utc(q.at), q.task, describe(&q.op)));
    }
    if o.queued.len() > max {
        s.push_str(&format!("\n  ... and {} more (`avp outbox` lists them all)", o.queued.len() - max));
    }
    let stuck = stuck(o);
    if let Some(l) = &stuck {
        s.push_str(&format!("\n{l}"));
    }
    if let Some(h) = &o.held {
        s.push_str(&format!("\n  waiting for task {h} to land (its queued land was handed back)"));
    }
    if let Some(f) = &o.last {
        s.push_str(&format!("\nlast flush: {} UTC, {} sent, {} left", crate::blame::utc(f.at), f.sent, f.left));
        // The stuck line above says why already.
        if let Some(e) = f.error.as_ref().filter(|_| stuck.is_none()) {
            s.push_str(&format!(", stopped: {e}"));
        }
        for l in f.said.iter().filter(|l| stuck.is_none() || !l.starts_with("outbox stuck on")) {
            s.push_str(&format!("\n  {l}"));
        }
    }
    s
}

/// `avp outbox`: every entry, and the last flush.
pub fn list(dir: &Path) -> Result<(serde_json::Value, String)> {
    let o = outbox(dir)?;
    let v = serde_json::json!({ "queued": o.queued, "held": o.held, "stuck": stuck(&o), "last_flush": o.last });
    Ok((v, text_of(&o, usize::MAX)))
}

/// `avp outbox retry`: the outbox sent now, in order, and what is left.
pub fn retry(client: &Client, dir: &Path) -> Result<(serde_json::Value, String)> {
    let link = flush(client, dir, true);
    let waiting = outbox(dir)?.queued.len();
    let mut said = link.said.clone();
    said.push(match waiting {
        0 if link.said.is_empty() => "outbox: empty, nothing to send".into(),
        0 => "outbox: empty".into(),
        n if link.down && !link.stuck => format!("outbox: {n} waiting, the server does not answer: sent by the next command that reaches it"),
        n => format!("outbox: {n} waiting"),
    });
    Ok((serde_json::json!({ "said": link.said, "waiting": waiting, "down": link.down, "stuck": link.stuck }), said.join("\n")))
}

/// `avp outbox drop`: entries out of the outbox, asking nothing. An open of a task of this
/// machine's takes the task's entries with it: none of them can go without it. What a dropped write
/// wrote stays in the folder, unsent again: the next sync sends it, or `avp open` carries it into
/// a new task.
pub fn drop_entries(dir: &Path, ns: &[u64]) -> Result<(serde_json::Value, String)> {
    anyhow::ensure!(!ns.is_empty(), "which entries? `avp outbox` lists them with their numbers");
    let one = lock(dir)?;
    let mut o = outbox(dir)?;
    if let Some(n) = ns.iter().find(|n| !o.queued.iter().any(|q| q.n == **n)) {
        anyhow::bail!("no entry #{n} in the outbox: `avp outbox` lists them");
    }
    let gone: BTreeSet<String> = o.queued.iter().filter(|q| ns.contains(&q.n) && matches!(q.op, Op::Open { .. }) && is_local(&q.task) && !o.ids.contains_key(&q.task)).map(|q| q.task.clone()).collect();
    let (dropped, kept): (Vec<Queued>, Vec<Queued>) = std::mem::take(&mut o.queued).into_iter().partition(|q| ns.contains(&q.n) || gone.contains(&q.task));
    o.queued = kept;
    if o.resume.as_ref().is_some_and(|t| gone.contains(t)) {
        o.resume = None;
    }
    if o.queued.is_empty() {
        o.ids.clear();
        o.exact.clear();
        (o.held, o.resume) = (None, None);
    }
    write_json(&outbox_file(dir), &o)?;
    drop(one);
    let mut said: Vec<String> = dropped.iter().map(|q| format!("dropped #{} ({} {})", q.n, q.task, describe(&q.op))).collect();
    if let Ok(mut state) = view::load(dir) {
        let mut unsent = Vec::new();
        for q in &dropped {
            if let Op::Write { path, blob, was, .. } = &q.op {
                // Only while nothing later wrote the path: the folder still holds this write.
                if state.known.get(path).map(|k| &k.0) == blob.as_ref() {
                    match was {
                        Some(w) => {
                            let mode = state.known.get(path).map_or(0o100644, |k| k.1);
                            state.known.insert(path.clone(), (w.clone(), mode));
                        }
                        None => {
                            state.known.remove(path);
                        }
                    }
                    unsent.push(path.clone());
                }
            }
        }
        if !unsent.is_empty() {
            said.push(format!("unsent again in this folder, sent by the next sync: {}", unsent.join(", ")));
        }
        if state.task.as_ref().is_some_and(|t| gone.contains(t)) {
            said.push(format!("task {} never reaches the server: this folder holds no task now, `avp open` opens one with what it holds", state.task.take().unwrap_or_default()));
        }
        view::save(dir, &state)?;
    }
    said.push(if o.queued.is_empty() { "outbox: empty".into() } else { format!("{} waiting, sent in order by the next command that reaches the server", o.queued.len()) });
    Ok((serde_json::json!({ "dropped": dropped, "waiting": o.queued.len() }), said.join("\n")))
}

/// `avp status` without the server's answer: the replica's head, the view's task, the outbox, the
/// last local proof, the last flush. `online`: the server answers, but the task is not on it yet.
pub fn status(server: &str, dir: &Path, state: &ViewState, online: bool) -> Result<(serde_json::Value, String)> {
    let line = Replica::find(server, &state.repo).map(|r| r.line()).unwrap_or_default();
    let o = outbox(dir).unwrap_or_default();
    // The last local proof while it still says something: of the open task, or of a queued land.
    let proof: Option<Proof> = read_json::<Proof>(&dir.join(".avp").join("local-proof.json")).filter(|p| p.task.is_some() && (p.task == state.task || o.queued.iter().any(|q| Some(&q.task) == p.task.as_ref())));
    let mut out = vec![if online { "online".to_string() } else { notice(&line) }];
    out.push(match &state.task {
        Some(t) => format!("task {t}{} on position {}", if is_local(t) { " (opened on this machine)" } else { "" }, state.base.as_ref().map_or("?".into(), |b| b.0.to_string())),
        None => "no task open here".into(),
    });
    out.push(outbox_text(dir));
    if let Some(p) = &proof {
        out.push(format!("last local proof (task {}): {:?}, {}", p.task.as_deref().unwrap_or("-"), p.verdict, p.steps.iter().map(|s| format!("{} {} ({:.1}s)", if s.exit == 0 { "✓" } else { "✗" }, s.name, s.seconds)).collect::<Vec<_>>().join(", ")));
    }
    let v = serde_json::json!({ "offline": !online, "as_of": line.head, "task": state.task, "base": state.base.as_ref().map(|b| b.0), "outbox": o.queued, "last_flush": o.last, "local_proof": proof });
    Ok((v, out.join("\n")))
}

// ---- the flush ----

/// What reaching for the server found: whether it answered, and what sending the outbox said.
#[derive(Default)]
pub struct Link {
    /// The server did not answer, or the outbox could not be sent: what changes something is queued
    /// behind it, in order.
    pub down: bool,
    /// Down, but not for want of an answer: the server refused an entry (or this machine could not
    /// send it), and what follows waits behind it.
    pub stuck: bool,
    pub said: Vec<String>,
}

/// Sends the view's outbox to the server in order, when it holds anything: what the folder changed
/// since is queued first. Each entry leaves the outbox once the server took it, so a flush that
/// stops (the server went away again, an error) keeps the rest for the next command. `told`: the
/// caller says now what was sent (the MCP server's background flush leaves it to the next call).
pub fn flush(client: &Client, dir: &Path, told: bool) -> Link {
    let mut link = Link::default();
    let o = match outbox(dir) {
        Ok(o) => o,
        Err(e) => {
            link.said.push(format!("the outbox cannot be read ({e:#}): it is kept as it is"));
            link.down = true;
            return link;
        }
    };
    if o.queued.is_empty() {
        if let Some(mut f) = o.last.clone().filter(|f| !f.told && told) {
            link.said = std::mem::take(&mut f.said);
            if let Ok(_one) = lock(dir) {
                if let Ok(mut o) = outbox(dir) {
                    if let Some(l) = o.last.as_mut() {
                        l.told = true;
                    }
                    let _ = write_json(&outbox_file(dir), &o);
                }
            }
        }
        return link;
    }
    // Another process sends it now: this command waits behind it, queued.
    let Ok(f) = fs::File::create(dir.join(".avp").join("outbox.lock")) else { return link };
    if f.try_lock().is_err() {
        link.down = true;
        return link;
    }
    let mut said = Vec::new();
    let (mut sent, mut at) = (0, None);
    let out = send(client, dir, &mut said, &mut sent, &mut at);
    link.down = out.is_err();
    link.stuck = out.as_ref().is_err_and(|e| !unreachable(e));
    let error = out.as_ref().err().map(|e| format!("{e:#}"));
    let quiet = out.as_ref().is_err_and(unreachable) && sent == 0;
    if let Ok(mut o) = outbox(dir) {
        if !quiet {
            if let Some(e) = &error {
                // The entry the server refused keeps why: `avp outbox` and `avp status` say it, with
                // the way out, since everything after it waits.
                let refused = match o.queued.iter_mut().find(|q| link.stuck && Some(q.n) == at) {
                    Some(q) => {
                        q.failed = Some(Failure { at: now_ms(), error: e.split_whitespace().collect::<Vec<_>>().join(" ") });
                        true
                    }
                    None => false,
                };
                said.push(match stuck(&o) {
                    Some(s) if refused => s,
                    _ => format!("the outbox stopped at its entry #{}: {e}. It keeps it and the {} after it, sent by the next command that reaches the server", o.queued.first().map_or(0, |q| q.n), o.queued.len().saturating_sub(1)),
                });
            }
            // What a flush between two calls said, and nobody read yet, is said with this one.
            let mut all = o.last.take().filter(|l| !l.told).map(|l| l.said).unwrap_or_default();
            all.extend(said.iter().cloned());
            o.last = Some(Flush { at: now_ms(), sent, left: o.queued.len(), said: all.clone(), error, told });
            let _ = write_json(&outbox_file(dir), &o);
            said = all;
        }
    }
    drop(f);
    if told {
        link.said = said;
    }
    link
}

/// The flush itself, under the outbox's lock. `at`: the entry being sent, when it fails.
fn send(client: &Client, dir: &Path, said: &mut Vec<String>, sent: &mut usize, at: &mut Option<u64>) -> Result<()> {
    let mut state = view::load(dir)?;
    let repo = state.repo.clone();
    let r = Replica::open(&client.base, &repo)?;
    let mut o = outbox(dir)?;
    let save = |o: &Outbox| write_json(&outbox_file(dir), o);
    if let Some(h) = o.held.clone() {
        let st: TaskStatus = client.get(&format!("/repos/{repo}/tasks/{h}"))?;
        if st.task.state == TaskState::Open {
            said.push(format!("{} queued after task {h} wait until it lands: settle it in this folder, then land it", o.queued.len()));
            return Ok(());
        }
        o.held = None;
        // The task this folder held when the other was handed back is its own again.
        if state.task.is_none() {
            state.task = o.resume.take().map(|t| o.ids.get(&t).cloned().unwrap_or(t));
            view::save(dir, &state)?;
        }
        save(&o)?;
    }
    // What the folder changed since it last queued goes first: the layout below must not write over it.
    if let Some(t) = state.task.clone() {
        let ops = scan_writes(&client.base, dir, &mut state)?;
        if !ops.is_empty() {
            add(&mut o, &t, ops);
            save(&o)?;
            view::save(dir, &state)?;
        }
    }
    // Each task's tree on the server as this flush leaves it: what a write is replayed onto.
    let mut trees: HashMap<String, Tree> = HashMap::new();
    let mut conflicts: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut replayed: BTreeSet<String> = BTreeSet::new();
    let (mut landed, mut back): (Option<Landed>, Option<String>) = (None, None);
    while let Some(q) = o.queued.first().cloned() {
        let task = o.ids.get(&q.task).cloned().unwrap_or_else(|| q.task.clone());
        let mut stop = false;
        *at = Some(q.n);
        match q.op {
            // Opened by a flush that stopped before it could say so: mapped already.
            Op::Open { .. } if !is_local(&task) => {}
            Op::Open { goal, scope, actor, transcript, base } => {
                // On the position the folder held: its writes go as they were made, and its land
                // replays it on what landed since, as for any task the line moved under. A server
                // from before `base` opens it on the head: its writes are merged here instead.
                let opened: crate::blame::Opened = client.post(&format!("/repos/{repo}/tasks?base={base}"), &OpenTask { goal, actor, scope, transcript })?;
                let t = opened.task;
                o.ids.insert(q.task.clone(), t.id.clone());
                if t.base_position == base {
                    o.exact.insert(t.id.clone());
                }
                save(&o)?;
                if state.task.as_deref() == Some(q.task.as_str()) {
                    state.task = Some(t.id.clone());
                    state.base = Some((t.base_position, t.base_tree.clone()));
                    view::save(dir, &state)?;
                }
                if t.base_position == base {
                    said.push(format!("task {} opened on the server as {}, on position {base} as this folder held it: its land replays it on what landed since", q.task, t.id));
                } else {
                    trees.insert(t.id.clone(), client.get(&format!("/repos/{repo}/tree/{}", t.base_position))?);
                    replayed.insert(t.id.clone());
                    said.push(format!("task {} opened on the server as {}, on position {} (this server opens tasks on the head only): its writes, made on position {base}, are merged with what landed since", q.task, t.id, t.base_position));
                }
            }
            Op::Write { path, kind, blob, mode, was } => {
                let sent_as = if o.exact.contains(&task) {
                    Some((kind, blob))
                } else {
                    let tree = match trees.entry(task.clone()) {
                        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::hash_map::Entry::Vacant(e) => e.insert(client.get(&format!("/repos/{repo}/tasks/{task}/tree"))?),
                    };
                    let now = tree.get(&path).map(|e| e.blob.clone());
                    let mine = conflicts.entry(task.clone()).or_default();
                    mine.remove(&path);
                    if now == was {
                        Some((kind, blob))
                    } else {
                        replay(client, &repo, &r, &path, kind, blob, was, now, mine, said)?
                    }
                };
                if let Some((kind, blob)) = sent_as {
                    if let Some(b) = &blob {
                        client.put_object(&repo, r.objects.get(b)?)?;
                    }
                    let out: WriteOutcome = client.post(&format!("/repos/{repo}/tasks/{task}/writes"), &PutWrite { path: path.clone(), kind, blob: blob.clone(), mode })?;
                    if out.lease == "refused" {
                        // Strict leases: the folder keeps the file, the next sync sends it again.
                        said.push(format!("{path}: refused, {} holds it", out.previous_holder.unwrap_or_default()));
                        state.known.remove(&path);
                    } else if let Some(tree) = trees.get_mut(&task) {
                        match blob.filter(|_| kind != WriteKind::Delete) {
                            Some(b) => tree.insert(path, Entry { blob: b, mode }),
                            None => tree.remove(&path),
                        };
                    }
                }
            }
            Op::Undo { to, path } => {
                let _: serde_json::Value = client.post(&format!("/repos/{repo}/tasks/{task}/undo"), &serde_json::json!({ "to": to, "path": path }))?;
                trees.remove(&task);
                match &path {
                    Some(p) => conflicts.entry(task.clone()).or_default().remove(p),
                    None => conflicts.remove(&task).is_some(),
                };
            }
            Op::Goal { goal } => {
                let _: serde_json::Value = client.post(&format!("/repos/{repo}/tasks/{task}/goal"), &serde_json::json!({ "goal": goal }))?;
            }
            Op::Claim { paths } => {
                let _: Vec<(String, Option<String>)> = client.post(&format!("/repos/{repo}/tasks/{task}/claim"), &serde_json::json!({ "paths": paths }))?;
            }
            Op::Release { paths } => {
                let _: serde_json::Value = client.post(&format!("/repos/{repo}/tasks/{task}/release"), &serde_json::json!({ "paths": paths }))?;
            }
            Op::Abandon => {
                let st: TaskStatus = client.get(&format!("/repos/{repo}/tasks/{task}"))?;
                if st.task.state == TaskState::Open {
                    let _: serde_json::Value = client.post(&format!("/repos/{repo}/tasks/{task}/abandon"), &serde_json::json!({}))?;
                }
                said.push(format!("task {task} abandoned"));
            }
            Op::Land { .. } => {
                let left: Vec<String> = conflicts.get(&task).map(|c| c.iter().cloned().collect()).unwrap_or_default();
                if !left.is_empty() {
                    said.push(format!("the land queued for task {task} did not go: replaying it on the line left conflicts in {}. They are in this folder with their markers: settle them, then land again", left.join(", ")));
                    back = Some(task.clone());
                    stop = true;
                } else {
                    match client.land(&repo, &task)? {
                        Ok(l) => {
                            said.push(format!("the land queued for task {task} landed at position {}{} (proof {})", l.position, if l.replayed || replayed.contains(&task) { ", replayed on the moved line" } else { "" }, l.proof));
                            if state.task.as_deref() == Some(task.as_str()) {
                                state.task = None;
                            }
                            landed = Some(l);
                        }
                        Err(n) => {
                            let mut s = format!("the land queued for task {task} did not land: {}", n.reason);
                            if !n.conflicts.is_empty() {
                                s.push_str(&format!(" (markers in {}: settle them, then land again)", n.conflicts.join(", ")));
                            }
                            if let Some(p) = &n.proof {
                                s.push_str(&format!("; the server's proof is {:?}: {}", p.verdict, p.steps.iter().filter(|s| s.exit != 0).map(|s| s.name.clone()).collect::<Vec<_>>().join(", ")));
                            }
                            if n.reason == "approval" {
                                s.push_str("; green, it waits for a person's approval");
                            } else if matches!(n.reason.as_str(), "conflict" | "red" | "blocked" | "refused") {
                                s.push_str("; it is back in this folder to fix, then land again");
                                back = Some(task.clone());
                            }
                            said.push(s);
                            stop = true;
                        }
                    }
                }
            }
        }
        o.queued.remove(0);
        *at = None;
        *sent += 1;
        if stop && !o.queued.is_empty() {
            o.held = Some(task.clone());
            if back.is_some() && state.task.as_deref() != Some(task.as_str()) {
                o.resume = state.task.clone();
            }
        }
        save(&o)?;
        if stop {
            break;
        }
    }
    // The open task stands where the folder stood, its writes as they were made: the folder holds it.
    let as_sent = back.is_none() && state.task.as_ref().is_some_and(|t| o.exact.contains(t));
    if o.queued.is_empty() {
        o.ids.clear();
        o.exact.clear();
        (o.held, o.resume) = (None, None);
        save(&o)?;
    }
    // The folder takes what the server now holds: the task handed back with its markers, the open
    // task with what landed meanwhile merged in, or the position a queued land made.
    if *sent > 0 {
        if as_sent {
            view::save(dir, &state)?;
        } else if let Some(t) = back.clone().or_else(|| state.task.clone().filter(|t| !is_local(t))) {
            let tree: Tree = client.get(&format!("/repos/{repo}/tasks/{t}/tree"))?;
            if back.is_some() {
                let st: TaskStatus = client.get(&format!("/repos/{repo}/tasks/{t}"))?;
                state.base = Some((st.task.base_position, st.task.base_tree));
            }
            state.task = Some(t);
            view::materialize(client, &tree, dir, &mut state)?;
        } else if let Some(l) = &landed {
            let tree: Tree = client.get(&format!("/repos/{repo}/tree/{}", l.position))?;
            state.base = Some((l.position, l.tree.clone()));
            view::materialize(client, &tree, dir, &mut state)?;
        } else {
            view::save(dir, &state)?;
        }
    }
    refresh(client, &repo, true);
    Ok(())
}

/// A write made on `was` where the server's task now holds `now`: merged three ways with it. None
/// when there is nothing to send. A conflict is sent as one, its markers in the file (the server
/// lands nothing until it is settled), and its path kept in `conflicts`.
#[allow(clippy::too_many_arguments)]
fn replay(client: &Client, repo: &str, r: &Replica, path: &str, kind: WriteKind, blob: Option<String>, was: Option<String>, now: Option<String>, conflicts: &mut BTreeSet<String>, said: &mut Vec<String>) -> Result<Option<(WriteKind, Option<String>)>> {
    match (kind, blob, now) {
        // Deleted on the line too.
        (WriteKind::Delete, _, None) => Ok(None),
        (WriteKind::Delete, _, Some(_)) => {
            said.push(format!("{path}: deleted here, changed on the line meanwhile: the line's version stays, delete it again if it must go"));
            Ok(None)
        }
        (_, Some(mine), Some(theirs)) => {
            let base = was.map(|w| r.object(client, repo, &w)).transpose()?.unwrap_or_default();
            let (merged, clean) = view::merge_bytes(&r.object(client, repo, &mine)?, &base, &r.object(client, repo, &theirs)?)?;
            if !clean {
                said.push(format!("{path}: changed here and on the line meanwhile: merged with conflict markers, to settle"));
                conflicts.insert(path.to_string());
            }
            Ok(Some((if clean { WriteKind::Put } else { WriteKind::Conflict }, Some(r.objects.put(&merged)?))))
        }
        (_, Some(mine), None) => {
            said.push(format!("{path}: changed here, deleted on the line meanwhile: yours is kept, to settle"));
            conflicts.insert(path.to_string());
            Ok(Some((WriteKind::Conflict, Some(mine))))
        }
        (_, None, _) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A position's patch, as the server stores it, takes the file forward to what the position made
    /// of it and back to what it was before: the blame walks the line through it.
    #[test]
    fn a_stored_patch_goes_both_ways() {
        let old: String = (0..30).map(|i| format!("{i}\n")).collect();
        let new = old.replacen("2\n", "two\n", 1).replacen("20\n", "twenty\nmore\n", 1) + "last";
        let full = diffy::create_patch(&old, &new).to_string();
        // What engine::file_diff keeps: the patch without its header.
        let stored = full.lines().skip_while(|l| l.starts_with("---") || l.starts_with("+++")).collect::<Vec<_>>().join("\n");
        assert_eq!(apply(&old, &stored, false).as_deref(), Some(new.as_str()));
        assert_eq!(apply(&new, &stored, true).as_deref(), Some(old.as_str()));
        assert_eq!(apply("", "@@ -0,0 +1,2 @@\n+a\n+b", false).as_deref(), Some("a\nb\n"), "a file added");
    }

    /// The open of a task of this machine's dropped: its entries go with it, the folder no longer
    /// holds it, and what its write wrote is unsent again. Another task's entry stays.
    #[test]
    fn dropping_an_open_takes_its_task_with_it() {
        let dir = std::env::temp_dir().join(format!("avp-drop-{}", ulid::Ulid::new()));
        let mut state = ViewState { task: Some("local_a".into()), ..Default::default() };
        state.known.insert("a.txt".into(), ("new".into(), 0o100644));
        view::save(&dir, &state).unwrap();
        let actor = Actor { person: "p".into(), agent: "a".into(), session: "s".into() };
        let write = Op::Write { path: "a.txt".into(), kind: WriteKind::Put, blob: Some("new".into()), mode: 0o100644, was: Some("old".into()) };
        push(&dir, "local_a", vec![Op::Open { goal: "g".into(), scope: vec![], actor, transcript: None, base: 3 }, write, Op::Land { proof: "p".into() }]).unwrap();
        push(&dir, "task_b", vec![Op::Goal { goal: "b".into() }]).unwrap();
        let (_, said) = drop_entries(&dir, &[1]).unwrap();
        assert_eq!(outbox(&dir).unwrap().queued.iter().map(|q| q.n).collect::<Vec<_>>(), vec![4]);
        let state = view::load(&dir).unwrap();
        assert_eq!(state.task, None);
        assert_eq!(state.known["a.txt"].0, "old");
        assert!(said.contains("dropped #3") && said.contains("unsent again") && said.contains("1 waiting"), "{said}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_server_folder_never_names_its_password() {
        assert_eq!(server_key("http://127.0.0.1:7117"), "127.0.0.1-7117");
        assert_eq!(server_key("https://ada:s3cret@avp.example.com/team/"), "avp.example.com-team");
    }
}
