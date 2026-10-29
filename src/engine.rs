//! The verbs. Everything the server does lives here so the CLI, the HTTP layer and tests call
//! the same functions.

use crate::check::{failed_files, import_neighbors, read_config, Part, Workspace};
use crate::db::{Db, Tag};
use crate::model::*;
use crate::objects::Objects;
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

pub const LEASE_TTL_MS: i64 = 10 * 60 * 1000;

/// The files that say how a task is proven. On AVP Cloud, changing them waits for a person's word
/// (25/10, a Product Hunt question: "can a task edit the checks that validate it?"). A task is
/// proven by its own, as a pull request runs its own CI: proven by the line's (25/10 to 28/10), a
/// change that broke the steps landed green once approved, and every task after it was red under
/// them, the one that fixed it and AVP's revert of it too. What a proof's steps get of the secrets
/// stays the line's (`line_secrets`): a task does not hand itself one before a person reads it.
pub const CHECK_FILES: [&str; 2] = ["avp.toml", "trame.toml"];

/// The approvals asked for that the webhooks were told of, by task and proof: an agent that lands
/// again while it waits asks nothing new.
static TOLD: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Clone)]
pub struct Engine {
    pub db: Arc<Mutex<Db>>,
    pub objects: Objects,
    pub data: PathBuf,
    /// One proof at a time per task: they share its folder, and the one that waits takes the
    /// verdict of the one before when it proved the same tree.
    task_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// What AVP Cloud keeps in memory beside (`seam::State`): nothing in the free AVP.
    pub cloud: Arc<crate::seam::State>,
    /// Each repo's landing queue.
    land_queues: Arc<Mutex<HashMap<String, Arc<LandQueue>>>>,
    /// What each running `avp wait` of an open task waits on, by task: who waits on whom.
    waits: Arc<Mutex<HashMap<String, Waiting>>>,
    /// The tasks a landing holds right now: another landing of one of them waits for it to end.
    landing: Arc<(Mutex<BTreeSet<String>>, Condvar)>,
}

/// What an open task's `avp wait` waits on: the tasks it names, and those holding the paths it
/// names. It looks every few seconds; what it waited on is forgotten a few seconds after it stops.
struct Waiting {
    on: BTreeSet<String>,
    until: i64,
}

/// How long a wait's last look stands for it: a few of its looks, 3 s apart.
const WAIT_SEEN_MS: i64 = 10_000;

/// The tasks a landing holds, given back when dropped.
struct Holding(Arc<(Mutex<BTreeSet<String>>, Condvar)>, Vec<String>);

impl Drop for Holding {
    fn drop(&mut self) {
        let mut held = self.0 .0.lock().unwrap_or_else(|e| e.into_inner());
        for t in &self.1 {
            held.remove(t);
        }
        self.0 .1.notify_all();
    }
}

/// A repo's landing queue: landings append to the line in turn, first come, first served.
#[derive(Default)]
struct LandQueue {
    /// The next ticket to give, and the ticket whose turn it is.
    tickets: Mutex<(u64, u64)>,
    turn: Condvar,
}

/// A landing's turn in its repo's queue, given up when dropped.
struct Turn(Arc<LandQueue>);

impl Drop for Turn {
    fn drop(&mut self) {
        self.0.tickets.lock().unwrap_or_else(|e| e.into_inner()).1 += 1;
        self.0.turn.notify_all();
    }
}

pub enum LandOutcome {
    Landed(Landed),
    NotLanded(NotLanded),
}

/// A save point: the state of an open task, named while it works. Kept with the task (a
/// `task.saved` event), never landed: `restore` takes the task back to it, or forward.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Save {
    /// Its number in the task: 1, 2, 3...
    pub n: i64,
    pub message: String,
    /// The sequence number of the task's last write then.
    pub seq: i64,
    /// The position the task stood on, that position's tree, and the task's tree on it.
    pub position: i64,
    pub base: Hash,
    pub tree: Hash,
    /// How many files the task changed then, and those of them holding conflict markers.
    pub files: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    pub at: i64,
}

/// What going back (or forward) to a save point did.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Restored {
    pub save: Save,
    /// The files that changed: as the save had them, or back to the base.
    pub files: Vec<String>,
    /// The position the task stands on now, when it was replayed on the line since the save: the
    /// save's changes are merged with what the line changed meanwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_on: Option<i64>,
    /// Files the save and the line changed on the same lines: markers to settle.
    pub conflicts: Vec<String>,
}

/// A file of `avp diff`: its diff, and whether it is too large to show one (its patch is empty then,
/// as an empty file's is).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Diffed {
    #[serde(flatten)]
    pub file: FileDiff,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub large: bool,
}

/// What `avp pick` brought into a task.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Picked {
    pub position: i64,
    /// The position's goal.
    pub goal: String,
    /// Files that came as the position has them: the task held them as the position's parent did.
    pub files: Vec<String>,
    /// Files the position's change was merged into, what changed there since kept.
    pub merged: Vec<String>,
    /// Files with conflict markers to settle: the position and the task changed the same lines.
    pub conflicts: Vec<String>,
    /// Files left as the task has them, and why: deleted on one side, binary, held by another task.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kept: BTreeMap<String, String>,
}

/// `avp saves` and the save points `avp status` lists.
pub fn saves_text(saves: &[Save]) -> String {
    if saves.is_empty() {
        return "no save point in this task: `avp save \"<message>\"` names its state now".into();
    }
    let mut out: Vec<String> = saves.iter().map(|s| format!("{:>3}  write {:<4} {} files  {}  {}", s.n, s.seq, s.files, crate::blame::utc(s.at), s.message)).collect();
    out.push("`avp undo --to <n>` takes the task back, or forward, to one".into());
    out.join("\n")
}

/// What a land answer says of files left with markers. Both sides are work to keep: a small model
/// told only "settle them" deletes one side with sed and loses a teammate's change (29/10 bench).
pub fn keep_both(files: &[String]) -> String {
    format!("in {}, both sides of every conflict are teammates' work to keep: never delete a side. `<<<<<<< ours` is this task's change, `>>>>>>> theirs` what landed on the line meanwhile, `||||||| original` what both started from: write one version holding both changes, without the markers, then land again", files.join(", "))
}

impl Engine {
    pub fn open(data: &Path) -> Result<Self> {
        std::fs::create_dir_all(data)?;
        // trame.sqlite: a server started before AVP was named AVP keeps its database.
        let db = if !data.join("avp.sqlite").exists() && data.join("trame.sqlite").exists() { data.join("trame.sqlite") } else { data.join("avp.sqlite") };
        let engine = Self { db: Arc::new(Mutex::new(Db::open(&db)?)), objects: Objects::open(&data.join("objects"))?, data: data.to_path_buf(), task_locks: Arc::default(), cloud: Arc::default(), land_queues: Arc::default(), waits: Arc::default(), landing: Arc::default() };
        crate::seam::opened(&engine);
        Ok(engine)
    }

    pub(crate) fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- repos and the line ----

    pub fn create_repo(&self, name: &str, policy: LeasePolicy) -> Result<Repo> {
        let db = self.db();
        let repo = db.create_repo(name, policy)?;
        db.emit(&repo.id, "repo.created", None, None, serde_json::json!({ "name": name }))?;
        Ok(repo)
    }

    /// Position 0 (or the next one, with a note) from a list of entries whose blobs are stored.
    pub fn import(&self, repo: &str, entries: &[ImportEntry], note: &str) -> Result<LineEntry> {
        self.import_git(repo, entries, note, None)
    }

    /// An import whose tree is a git commit: the position remembers the commit.
    pub fn import_git(&self, repo: &str, entries: &[ImportEntry], note: &str, git: Option<&str>) -> Result<LineEntry> {
        let mut tree = Tree::new();
        for e in entries {
            anyhow::ensure!(safe_path(&e.path), "bad path: {:?}", e.path);
            anyhow::ensure!(self.objects.has(&e.blob), "blob {} missing for {}", e.blob, e.path);
            tree.insert(e.path.clone(), Entry { blob: e.blob.clone(), mode: e.mode });
        }
        let hash = self.objects.put_tree(&tree)?;
        let db = self.db();
        let head = db.head(repo)?.map(|h| h.position).unwrap_or(-1);
        let mut entry = db.append_line(repo, head, None, &hash, None, note)?;
        if let Some(sha) = git {
            db.set_line_git(repo, entry.position, sha)?;
            entry.git = Some(sha.to_string());
        }
        db.emit(repo, "line.imported", None, None, serde_json::json!({ "position": entry.position, "files": tree.len(), "git": git }))?;
        Ok(entry)
    }

    /// The export wrote this position as a git commit.
    pub fn set_git(&self, repo: &str, position: i64, sha: &str) -> Result<LineEntry> {
        let db = self.db();
        db.set_line_git(repo, position, sha)?;
        db.emit(repo, "line.exported", None, None, serde_json::json!({ "position": position, "git": sha }))?;
        db.line_at(repo, position)
    }

    /// The mirror says how its push went: `mirror.blocked` when the branch holds a commit AVP did not
    /// write, `mirror.pushed` once a push goes through. A follow loop repeats itself every few
    /// seconds, so an event that says what the last one said is not recorded again.
    pub fn mirror_event(&self, repo: &str, blocked: bool, branch: &str, commit: Option<&str>, position: i64) -> Result<()> {
        let db = self.db();
        let kind = if blocked { "mirror.blocked" } else { "mirror.pushed" };
        let last = db.events_since(repo, 0, i64::MAX)?.into_iter().rev().find(|e| e.kind.starts_with("mirror.") && e.data["branch"] == branch);
        if let Some(l) = last {
            if l.kind == kind && l.data["commit"].as_str() == commit && (blocked || l.data["position"] == position) {
                return Ok(());
            }
        }
        db.emit(repo, kind, None, None, serde_json::json!({ "branch": branch, "commit": commit, "position": position }))
    }

    /// The last position at or below `at` that has a git commit.
    pub fn last_git(&self, repo: &str, at: i64) -> Result<Option<LineEntry>> {
        self.db().last_git(repo, at)
    }

    pub fn head(&self, repo: &str) -> Result<LineEntry> {
        self.db().head(repo)?.ok_or_else(|| anyhow!("repo {repo} has no line yet: import first"))
    }

    pub fn tree_at(&self, repo: &str, position: i64) -> Result<Tree> {
        let entry = self.db().line_at(repo, position)?;
        self.objects.get_tree(&entry.tree)
    }

    // ---- tasks ----

    pub fn open_task(&self, repo: &str, req: OpenTask) -> Result<Task> {
        self.open_task_at(repo, req, None)
    }

    /// A task on `base`, any position of the line (none: the head): where a view that worked
    /// offline stood, so its writes go as they were made. Its land replays it on the head, as for
    /// any task the line moved under.
    pub fn open_task_at(&self, repo: &str, req: OpenTask, base: Option<i64>) -> Result<Task> {
        let at = match base {
            Some(p) => self.db().line_at(repo, p).map_err(|_| anyhow!("no position {p} on the line of {repo}: a task opens on a position the line holds (none: the head)"))?,
            None => self.head(repo)?,
        };
        let similar: Vec<String> = self
            .db()
            .tasks(repo, Some(TaskState::Open))?
            .into_iter()
            .filter(|t| goal_likeness(&t.goal, &req.goal) >= 0.5)
            .map(|t| format!("{} ({} via {}): {}", t.id, t.actor.person, t.actor.agent, t.goal))
            .collect();
        let task = Task {
            id: new_id("task"),
            repo: repo.to_string(),
            goal: req.goal,
            actor: req.actor,
            scope: req.scope.clone(),
            base_position: at.position,
            base_tree: at.tree.clone(),
            state: TaskState::Open,
            created_at: now_ms(),
            landed_position: None,
            transcript: req.transcript,
            similar,
            on: None,
        };
        let db = self.db();
        db.insert_task(&task)?;
        for p in &req.scope {
            db.set_lease(repo, p, &task.id, LEASE_TTL_MS)?;
        }
        db.emit(repo, "task.opened", Some(&task.id), None, serde_json::json!({ "goal": task.goal, "scope": task.scope, "actor": task.actor, "similar": task.similar }))?;
        Ok(task)
    }

    /// A task stacked on another (`avp open --on`, `on` its id or the start of one): it builds on
    /// that task's tree as it is now, the work not landed yet, and lands after it. Long work comes in
    /// as a stack of tasks, each proven, instead of a branch. On a task that landed already, it
    /// opens on the head, which holds that work.
    pub fn open_stacked(&self, repo: &str, req: OpenTask, on: &str) -> Result<Task> {
        let parent = {
            let db = self.db();
            let all = db.tasks(repo, None)?;
            let found: Vec<&Task> = all.iter().filter(|t| t.id == on || (on.len() >= 6 && t.id.starts_with(on))).collect();
            match found.as_slice() {
                [t] => (*t).clone(),
                [] => anyhow::bail!("no task {on} on this repo: `avp tasks` lists the open ones"),
                _ => anyhow::bail!("{on} starts {} tasks: give more of the id", found.len()),
            }
        };
        anyhow::ensure!(parent.state != TaskState::Abandoned, "task {} was abandoned: there is nothing of it to build on. Open on the head (no --on)", parent.id);
        let stacked = parent.state == TaskState::Open;
        let mut task = self.open_task_at(repo, req, stacked.then_some(parent.base_position))?;
        if stacked {
            let writes = self.db().writes(&parent.id)?;
            task.base_tree = self.objects.put_tree(&self.candidate(&parent, &writes)?)?;
        }
        let db = self.db();
        db.stack_task(&task.id, &parent.id, &task.base_tree)?;
        db.emit(repo, "task.stacked", Some(&task.id), None, serde_json::json!({ "on": parent.id, "goal": task.goal, "parent_goal": parent.goal, "landed": parent.landed_position }))?;
        task.on = Some(parent.id);
        Ok(task)
    }

    /// The tasks a task is stacked on that have not landed, the one right under it first.
    fn open_below(db: &Db, task: &Task) -> Result<Vec<Task>> {
        let mut below = Vec::new();
        let mut on = task.on.clone();
        while let Some(p) = on.as_deref().map(|p| db.task(p)).transpose()?.filter(|p| p.state == TaskState::Open) {
            on = p.on.clone();
            below.push(p);
        }
        Ok(below)
    }

    /// The open tasks of a task's stack but itself: those under it and those stacked on it, at any
    /// depth. What one of them holds is the stack's: a write of another takes nothing from it.
    fn stack_mates(db: &Db, task: &Task) -> Result<BTreeSet<String>> {
        let mut mates: BTreeSet<String> = Self::open_below(db, task)?.into_iter().map(|t| t.id).collect();
        let mut up = vec![task.id.clone()];
        while let Some(id) = up.pop() {
            for t in db.stacked_on(&id)? {
                if mates.insert(t.id.clone()) {
                    up.push(t.id);
                }
            }
        }
        Ok(mates)
    }

    /// Where a stacked task's parent stands, as its agent reads it, with what to do next: not landed
    /// yet, or gone (abandoned, or landed and reverted since). None for a task stacked on nothing, or
    /// on a task that landed and stays on the line: it is a task like another now.
    pub fn stack_word(&self, task: &Task) -> Result<Option<String>> {
        let Some(on) = task.on.as_deref() else { return Ok(None) };
        let db = self.db();
        let p = db.task(on)?;
        let of = format!("task {} ({})", p.id, p.goal.chars().take(80).collect::<String>());
        let gone = "this task stays open and lands its own changes without them: `avp refresh` lays that out here, and what needed them goes red until they come back";
        Ok(match (p.state, p.landed_position) {
            (TaskState::Open, _) => Some(format!("stacked on {of}, which has not landed: this task lands after it (`avp land --stack` lands both, bottom first); `avp refresh` brings its latest changes here")),
            (TaskState::Abandoned, _) => Some(format!("{of}, which this task was stacked on, was abandoned: {gone}")),
            (TaskState::Landed, Some(at)) => {
                // Reverted by AVP (its full suite), or by a person's `avp revert`: both land as
                // "Revert position <n>: ..." (the newest 1000 positions are read).
                let head = db.head(&task.repo)?.map(|h| h.position).unwrap_or(at);
                let by = db.line(&task.repo, head, (head - at).clamp(0, 1000))?.into_iter().filter(|e| e.position > at).find(|e| e.note.starts_with(&format!("Revert position {at}:"))).map(|e| e.position);
                let by = by.or(db.line_at(&task.repo, at)?.reverted.map(|r| r.by));
                by.map(|r| format!("{of}, which this task was stacked on, landed at position {at} and was reverted at position {r}: {gone}"))
            }
            (TaskState::Landed, None) => None,
        })
    }

    /// Tells the open tasks stacked on `parent` what became of it (`task.parent_landed`,
    /// `task.parent_abandoned`, `task.parent_reverted`): an event of their own, read at their next turn.
    fn tell_stacked(db: &Db, repo: &str, parent: &str, kind: &str, mut data: serde_json::Value) -> Result<()> {
        data["parent"] = parent.into();
        for t in db.stacked_on(parent)? {
            db.emit(repo, kind, Some(&t.id), None, data.clone())?;
        }
        Ok(())
    }

    pub fn status(&self, task_id: &str) -> Result<TaskStatus> {
        let db = self.db();
        let task = db.task(task_id)?;
        let head = db.head(&task.repo)?.map(|h| h.position).unwrap_or(-1);
        let with = Self::landed_with(&db, &task)?;
        let status = TaskStatus { writes: db.writes(task_id)?, leases: db.leases_of(task_id)?, last_proof: db.last_proof(task_id)?, approval: db.approval(task_id)?, task, head_position: head, stack: None, with };
        drop(db);
        let stack = if status.task.state == TaskState::Open { self.stack_word(&status.task)? } else { None };
        Ok(TaskStatus { stack, ..status })
    }

    /// What a task changes, file by file, against the position it stands on: what a person reads
    /// before approving it.
    pub fn diff(&self, task_id: &str) -> Result<Vec<FileDiff>> {
        let (task, writes) = {
            let db = self.db();
            (db.task(task_id)?, db.writes(task_id)?)
        };
        let base = self.objects.get_tree(&task.base_tree)?;
        let candidate = self.candidate(&task, &writes)?;
        let mut out = Vec::new();
        for path in Self::effective_writes(&writes).keys() {
            out.extend(self.file_diff(path, &base, &candidate, 3)?);
        }
        Ok(out)
    }

    /// What changes from one tree to the other, file by file (`avp diff`): `from` and `to` are each
    /// a position, `head`, or with a task, `base` (the position it stands on) and `task` (its tree:
    /// base plus its writes). Only the `paths` named, when some are (a folder: what is under it),
    /// with `context` lines around each change.
    pub fn diff_trees(&self, repo: &str, task: Option<&str>, from: &str, to: &str, paths: &[String], context: usize) -> Result<Vec<Diffed>> {
        let task = task.map(|t| self.db().task(t)).transpose()?;
        let (before, after) = (self.tree_named(repo, task.as_ref(), from)?, self.tree_named(repo, task.as_ref(), to)?);
        let wanted = |p: &str| {
            paths.is_empty() || paths.iter().map(|w| w.trim_end_matches('/')).any(|w| w.is_empty() || w == "." || p == w || p.starts_with(&format!("{w}/")))
        };
        let blob = |t: &Tree, p: &str| t.get(p).map(|e| e.blob.clone());
        let changed: BTreeSet<&String> = before.keys().chain(after.keys()).filter(|p| wanted(p) && blob(&before, p) != blob(&after, p)).collect();
        let empty = crate::objects::hash_bytes(b"");
        let mut out = Vec::new();
        for path in changed {
            if let Some(file) = self.file_diff(path, &before, &after, context)? {
                let large = !file.binary && file.patch.is_empty() && [before.get(path), after.get(path)].into_iter().flatten().any(|e| e.blob != empty);
                out.push(Diffed { file, large });
            }
        }
        Ok(out)
    }

    fn tree_named(&self, repo: &str, task: Option<&Task>, name: &str) -> Result<Tree> {
        match (name, task) {
            ("head", _) => self.objects.get_tree(&self.head(repo)?.tree),
            ("base", Some(t)) => self.objects.get_tree(&t.base_tree),
            ("task", Some(t)) => {
                let writes = self.db().writes(&t.id)?;
                self.candidate(t, &writes)
            }
            (n, _) => match n.parse::<i64>() {
                Ok(p) => self.tree_at(repo, p),
                Err(_) => anyhow::bail!("{n:?} is not a position: a number, `head`, or with a task `base` and `task`"),
            },
        }
    }

    /// What a position changed, file by file, against the position before it: what AVP Live shows
    /// when a person clicks a node of the line.
    pub fn position_diff(&self, repo: &str, position: i64) -> Result<Vec<FileDiff>> {
        anyhow::ensure!(position > 0, "position 0 is where the line starts: it changed nothing");
        // The position first: a position past the head is the one named as missing.
        let after = self.tree_at(repo, position)?;
        let before = self.tree_at(repo, position - 1)?;
        // Same blob, same bytes: only the paths whose blob moved are read.
        let blob = |t: &Tree, p: &str| t.get(p).map(|e| e.blob.clone());
        let paths: BTreeSet<&String> = before.keys().chain(after.keys()).filter(|p| blob(&before, p) != blob(&after, p)).collect();
        let mut out = Vec::new();
        for path in paths {
            out.extend(self.file_diff(path, &before, &after, 3)?);
        }
        Ok(out)
    }

    /// One file from one tree to the other as a unified diff with `context` lines around each
    /// change, or None when its bytes are the same.
    fn file_diff(&self, path: &str, before: &Tree, after: &Tree, context: usize) -> Result<Option<FileDiff>> {
        const LARGE: usize = 256 * 1024;
        let before = before.get(path).map(|e| self.objects.get(&e.blob)).transpose()?;
        let after = after.get(path).map(|e| self.objects.get(&e.blob)).transpose()?;
        if before == after {
            return Ok(None);
        }
        let change = match (&before, &after) {
            (None, _) => "added",
            (_, None) => "deleted",
            _ => "modified",
        };
        let (b, a) = (before.unwrap_or_default(), after.unwrap_or_default());
        let text = (std::str::from_utf8(&b), std::str::from_utf8(&a));
        let (patch, binary) = match text {
            (Ok(b), Ok(a)) if b.len() + a.len() <= LARGE => {
                let p = diffy::DiffOptions::new().set_context_len(context).create_patch(b, a).to_string();
                // The header names "original" and "modified": the path says it better.
                (p.lines().skip_while(|l| l.starts_with("---") || l.starts_with("+++")).collect::<Vec<_>>().join("\n"), false)
            }
            (Ok(_), Ok(_)) => (String::new(), false),
            _ => (String::new(), true),
        };
        Ok(Some(FileDiff { path: path.to_string(), change: change.into(), patch, binary }))
    }

    /// Who holds a path right now: the lease, or an open task's live write on that path.
    fn holder(&self, db: &Db, repo: &str, path: &str, except: &str) -> Result<Option<String>> {
        // The path itself, then each directory above it: a lease on `src` covers `src/a.txt`.
        let mut candidates = vec![path.to_string()];
        let mut p = path;
        while let Some(i) = p.rfind('/') {
            p = &p[..i];
            candidates.push(p.to_string());
        }
        for c in candidates {
            if let Some(l) = db.lease(repo, &c)? {
                if l.task != except && l.expires_at > now_ms() {
                    return Ok(Some(l.task));
                }
            }
        }
        Ok(None)
    }

    /// The prefix a lease covers: a lease on `apps/web/src` covers `apps/web/src/a.ts`.
    fn covered(leases: &[Lease], path: &str) -> bool {
        leases.iter().any(|l| l.path == path || path.starts_with(&format!("{}/", l.path.trim_end_matches('/'))))
    }

    pub fn write(&self, task_id: &str, req: PutWrite) -> Result<WriteOutcome> {
        anyhow::ensure!(safe_path(&req.path), "bad path: {:?}", req.path);
        let db = self.db();
        let task = db.task(task_id)?;
        anyhow::ensure!(task.state == TaskState::Open, "task is {:?}", task.state);
        let repo = db.repo(&task.repo)?;
        if let Some(b) = &req.blob {
            anyhow::ensure!(self.objects.has(b), "blob {b} not uploaded");
        }
        // A put that leaves the path as the candidate already has it (same bytes, same mode) is not
        // a write. A mode change alone is one: `chmod +x` must reach the line.
        if req.kind == WriteKind::Put {
            let writes = db.writes(task_id)?;
            let current = match Self::effective_writes(&writes).get(&req.path) {
                Some(w) if w.kind != WriteKind::Delete => w.blob.clone().map(|b| (b, w.mode)),
                Some(_) => None,
                None => self.objects.get_tree(&task.base_tree)?.get(&req.path).map(|e| (e.blob.clone(), e.mode)),
            };
            if current.is_some() && current == req.blob.clone().map(|b| (b, req.mode)) {
                return Ok(WriteOutcome {
                    write: Write { task: task_id.into(), seq: 0, path: req.path, kind: req.kind, blob: req.blob, mode: req.mode, at: now_ms(), undone: true },
                    lease: "unchanged".into(),
                    previous_holder: None,
                });
            }
        }
        let mine = db.leases_of(task_id)?;
        let held = if Self::covered(&mine, &req.path) { None } else { self.holder(&db, &task.repo, &req.path, task_id)? };
        // A path its stack holds is the stack's: the task under it (or over it) keeps the lease.
        let mate = match &held {
            Some(h) => Self::stack_mates(&db, &task)?.contains(h),
            None => false,
        };
        let previous = held.filter(|_| !mate);
        let lease = match (&previous, repo.policy) {
            (Some(holder), LeasePolicy::Strict) => {
                return Ok(WriteOutcome {
                    write: Write { task: task_id.into(), seq: 0, path: req.path, kind: req.kind, blob: req.blob, mode: req.mode, at: now_ms(), undone: true },
                    lease: "refused".into(),
                    previous_holder: Some(holder.clone()),
                });
            }
            (Some(_), LeasePolicy::Advisory) => "taken_over",
            (None, _) if mate || Self::covered(&mine, &req.path) => "held",
            (None, _) => "taken",
        };
        if lease != "held" {
            db.set_lease(&task.repo, &req.path, task_id, LEASE_TTL_MS)?;
        } else {
            for l in &mine {
                db.set_lease(&task.repo, &l.path, task_id, LEASE_TTL_MS)?;
            }
        }
        let write = db.append_write(task_id, &req.path, req.kind, req.blob.as_deref(), req.mode)?;
        db.emit(&task.repo, "write", Some(task_id), Some(&req.path), serde_json::json!({ "kind": req.kind, "seq": write.seq, "lease": lease, "previous_holder": previous }))?;
        if let Some(h) = &previous {
            db.emit(&task.repo, "lease.taken_over", Some(h), Some(&req.path), serde_json::json!({ "by": task_id }))?;
        }
        Ok(WriteOutcome { write, lease: lease.into(), previous_holder: previous })
    }

    pub fn claim(&self, task_id: &str, paths: &[String]) -> Result<Vec<(String, Option<String>)>> {
        let db = self.db();
        let task = db.task(task_id)?;
        let repo = db.repo(&task.repo)?;
        let mut out = Vec::new();
        for p in paths {
            let holder = self.holder(&db, &task.repo, p, task_id)?;
            if holder.is_some() && repo.policy == LeasePolicy::Strict {
                out.push((p.clone(), holder));
                continue;
            }
            db.set_lease(&task.repo, p, task_id, LEASE_TTL_MS)?;
            db.emit(&task.repo, "lease.taken", Some(task_id), Some(p), serde_json::json!({ "previous_holder": holder }))?;
            out.push((p.clone(), holder));
        }
        Ok(out)
    }

    pub fn release(&self, task_id: &str, paths: Option<&[String]>) -> Result<usize> {
        let db = self.db();
        let task = db.task(task_id)?;
        let n = db.release_leases(task_id, paths)?;
        db.emit(&task.repo, "lease.released", Some(task_id), None, serde_json::json!({ "paths": paths, "count": n }))?;
        Ok(n)
    }

    /// `path` given: that path goes back to the base. Otherwise every write after `to_seq`.
    pub fn undo(&self, task_id: &str, to_seq: i64, path: Option<&str>) -> Result<usize> {
        let db = self.db();
        let task = db.task(task_id)?;
        let n = match path {
            Some(p) => db.undo_path(task_id, p)?,
            None => db.undo_after(task_id, to_seq)?,
        };
        db.emit(&task.repo, "task.undo", Some(task_id), path, serde_json::json!({ "to": to_seq, "path": path, "undone": n }))?;
        Ok(n)
    }

    /// Names the task's state now: the tree its writes make on its base, kept with the task as a
    /// `task.saved` event. Nothing lands.
    pub fn save(&self, task_id: &str, message: &str) -> Result<Save> {
        let message = message.trim();
        anyhow::ensure!(!message.is_empty(), "a save point says what state it names: `avp save \"tests green\"`");
        let db = self.db();
        let task = db.task(task_id)?;
        anyhow::ensure!(task.state == TaskState::Open, "task is {:?}: save points are an open task's", task.state);
        let writes = db.writes(task_id)?;
        let live = Self::effective_writes(&writes);
        let save = Save {
            n: Self::saves_of(&db, &task)?.len() as i64 + 1,
            message: message.to_string(),
            seq: writes.iter().map(|w| w.seq).max().unwrap_or(0),
            position: task.base_position,
            base: task.base_tree.clone(),
            tree: self.objects.put_tree(&self.candidate(&task, &writes)?)?,
            files: live.len(),
            conflicts: live.values().filter(|w| w.kind == WriteKind::Conflict).map(|w| w.path.clone()).collect(),
            at: now_ms(),
        };
        db.emit(&task.repo, "task.saved", Some(task_id), None, serde_json::to_value(&save)?)?;
        Ok(save)
    }

    /// The task's save points, oldest first.
    pub fn saves(&self, task_id: &str) -> Result<Vec<Save>> {
        let db = self.db();
        let task = db.task(task_id)?;
        Self::saves_of(&db, &task)
    }

    fn saves_of(db: &Db, task: &Task) -> Result<Vec<Save>> {
        let saved = db.events_of(&task.repo, &["task.saved"], task.created_at)?;
        Ok(saved.into_iter().filter(|e| e.task.as_deref() == Some(task.id.as_str())).filter_map(|e| serde_json::from_value(e.data).ok()).collect())
    }

    /// Takes the task to a save point (its number, or its message): each file as the save had it,
    /// written again, or back to the base where the save had not changed it. Writes are added, never
    /// taken out, so every save stays reachable, later ones too. When the task was replayed on the
    /// line since the save, the save is replayed the same way: its changes merged with the line's,
    /// markers where both changed the same lines.
    pub fn restore(&self, task_id: &str, to: &str) -> Result<Restored> {
        let db = self.db();
        let task = db.task(task_id)?;
        anyhow::ensure!(task.state == TaskState::Open, "task is {:?}", task.state);
        let saves = Self::saves_of(&db, &task)?;
        let to = to.trim();
        let found = match to.parse::<i64>() {
            Ok(n) => saves.iter().find(|s| s.n == n),
            Err(_) => saves.iter().rev().find(|s| s.message == to),
        };
        let save = found.cloned().ok_or_else(|| match saves.is_empty() {
            true => anyhow!("no save point {to:?}: this task has none yet, `avp save \"<message>\"` names its state"),
            false => anyhow!("no save point {to:?} in this task: {}", saves.iter().map(|s| format!("{} ({})", s.n, s.message)).collect::<Vec<_>>().join(", ")),
        })?;
        let writes = db.writes(task_id)?;
        let live = Self::effective_writes(&writes);
        let (then, saved, now) = (self.objects.get_tree(&save.base)?, self.objects.get_tree(&save.tree)?, self.objects.get_tree(&task.base_tree)?);
        let paths: BTreeSet<&String> = then.keys().chain(saved.keys()).chain(now.keys()).chain(live.keys()).collect();
        let (mut files, mut conflicts) = (Vec::new(), Vec::new());
        for path in paths {
            let (was, kept, base) = (then.get(path), saved.get(path), now.get(path));
            // The file as the save has it, on the base the task stands on now.
            let (want, conflict) = if kept == was {
                (base.cloned(), false)
            } else if base == was {
                (kept.cloned(), save.conflicts.contains(path))
            } else {
                self.merge_entries(was, kept, base)?
            };
            let have = live.get(path);
            if want.as_ref() == base && !conflict {
                // As the base has it: no write at all, rather than one of the base's bytes.
                if have.is_some() {
                    db.undo_path(task_id, path)?;
                    files.push(path.clone());
                }
                continue;
            }
            let kind = match (&want, conflict) {
                (None, _) => WriteKind::Delete,
                (Some(_), true) => WriteKind::Conflict,
                (Some(_), false) => WriteKind::Put,
            };
            let (blob, mode) = (want.as_ref().map(|e| e.blob.clone()), want.as_ref().map_or(0o100644, |e| e.mode));
            if !have.is_some_and(|w| w.kind == kind && w.blob == blob && (blob.is_none() || w.mode == mode)) {
                db.append_write(task_id, path, kind, blob.as_deref(), mode)?;
                files.push(path.clone());
            }
            if conflict {
                conflicts.push(path.clone());
            }
        }
        db.emit(&task.repo, "task.restored", Some(task_id), None, serde_json::json!({ "save": save.n, "message": save.message, "files": files, "conflicts": conflicts }))?;
        let replayed_on = (save.base != task.base_tree).then_some(task.base_position);
        Ok(Restored { save, files, replayed_on, conflicts })
    }

    /// A file a task and the line both changed from `was`: merged three ways, as a replay merges
    /// it, and whether it is in conflict. Deleted on one side, or binary: the task's version, in
    /// conflict.
    fn merge_entries(&self, was: Option<&Entry>, mine: Option<&Entry>, theirs: Option<&Entry>) -> Result<(Option<Entry>, bool)> {
        let (Some(m), Some(t)) = (mine, theirs) else { return Ok((mine.cloned(), true)) };
        let ancestor = was.map(|e| self.objects.get(&e.blob)).transpose()?.unwrap_or_default();
        let (mb, tb) = (self.objects.get(&m.blob)?, self.objects.get(&t.blob)?);
        let (Ok(a), Ok(mt), Ok(tt)) = (std::str::from_utf8(&ancestor), std::str::from_utf8(&mb), std::str::from_utf8(&tb)) else { return Ok((Some(m.clone()), true)) };
        let (text, conflict) = match diffy::merge(a, mt, tt) {
            Ok(merged) => (merged, false),
            Err(with_markers) => match crate::merge::settle_additions(&with_markers) {
                Some(settled) => (settled, false),
                None => (with_markers, true),
            },
        };
        Ok((Some(Entry { blob: self.objects.put(text.as_bytes())?, mode: m.mode }), conflict))
    }

    pub fn abandon(&self, task_id: &str) -> Result<()> {
        let db = self.db();
        let task = db.task(task_id)?;
        // A landed task is on the line: abandoning it would hide that, `revert` takes it back.
        anyhow::ensure!(task.state != TaskState::Landed, "task landed at position {}: `avp revert {}` takes it back", task.landed_position.unwrap_or(-1), task.landed_position.unwrap_or(-1));
        db.set_task_state(task_id, TaskState::Abandoned, None)?;
        db.release_leases(task_id, None)?;
        db.emit(&task.repo, "task.abandoned", Some(task_id), None, serde_json::json!({}))?;
        // The tasks stacked on it stay open: their own work is still theirs to land.
        Self::tell_stacked(&db, &task.repo, task_id, "task.parent_abandoned", serde_json::json!({ "goal": task.goal }))?;
        Ok(())
    }

    /// A goal said better before the task lands: once landed, it is the line's record of the change.
    pub fn set_goal(&self, task_id: &str, goal: &str) -> Result<()> {
        let goal = goal.trim();
        anyhow::ensure!(!goal.is_empty(), "a goal says what the task does");
        let db = self.db();
        let task = db.task(task_id)?;
        anyhow::ensure!(task.state == TaskState::Open, "task is {:?}: a landed goal stays as it was", task.state);
        db.set_task_goal(task_id, goal)?;
        db.emit(&task.repo, "task.goal", Some(task_id), None, serde_json::json!({ "goal": goal }))?;
        Ok(())
    }

    /// The latest live write per path.
    pub fn effective_writes(writes: &[Write]) -> BTreeMap<String, &Write> {
        let mut m = BTreeMap::new();
        for w in writes.iter().filter(|w| !w.undone) {
            m.insert(w.path.clone(), w);
        }
        m
    }

    /// Base tree plus the task's live writes.
    pub fn candidate(&self, task: &Task, writes: &[Write]) -> Result<Tree> {
        let mut tree = self.objects.get_tree(&task.base_tree)?;
        for (path, w) in Self::effective_writes(writes) {
            match w.kind {
                WriteKind::Delete => {
                    tree.remove(&path);
                }
                WriteKind::Put | WriteKind::Conflict => {
                    tree.insert(path, Entry { blob: w.blob.clone().unwrap_or_default(), mode: w.mode });
                }
            }
        }
        Ok(tree)
    }

    // ---- proofs ----

    /// Runs a candidate's checks, or one part of them: where the seam says (AVP Cloud's workers),
    /// here otherwise, or when no worker answers in time.
    #[allow(clippy::too_many_arguments)]
    pub fn prove(&self, repo: &str, tree: &Tree, tree_hash: &str, name: &str, files: &[String], steps_override: Option<&str>, part: Option<&Part>, speculative: bool) -> Result<Vec<Step>> {
        let job = crate::worker::Job { id: new_id("job"), repo: repo.into(), tree: tree.clone(), tree_hash: tree_hash.into(), name: name.into(), files: files.to_vec(), steps_override: steps_override.map(String::from), part: part.cloned() };
        if let Some(steps) = crate::seam::prove_away(job, speculative)? {
            return Ok(steps);
        }
        let ws = Workspace::new(&self.data, repo);
        Ok(ws.check(&self.objects, tree, tree_hash, name, files, steps_override, part)?.0)
    }

    /// A task's proof. A step with `split`, when the task reaches more related tests than one job's
    /// share, runs as several jobs side by side, beside one job for the other steps; the first
    /// part to go red stops the others. The steps come back in avp.toml's order, a split step as
    /// one step whose output holds each share's.
    fn prove_task(&self, task: &Task, tree: &Tree, tree_hash: &str, files: &[String], speculative: bool) -> Result<Vec<Step>> {
        let cfg = read_config(tree, &self.objects)?.check;
        let line_secrets = crate::seam::line_secrets(self, task, tree)?;
        let most = std::env::var("AVP_SPLIT_MAX").ok().and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(8);
        let splits: Vec<(String, usize)> = if cfg.steps.iter().any(|s| s.split.is_some()) {
            let related = crate::check::related_tests(&self.objects, tree, files);
            cfg.steps.iter().filter_map(|s| s.split.map(|per| (s.name.clone(), related.div_ceil(per.max(1)).clamp(1, most)))).filter(|(_, n)| *n > 1).collect()
        } else {
            vec![]
        };
        if splits.is_empty() {
            let part = line_secrets.map(|s| Part { line_secrets: Some(s), ..Default::default() });
            return self.prove(&task.repo, tree, tree_hash, &task.id, files, None, part.as_ref(), speculative);
        }
        let mut parts = vec![Part { skip: splits.iter().map(|(n, _)| n.clone()).collect(), line_secrets: line_secrets.clone(), ..Default::default() }];
        for (name, n) in &splits {
            for i in 1..=*n {
                parts.push(Part { step: Some(name.clone()), shard: Some(format!("{i}/{n}")), line_secrets: line_secrets.clone(), ..Default::default() });
            }
        }
        let red = |steps: &[Step]| steps.iter().any(|s| s.exit != 0 && s.exit != crate::check::CANCELLED);
        let results: Vec<Vec<Step>> = std::thread::scope(|scope| {
            let running: Vec<_> = parts
                .iter()
                .map(|p| {
                    scope.spawn(move || {
                        let steps = self.prove(&task.repo, tree, tree_hash, &task.id, files, None, Some(p), speculative)?;
                        if red(&steps) {
                            crate::check::cancel_tree(&task.id, tree_hash);
                            crate::seam::cancel_tree(&task.id, tree_hash);
                        }
                        Ok::<_, anyhow::Error>(steps)
                    })
                })
                .collect();
            running.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("a part of the proof stopped")))).collect::<Result<Vec<_>>>()
        })?;
        let any_red = results.iter().any(|r| red(r));
        let mut out = Vec::new();
        for s in &cfg.steps {
            let Some((_, n)) = splits.iter().find(|(name, _)| *name == s.name) else {
                out.extend(results[0].iter().filter(|st| st.name == s.name).cloned());
                continue;
            };
            let shares: Vec<(&str, &Step)> = parts.iter().zip(&results).filter(|(p, _)| p.step.as_deref() == Some(s.name.as_str())).filter_map(|(p, r)| r.first().map(|st| (p.shard.as_deref().unwrap_or(""), st))).collect();
            if shares.is_empty() {
                continue;
            }
            let failed = shares.iter().find(|(_, st)| st.exit != 0 && st.exit != crate::check::CANCELLED);
            let all_stopped = shares.iter().all(|(_, st)| st.exit == crate::check::CANCELLED);
            let mut output = format!("{} jobs side by side ({n} shares of the related tests)\n", shares.len());
            // The failures first: they are what the agent reads.
            let mut shown: Vec<&(&str, &Step)> = shares.iter().filter(|(_, st)| st.exit != crate::check::CANCELLED).collect();
            shown.sort_by_key(|(_, st)| st.exit == 0);
            for (shard, st) in shown {
                output.push_str(&format!("\n== share {shard}, {:.0}s, exit {}\n{}\n", st.seconds, st.exit, st.output));
            }
            let merged = Step {
                name: s.name.clone(),
                command: shares[0].1.command.clone(),
                exit: failed.map(|(_, st)| st.exit).unwrap_or(if all_stopped { crate::check::CANCELLED } else { 0 }),
                seconds: shares.iter().map(|(_, st)| st.seconds).fold(0.0, f64::max),
                output,
            };
            // Its whole log and artifacts, its shares' together.
            crate::check::Kept::new(&self.data).merge(&merged, &shares);
            out.push(merged);
        }
        // What is no step of avp.toml: a worker that could not run its part, a part stopped.
        out.extend(results[0].iter().filter(|st| !cfg.steps.iter().any(|s| s.name == st.name)).cloned());
        // Stopped because another part went red: only the red says something.
        if any_red {
            out.retain(|s| s.exit != crate::check::CANCELLED);
        }
        Ok(out)
    }

    pub fn check(&self, task_id: &str) -> Result<Proof> {
        self.check_as(task_id, false)
    }

    /// Proves the task's current tree in the background while its agent is still busy (its own
    /// tests, its last read): when it lands, the proof is there, since landing reuses a green proof
    /// of the same tree. On workers too: a newer tree stops the older proof there. False when not
    /// started.
    pub fn speculate(&self, task_id: &str) -> bool {
        if std::env::var("AVP_SPECULATE").is_ok_and(|v| v == "0") {
            return false;
        }
        let (e, t) = (self.clone(), task_id.to_string());
        std::thread::spawn(move || {
            let _ = e.check_as(&t, true);
        });
        true
    }

    fn task_lock(&self, task_id: &str) -> Arc<Mutex<()>> {
        self.task_locks.lock().unwrap_or_else(|e| e.into_inner()).entry(task_id.to_string()).or_default().clone()
    }

    fn check_as(&self, task_id: &str, speculative: bool) -> Result<Proof> {
        let (task, writes) = {
            let db = self.db();
            (db.task(task_id)?, db.writes(task_id)?)
        };
        anyhow::ensure!(task.state == TaskState::Open, "task is {:?}", task.state);
        let tree = self.candidate(&task, &writes)?;
        let tree_hash = self.objects.put_tree(&tree)?;
        // A proof of an older tree of this task is worth nothing now: stop it, here or on a worker.
        crate::check::cancel_other(task_id, &tree_hash);
        crate::seam::cancel_other(task_id, &tree_hash);
        let lock = self.task_lock(task_id);
        let _one = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = self.db().last_proof(task_id)? {
            if p.verdict == Verdict::Green && p.candidate_tree == tree_hash {
                return Ok(p);
            }
        }
        let files: Vec<String> = Self::effective_writes(&writes).keys().cloned().collect();
        let conflicts: Vec<String> = Self::effective_writes(&writes).values().filter(|w| w.kind == WriteKind::Conflict).map(|w| w.path.clone()).collect();
        let steps = if conflicts.is_empty() {
            self.prove_task(&task, &tree, &tree_hash, &files, speculative)?
        } else if speculative {
            anyhow::bail!("unsettled conflicts: nothing to prove yet");
        } else {
            vec![Step { name: "conflicts".into(), command: String::new(), exit: 1, seconds: 0.0, output: format!("unsettled conflicts in:\n{}\n{}", conflicts.join("\n"), keep_both(&conflicts)) }]
        };
        anyhow::ensure!(!steps.iter().any(|s| s.exit == crate::check::CANCELLED), "cancelled: the task's files changed, a newer tree is being proven");
        let red = steps.iter().any(|s| s.exit != 0);
        let mut blamed = Vec::new();
        let mut waits = BTreeSet::new();
        if red {
            let db = self.db();
            let output = steps.iter().map(|s| s.output.as_str()).collect::<Vec<_>>().join("\n");
            for f in failed_files(&output, &tree) {
                let holder = self.holder(&db, &task.repo, &f, task_id)?;
                if let Some(h) = &holder {
                    if !files.contains(&f) {
                        waits.insert(h.clone());
                    }
                }
                blamed.push((f, holder));
            }
        }
        let verdict = if !red { Verdict::Green } else if !waits.is_empty() && blamed.iter().all(|(f, h)| h.is_some() && !files.contains(f)) { Verdict::Blocked } else { Verdict::Red };
        let proof = Proof { id: new_id("proof"), task: Some(task_id.into()), base_position: task.base_position, candidate_tree: tree_hash, files, steps, verdict, blamed, created_at: now_ms() };
        let db = self.db();
        db.insert_proof(&task.repo, &proof)?;
        // A speculative proof is the agent's business until it lands: no event for the others.
        if !speculative {
            db.emit(&task.repo, "proof", Some(task_id), None, serde_json::json!({ "proof": proof.id, "verdict": verdict, "waits_for": waits }))?;
        }
        Ok(proof)
    }

    // ---- landing ----

    /// Replays the task on the current head. Returns the paths left in conflict.
    fn replay(&self, task: &Task, writes: &[Write], head: &LineEntry) -> Result<Vec<String>> {
        self.replay_onto(task, writes, head.position, &head.tree, None)
    }

    /// What an open task should stand on now: the tree of the open task it is stacked on as that one
    /// is now (on the position under it), or else the head of the line.
    fn ground(&self, task: &Task) -> Result<(i64, Hash, Option<String>)> {
        let parent = task.on.as_deref().map(|p| self.db().task(p)).transpose()?.filter(|p| p.state == TaskState::Open);
        if let Some(p) = parent {
            let writes = self.db().writes(&p.id)?;
            return Ok((p.base_position, self.objects.put_tree(&self.candidate(&p, &writes)?)?, Some(p.id)));
        }
        let head = self.head(&task.repo)?;
        Ok((head.position, head.tree, None))
    }

    /// Replays the task on `tree`, the tree of `position` or, `on` a task it is stacked on, that
    /// task's tree on it.
    fn replay_onto(&self, task: &Task, writes: &[Write], position: i64, tree: &str, on: Option<&str>) -> Result<Vec<String>> {
        let base = self.objects.get_tree(&task.base_tree)?;
        let head_tree = self.objects.get_tree(tree)?;
        let mut conflicts = Vec::new();
        let mut settled: Vec<String> = Vec::new();
        let mut rewrites: Vec<PutWrite> = Vec::new();
        for (path, w) in Self::effective_writes(writes) {
            let in_base = base.get(&path);
            let in_head = head_tree.get(&path);
            if in_base == in_head {
                continue; // the line did not touch it: the task's version stands
            }
            match (w.kind, in_head) {
                (WriteKind::Delete, None) => continue,
                (WriteKind::Delete, Some(_)) => {
                    conflicts.push(path.clone());
                    continue;
                }
                (_, None) => {
                    conflicts.push(path.clone()); // the line deleted what the task changed
                    continue;
                }
                (_, Some(theirs)) => {
                    let mine = self.objects.get(w.blob.as_deref().unwrap_or(""))?;
                    let theirs_b = self.objects.get(&theirs.blob)?;
                    let ancestor = in_base.map(|e| self.objects.get(&e.blob)).transpose()?.unwrap_or_default();
                    let (Ok(a), Ok(m), Ok(t)) = (std::str::from_utf8(&ancestor), std::str::from_utf8(&mine), std::str::from_utf8(&theirs_b)) else {
                        conflicts.push(path.clone());
                        continue;
                    };
                    match diffy::merge(a, m, t) {
                        Ok(merged) => {
                            let blob = self.objects.put(merged.as_bytes())?;
                            rewrites.push(PutWrite { path: path.clone(), kind: WriteKind::Put, blob: Some(blob), mode: w.mode });
                        }
                        Err(with_markers) => match crate::merge::settle_additions(&with_markers) {
                            Some(merged) => {
                                let blob = self.objects.put(merged.as_bytes())?;
                                rewrites.push(PutWrite { path: path.clone(), kind: WriteKind::Put, blob: Some(blob), mode: w.mode });
                                settled.push(path.clone());
                            }
                            None => {
                                let blob = self.objects.put(with_markers.as_bytes())?;
                                rewrites.push(PutWrite { path: path.clone(), kind: WriteKind::Conflict, blob: Some(blob), mode: w.mode });
                                conflicts.push(path.clone());
                            }
                        },
                    }
                }
            }
        }
        let db = self.db();
        for r in rewrites {
            db.append_write(&task.id, &r.path, r.kind, r.blob.as_deref(), r.mode)?;
        }
        db.rebase_task(&task.id, position, tree)?;
        let mut data = serde_json::json!({ "from": task.base_position, "to": position, "conflicts": conflicts, "settled_additions": settled });
        if let Some(on) = on {
            data["on"] = on.into();
        }
        db.emit(&task.repo, "task.replayed", Some(&task.id), None, data)?;
        Ok(conflicts)
    }

    /// Moves an open task onto the head of the line without landing it: its writes replayed on what
    /// landed since, overlaps left as conflict markers, as a landing would. What an agent needs once
    /// the work it waited for is on the line, to build on it (25/10 hard flow bench: `avp refresh`
    /// only laid the task's old base out again, and an agent polled it for 20 minutes for code that
    /// had landed long before).
    /// A task stacked on an open task moves onto that task's tree as it is now, its latest changes
    /// with it; once that one landed, or was abandoned, onto the head.
    pub fn catch_up(&self, task_id: &str) -> Result<CaughtUp> {
        let (task, writes) = {
            let db = self.db();
            let t = db.task(task_id)?;
            anyhow::ensure!(t.state == TaskState::Open, "task is {:?}", t.state);
            (t.clone(), db.writes(task_id)?)
        };
        let (position, tree, on) = self.ground(&task)?;
        let stack = self.stack_word(&task)?;
        // The tree too: a task stacked on one abandoned since stands on that one's tree, on the head.
        if task.base_position == position && task.base_tree == tree {
            return Ok(CaughtUp { position, moved: false, conflicts: vec![], on, stack });
        }
        let conflicts = self.replay_onto(&task, &writes, position, &tree, on.as_deref())?;
        Ok(CaughtUp { position, moved: true, conflicts, on, stack })
    }

    /// Whether a green proof on an older base still holds: no file changed on the line since
    /// touches the task's files or their import neighbors.
    fn proof_still_holds(&self, task: &Task, proof: &Proof, old_base_tree: &str, head: &LineEntry) -> Result<bool> {
        if proof.base_position == head.position {
            return Ok(true);
        }
        let old = self.objects.get_tree(old_base_tree)?;
        let new = self.objects.get_tree(&head.tree)?;
        let changed: BTreeSet<String> = old.iter().filter(|(p, e)| new.get(*p) != Some(e)).map(|(p, _)| p.clone()).chain(new.keys().filter(|p| !old.contains_key(*p)).cloned()).collect();
        let mine: BTreeSet<String> = proof.files.iter().cloned().collect();
        if changed.iter().any(|c| mine.contains(c)) {
            return Ok(false);
        }
        let _ = task;
        Ok(import_neighbors(&self.objects, &new, &mine, &changed).is_empty())
    }

    /// Waits for the repo's landing turn: no other landing appends to its line until it is dropped.
    fn landing_turn(&self, repo: &str) -> Turn {
        let q = self.land_queues.lock().unwrap_or_else(|e| e.into_inner()).entry(repo.to_string()).or_default().clone();
        let mut t = q.tickets.lock().unwrap_or_else(|e| e.into_inner());
        let mine = t.0;
        t.0 += 1;
        while t.1 != mine {
            t = q.turn.wait(t).unwrap_or_else(|e| e.into_inner());
        }
        drop(t);
        Turn(q)
    }

    pub fn land(&self, task_id: &str) -> Result<LandOutcome> {
        // A landing of it with others that runs now goes first: it may land it.
        let _held = self.hold(&[task_id.to_string()]);
        self.land_in_turn(task_id, None)
    }

    /// Waits until no landing holds any of `tasks`, then holds them until dropped: two agents told
    /// the same cycle both land it together, and the second finds it landed.
    fn hold(&self, tasks: &[String]) -> Holding {
        let (held, freed) = &*self.landing;
        let mut h = held.lock().unwrap_or_else(|e| e.into_inner());
        while tasks.iter().any(|t| h.contains(t)) {
            h = freed.wait(h).unwrap_or_else(|e| e.into_inner());
        }
        h.extend(tasks.iter().cloned());
        Holding(self.landing.clone(), tasks.to_vec())
    }

    /// `turn`: the repo's landing turn, when the task already holds it.
    /// A landed task's landing, as its land answered: landing it again says where it is.
    fn already_landed(db: &Db, t: &Task) -> Result<Option<LandOutcome>> {
        let (TaskState::Landed, Some(position)) = (t.state, t.landed_position) else { return Ok(None) };
        let e = db.line_at(&t.repo, position)?;
        Ok(Some(LandOutcome::Landed(Landed { position, tree: e.tree, proof: e.proof.unwrap_or_default(), replayed: false, stack: vec![], with: Self::landed_with(db, t)? })))
    }

    /// The other tasks a landed task landed with, at its position (`avp land --with`).
    fn landed_with(db: &Db, t: &Task) -> Result<Vec<String>> {
        let (TaskState::Landed, Some(position)) = (t.state, t.landed_position) else { return Ok(vec![]) };
        Ok(db.landed_at(&t.repo, position)?.into_iter().map(|o| o.id).filter(|o| *o != t.id).collect())
    }

    /// Lands a stack (`avp land --stack`): the open tasks this one is stacked on, the bottom one
    /// first, then this one, each proven and landed on the head the one under it just made. What
    /// each task's folder has not sent stays there: what the server holds lands. The first that does
    /// not land stops the stack, and this task's answer says which one and why; what landed under it
    /// stays landed.
    pub fn land_stack(&self, task_id: &str) -> Result<LandOutcome> {
        let mut below = {
            let db = self.db();
            Self::open_below(&db, &db.task(task_id)?)?
        };
        below.reverse();
        let mut landed = Vec::new();
        for t in below {
            match self.land(&t.id)? {
                LandOutcome::Landed(l) => landed.push(StackLanded { task: t.id, position: l.position }),
                LandOutcome::NotLanded(n) => {
                    let why = match n.reason.as_str() {
                        "conflict" => format!("its replay on the line left markers in {}", n.conflicts.join(", ")),
                        "red" | "blocked" => format!("its proof is {} (`avp proof {}`)", n.reason, n.proof.as_ref().map(|p| p.id.as_str()).unwrap_or("")),
                        "approval" => "it waits for a person's approval".into(),
                        "refused" => format!("a person refused it: {}", n.word.as_ref().map(|w| w.reason.as_str()).unwrap_or("")),
                        "empty" => "it has no write: its agent abandons it, or writes".into(),
                        r => r.to_string(),
                    };
                    let note = format!("task {} under this one did not land: {why}. That is for its agent to settle; `avp wait --task {}` waits until it lands, then `avp land`", t.id, t.id);
                    return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: "stacked".into(), word: None, conflicts: vec![], proof: None, waits_for: vec![t.id], note: Some(note), stack: landed }));
                }
            }
        }
        Ok(match self.land(task_id)? {
            LandOutcome::Landed(l) => LandOutcome::Landed(Landed { stack: landed, ..l }),
            LandOutcome::NotLanded(n) => LandOutcome::NotLanded(NotLanded { stack: landed, ..n }),
        })
    }

    fn land_in_turn(&self, task_id: &str, turn: Option<Turn>) -> Result<LandOutcome> {
        let (task, writes, head) = {
            let db = self.db();
            let t = db.task(task_id)?;
            // Landed already (a person's approval landed it, or a land beside this one): say where.
            if let Some(landed) = Self::already_landed(&db, &t)? {
                return Ok(landed);
            }
            anyhow::ensure!(t.state == TaskState::Open, "task is {:?}", t.state);
            // Stacked on a task that has not landed: it lands after that one, never before.
            if let Some(p) = t.on.as_deref().map(|p| db.task(p)).transpose()?.filter(|p| p.state == TaskState::Open) {
                let note = format!("stacked on task {} ({}), which has not landed: `avp land --stack` lands it, then this one; or `avp wait --task {}`, then `avp land`", p.id, p.goal.chars().take(80).collect::<String>(), p.id);
                return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: "stacked".into(), conflicts: vec![], proof: None, waits_for: vec![p.id], word: None, note: Some(note), stack: vec![] }));
            }
            (t.clone(), db.writes(task_id)?, db.head(&t.repo)?.ok_or_else(|| anyhow!("no line"))?)
        };
        let old_base_tree = task.base_tree.clone();
        let mut replayed = false;
        // The tree too: a task stacked on one that was abandoned stands on that one's tree, on the
        // head, and goes back to the head's alone.
        if task.base_position != head.position || task.base_tree != head.tree {
            let conflicts = self.replay(&task, &writes, &head)?;
            replayed = true;
            if !conflicts.is_empty() {
                let note = Some(keep_both(&conflicts));
                return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: "conflict".into(), conflicts, proof: None, waits_for: vec![], word: None, note, stack: vec![] }));
            }
        }
        let task = self.db().task(task_id)?;
        let writes = self.db().writes(task_id)?;
        if Self::effective_writes(&writes).is_empty() {
            return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: "empty".into(), conflicts: vec![], proof: None, waits_for: vec![], word: None, note: None, stack: vec![] }));
        }
        let last = self.db().last_proof(task_id)?;
        let candidate = self.candidate(&task, &writes)?;
        let candidate_hash = self.objects.put_tree(&candidate)?;
        // Writes that change nothing on the base: another task already landed the same change.
        if candidate_hash == task.base_tree {
            return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: "already".into(), conflicts: vec![], proof: None, waits_for: vec![], word: None, note: None, stack: vec![] }));
        }
        let reusable = match &last {
            Some(p) if p.verdict == Verdict::Green && p.candidate_tree == candidate_hash => true,
            Some(p) if p.verdict == Verdict::Green && !replayed => false,
            // A stacked task's proof was made on the work under it, and that work may have gone
            // (abandoned, reverted) or changed: only the very same tree is worth it.
            Some(_) if task.on.is_some() => false,
            Some(p) if p.verdict == Verdict::Green => {
                // Same writes, new base: does the old proof still cover it?
                let old_files: BTreeSet<&String> = p.files.iter().collect();
                let eff = Self::effective_writes(&writes);
                let new_files: BTreeSet<&String> = eff.keys().collect();
                old_files == new_files && self.proof_still_holds(&task, p, &old_base_tree, &head)?
            }
            _ => false,
        };
        let proof = if reusable { last.unwrap() } else { self.check(task_id)? };
        // What holds a green proof back (a person's approval, in AVP Cloud), or a red one proven
        // without the secrets it names.
        if let Some(n) = crate::seam::landing_gate(self, &task, &writes, &candidate, &proof)? {
            return Ok(LandOutcome::NotLanded(n));
        }
        if proof.verdict != Verdict::Green {
            // Only a blocked proof waits for someone: a red one is this task's to fix, even when some
            // of the failing files are held elsewhere (25/10 bench: "waits for" on a red proof sent an
            // agent polling for 13 minutes instead of fixing the tests its change broke).
            let waits: Vec<String> = if proof.verdict == Verdict::Blocked { proof.blamed.iter().filter_map(|(_, h)| h.clone()).collect::<BTreeSet<_>>().into_iter().collect() } else { vec![] };
            return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: if proof.verdict == Verdict::Blocked { "blocked".into() } else { "red".into() }, conflicts: vec![], proof: Some(proof), waits_for: waits, word: None, note: None, stack: vec![] }));
        }
        let entry = {
            // Every landing appends in its repo's turn.
            let turn = match turn {
                Some(t) => t,
                None => self.landing_turn(&task.repo),
            };
            let db = self.db();
            let head_now = db.head(&task.repo)?.map(|h| h.position).unwrap_or(-1);
            if head_now != task.base_position {
                drop(db);
                // The line moved while the task proved. Going again side by side with the other
                // landings can lose every race when many land (25/10 bench: a green task proved 14
                // times over 108 s): it goes again holding its turn, where nothing lands before it,
                // so it replays and proves once more at most.
                return self.land_in_turn(task_id, Some(turn));
            }
            // The proof took minutes: the task may have been abandoned meanwhile, or its goal said better.
            let now = db.task(task_id)?;
            if let Some(landed) = Self::already_landed(&db, &now)? {
                return Ok(landed);
            }
            anyhow::ensure!(now.state == TaskState::Open, "task is {:?}", now.state);
            let entry = db.append_line(&task.repo, task.base_position, Some(task_id), &candidate_hash, Some(&proof.id), &now.goal)?;
            db.set_task_state(task_id, TaskState::Landed, Some(entry.position))?;
            db.release_leases(task_id, None)?;
            self.emit_told(&db, &task.repo, "task.landed", Some(task_id), serde_json::json!({ "position": entry.position, "goal": now.goal, "files": proof.files, "proof": proof.id }))?;
            Self::tell_stacked(&db, &task.repo, task_id, "task.parent_landed", serde_json::json!({ "position": entry.position }))?;
            // A revert (AVP's, or a person's `avp revert`) takes back the work stacked tasks built on.
            let reverted = now.goal.strip_prefix("Revert position ").and_then(|g| g.split(':').next()).and_then(|p| p.parse::<i64>().ok());
            if let Some(t) = reverted.and_then(|p| db.line_at(&task.repo, p).ok()).and_then(|e| e.task) {
                Self::tell_stacked(&db, &task.repo, &t, "task.parent_reverted", serde_json::json!({ "position": reverted, "by": entry.position }))?;
            }
            entry
        };
        crate::seam::after_landing(self, &task, entry.position, &candidate_hash);
        Ok(LandOutcome::Landed(Landed { position: entry.position, tree: candidate_hash, proof: proof.id, replayed, stack: vec![], with: vec![] }))
    }

    // ---- tasks that need each other ----

    /// An open task's `avp wait`, each time it looks: what it waits on (the tasks `tasks` names, ids
    /// or the start of one, and the open tasks holding or writing `paths`) is remembered a few
    /// seconds. The answer says what waiting will not bring: the tasks it waits on need it in turn,
    /// at any length (A waits on B, B on C, C on A), so none of them lands alone; or it landed
    /// meanwhile, with others. 29/10 bench: two agents whose tests needed each other's section each
    /// waited on the other, and nothing landed in 22 minutes.
    pub fn waiting(&self, task_id: &str, tasks: &[String], paths: &[String]) -> Result<WaitSeen> {
        let db = self.db();
        let task = db.task(task_id)?;
        if task.state == TaskState::Landed {
            return Ok(WaitSeen { landed: task.landed_position, with: Self::landed_with(&db, &task)?, ..Default::default() });
        }
        anyhow::ensure!(task.state == TaskState::Open, "task is {:?}", task.state);
        let open: BTreeSet<String> = db.tasks(&task.repo, Some(TaskState::Open))?.into_iter().map(|t| t.id).collect();
        // A start of an id names the first open task it starts, as the wait reads it.
        let mut on: BTreeSet<String> = tasks.iter().filter(|w| !w.is_empty()).filter_map(|w| open.iter().find(|t| **t != task.id && t.starts_with(w.as_str())).cloned()).collect();
        for p in paths {
            on.extend(self.holder(&db, &task.repo, p, task_id)?);
            on.extend(db.writers(&task.repo, p)?);
        }
        on.retain(|t| *t != task.id && open.contains(t));
        let waits: HashMap<String, BTreeSet<String>> = {
            let mut w = self.waits.lock().unwrap_or_else(|e| e.into_inner());
            let now = now_ms();
            w.retain(|_, x| x.until > now);
            w.insert(task.id.clone(), Waiting { on, until: now + WAIT_SEEN_MS });
            w.iter().filter(|(t, _)| open.contains(*t)).map(|(t, x)| (t.clone(), x.on.clone())).collect()
        };
        if self.landing.0.lock().unwrap_or_else(|e| e.into_inner()).contains(task_id) {
            return Ok(WaitSeen { landing: true, ..Default::default() });
        }
        let cycle = Self::cycle_from(&db, &task.id, &open, &waits)?;
        Ok(WaitSeen { cycle: cycle.iter().map(|(t, _)| t.clone()).collect(), needs: cycle.into_iter().map(|(_, n)| n).collect(), ..Default::default() })
    }

    /// The shortest cycle of needs through `start`, each task with how it needs the next, or none.
    /// A task needs what its running wait waits on and, while its last proof is not green, whoever
    /// holds a file that proof fails on.
    fn cycle_from(db: &Db, start: &str, open: &BTreeSet<String>, waits: &HashMap<String, BTreeSet<String>>) -> Result<Vec<(String, String)>> {
        let needs = |x: &str| -> Result<Vec<(String, String)>> {
            let mut out: Vec<(String, String)> = waits.get(x).into_iter().flatten().map(|y| (y.clone(), format!("{x} waits for {y}"))).collect();
            if let Some(p) = db.last_proof(x)?.filter(|p| p.verdict != Verdict::Green) {
                for (f, h) in p.blamed {
                    if let Some(h) = h.filter(|h| !out.iter().any(|(y, _)| y == h)) {
                        out.push((h.clone(), format!("{x} is red on {f}, which {h} holds")));
                    }
                }
            }
            out.retain(|(y, _)| y != x && open.contains(y));
            Ok(out)
        };
        // Breadth first: the shortest cycle is the fewest tasks to land together.
        let mut came: HashMap<String, (String, String)> = HashMap::new();
        let mut next = std::collections::VecDeque::from([start.to_string()]);
        while let Some(x) = next.pop_front() {
            for (y, how) in needs(&x)? {
                if y == start {
                    let mut cycle = vec![(x.clone(), how)];
                    let mut at = x.clone();
                    while at != start {
                        let (from, how) = came[&at].clone();
                        cycle.push((from.clone(), how));
                        at = from;
                    }
                    cycle.reverse();
                    return Ok(cycle);
                }
                if !came.contains_key(&y) {
                    came.insert(y.clone(), (x.clone(), how));
                    next.push_back(y);
                }
            }
        }
        Ok(vec![])
    }

    /// Lands this task together with other open tasks (`avp land --with`, `with` their ids or the
    /// start of one): tasks that need each other, red each alone. `--stack` was no way out: it lands
    /// the bottom task first, alone, red. Each task's change, and those of the open tasks it is
    /// stacked on, are merged on the head and proven as one candidate; green, they land as one
    /// position, each task landed at it. A conflict or a red: nothing lands, nothing of any task
    /// changes, and the answer says whose files are at fault.
    pub fn land_with(&self, task_id: &str, with: &[String]) -> Result<LandOutcome> {
        let ids = {
            let db = self.db();
            let lead = db.task(task_id)?;
            let all = db.tasks(&lead.repo, None)?;
            let mut asked = vec![lead];
            for w in with {
                let found: Vec<&Task> = all.iter().filter(|t| t.id == *w || (w.len() >= 6 && t.id.starts_with(w.as_str()))).collect();
                match found.as_slice() {
                    [t] => asked.push((*t).clone()),
                    [] => anyhow::bail!("no task {w} on this repo: `avp tasks` lists the open ones"),
                    _ => anyhow::bail!("{w} starts {} tasks: give more of the id", found.len()),
                }
            }
            // A task lands with its stack: the open tasks under it first, the bottom one first.
            let mut ids: Vec<String> = Vec::new();
            for t in asked {
                let mut below: Vec<String> = Self::open_below(&db, &t)?.into_iter().map(|b| b.id).collect();
                below.reverse();
                for id in below.into_iter().chain([t.id]) {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
            ids
        };
        let _held = self.hold(&ids);
        self.land_together(task_id, &ids, None)
    }

    /// `land_with` once its tasks are held. `turn`: the repo's landing turn, when it holds it already.
    fn land_together(&self, lead: &str, ids: &[String], turn: Option<Turn>) -> Result<LandOutcome> {
        let (tasks, head) = {
            let db = self.db();
            let t = db.task(lead)?;
            // Landed already: the other agent of the cycle landed them together a moment ago.
            if let Some(landed) = Self::already_landed(&db, &t)? {
                return Ok(landed);
            }
            anyhow::ensure!(t.state == TaskState::Open, "task is {:?}", t.state);
            let mut tasks = Vec::new();
            for id in ids {
                let o = db.task(id)?;
                match o.state {
                    TaskState::Open => tasks.push((o, db.writes(id)?)),
                    // Landed since: the head holds its work.
                    TaskState::Landed => {}
                    TaskState::Abandoned => anyhow::bail!("task {id} was abandoned: its work is gone. Land without it"),
                }
            }
            (tasks, db.head(&t.repo)?.ok_or_else(|| anyhow!("no line"))?)
        };
        if tasks.len() == 1 {
            return self.land_in_turn(lead, turn);
        }
        let not_landed = |reason: &str, note: String, conflicts: Vec<String>, proof: Option<Proof>, waits_for: Vec<String>| -> Result<LandOutcome> {
            Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: reason.into(), word: None, conflicts, proof, waits_for, note: Some(note), stack: vec![] }))
        };
        let repo = tasks[0].0.repo.clone();
        let others: Vec<String> = tasks.iter().map(|(t, _)| t.id.clone()).filter(|t| t != lead).collect();
        // Each task's change merged on the head after those before it, three ways, as a replay
        // merges one; in memory, so that a conflict changes nothing of any task.
        let mut tree = self.objects.get_tree(&head.tree)?;
        let mut by: BTreeMap<String, String> = BTreeMap::new();
        let (mut faults, mut conflicts) = (Vec::new(), Vec::new());
        for (t, writes) in &tasks {
            let live = Self::effective_writes(writes);
            if live.is_empty() {
                return not_landed("empty", format!("task {} has no write: its agent writes or abandons it, then land together again", t.id), vec![], None, vec![]);
            }
            let base = self.objects.get_tree(&t.base_tree)?;
            for (path, w) in live {
                let mine = (w.kind != WriteKind::Delete).then(|| Entry { blob: w.blob.clone().unwrap_or_default(), mode: w.mode });
                let (was, now) = (base.get(&path), tree.get(&path));
                let (merged, conflict) = if w.kind == WriteKind::Conflict {
                    (mine, true)
                } else if was == now || mine.as_ref() == now {
                    (mine, false)
                } else {
                    self.merge_entries(was, mine.as_ref(), now)?
                };
                if conflict {
                    faults.push(match (w.kind, by.get(&path)) {
                        (WriteKind::Conflict, _) => format!("task {} holds conflict markers in {path}, to settle", t.id),
                        (_, Some(o)) => format!("tasks {o} and {} change the same lines of {path}: one of them leaves them to the other", t.id),
                        _ => format!("the line changed the same lines of {path} as task {} since it opened: its `avp refresh` lays the markers out", t.id),
                    });
                    conflicts.push(path);
                    continue;
                }
                match merged {
                    Some(e) => tree.insert(path.clone(), e),
                    None => tree.remove(&path),
                };
                by.insert(path, t.id.clone());
            }
        }
        if !faults.is_empty() {
            return not_landed("conflict", format!("nothing landed, no task changed: {}. Then land together again", faults.join("; ")), conflicts, None, vec![]);
        }
        let hash = self.objects.put_tree(&tree)?;
        if hash == head.tree {
            return not_landed("already", "the line already holds these changes".into(), vec![], None, vec![]);
        }
        let lead_task = &tasks.iter().find(|(t, _)| t.id == lead).ok_or_else(|| anyhow!("task {lead} is not open"))?.0;
        let files: Vec<String> = by.keys().cloned().collect();
        let proof = {
            // Proven in the lead's folder, one proof at a time there.
            crate::check::cancel_other(lead, &hash);
            crate::seam::cancel_other(lead, &hash);
            let lock = self.task_lock(lead);
            let _one = lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut known = None;
            for (t, _) in &tasks {
                known = known.or(self.db().last_proof(&t.id)?.filter(|p| p.verdict == Verdict::Green && p.candidate_tree == hash));
            }
            match known {
                Some(p) => p,
                None => {
                    let steps = self.prove_task(lead_task, &tree, &hash, &files, false)?;
                    anyhow::ensure!(!steps.iter().any(|s| s.exit == crate::check::CANCELLED), "cancelled: a newer tree of task {lead} is being proven");
                    let red = steps.iter().any(|s| s.exit != 0);
                    let db = self.db();
                    let mut blamed = Vec::new();
                    if red {
                        let output = steps.iter().map(|s| s.output.as_str()).collect::<Vec<_>>().join("\n");
                        for f in failed_files(&output, &tree) {
                            // The task whose change the file holds, or else whoever holds it.
                            let holder = match by.get(&f) {
                                Some(t) => Some(t.clone()),
                                None => self.holder(&db, &repo, &f, "")?,
                            };
                            blamed.push((f, holder));
                        }
                    }
                    let outside: BTreeSet<String> = blamed.iter().filter_map(|(_, h)| h.clone()).filter(|h| !ids.contains(h)).collect();
                    let verdict = if !red { Verdict::Green } else if !outside.is_empty() && blamed.iter().all(|(_, h)| h.as_ref().is_some_and(|h| outside.contains(h))) { Verdict::Blocked } else { Verdict::Red };
                    let proof = Proof { id: new_id("proof"), task: Some(lead.into()), base_position: head.position, candidate_tree: hash.clone(), files: files.clone(), steps, verdict, blamed, created_at: now_ms() };
                    db.insert_proof(&repo, &proof)?;
                    db.emit(&repo, "proof", Some(lead), None, serde_json::json!({ "proof": proof.id, "verdict": verdict, "waits_for": outside, "with": others }))?;
                    proof
                }
            }
        };
        // What holds a green proof back (a person's approval, in AVP Cloud), task by task.
        for (t, writes) in &tasks {
            if let Some(n) = crate::seam::landing_gate(self, t, writes, &tree, &proof)? {
                let why = n.note.clone().unwrap_or_else(|| n.reason.clone());
                return Ok(LandOutcome::NotLanded(NotLanded { note: Some(format!("task {}, of this landing: {why}", t.id)), ..n }));
            }
        }
        if proof.verdict != Verdict::Green {
            let at_fault: BTreeSet<&str> = proof.blamed.iter().filter_map(|(_, h)| h.as_deref()).filter(|h| ids.iter().any(|i| i == h)).collect();
            let blocked = proof.verdict == Verdict::Blocked;
            let waits: Vec<String> = if blocked { proof.blamed.iter().filter_map(|(_, h)| h.clone()).collect::<BTreeSet<_>>().into_iter().collect() } else { vec![] };
            let note = match at_fault.is_empty() {
                true => format!("proven together, still {}: nothing landed. `avp proof {}` says where it fails", if blocked { "blocked" } else { "red" }, proof.id),
                false => format!("proven together, still red in the files of task {}: nothing landed. Each agent fixes its own (`avp proof {}`), then land together again", at_fault.into_iter().collect::<Vec<_>>().join(", task "), proof.id),
            };
            return not_landed(if blocked { "blocked" } else { "red" }, note, vec![], Some(proof), waits);
        }
        let turn = match turn {
            Some(t) => t,
            None => self.landing_turn(&repo),
        };
        let entry = {
            let db = self.db();
            let head_now = db.head(&repo)?.map(|h| h.position).unwrap_or(-1);
            let now: Vec<Task> = tasks.iter().map(|(t, _)| db.task(&t.id)).collect::<Result<_>>()?;
            // The line moved while they were proven, or one of them landed or went: again, holding
            // the turn, where nothing lands before them.
            if head_now != head.position || now.iter().any(|t| t.state != TaskState::Open) {
                drop(db);
                return self.land_together(lead, ids, Some(turn));
            }
            // The position's record names every task: this one's goal, then each other's.
            let goal = now.iter().find(|t| t.id == lead).map(|t| t.goal.clone()).unwrap_or_default();
            let note = std::iter::once(goal).chain(now.iter().filter(|t| t.id != lead).map(|t| format!("together with {}: {}", t.id, t.goal))).collect::<Vec<_>>().join("; ");
            let entry = db.append_line(&repo, head.position, Some(lead), &hash, Some(&proof.id), &note)?;
            for t in &now {
                db.set_task_state(&t.id, TaskState::Landed, Some(entry.position))?;
                db.release_leases(&t.id, None)?;
            }
            for (t, (_, writes)) in now.iter().zip(&tasks) {
                let with: Vec<&String> = now.iter().map(|o| &o.id).filter(|o| **o != t.id).collect();
                let files: Vec<String> = Self::effective_writes(writes).into_keys().collect();
                self.emit_told(&db, &repo, "task.landed", Some(&t.id), serde_json::json!({ "position": entry.position, "goal": t.goal, "files": files, "proof": proof.id, "with": with }))?;
            }
            for t in &now {
                Self::tell_stacked(&db, &repo, &t.id, "task.parent_landed", serde_json::json!({ "position": entry.position }))?;
            }
            entry
        };
        drop(turn);
        crate::seam::after_landing(self, lead_task, entry.position, &hash);
        Ok(LandOutcome::Landed(Landed { position: entry.position, tree: hash, proof: proof.id, replayed: lead_task.base_position != head.position, stack: vec![], with: others }))
    }

    /// The reverts of the last day a task should hear of as it opens: those of positions that
    /// changed the paths it opens on, and those of its own person and agent.
    pub fn reverted_for(&self, task_id: &str) -> Result<Vec<Event>> {
        let db = self.db();
        let task = db.task(task_id)?;
        let mine: BTreeSet<String> = task.scope.iter().map(|p| p.trim_end_matches('/').to_string()).chain(db.writes(task_id)?.into_iter().map(|w| w.path)).collect();
        let meets = |p: &str| mine.iter().any(|m| p == m || p.starts_with(&format!("{m}/")));
        let theirs = |e: &Event| e.data["person"] == task.actor.person && e.data["agent"] == task.actor.agent;
        Ok(db
            .events_of(&task.repo, &["line.reverted"], now_ms() - 24 * 3600 * 1000)?
            .into_iter()
            .filter(|e| theirs(e) || e.data["files"].as_array().is_some_and(|fs| fs.iter().filter_map(|f| f.as_str()).any(meets)))
            .collect())
    }

    // ---- picks and tags ----

    /// Brings position `position`'s change into an open task, three ways, as a replay merges it: a
    /// file the task holds as the position's parent had it takes the position's version, one changed
    /// since gets the change merged in, with conflict markers where both changed the same lines. A
    /// file deleted on one side, or binary, keeps the task's version and is named. The writes are the
    /// task's own, leases and all: how a change AVP took back comes back on the head, with its fix.
    pub fn pick(&self, task_id: &str, position: i64) -> Result<Picked> {
        let (task, writes) = {
            let db = self.db();
            (db.task(task_id)?, db.writes(task_id)?)
        };
        anyhow::ensure!(task.state == TaskState::Open, "task is {:?}", task.state);
        anyhow::ensure!(position > 0, "position 0 is where the line starts: it changed nothing to pick");
        let entry = self.db().line_at(&task.repo, position)?;
        let (before, after, now) = (self.tree_at(&task.repo, position - 1)?, self.objects.get_tree(&entry.tree)?, self.candidate(&task, &writes)?);
        let paths: BTreeSet<&String> = before.keys().chain(after.keys()).filter(|p| before.get(*p) != after.get(*p)).collect();
        let mut picked = Picked { position, goal: entry.note.trim().to_string(), files: vec![], merged: vec![], conflicts: vec![], kept: BTreeMap::new() };
        for path in paths {
            let (was, became, have) = (before.get(path), after.get(path), now.get(path));
            if have == became {
                continue;
            }
            let (want, conflict) = if have == was { (became.cloned(), false) } else { self.merge_entries(was, have, became)? };
            if want.as_ref() == have {
                // Merged clean, the task already held the change; in conflict, the task's version stands.
                if conflict {
                    let why = match (have, became) {
                        (None, _) => format!("deleted here, changed by position {position}"),
                        (_, None) => format!("deleted by position {position}, changed here since"),
                        _ => "binary, changed on both sides".to_string(),
                    };
                    picked.kept.insert(path.clone(), why);
                }
                continue;
            }
            let kind = match (&want, conflict) {
                (None, _) => WriteKind::Delete,
                (Some(_), true) => WriteKind::Conflict,
                (Some(_), false) => WriteKind::Put,
            };
            let out = self.write(task_id, PutWrite { path: path.clone(), kind, blob: want.as_ref().map(|e| e.blob.clone()), mode: want.as_ref().map_or(default_mode(), |e| e.mode) })?;
            match (out.lease.as_str(), conflict, have == was) {
                ("refused", _, _) => {
                    picked.kept.insert(path.clone(), format!("held by {} (strict leases)", out.previous_holder.unwrap_or_default()));
                }
                (_, true, _) => picked.conflicts.push(path.clone()),
                (_, false, true) => picked.files.push(path.clone()),
                (_, false, false) => picked.merged.push(path.clone()),
            }
        }
        self.db().emit(&task.repo, "task.picked", Some(task_id), None, serde_json::json!({ "position": position, "files": picked.files, "merged": picked.merged, "conflicts": picked.conflicts, "kept": picked.kept }))?;
        Ok(picked)
    }

    /// The repo's tags, newest position first; with `deleted`, those whose git tag the mirror has
    /// not deleted yet too.
    pub fn tags(&self, repo: &str, deleted: bool) -> Result<Vec<Tag>> {
        self.db().tags(repo, deleted)
    }

    /// Names a position (none: the head). A name already on another position moves only with
    /// `force`, as `git tag -f` moves one.
    pub fn tag(&self, repo: &str, name: &str, position: Option<i64>, force: bool, person: &str) -> Result<Tag> {
        let name = name.trim();
        check_tag_name(name)?;
        let db = self.db();
        let head = db.head(repo)?.ok_or_else(|| anyhow!("no line"))?.position;
        let position = position.unwrap_or(head);
        anyhow::ensure!((0..=head).contains(&position), "no position {position}: the line holds 0 to {head}");
        let was = db.tags(repo, false)?.into_iter().find(|t| t.name == name);
        match &was {
            Some(t) if t.position == position => return Ok(t.clone()),
            Some(t) if !force => anyhow::bail!("tag {name} names position {} already: `avp tag -f {name} {position}` moves it, `avp tag -d {name}` deletes it", t.position),
            _ => {}
        }
        db.set_tag(repo, name, position, person)?;
        db.emit(repo, "line.tagged", None, None, serde_json::json!({ "tag": name, "position": position, "from": was.map(|t| t.position), "person": person }))?;
        db.tags(repo, false)?.into_iter().find(|t| t.name == name).ok_or_else(|| anyhow!("no tag {name}"))
    }

    /// Deletes a tag: the mirror deletes its git tag on its next pass.
    pub fn untag(&self, repo: &str, name: &str, person: &str) -> Result<Tag> {
        let db = self.db();
        let tag = db.tags(repo, false)?.into_iter().find(|t| t.name == name).ok_or_else(|| anyhow!("no tag {name}: `avp tags` lists them"))?;
        db.delete_tag(repo, name)?;
        db.emit(repo, "line.untagged", None, None, serde_json::json!({ "tag": name, "position": tag.position, "person": person }))?;
        Ok(tag)
    }

    /// The mirror wrote a tag's git tag on `sha`, or deleted it (None).
    pub fn tag_git(&self, repo: &str, name: &str, sha: Option<&str>) -> Result<()> {
        self.db().set_tag_git(repo, name, sha)
    }

    // ---- reading ----

    pub fn why(&self, repo: &str, path: &str, at: Option<i64>) -> Result<Why> {
        let db = self.db();
        let head = db.head(repo)?.ok_or_else(|| anyhow!("no line"))?;
        let mut position = at.unwrap_or(head.position);
        let mut current = self.objects.get_tree(&db.line_at(repo, position)?.tree)?.get(path).cloned();
        while position > 0 {
            let before = self.objects.get_tree(&db.line_at(repo, position - 1)?.tree)?.get(path).cloned();
            if before != current {
                break;
            }
            current = before;
            position -= 1;
        }
        let entry = db.line_at(repo, position)?;
        let task = Self::writer(&db, &entry, path)?;
        let proof = entry.proof.as_deref().map(|p| db.proof(p)).transpose()?.flatten();
        Ok(Why { path: path.to_string(), position, task, proof, line: None, text: None })
    }

    /// Why line `line` (1-based) of `path` is what it is: the position whose task wrote that line,
    /// followed back through every change since, with its task, goal, agent and proof.
    pub fn why_line(&self, repo: &str, path: &str, line: usize, at: Option<i64>) -> Result<Why> {
        anyhow::ensure!(line > 0, "lines count from 1");
        let db = self.db();
        let head = db.head(repo)?.ok_or_else(|| anyhow!("no line"))?;
        let mut position = at.unwrap_or(head.position);
        let read = |pos: i64| -> Result<Option<String>> {
            let tree = self.objects.get_tree(&db.line_at(repo, pos)?.tree)?;
            tree.get(path).map(|e| self.objects.get(&e.blob).map(|b| String::from_utf8_lossy(&b).to_string())).transpose()
        };
        let mut current = read(position)?.ok_or_else(|| anyhow!("{path} is not in position {position}"))?;
        let text = current.lines().nth(line - 1).ok_or_else(|| anyhow!("{path} has {} lines at position {position}", current.lines().count()))?.to_string();
        let mut n = line - 1;
        while position > 0 {
            let Some(before) = read(position - 1)? else { break };
            if before != current {
                match line_before(&before, &current, n) {
                    Some(m) => n = m,
                    None => break,
                }
            }
            current = before;
            position -= 1;
        }
        let entry = db.line_at(repo, position)?;
        let task = Self::writer(&db, &entry, path)?;
        let proof = entry.proof.as_deref().map(|p| db.proof(p)).transpose()?.flatten();
        Ok(Why { path: path.to_string(), position, task, proof, line: Some(line), text: Some(text) })
    }

    /// The task that wrote `path` at a position: the position's task or, of the tasks landed with it
    /// (`avp land --with`), the one whose writes hold the path.
    fn writer(db: &Db, entry: &LineEntry, path: &str) -> Result<Option<Task>> {
        let task = entry.task.as_deref().map(|t| db.task(t)).transpose()?;
        let Some(t) = task.as_ref().filter(|t| t.landed_position == Some(entry.position)) else { return Ok(task) };
        for o in Self::landed_with(db, t)? {
            if Self::effective_writes(&db.writes(&o)?).contains_key(path) {
                return Ok(Some(db.task(&o)?));
            }
        }
        Ok(task)
    }

    /// Events a task should see: its own, those on paths it holds or wrote, and the line's.
    pub fn events_for(&self, repo: &str, task: Option<&str>, since: i64, limit: i64) -> Result<Vec<Event>> {
        let db = self.db();
        let all = db.events_since(repo, since, limit.max(1) * 4)?;
        let Some(task_id) = task else { return Ok(all.into_iter().take(limit as usize).collect()) };
        let leases = db.leases_of(task_id)?;
        let mine: BTreeSet<String> = db.writes(task_id)?.iter().map(|w| w.path.clone()).collect();
        Ok(all
            .into_iter()
            .filter(|e| {
                e.task.as_deref() == Some(task_id)
                    || e.kind.starts_with("line.")
                    || e.kind == "task.opened"
                    || e.kind == "task.landed"
                    || e.path.as_deref().map(|p| mine.contains(p) || Self::covered(&leases, p)).unwrap_or(false)
            })
            .take(limit as usize)
            .collect())
    }

    /// avp.toml as the head of the line has it.
    pub fn config(&self, repo: &str) -> Result<crate::check::AvpConfig> {
        let head = { self.db().head(repo)? };
        let Some(head) = head else { return Ok(Default::default()) };
        let tree = self.objects.get_tree(&head.tree)?;
        read_config(&tree, &self.objects)
    }

    // ---- notifications ----

    /// Emits an event, and tells the webhooks of `[notify]` when a person must hear of it.
    pub(crate) fn emit_told(&self, db: &Db, repo: &str, kind: &str, task: Option<&str>, data: serde_json::Value) -> Result<()> {
        db.emit(repo, kind, task, None, data.clone())?;
        self.notify(repo, kind, task, data);
        Ok(())
    }

    /// Posts what an event says to the webhooks `[notify]` names in the head's avp.toml, when a person
    /// must hear of it: the full suite red (on its retry too), a position reverted, a task waiting for
    /// a person's approval, a deploy done or failed. On threads of their own, a few tries each: a
    /// webhook that does not answer never holds the line.
    fn notify(&self, repo: &str, kind: &str, task: Option<&str>, data: serde_json::Value) {
        let news = match kind {
            "line.red" | "line.reverted" | "env.moved" | "env.failed" => true,
            // A person's revert lands as a task like another (`avp revert`): its goal says what it
            // takes back. AVP's own is told by its `line.reverted`, which says more.
            "task.landed" => data["goal"].as_str().is_some_and(|g| g.starts_with("Revert position ")),
            "task.awaiting_approval" => TOLD.lock().unwrap_or_else(|e| e.into_inner()).insert(format!("{} {}", task.unwrap_or(""), data["proof"])),
            _ => false,
        };
        if !news {
            return;
        }
        let (e, repo, kind, task) = (self.clone(), repo.to_string(), kind.to_string(), task.map(String::from));
        std::thread::spawn(move || {
            if let Err(err) = e.tell(&repo, &kind, task.as_deref(), &data) {
                eprintln!("notify {kind}: {err:#}");
            }
        });
    }

    fn tell(&self, repo: &str, kind: &str, task: Option<&str>, data: &serde_json::Value) -> Result<()> {
        let (name, goal, head) = {
            let db = self.db();
            let Some(head) = db.head(repo)? else { return Ok(()) };
            // A position no task landed (an import) still has its note.
            let note = data["position"].as_i64().and_then(|p| db.line_at(repo, p).ok()).map(|l| l.note).filter(|n| !n.is_empty());
            let task = task.map(|t| db.task(t)).transpose()?;
            if kind == "task.landed" && task.as_ref().is_some_and(|t| t.actor.person == avp_actor().person && t.actor.agent == avp_actor().agent) {
                return Ok(());
            }
            (db.repo(repo)?.name, task.map(|t| t.goal).or(note), head.tree)
        };
        let hooks = read_config(&self.objects.get_tree(&head)?, &self.objects)?.notify.webhooks;
        if hooks.is_empty() {
            return Ok(());
        }
        let mut body = serde_json::json!({ "text": notice(&name, kind, task, goal.as_deref(), data), "repo": name, "event": kind, "task": task });
        for k in ["position", "env", "proof"] {
            if !data[k].is_null() {
                body[k] = data[k].clone();
            }
        }
        // A revert names the position that took it back and the shard that was red. An
        // environment's `by` is a person, which its webhook does not carry.
        if kind == "line.reverted" {
            for k in ["by", "shard"] {
                body[k] = data[k].clone();
            }
        }
        for hook in hooks {
            let body = body.clone();
            std::thread::spawn(move || {
                if let Err(err) = post_webhook(&hook, &body) {
                    eprintln!("notify: {err:#}");
                }
            });
        }
        Ok(())
    }

}

/// What a webhook says of an event: a sentence a person reads in a chat, and what to do next.
fn notice(repo: &str, kind: &str, task: Option<&str>, goal: Option<&str>, d: &serde_json::Value) -> String {
    let position = d["position"].as_i64().unwrap_or(-1);
    let of = goal.map(|g| format!(" ({g})")).unwrap_or_default();
    let (env, proof, task) = (d["env"].as_str().unwrap_or(""), d["proof"].as_str().unwrap_or(""), task.unwrap_or(""));
    match kind {
        "line.red" => {
            let suite = d["scheduled"].as_str().map(|at| format!("the suite scheduled at {at} UTC")).unwrap_or_else(|| "the full suite".into());
            format!("{repo}: {suite} is red on position {position}{of}, on its retry too. `avp proof {proof}` says why.")
        }
        "line.reverted" => {
            let tests: Vec<&str> = d["tests"].as_array().map(|t| t.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
            let failing = if tests.is_empty() { String::new() } else { format!(" ({})", tests.join(" | ")) };
            format!("{repo}: AVP reverted position {position}{of} at position {}: it turned shard {} red twice{failing}. `avp show {position}` has the change.", d["by"], d["shard"].as_str().unwrap_or("?"))
        }
        "task.landed" => {
            let reverted = goal.and_then(|g| g.strip_prefix("Revert position ")).and_then(|g| g.split(':').next()).unwrap_or("?");
            format!("{repo}: position {reverted} is reverted, by position {position}{of}.")
        }
        "task.awaiting_approval" => {
            let guarded: Vec<&str> = d["guarded"].as_array().map(|g| g.iter().filter_map(|p| p.as_str()).collect()).unwrap_or_default();
            let changes = if guarded.is_empty() { String::new() } else { format!(": it changes {}", guarded.join(", ")) };
            format!("{repo}: {task} waits for a person's approval{of}{changes}. `avp diff {task}` shows it, `avp approve {task}` lands it, `avp refuse {task} --why \"...\"` hands it back.")
        }
        "env.moved" => match d["from"].as_i64() {
            Some(from) => format!("{repo}: {env} is rolled back to position {position}{of}, from position {from}."),
            None => format!("{repo}: {env} serves position {position}{of}, its deploy is done."),
        },
        "env.failed" => {
            let what = if d["rollback"] == true { format!("the rollback of {env} to position {position}") } else { format!("the deploy of position {position} to {env}") };
            let why = match (d["exit"].as_i64(), d["reason"].as_str()) {
                (Some(x), _) => format!(" (exit {x})"),
                (None, Some(r)) => format!(": {r}"),
                _ => String::new(),
            };
            let more = if proof.is_empty() { String::new() } else { format!(" `avp proof {proof}` has its output.") };
            format!("{repo}: {what} failed{why}, {env} stays where it was.{more}")
        }
        _ => format!("{repo}: {kind}"),
    }
}

/// A webhook as `[notify]` names it: the URL, or `$AVP_NOTIFY_<NAME>`, the variable of the
/// server's environment that holds it. No other name: avp.toml reaches no other secret of the server.
fn webhook_url(named: &str) -> Result<String> {
    let Some(var) = named.strip_prefix('$') else { return Ok(named.to_string()) };
    let var = var.trim_start_matches('{').trim_end_matches('}');
    anyhow::ensure!(var.starts_with("AVP_NOTIFY_"), "webhook ${var}: avp.toml names AVP_NOTIFY_* variables only");
    std::env::var(var).ok().filter(|u| !u.trim().is_empty()).ok_or_else(|| anyhow!("webhook ${var} is not set where the server runs"))
}

/// A webhook as a log shows it: its host, never its path, query or password, where a token sits.
fn shown(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => format!("{}://{}/...", u.scheme(), u.host_str().unwrap_or("?")),
        Err(_) => "a webhook that is no URL".into(),
    }
}

/// Posts `body` to a webhook, four tries at most, 1, 5 then 25 seconds apart: a webhook down for
/// half a minute still hears of it. A 4xx answer other than 429 is the request's fault, not tried again.
fn post_webhook(named: &str, body: &serde_json::Value) -> Result<()> {
    let url = webhook_url(named)?;
    let http = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;
    let mut last = String::new();
    for wait in [0, 1, 5, 25] {
        std::thread::sleep(std::time::Duration::from_secs(wait));
        match http.post(&url).json(body).send() {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => anyhow::bail!("{} answered {}", shown(&url), r.status()),
            Ok(r) => last = format!("answered {}", r.status()),
            // Without the URL: reqwest names it in its errors.
            Err(e) => last = format!("{:#}", anyhow::Error::from(e.without_url())),
        }
    }
    anyhow::bail!("{} did not take it in 4 tries: {last}", shown(&url))
}

/// Open tasks as `avp tasks` lists them: each stack together, every task right after the one it is
/// stacked on, with its depth in the stack (0: on the line), otherwise in the order given.
pub fn stack_order(tasks: &[Task]) -> Vec<(usize, &Task)> {
    fn put<'a>(t: &'a Task, depth: usize, tasks: &'a [Task], out: &mut Vec<(usize, &'a Task)>) {
        out.push((depth, t));
        for above in tasks.iter().filter(|a| a.on.as_deref() == Some(t.id.as_str())) {
            put(above, depth + 1, tasks, out);
        }
    }
    let mut out = Vec::new();
    for t in tasks.iter().filter(|t| t.on.as_deref().is_none_or(|p| !tasks.iter().any(|o| o.id == p))) {
        put(t, 0, tasks, &mut out);
    }
    out
}

/// What a task of `avp tasks` stands on: the task under it in its stack, or its position.
pub fn stands_on(t: &Task, depth: usize) -> String {
    match (&t.on, depth) {
        (Some(p), 1..) => format!("on {p}"),
        _ => format!("pos {}", t.base_position),
    }
}

/// A `line.reverted` event as an agent reads it when it opens a task: which position went, why,
/// and what to do before landing it again.
/// The first bad position between `good` (known good) and `bad` (known bad), halving: `is_bad` is
/// asked of the position in the middle of the two, which takes the place of one of them, until they
/// meet. What a red full suite's auto-revert and `avp bisect` both search with.
pub fn halve(mut good: i64, mut bad: i64, mut is_bad: impl FnMut(i64) -> Result<bool>) -> Result<i64> {
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if is_bad(mid)? {
            bad = mid;
        } else {
            good = mid;
        }
    }
    Ok(bad)
}

/// A tag name git takes as a tag (`git check-ref-format`), and that does not read as a position.
fn check_tag_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-/+".contains(c))
        && !name.starts_with(['-', '.', '/'])
        && !name.ends_with(['.', '/'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("/.");
    anyhow::ensure!(ok, "{name:?} cannot be a tag: letters, digits and . _ - / + as git takes them (v1.2, release/2026-10)");
    anyhow::ensure!(name.parse::<i64>().is_err(), "{name} reads as a position: a tag needs a letter (v{name})");
    Ok(())
}

pub fn revert_hint(e: &Event) -> String {
    let d = &e.data;
    let who = match (d["person"].as_str(), d["agent"].as_str()) {
        (Some(p), Some(a)) => format!(", by {p} via {a}"),
        _ => String::new(),
    };
    let tests: Vec<&str> = d["tests"].as_array().map(|t| t.iter().filter_map(|l| l.as_str()).collect()).unwrap_or_default();
    format!(
        "position {} ({}{who}) was reverted by AVP at position {}: the full suite's shard {} went red twice on it ({}). `avp show {}` has the change, `avp pick {}` brings it into a task: fix it there before landing it again.",
        d["position"],
        d["goal"].as_str().unwrap_or(""),
        d["by"],
        d["shard"].as_str().unwrap_or(""),
        tests.join(" | "),
        d["position"],
        d["position"]
    )
}

/// Where line `n` (0-based) of `new` was in `old`, or None when the change from `old` to `new`
/// wrote it. Lines outside the diff's hunks shift by what the hunks before them added or removed.
fn line_before(old: &str, new: &str, n: usize) -> Option<usize> {
    let patch = diffy::create_patch(old, new);
    // A range of length 0 names the line before it; any other range starts at its first line.
    let zero = |r: diffy::HunkRange| if r.len() == 0 { r.start() } else { r.start() - 1 };
    let mut offset: isize = 0;
    for h in patch.hunks() {
        let (mut o, mut w) = (zero(h.old_range()), zero(h.new_range()));
        if n < w {
            break;
        }
        for l in h.lines() {
            match l {
                diffy::Line::Context(_) => {
                    if w == n {
                        return Some(o);
                    }
                    o += 1;
                    w += 1;
                }
                diffy::Line::Delete(_) => o += 1,
                diffy::Line::Insert(_) => {
                    if w == n {
                        return None;
                    }
                    w += 1;
                }
            }
        }
        offset = o as isize - w as isize;
    }
    Some((n as isize + offset) as usize)
}
