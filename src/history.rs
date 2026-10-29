//! Reading and undoing the line the way a person audits it: `avp show` prints what a position
//! changed, with its goal, author and proof; `avp revert` opens a task that undoes it on the head,
//! proven like any other change before it lands; `avp context` says what the line remembers of
//! paths, before an agent reads them; `avp bisect` finds the position that broke a check, and `avp
//! pick` brings a position's change into a task.

use crate::blame::PathContext;
use crate::client::Client;
use crate::engine::Picked;
use crate::model::*;
use crate::view;
use anyhow::{anyhow, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `src/a.ts` or `src/a.ts:42`: the path, and the line when one is named.
pub fn split_line(target: &str) -> (&str, Option<usize>) {
    match target.rsplit_once(':') {
        Some((p, n)) if !p.is_empty() && n.parse::<usize>().is_ok() => (p, n.parse().ok()),
        _ => (target, None),
    }
}

/// Who wrote a file, or one of its lines, and why: the task, its goal, the person and the agent,
/// the proof, and the agent's conversation when it is known.
pub fn why(client: &Client, repo: &str, target: &str) -> Result<String> {
    Ok(why_text(&why_of(client, repo, target)?))
}

fn why_of(client: &Client, repo: &str, target: &str) -> Result<Why> {
    let (path, line) = split_line(target);
    let mut url = format!("/repos/{repo}/why?path={path}");
    if let Some(n) = line {
        url.push_str(&format!("&line={n}"));
    }
    client.get(&url)
}

/// `why` as a person reads it.
pub fn why_text(w: &Why) -> String {
    let what = match (&w.line, &w.text) {
        (Some(n), Some(t)) => format!("{}:{n}  {}\n  written at position {}", w.path, t.trim(), w.position),
        _ => format!("{} last changed at position {}", w.path, w.position),
    };
    match &w.task {
        Some(t) => format!(
            "{what} by task {}\n  goal: {}\n  by: {} via {}\n  proof: {}{}",
            t.id,
            t.goal,
            t.actor.person,
            t.actor.agent,
            w.proof.as_ref().map(|p| format!("{} {:?}, {}", p.id, p.verdict, p.steps.iter().map(|s| format!("{} {}", if s.exit == 0 { "✓" } else { "✗" }, s.name)).collect::<Vec<_>>().join(", "))).unwrap_or_else(|| "none".into()),
            t.transcript.as_ref().map(|x| format!("\n  conversation: {x}")).unwrap_or_default()
        ),
        None => format!("{what} (imported from Git, no task)"),
    }
}

/// What the line remembers of some paths, and why the lines asked about are what they are.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Context {
    pub paths: Vec<PathContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<Why>,
}

/// `avp context` and the avp_context tool, before reading a big file: for each of `targets` (`path`
/// or `path:line`), its last changes and the open task other than `task` that holds it; and for
/// each `path:line`, why that line is what it is.
pub fn context(client: &Client, repo: &str, targets: &[String], task: Option<&str>) -> Result<Context> {
    let paths: Vec<&str> = targets.iter().map(|t| split_line(t).0).collect();
    let found: Vec<PathContext> = client.post(&format!("/repos/{repo}/history/context"), &serde_json::json!({ "paths": paths, "task": task }))?;
    let why = targets.iter().filter(|t| split_line(t).1.is_some()).map(|t| why_of(client, repo, t)).collect::<Result<_>>()?;
    Ok(Context { paths: found, why })
}

pub fn context_text(c: &Context) -> String {
    let mut out = crate::blame::context_text(&c.paths, now_ms());
    if out.is_empty() {
        out = "no position of the line changed these paths".into();
    }
    for w in &c.why {
        out.push_str(&format!("\n{}", why_text(w)));
    }
    out
}

/// The paths of these writes that took a lease, outside the task's scope: its first writes there.
pub fn outside_scope<'a>(writes: impl IntoIterator<Item = &'a WriteOutcome>) -> Vec<String> {
    writes.into_iter().filter(|w| w.lease == "taken" || w.lease == "taken_over").map(|w| w.write.path.clone()).collect()
}

/// The line's memory of paths a task starts to write outside its scope, as the answer that says so
/// adds it. Nothing when they have no history, or the server cannot tell (one from before it): the
/// answer never fails for it.
pub fn first_writes(client: &Client, repo: &str, task: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let found: Vec<PathContext> = client.post(&format!("/repos/{repo}/history/context"), &serde_json::json!({ "paths": paths, "task": task })).unwrap_or_default();
    crate::blame::context_text(&found, now_ms())
}

fn entry(client: &Client, repo: &str, position: i64) -> Result<LineEntry> {
    client.get::<Vec<LineEntry>>(&format!("/repos/{repo}/line?from={position}&limit=1"))?.into_iter().find(|e| e.position == position).ok_or_else(|| anyhow!("no position {position}"))
}

fn text(client: &Client, repo: &str, tree: &Tree, path: &str) -> Result<Option<Vec<u8>>> {
    tree.get(path).map(|e| client.get_object(repo, &e.blob)).transpose()
}

/// The paths a position changed against the one before it, in order.
fn changed(before: &Tree, after: &Tree) -> Vec<String> {
    let mut paths: Vec<String> = after.iter().filter(|(p, e)| before.get(*p).map(|b| b.blob != e.blob || b.mode != e.mode).unwrap_or(true)).map(|(p, _)| p.clone()).collect();
    paths.extend(before.keys().filter(|p| !after.contains_key(*p)).cloned());
    paths.sort();
    paths
}

/// What position `position` changed: who, why, with which proof, then the diff file by file.
pub fn show(client: &Client, repo: &str, position: i64) -> Result<String> {
    anyhow::ensure!(position > 0, "position 0 is where the line starts: it changed nothing");
    let e = entry(client, repo, position)?;
    let after: Tree = client.get(&format!("/repos/{repo}/tree/{position}"))?;
    let before: Tree = client.get(&format!("/repos/{repo}/tree/{}", position - 1))?;
    let mut out = format!("position {position}  {}\n", e.note.trim());
    if let Some(t) = &e.task {
        let s: TaskStatus = client.get(&format!("/repos/{repo}/tasks/{t}"))?;
        out.push_str(&format!("  task {t}, by {} via {}\n", s.task.actor.person, s.task.actor.agent));
    }
    if let Some(p) = &e.proof {
        let proof: Proof = client.get(&format!("/repos/{repo}/proofs/{p}"))?;
        out.push_str(&format!("  proof {p}: {:?}, {}\n", proof.verdict, proof.steps.iter().map(|s| format!("{} {}", if s.exit == 0 { "✓" } else { "✗" }, s.name)).collect::<Vec<_>>().join(", ")));
    }
    out.push_str(&format!("  full suite: {}", e.full.map(|v| format!("{v:?}")).unwrap_or_else(|| "not run yet".into())));
    if let Some(g) = &e.git {
        out.push_str(&format!(", git commit {}", &g[..g.len().min(12)]));
    }
    out.push('\n');
    for path in changed(&before, &after) {
        let (old, new) = (text(client, repo, &before, &path)?, text(client, repo, &after, &path)?);
        out.push_str(&format!("\n--- a/{path}\n+++ b/{path}\n"));
        match (old.as_deref().map(std::str::from_utf8), new.as_deref().map(std::str::from_utf8)) {
            (Some(Err(_)), _) | (_, Some(Err(_))) => out.push_str("binary file changed\n"),
            (o, n) => {
                let (o, n) = (o.map(|r| r.unwrap()).unwrap_or(""), n.map(|r| r.unwrap()).unwrap_or(""));
                let patch = diffy::create_patch(o, n).to_string();
                // diffy names the sides "original" and "modified": the paths are already above.
                out.push_str(&patch.lines().skip(2).map(|l| format!("{l}\n")).collect::<String>());
            }
        }
    }
    Ok(out)
}

/// Opens a task in `dir` that undoes position `position` on the head of the line, and sends its
/// writes. A file nobody touched since comes back as it was; a file changed since gets the inverse
/// change merged in, or conflict markers where both touched the same lines. Landing proves it.
pub fn revert(client: &Client, dir: &Path, repo: &str, position: i64, actor: Actor) -> Result<String> {
    anyhow::ensure!(position > 0, "position 0 is where the line starts: there is nothing to revert");
    let e = entry(client, repo, position)?;
    let after: Tree = client.get(&format!("/repos/{repo}/tree/{position}"))?;
    let before: Tree = client.get(&format!("/repos/{repo}/tree/{}", position - 1))?;
    let mut state = view::load(dir).unwrap_or_default();
    if let Some(t) = &state.task {
        let st: TaskStatus = client.get(&format!("/repos/{}/tasks/{t}", state.repo))?;
        anyhow::ensure!(st.task.state != TaskState::Open, "task {t} is still open here: land or abandon it first");
    }
    let r: Repo = client.get(&format!("/repos/{repo}"))?;
    state.repo = r.id.clone();
    state.server = client.base.clone();
    let goal = format!("Revert position {position}: {}", e.note.trim());
    let task: Task = client.post(&format!("/repos/{}/tasks", r.id), &OpenTask { goal, actor, scope: vec![], transcript: None })?;
    state.task = Some(task.id.clone());
    let head: Tree = client.get(&format!("/repos/{}/tree/{}", r.id, task.base_position))?;
    view::materialize(client, &head, dir, &mut state)?;
    let (mut back, mut merged, mut conflicts) = (0, 0, Vec::new());
    for path in changed(&before, &after) {
        let target = dir.join(&path);
        let (was, became, now) = (text(client, &r.id, &before, &path)?, text(client, &r.id, &after, &path)?, text(client, &r.id, &head, &path)?);
        if now == became {
            // Untouched since: exactly as it was before the position.
            match &was {
                Some(b) => {
                    std::fs::create_dir_all(target.parent().context("path")?)?;
                    std::fs::write(&target, b)?;
                }
                None => {
                    let _ = std::fs::remove_file(&target);
                }
            }
            back += 1;
            continue;
        }
        let as_text = |b: &Option<Vec<u8>>| b.as_deref().map(|x| String::from_utf8_lossy(x).to_string()).unwrap_or_default();
        match (&was, &became, &now) {
            (Some(_), Some(_), Some(_)) => match diffy::merge(&as_text(&became), &as_text(&now), &as_text(&was)) {
                Ok(t) => {
                    std::fs::write(&target, t)?;
                    merged += 1;
                }
                Err(t) => {
                    std::fs::write(&target, t)?;
                    conflicts.push(path);
                }
            },
            // Added by the position, or deleted by it, and changed again since: a person decides.
            _ => conflicts.push(path),
        }
    }
    view::sync(client, dir, &mut state)?;
    let mut out = format!("task {} opened to revert position {position} on position {}: {back} files back as they were, {merged} with the inverse change merged", task.id, task.base_position);
    if conflicts.is_empty() {
        out.push_str(". `avp land` proves and lands it.");
    } else {
        out.push_str(&format!(", {} to settle (changed again since):", conflicts.len()));
        for c in &conflicts {
            out.push_str(&format!("\n  {c}"));
        }
        out.push_str("\nSettle them in this folder, then `avp land`.");
    }
    Ok(out)
}

/// One position `avp bisect` laid out and ran its check on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probe {
    pub position: i64,
    /// The check's exit: 0 is good, anything else bad.
    pub exit: i32,
    pub seconds: f64,
}

/// The first position a check fails on, with what `avp show` says of it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bisected {
    pub check: String,
    pub first_bad: i64,
    /// The position before it, where the check passes.
    pub last_good: i64,
    pub goal: String,
    pub task: Option<String>,
    pub person: Option<String>,
    pub agent: Option<String>,
    pub proof: Option<String>,
    pub verdict: Option<Verdict>,
    /// The check's last lines on the first bad position.
    pub output: String,
    /// Each position checked, in order: both ends first, then the halving.
    pub probes: Vec<Probe>,
    /// The scratch folder, left holding the first bad position.
    pub dir: PathBuf,
}

/// `avp bisect`: halves the positions between `good` (none: the newest below `bad` whose full suite
/// is green, else 0) and `bad` (none: the head), laying each out in a scratch folder of its own and
/// running `check` there, with the halving AVP's auto-revert uses (`engine::halve`). The check's
/// exit says: 0 good, anything else bad. Both ends are checked first: a check that fails on every
/// position, or passes on the bad one, blames nobody. `say` hears of each position as it is checked.
pub fn bisect(client: &Client, repo: &str, check: &str, good: Option<i64>, bad: Option<i64>, say: &dyn Fn(&str)) -> Result<Bisected> {
    let r: Repo = client.get(&format!("/repos/{repo}"))?;
    let head: LineEntry = client.get(&format!("/repos/{}/head", r.id))?;
    let bad = bad.unwrap_or(head.position);
    anyhow::ensure!((1..=head.position).contains(&bad), "no position {bad} to look below: the line holds 0 to {}", head.position);
    let given = good;
    let mut good = match good {
        Some(g) => g,
        None => client.get::<Vec<LineEntry>>(&format!("/repos/{}/line?from={}&limit=500", r.id, bad - 1))?.into_iter().find(|e| matches!(e.full, Some(Verdict::Green | Verdict::Flaky))).map_or(0, |e| e.position),
    };
    anyhow::ensure!((0..bad).contains(&good), "--good {good} must come before --bad {bad}");
    let name: String = r.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let dir = std::env::temp_dir().join(new_id(&format!("avp-bisect-{name}")));
    let mut state = view::ViewState { server: client.base.clone(), repo: r.id.clone(), ..Default::default() };
    let (mut probes, mut tails) = (Vec::new(), std::collections::BTreeMap::new());
    let mut is_bad = |position: i64| -> Result<bool> {
        let tree: Tree = client.get(&format!("/repos/{}/tree/{position}", r.id))?;
        lay_clean(client, &tree, &dir, &mut state)?;
        let start = std::time::Instant::now();
        let out = std::process::Command::new("sh").args(["-c", check]).current_dir(&dir).env("AVP_BISECT_POSITION", position.to_string()).stdin(std::process::Stdio::null()).output().context("sh -c <check>")?;
        let exit = out.status.code().ok_or_else(|| anyhow!("the check was stopped by a signal on position {position}: no position is blamed for it"))?;
        let seconds = start.elapsed().as_secs_f64();
        say(&format!("position {position}: {} (exit {exit}, {seconds:.1}s)", if exit == 0 { "good" } else { "bad" }));
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        tails.insert(position, text.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
        probes.push(Probe { position, exit, seconds });
        Ok(exit != 0)
    };
    anyhow::ensure!(is_bad(bad)?, "`{check}` passes on position {bad}: nothing to look for below it (--bad names a position where it fails)");
    if is_bad(good)? {
        anyhow::ensure!(given.is_none() && good > 0, "`{check}` fails on position {good} too: give --good a position where it passes");
        // A green full suite says nothing of this check: the line's start may.
        anyhow::ensure!(!is_bad(0)?, "`{check}` fails on position {good} and on position 0, where the line starts: nothing to blame");
        good = 0;
    }
    let first_bad = crate::engine::halve(good, bad, &mut is_bad)?;
    // The folder holds the first bad position, for a person to look at.
    if probes.last().map(|p| p.position) != Some(first_bad) {
        let tree: Tree = client.get(&format!("/repos/{}/tree/{first_bad}", r.id))?;
        lay_clean(client, &tree, &dir, &mut state)?;
    }
    let e = entry(client, &r.id, first_bad)?;
    let actor = e.task.as_ref().map(|t| client.get::<TaskStatus>(&format!("/repos/{}/tasks/{t}", r.id))).transpose()?.map(|s| s.task.actor);
    let verdict = e.proof.as_ref().map(|p| client.get::<Proof>(&format!("/repos/{}/proofs/{p}", r.id))).transpose()?.map(|p| p.verdict);
    Ok(Bisected {
        check: check.to_string(),
        first_bad,
        last_good: first_bad - 1,
        goal: e.note.trim().to_string(),
        task: e.task,
        person: actor.as_ref().map(|a| a.person.clone()),
        agent: actor.map(|a| a.agent),
        proof: e.proof,
        verdict,
        output: tails.remove(&first_bad).unwrap_or_default(),
        probes,
        dir,
    })
}

/// Lays `tree` out in `dir` as a fresh checkout would hold it: what an earlier check changed of the
/// tree's files, or added beside them, goes; what the ignore rules hide (a build's output) stays,
/// so the next check builds warm.
fn lay_clean(client: &Client, tree: &Tree, dir: &Path, state: &mut view::ViewState) -> Result<()> {
    if dir.exists() {
        for (path, now) in view::scan(dir, &state.known, &mut state.stamps)? {
            if state.known.get(&path) != Some(&now) {
                state.known.remove(&path);
                if !tree.contains_key(&path) {
                    std::fs::remove_file(dir.join(&path))?;
                }
            }
        }
    }
    view::materialize(client, tree, dir, state)?;
    Ok(())
}

pub fn bisect_text(b: &Bisected) -> String {
    let mut out = format!("first bad position: {}  {}\n", b.first_bad, b.goal);
    match (&b.task, &b.person, &b.agent) {
        (Some(t), Some(p), Some(a)) => out.push_str(&format!("  task {t}, by {p} via {a}\n")),
        _ => out.push_str("  imported, no task\n"),
    }
    if let (Some(p), Some(v)) = (&b.proof, &b.verdict) {
        out.push_str(&format!("  proof {p}: {v:?}\n"));
    }
    let probes: Vec<String> = b.probes.iter().map(|p| format!("{} {}", p.position, if p.exit == 0 { "good" } else { "bad" })).collect();
    out.push_str(&format!("  `{}` passes on position {} and fails on {} ({} checked: {})\n", b.check, b.last_good, b.first_bad, probes.len(), probes.join(", ")));
    if !b.output.trim().is_empty() {
        out.push_str(&format!("  ---\n  {}\n", b.output.trim_end().replace('\n', "\n  ")));
    }
    out.push_str(&format!("{} holds it. `avp show {}` has its change, `avp revert {}` takes it back.", b.dir.display(), b.first_bad, b.first_bad));
    out
}

/// `avp pick`: what this folder changed goes to the task first, then position `position`'s change
/// comes into it (`Engine::pick`), and the task's tree is laid out here.
pub fn pick(client: &Client, dir: &Path, state: &mut view::ViewState, position: i64) -> Result<Picked> {
    let task = state.task.clone().context("no task open here: `avp open` one, then pick into it")?;
    view::sync(client, dir, state)?;
    let picked: Picked = client.post(&format!("/repos/{}/tasks/{task}/pick", state.repo), &serde_json::json!({ "position": position }))?;
    let tree: Tree = client.get(&format!("/repos/{}/tasks/{task}/tree", state.repo))?;
    view::materialize(client, &tree, dir, state)?;
    Ok(picked)
}

pub fn picked_text(p: &Picked) -> String {
    if p.files.is_empty() && p.merged.is_empty() && p.conflicts.is_empty() && p.kept.is_empty() {
        return format!("the task already holds position {}'s change ({}): nothing to pick", p.position, p.goal);
    }
    let mut out = format!("position {} ({}) picked into the task: {} files as it has them, {} merged with what changed since", p.position, p.goal, p.files.len(), p.merged.len());
    if !p.conflicts.is_empty() {
        out.push_str(&format!(", {} to settle", p.conflicts.len()));
    }
    for c in &p.conflicts {
        out.push_str(&format!("\n  conflict, settle the markers: {c}"));
    }
    for (path, why) in &p.kept {
        out.push_str(&format!("\n  left as the task has it: {path} ({why})"));
    }
    out.push_str(if p.conflicts.is_empty() { "\nFix what needs fixing, then `avp land` proves the whole." } else { "\nSettle the markers, then `avp land`." });
    out
}
