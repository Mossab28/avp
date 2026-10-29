//! The verbs. Everything the server does lives here so the CLI, the HTTP layer and tests call
//! the same functions.

use crate::check::{failed_files, failing_tests, import_neighbors, read_config, Deploy, Part, Schedule, Shard, Workspace};
use crate::db::{Db, DeployRun, Tag};
use crate::model::*;
use crate::objects::Objects;
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

pub const LEASE_TTL_MS: i64 = 10 * 60 * 1000;

/// The files that say how a task is proven. Changing them waits for a person's word (25/10, a
/// Product Hunt question: "can a task edit the checks that validate it?"). A task is proven by its
/// own, as a pull request runs its own CI: proven by the line's (25/10 to 28/10), a change that
/// broke the steps landed green once approved, and every task after it was red under them, the
/// one that fixed it and AVP's revert of it too. What a proof's steps get of the secrets stays the
/// line's (`line_secrets`): a task does not hand itself one before a person reads it.
pub const CHECK_FILES: [&str; 2] = ["avp.toml", "trame.toml"];

/// Full suites share one laid-out dir per repo: one at a time.
static LINE_LOCK: Mutex<()> = Mutex::new(());

/// The approvals asked for that the webhooks were told of, by task and proof: an agent that lands
/// again while it waits asks nothing new.
static TOLD: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// When each scheduled suite last started, by repo and schedule (its `at` and shards), in ms.
static SCHEDULED: Mutex<BTreeMap<(String, String), i64>> = Mutex::new(BTreeMap::new());

#[derive(Clone)]
pub struct Engine {
    pub db: Arc<Mutex<Db>>,
    pub objects: Objects,
    pub data: PathBuf,
    /// One proof at a time per task: they share its folder, and the one that waits takes the
    /// verdict of the one before when it proved the same tree.
    task_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// The step each environment's deploy is at, kept in memory: as the bridge last read it (it
    /// sends it again every half minute), or the last line a deploy AVP runs printed. A deploy AVP
    /// runs keeps its state in the database too (`deploys`), its step written at most once a second.
    progress: Arc<Mutex<HashMap<(String, String), DeployProgress>>>,
    /// The deploys AVP runs itself (`run`): one at a time per environment.
    deploy_locks: Arc<Mutex<HashMap<(String, String), Arc<Mutex<()>>>>>,
    /// Each repo's landing queue.
    land_queues: Arc<Mutex<HashMap<String, Arc<LandQueue>>>>,
}

/// Where AVP's own revert went: landed, or proven green and waiting for a person's approval, with
/// the guarded paths it changes (none when the repo's every landing waits for one).
enum Reverting {
    Landed(Landed),
    Waits(Vec<String>),
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

/// `GET /work`. Proofs count wherever they run: on workers, and in the server's own slots.
#[derive(Debug, serde::Serialize)]
pub struct Work {
    /// "off": the server proves everything itself (no AVP_WORKERS=1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<String>,
    /// Proofs not started: in the workers' queue, or waiting for a slot here (or for the drain).
    pub waiting: usize,
    /// Proofs running: claimed by workers, or in the server's slots.
    pub running: usize,
    /// Workers wanted right now: the queue and what they run (what an autoscaler follows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demand: Option<usize>,
    /// Deploys AVP runs that are not over, running or waiting for their turn.
    pub deploys: usize,
    /// Those running: a restart cuts them. One that waits, the server that starts runs it.
    pub deploying: usize,
    /// The server starts no new work (`POST /drain`).
    pub draining: bool,
    /// When it drains, the seconds before the drain lifts by itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drain_left_s: Option<u64>,
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

impl Engine {
    pub fn open(data: &Path) -> Result<Self> {
        std::fs::create_dir_all(data)?;
        // trame.sqlite: a server started before AVP was named AVP keeps its database.
        let db = if !data.join("avp.sqlite").exists() && data.join("trame.sqlite").exists() { data.join("trame.sqlite") } else { data.join("avp.sqlite") };
        let engine = Self { db: Arc::new(Mutex::new(Db::open(&db)?)), objects: Objects::open(&data.join("objects"))?, data: data.to_path_buf(), task_locks: Arc::default(), progress: Arc::default(), deploy_locks: Arc::default(), land_queues: Arc::default() };
        if let Err(e) = engine.resume_deploys() {
            eprintln!("deploys left unfinished: {e:#}");
        }
        Ok(engine)
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
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
        let status = TaskStatus { writes: db.writes(task_id)?, leases: db.leases_of(task_id)?, last_proof: db.last_proof(task_id)?, approval: db.approval(task_id)?, task, head_position: head, stack: None };
        drop(db);
        let stack = if status.task.state == TaskState::Open { self.stack_word(&status.task)? } else { None };
        Ok(TaskStatus { stack, ..status })
    }

    /// Paths whose change needs a person's approval, whatever the proof says.
    pub fn guard(&self, repo: &str, add: &[String], remove: &[String]) -> Result<Vec<String>> {
        let db = self.db();
        db.guard(repo, add, remove)?;
        let now = db.guarded(repo)?;
        db.emit(repo, "repo.guarded", None, None, serde_json::json!({ "added": add, "removed": remove, "guarded": now }))?;
        Ok(now)
    }

    pub fn set_landing(&self, repo: &str, rule: LandingRule) -> Result<()> {
        let db = self.db();
        db.set_landing(repo, rule)?;
        db.emit(repo, "repo.landing", None, None, serde_json::json!({ "landing": rule }))?;
        Ok(())
    }

    /// A person's word on a task. A refusal reaches the agent as the reason of its next land. An
    /// approval of a task whose proof is green lands it at once: the person who read the change
    /// is the last step, nobody has to ask the agent to land again.
    pub fn approve(&self, task_id: &str, person: &str, approved: bool, reason: &str) -> Result<Approval> {
        let (green, task, writes) = {
            let db = self.db();
            let task = db.task(task_id)?;
            anyhow::ensure!(task.state == TaskState::Open, "task is {:?}", task.state);
            let a = Approval { task: task_id.into(), person: person.into(), approved, reason: reason.into(), at: now_ms() };
            db.set_approval(&a)?;
            db.emit(&task.repo, if approved { "task.approved" } else { "task.refused" }, Some(task_id), None, serde_json::json!({ "person": person, "reason": reason }))?;
            (db.last_proof(task_id)?.is_some_and(|p| p.verdict == Verdict::Green), task, db.writes(task_id)?)
        };
        // Red only without the secrets its checks name: approved, it is proven again with them.
        let withheld = approved && !green && !self.withheld_secrets(&task, &self.candidate(&task, &writes)?)?.is_empty();
        if approved && (green || withheld) {
            let (e, t) = (self.clone(), task_id.to_string());
            std::thread::spawn(move || {
                let _ = e.land(&t);
            });
        }
        Ok(Approval { task: task_id.into(), person: person.into(), approved, reason: reason.into(), at: now_ms() })
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

    /// Runs a candidate's checks, or one part of them: on a worker when the server has them
    /// (AVP_WORKERS=1), here otherwise, or when no worker answers in time. A speculative proof
    /// goes to a worker or nowhere: the server does not spend itself on a guess.
    #[allow(clippy::too_many_arguments)]
    pub fn prove(&self, repo: &str, tree: &Tree, tree_hash: &str, name: &str, files: &[String], steps_override: Option<&str>, part: Option<&Part>, speculative: bool) -> Result<Vec<Step>> {
        if let Some(d) = crate::workers::dispatcher() {
            let job = crate::workers::Job { id: new_id("job"), repo: repo.into(), tree: tree.clone(), tree_hash: tree_hash.into(), name: name.into(), files: files.to_vec(), steps_override: steps_override.map(String::from), part: part.cloned() };
            // Listed until a worker claims it: a land the drain holds in the queue hears why.
            let _queued = crate::check::queued(&job.id, name);
            if let Some(steps) = d.run(job) {
                return Ok(steps);
            }
            anyhow::ensure!(!speculative, "no worker took the speculative proof");
            eprintln!("no worker answered for {name}: proving here");
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
        let line_secrets = self.line_secrets(task, tree)?;
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
                            if let Some(d) = crate::workers::dispatcher() {
                                d.cancel_tree(&task.id, tree_hash);
                            }
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

    /// The secrets a task that changes the checks may give its steps: those the checks of the
    /// position it stands on give theirs, which its code already reaches, until a person approves
    /// it as it now is; then every one its checks name (avp migrate's first avp.toml names the
    /// secrets the tests need, and no line gave them yet). None when it leaves them alone (its
    /// avp.toml is the line's) or once approved.
    fn line_secrets(&self, task: &Task, tree: &Tree) -> Result<Option<Vec<String>>> {
        if self.approved_as_written(&task.id)? {
            return Ok(None);
        }
        self.secrets_of_base(task, tree)
    }

    /// A person's yes given after the task's last write: a later write does not ride on it.
    fn approved_as_written(&self, task_id: &str) -> Result<bool> {
        let db = self.db();
        let last = db.writes(task_id)?.iter().map(|w| w.at).max().unwrap_or(0);
        Ok(db.approval(task_id)?.is_some_and(|w| w.approved && w.at >= last))
    }

    /// The secrets a task's checks name that the line's do not give: its proof runs without them
    /// until a person approves it.
    fn withheld_secrets(&self, task: &Task, tree: &Tree) -> Result<Vec<String>> {
        let Some(given) = self.secrets_of_base(task, tree)? else { return Ok(vec![]) };
        let named = read_config(tree, &self.objects).map(|c| crate::check::secrets_named(&c.check, None, None)).unwrap_or_default();
        Ok(named.into_iter().filter(|s| !given.contains(s)).collect())
    }

    /// What the checks of the position a task stands on give its steps, when the task changes them.
    fn secrets_of_base(&self, task: &Task, tree: &Tree) -> Result<Option<Vec<String>>> {
        let base = self.objects.get_tree(&task.base_tree)?;
        if CHECK_FILES.iter().all(|n| base.get(*n) == tree.get(*n)) {
            return Ok(None);
        }
        // Checks on the line that do not read give none.
        let given = read_config(&base, &self.objects).map(|c| crate::check::secrets_named(&c.check, None, None)).unwrap_or_default();
        Ok(Some(given.into_iter().collect()))
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
        if let Some(d) = crate::workers::dispatcher() {
            d.cancel_other(task_id, &tree_hash);
        }
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
            vec![Step { name: "conflicts".into(), command: String::new(), exit: 1, seconds: 0.0, output: format!("unsettled conflicts in:\n{}", conflicts.join("\n")) }]
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
        self.land_in_turn(task_id, None)
    }

    /// `turn`: the repo's landing turn, when the task already holds it.
    /// A landed task's landing, as its land answered: landing it again says where it is.
    fn already_landed(db: &Db, t: &Task) -> Result<Option<LandOutcome>> {
        let (TaskState::Landed, Some(position)) = (t.state, t.landed_position) else { return Ok(None) };
        let e = db.line_at(&t.repo, position)?;
        Ok(Some(LandOutcome::Landed(Landed { position, tree: e.tree, proof: e.proof.unwrap_or_default(), replayed: false, stack: vec![] })))
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
                return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: "conflict".into(), conflicts, proof: None, waits_for: vec![], word: None, note: None, stack: vec![] }));
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
        let withheld = if proof.verdict == Verdict::Red { self.withheld_secrets(&task, &candidate)? } else { vec![] };
        if !withheld.is_empty() && !self.approved_as_written(task_id)? {
            // Red, proven without secrets its checks name and the line's do not give: a person's
            // word gives them, and `approve` proves it again with them.
            let note = format!("proven without {}, which its avp.toml names and the line's does not give: a person's approval gives them, and it is proven again", withheld.join(", "));
            let db = self.db();
            self.emit_told(&db, &task.repo, "task.awaiting_approval", Some(task_id), serde_json::json!({ "proof": proof.id, "goal": task.goal, "secrets": withheld }))?;
            return Ok(LandOutcome::NotLanded(NotLanded { guarded: CHECK_FILES.iter().map(|f| f.to_string()).filter(|f| candidate.get(f.as_str()).is_some()).collect(), reason: "approval".into(), conflicts: vec![], proof: Some(proof), waits_for: vec![], word: None, note: Some(note), stack: vec![] }));
        }
        if proof.verdict != Verdict::Green {
            // Only a blocked proof waits for someone: a red one is this task's to fix, even when some
            // of the failing files are held elsewhere (25/10 bench: "waits for" on a red proof sent an
            // agent polling for 13 minutes instead of fixing the tests its change broke).
            let waits: Vec<String> = if proof.verdict == Verdict::Blocked { proof.blamed.iter().filter_map(|(_, h)| h.clone()).collect::<BTreeSet<_>>().into_iter().collect() } else { vec![] };
            return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: if proof.verdict == Verdict::Blocked { "blocked".into() } else { "red".into() }, conflicts: vec![], proof: Some(proof), waits_for: waits, word: None, note: None, stack: vec![] }));
        }
        // The landing rule: a green proof may still need a person's word, for the whole repo or
        // because the task touches a guarded path (kept on the server, not in the tree: a task
        // cannot lift its own guard).
        let (rule, word, patterns) = {
            let db = self.db();
            (db.repo(&task.repo)?.landing, db.approval(task_id)?, db.guarded(&task.repo)?)
        };
        // The checks themselves are always guarded: a task proven by its own checks may change
        // them, but only a person makes that change count.
        let base = self.objects.get_tree(&task.base_tree)?;
        let guarded: Vec<String> = Self::effective_writes(&writes)
            .keys()
            .filter(|p| patterns.iter().any(|g| path_matches(g, p)) || (CHECK_FILES.contains(&p.as_str()) && base.get(*p) != candidate.get(*p)))
            .cloned()
            .collect();
        if rule == LandingRule::ProofAndApproval || !guarded.is_empty() {
            match &word {
                Some(w) if w.approved && w.at >= proof.created_at.min(task.created_at) => {}
                Some(w) if !w.approved => {
                    return Ok(LandOutcome::NotLanded(NotLanded { guarded: vec![], reason: "refused".into(), conflicts: vec![], proof: Some(proof), waits_for: vec![], word: word.clone(), note: None, stack: vec![] }));
                }
                _ => {
                    let db = self.db();
                    self.emit_told(&db, &task.repo, "task.awaiting_approval", Some(task_id), serde_json::json!({ "proof": proof.id, "goal": task.goal, "guarded": guarded }))?;
                    return Ok(LandOutcome::NotLanded(NotLanded { guarded: guarded.clone(), reason: "approval".into(), conflicts: vec![], proof: Some(proof), waits_for: vec![], word: None, note: None, stack: vec![] }));
                }
            }
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
        if task.actor.person == avp_actor().person && task.actor.agent == avp_actor().agent {
            if let Err(e) = self.revert_approved(&task.repo, task_id, entry.position) {
                eprintln!("revert landed at {}: {e:#}", entry.position);
            }
        }
        self.full_suite_after(&task.repo, entry.position, candidate_hash.clone(), true, None);
        Ok(LandOutcome::Landed(Landed { position: entry.position, tree: candidate_hash, proof: proof.id, replayed, stack: vec![] }))
    }

    /// The whole suite, after the fact, on a copy of the engine. Red reopens nothing by itself:
    /// it records and tells; the position stays. (Bisection is trivial on a line: the previous
    /// green position bounds it.) `scheduled`: a suite `[[check.schedule]]` started, of the shards it
    /// names.
    fn full_suite_after(&self, repo: &str, position: i64, tree_hash: String, give_way: bool, scheduled: Option<Schedule>) {
        let engine = self.clone();
        let repo = repo.to_string();
        std::thread::spawn(move || {
            let run = || -> Result<()> {
                let _one_at_a_time = LINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let tree = engine.objects.get_tree(&tree_hash)?;
                let cfg = read_config(&tree, &engine.objects)?.check;
                let mut shards = cfg.shards();
                if shards.is_empty() {
                    return Ok(());
                }
                // A scheduled suite runs the shards it names (each job of a shard's matrix), every one
                // when it names none. Some of the shards green say nothing of the whole: then only a
                // red counts for the position.
                let every = shards.len();
                if let Some(s) = scheduled.as_ref().filter(|s| !s.shards.is_empty()) {
                    shards.retain(|shard| s.shards.iter().any(|n| shard.name == *n || shard.name.starts_with(&format!("{n} ("))));
                    anyhow::ensure!(!shards.is_empty(), "the suite scheduled at {} names no shard of avp.toml ({})", s.at, s.shards.join(", "));
                }
                let whole = shards.len() == every;
                // A batch: landings come faster than a suite runs. The newest position holds every
                // earlier one, so a suite that waited while the line moved gives its turn to it.
                let head = engine.db().head(&repo)?.map(|h| h.position).unwrap_or(position);
                if give_way && head > position {
                    engine.db().emit(&repo, "line.full_covered", None, None, serde_json::json!({ "position": position, "by": head }))?;
                    return Ok(());
                }
                // Every shard at once: one job each, so as many workers as shards run the suite. A
                // shard comes back with its steps and, when it flaked, the tests its red run failed.
                let (runs, flakes): (Vec<Vec<Step>>, Vec<Option<Vec<String>>>) = std::thread::scope(|sc| {
                    let runs: Vec<_> = shards
                        .iter()
                        .enumerate()
                        .map(|(i, shard)| {
                            let (engine, repo, tree, tree_hash) = (&engine, &repo, &tree, &tree_hash);
                            let (name, run, part) = (&shard.name, &shard.run, Part::of_shard(shard));
                            sc.spawn(move || {
                                let attempt = || match engine.prove(repo, tree, tree_hash, &format!("line-{i}"), &[], Some(run), Some(&part), false) {
                                    Ok(mut st) => {
                                        for s in st.iter_mut() {
                                            s.name = name.clone();
                                        }
                                        st
                                    }
                                    Err(e) => vec![Step { name: name.clone(), command: run.clone(), exit: 1, seconds: 0.0, output: format!("{e:#}") }],
                                };
                                let first = attempt();
                                if first.iter().all(|s| s.exit == 0) {
                                    return (first, None);
                                }
                                // Red once may be the machine (a browser that did not start, a timing
                                // test under load: on Murmell's line, 24/10, positions went red on tests
                                // their changes did not reach, and were blamed): the shard runs again,
                                // once, on the same tree. Green the second time is flaky and says so,
                                // with the first run's end and the tests it failed; red twice is red. A
                                // task's proof never runs again: it gates a landing, and a red there is
                                // the agent's to read.
                                let mut second = attempt();
                                if second.iter().any(|s| s.exit != 0) {
                                    return (second, None);
                                }
                                let red: Vec<&str> = first.iter().filter(|s| s.exit != 0).map(|s| s.output.as_str()).collect();
                                let tail = red.iter().map(|o| o.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")).collect::<Vec<_>>().join("\n");
                                for s in second.iter_mut() {
                                    s.name = format!("{} (flaky: red, then green on its retry)", s.name);
                                    s.output = format!("first run, red:\n{tail}\n--- retry ---\n{}", s.output);
                                }
                                (second, Some(failing_tests(&red.join("\n"))))
                            })
                        })
                        .collect();
                    runs.into_iter().map(|h| h.join().unwrap_or_default()).unzip()
                });
                let steps: Vec<Step> = runs.into_iter().flatten().collect();
                let flaky: Vec<(&String, Vec<String>)> = shards.iter().map(|s| &s.name).zip(flakes).filter_map(|(name, tests)| tests.map(|t| (name, t))).collect();
                let red = steps.iter().any(|s| s.exit != 0);
                let verdict = if red { Verdict::Red } else if flaky.is_empty() { Verdict::Green } else { Verdict::Flaky };
                let output = steps.iter().map(|s| s.output.as_str()).collect::<Vec<_>>().join("\n");
                let blamed = if red { failed_files(&output, &tree).into_iter().map(|f| (f, None)).collect() } else { vec![] };
                let proof = Proof { id: new_id("proof"), task: None, base_position: position, candidate_tree: tree_hash.clone(), files: vec![], steps, verdict, blamed, created_at: now_ms() };
                let db = engine.db();
                db.insert_proof(&repo, &proof)?;
                if whole || red {
                    db.set_full(&repo, position, proof.verdict, &proof.id)?;
                }
                let task = db.line_at(&repo, position)?.task;
                // Each shard that flaked says so, whatever the others did: a flaky test is worth
                // knowing on a red position too. A flaky position has no other verdict event.
                for (shard, tests) in &flaky {
                    db.emit(&repo, "line.flaky", task.as_deref(), None, serde_json::json!({ "position": position, "proof": proof.id, "shard": shard, "tests": tests }))?;
                }
                if verdict != Verdict::Flaky {
                    let mut data = serde_json::json!({ "position": position, "proof": proof.id });
                    if let Some(s) = &scheduled {
                        data["scheduled"] = s.at.clone().into();
                    }
                    engine.emit_told(&db, &repo, if red { "line.red" } else { "line.green" }, task.as_deref(), data)?;
                }
                drop(db);
                // Environments that follow the full suite take the newest green position, or flaky.
                if !red {
                    engine.follow_full(&repo)?;
                } else if give_way {
                    // Red twice after a landing: the line does not stay red.
                    engine.repair(&repo, position, &shards, &proof);
                }
                Ok(())
            };
            if let Err(e) = run() {
                eprintln!("full suite at {position}: {e:#}");
            }
        });
    }

    /// A full suite red twice after a landing. With agents landing every few minutes, a line left red
    /// until a person acts is a line the next ones build on: the position that broke it goes back
    /// off the line, by a task AVP opens, proves and lands itself (`line.reverted`). What AVP cannot
    /// settle that way stays red for a person, and `line.revert_failed` says why. It runs in the
    /// suite's thread, holding the line's lock: the suites of what lands meanwhile wait, then the
    /// newest of them runs, on the revert or after it.
    fn repair(&self, repo: &str, red_at: i64, shards: &[Shard], proof: &Proof) {
        let Some(red) = proof.steps.iter().find(|s| s.exit != 0) else { return };
        let Some(shard) = shards.iter().find(|s| s.name == red.name) else { return };
        if let Err(e) = self.revert_red(repo, red_at, shard, red, &proof.id) {
            let task = self.db().line_at(repo, red_at).ok().and_then(|l| l.task);
            let _ = self.db().emit(repo, "line.revert_failed", task.as_deref(), None, serde_json::json!({ "position": red_at, "shard": shard.name, "proof": proof.id, "reason": format!("{e:#}") }));
        }
    }

    /// Finds the position that turned shard `shard` red at `red_at`, and reverts it. The batch is
    /// what landed since the newest position whose suite ran (or that AVP's last revert landed): one
    /// position, it is that one; several, the shard runs on the positions in between, halving, down
    /// to the first red one. A batch whose base is not known green runs the shard there first: red
    /// there too, the red is older than the batch and nothing in it is to blame.
    fn revert_red(&self, repo: &str, red_at: i64, red_shard: &Shard, red: &Step, proof: &str) -> Result<()> {
        let (shard, red_proof) = (red_shard.name.as_str(), proof);
        let (head, below) = {
            let db = self.db();
            (db.head(repo)?.ok_or_else(|| anyhow!("no line"))?, db.line(repo, red_at - 1, 1000)?)
        };
        if !read_config(&self.objects.get_tree(&head.tree)?, &self.objects)?.line.auto_revert {
            return Ok(());
        }
        anyhow::ensure!(!self.by_avp(repo, red_at)?, "position {red_at} is AVP's own revert, and red: a person decides");
        let reverts: BTreeSet<i64> = below.iter().filter_map(|e| e.reverted.as_ref().map(|r| r.by)).collect();
        let base = below.iter().find(|e| e.full.is_some() || reverts.contains(&e.position)).or(below.last()).ok_or_else(|| anyhow!("nothing below position {red_at} to compare it with"))?;
        let mut probes = Vec::new();
        if !matches!(base.full, Some(Verdict::Green | Verdict::Flaky)) {
            let r = self.shard_red(repo, base.position, red_shard)?;
            probes.push(serde_json::json!({ "position": base.position, "red": r.is_some() }));
            anyhow::ensure!(r.is_none(), "shard {shard} is red on position {} too, before the positions it would blame: a person decides", base.position);
        }
        let mut failed = red.clone();
        let culprit = halve(base.position, red_at, |mid| {
            let r = self.shard_red(repo, mid, red_shard)?;
            probes.push(serde_json::json!({ "position": mid, "red": r.is_some() }));
            Ok(r.map(|step| failed = step).is_some())
        })?;
        anyhow::ensure!(!self.by_avp(repo, culprit)?, "position {culprit}, AVP's own revert, turned shard {shard} red: a person decides");
        let entry = self.db().line_at(repo, culprit)?;
        let mut tests = failing_tests(&failed.output);
        if tests.is_empty() {
            // A runner that names no failing test: its last words.
            tests = failed.output.lines().rev().map(str::trim).filter(|l| !l.is_empty()).take(3).map(String::from).collect();
            tests.reverse();
        }
        tests.truncate(3);
        // A culprit below the red position read red on that shard: its verdict says so.
        let proof = if culprit == red_at {
            proof.to_string()
        } else {
            let tree = self.objects.get_tree(&entry.tree)?;
            let p = Proof { id: new_id("proof"), task: None, base_position: culprit, candidate_tree: entry.tree.clone(), files: vec![], steps: vec![failed.clone()], verdict: Verdict::Red, blamed: failed_files(&failed.output, &tree).into_iter().map(|f| (f, None)).collect(), created_at: now_ms() };
            let db = self.db();
            db.insert_proof(repo, &p)?;
            db.set_full(repo, culprit, Verdict::Red, &p.id)?;
            p.id
        };
        let note: String = entry.note.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
        let goal = format!("Revert position {culprit}: {note} (full suite red twice, shard {shard}: {})", tests.join(" | ").chars().take(300).collect::<String>());
        let (revert, landed, files) = self.revert_as_avp(repo, culprit, goal)?;
        let culprit_task = entry.task.as_deref().map(|t| self.db().task(t)).transpose()?;
        let tail = failed.output.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        let said = serde_json::json!({
            "position": culprit, "revert_task": revert.id, "red": red_at, "shard": shard, "tests": tests, "output": tail, "proof": proof,
            "goal": entry.note, "person": culprit_task.as_ref().map(|t| &t.actor.person), "agent": culprit_task.as_ref().map(|t| &t.actor.agent), "files": files, "probes": probes,
        });
        match landed {
            Reverting::Landed(l) => self.record_revert(repo, said, l.position),
            // Proven green, it waits for a person, open: approved, it lands as any task does, and
            // its landing records it (`revert_approved`) from what this event keeps.
            Reverting::Waits(guarded) => {
                let changes = if guarded.is_empty() { String::new() } else { format!(", as it changes {}", guarded.join(", ")) };
                let reason = format!("the revert of position {culprit} is proven green and waits for a person's approval{changes}: `avp diff {0}` shows it, `avp approve {0}` lands it", revert.id);
                let task = self.db().line_at(repo, red_at)?.task;
                self.db().emit(repo, "line.revert_failed", task.as_deref(), None, serde_json::json!({ "position": red_at, "shard": shard, "proof": red_proof, "reason": reason, "revert": said }))?;
                // Approved and landed before this event was there for its landing to find.
                match self.db().task(&revert.id)?.landed_position {
                    Some(by) => self.record_revert(repo, said, by),
                    None => Ok(()),
                }
            }
        }
    }

    /// AVP's revert landed at `by`: the position it took back says by what and why, and
    /// `line.reverted` hands it back to its person and agent, once. `said`: what the event says, `by`
    /// aside.
    fn record_revert(&self, repo: &str, mut said: serde_json::Value, by: i64) -> Result<()> {
        said["by"] = by.into();
        let position = said["position"].as_i64().ok_or_else(|| anyhow!("a revert names no position"))?;
        let text = |k: &str| said[k].as_str().unwrap_or_default().to_string();
        let r = Reverted { by, task: text("revert_task"), shard: text("shard"), tests: serde_json::from_value(said["tests"].clone()).unwrap_or_default(), proof: text("proof") };
        let db = self.db();
        let entry = db.line_at(repo, position)?;
        if entry.reverted.is_some() {
            return Ok(());
        }
        let task = entry.task;
        db.set_reverted(repo, position, &r)?;
        self.emit_told(&db, repo, "line.reverted", task.as_deref(), said)
    }

    /// AVP's revert of a guarded path, landed once a person approved it: recorded as one that lands
    /// at once, from the `line.revert_failed` that said it waited. Nothing when none did.
    fn revert_approved(&self, repo: &str, task_id: &str, by: i64) -> Result<()> {
        let waited = self.db().events_of(repo, &["line.revert_failed"], 0)?.into_iter().rev().find(|e| e.data["revert"]["revert_task"] == task_id);
        match waited {
            Some(e) => self.record_revert(repo, e.data["revert"].clone(), by),
            None => Ok(()),
        }
    }

    /// Whether AVP's own revert landed this position.
    fn by_avp(&self, repo: &str, position: i64) -> Result<bool> {
        let db = self.db();
        let Some(t) = db.line_at(repo, position)?.task else { return Ok(false) };
        let a = db.task(&t)?.actor;
        Ok(a.person == avp_actor().person && a.agent == avp_actor().agent)
    }

    /// A shard on position `position`, run again once when red as the full suite runs it: its red
    /// step when red twice, None when green. A shard that cannot run stops the bisection: a position
    /// is never blamed for the machine. The shard of that name in the position's own avp.toml, as
    /// its suite ran it: a position that changed the command would otherwise blame every one before
    /// it. One without it is not red on it.
    fn shard_red(&self, repo: &str, position: i64, shard: &Shard) -> Result<Option<Step>> {
        let entry = self.db().line_at(repo, position)?;
        let tree = self.objects.get_tree(&entry.tree)?;
        let Some(shard) = read_config(&tree, &self.objects)?.check.shards().into_iter().find(|s| s.name == shard.name) else { return Ok(None) };
        let mut red = None;
        for _ in 0..2 {
            let steps = self.prove(repo, &tree, &entry.tree, &format!("bisect-{position}"), &[], Some(&shard.run), Some(&Part::of_shard(&shard)), false)?;
            match steps.into_iter().find(|s| s.exit != 0) {
                Some(s) => red = Some(Step { name: shard.name.clone(), ..s }),
                None => return Ok(None),
            }
        }
        Ok(red)
    }

    /// Takes position `position` back on the head, as `avp revert` does, in a task AVP opens itself:
    /// proven and landed like any other. One proven green that waits for a person (it changes a
    /// guarded path: avp.toml, always) stays open for them; one that does not land is abandoned, and
    /// the error says why. Returns the task, where it went, and the files it took back.
    fn revert_as_avp(&self, repo: &str, position: i64, goal: String) -> Result<(Task, Reverting, Vec<String>)> {
        let task = self.open_task(repo, OpenTask { goal, actor: avp_actor(), scope: vec![], transcript: None })?;
        let landed = self.write_revert(&task, position).and_then(|files| match self.land(&task.id)? {
            LandOutcome::Landed(l) => Ok((Reverting::Landed(l), files)),
            LandOutcome::NotLanded(n) if n.reason == "approval" => Ok((Reverting::Waits(n.guarded), files)),
            LandOutcome::NotLanded(n) => {
                let red = n.proof.as_ref().and_then(|p| p.steps.iter().find(|s| s.exit != 0)).map(|s| format!(", {} ends with:\n{}", s.name, crate::check::red_part(&s.output).lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))).unwrap_or_default();
                Err(match n.reason.as_str() {
                    "conflict" => anyhow!("the revert of position {position} conflicts with what landed meanwhile, in {}: a person settles it (`avp revert {position}`)", n.conflicts.join(", ")),
                    "empty" | "already" => anyhow!("the head no longer holds what position {position} changed: nothing to take back"),
                    r => anyhow!("the revert of position {position} does not land: its proof is {r}{red}"),
                })
            }
        });
        match landed {
            Ok((l, files)) => Ok((task, l, files)),
            Err(e) => {
                let _ = self.abandon(&task.id);
                Err(e)
            }
        }
    }

    /// The writes that take `position` back on the task's base. A file nobody changed since comes
    /// back as it was; one changed since gets the inverse change merged in; lines changed again
    /// since stop it, for a person to settle. AVP takes no lease: its revert is the line's own, and
    /// neither waits for an agent's lease nor takes one away.
    fn write_revert(&self, task: &Task, position: i64) -> Result<Vec<String>> {
        anyhow::ensure!(position > 0, "position 0 is where the line starts: there is nothing to revert");
        let (before, after) = (self.tree_at(&task.repo, position - 1)?, self.tree_at(&task.repo, position)?);
        let head = self.objects.get_tree(&task.base_tree)?;
        let paths: BTreeSet<&String> = before.keys().chain(after.keys()).filter(|p| before.get(*p) != after.get(*p)).collect();
        let text = |e: &Entry| -> Result<Option<String>> { Ok(String::from_utf8(self.objects.get(&e.blob)?).ok()) };
        let (mut writes, mut conflicts) = (Vec::new(), Vec::new());
        for path in paths {
            let (was, became, now) = (before.get(path), after.get(path), head.get(path));
            if now == became {
                writes.push(match was {
                    Some(e) => PutWrite { path: path.clone(), kind: WriteKind::Put, blob: Some(e.blob.clone()), mode: e.mode },
                    None => PutWrite { path: path.clone(), kind: WriteKind::Delete, blob: None, mode: default_mode() },
                });
                continue;
            }
            let merged = match (was, became, now) {
                (Some(w), Some(b), Some(n)) => match (text(w)?, text(b)?, text(n)?) {
                    (Some(w), Some(b), Some(t)) => diffy::merge(&b, &t, &w).ok(),
                    _ => None,
                },
                // Added by the position, or deleted by it, and changed again since: a person decides.
                _ => None,
            };
            match (merged, now) {
                (Some(m), Some(n)) => writes.push(PutWrite { path: path.clone(), kind: WriteKind::Put, blob: Some(self.objects.put(m.as_bytes())?), mode: n.mode }),
                _ => conflicts.push(path.clone()),
            }
        }
        anyhow::ensure!(conflicts.is_empty(), "the lines position {position} changed were changed again since, in {}: a person settles them (`avp revert {position}`)", conflicts.join(", "));
        let db = self.db();
        for w in &writes {
            let write = db.append_write(&task.id, &w.path, w.kind, w.blob.as_deref(), w.mode)?;
            db.emit(&task.repo, "write", Some(&task.id), Some(&w.path), serde_json::json!({ "kind": w.kind, "seq": write.seq, "reverts": position }))?;
        }
        Ok(writes.into_iter().map(|w| w.path).collect())
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

    /// Each environment that follows the full suite takes the newest green position, once it
    /// serves the one it was last promoted to. Promoted again while a deploy runs, it would start
    /// another deploy that takes the place of the first (on Murmell, staging moved from 108 to
    /// nowhere for 30 minutes while green positions kept coming). A deploy that has not been served
    /// in AVP_FOLLOW_STUCK_S (default 1800) is taken for lost, and the next one goes. A deploy AVP
    /// runs itself waits for the one before it on its own: the newest green position goes at once.
    pub fn follow_full(&self, repo: &str) -> Result<()> {
        let stuck_ms = std::env::var("AVP_FOLLOW_STUCK_S").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(1800) * 1000;
        for (name, cfg) in self.env_config(repo)? {
            if cfg.follow.as_deref() != Some("full") {
                continue;
            }
            let (newest, wanted, serves) = {
                let db = self.db();
                (db.newest_green(repo)?, db.promotion(repo, &name)?, db.env(repo, &name)?.map(|e| e.position))
            };
            let Some(newest) = newest else { continue };
            let settled = match &wanted {
                _ if matches!(cfg.deploy, Some(Deploy::Run(_))) => true,
                None => true,
                Some(w) => serves.is_some_and(|s| s >= w.position) || now_ms() - w.at > stuck_ms,
            };
            if settled && wanted.as_ref().is_none_or(|w| w.position < newest) && serves.is_none_or(|s| s < newest) {
                // In the background: the full suite that called holds the line's lock.
                if let Err(e) = self.promote_as(repo, &name, Some(newest), false, "avp", "full suite", false, None) {
                    eprintln!("{name} does not follow position {newest}: {e:#}");
                }
            }
        }
        Ok(())
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
        let task = entry.task.as_deref().map(|t| db.task(t)).transpose()?;
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
        let task = entry.task.as_deref().map(|t| db.task(t)).transpose()?;
        let proof = entry.proof.as_deref().map(|p| db.proof(p)).transpose()?.flatten();
        Ok(Why { path: path.to_string(), position, task, proof, line: Some(line), text: Some(text) })
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

    /// What an environment really serves, read from the git commit it publishes (a deployment
    /// that AVP did not drive: GitHub's workflows during the transition). Moves the pointer
    /// without asking for a proof, and says so in the event.
    pub fn observe_env(&self, repo: &str, name: &str, sha: &str) -> Result<Environment> {
        let db = self.db();
        let position = db.position_of_git(repo, sha)?.ok_or_else(|| anyhow!("no position of the line is git commit {sha} (served from before the line?)"))?;
        if db.env(repo, name)?.map(|e| e.position) == Some(position) {
            return Ok(db.env(repo, name)?.unwrap());
        }
        let env = db.set_env(repo, name, position)?;
        let task = db.line_at(repo, position)?.task;
        let rollback = Self::rollback_from(&db, repo, name, position)?;
        self.emit_told(&db, repo, "env.moved", task.as_deref(), rolled_back(serde_json::json!({ "env": name, "position": position, "observed": true, "git": sha }), rollback))?;
        drop(db);
        // Its deploy is done: an environment that follows the full suite takes the newest green.
        self.follow_full(repo)?;
        Ok(env)
    }

    /// Runs the full suite of a position again (none: the head), whatever its last verdict: after a
    /// red that looked like the machine, or to prove an older position. Starts it and returns.
    pub fn rerun_full(&self, repo: &str, position: Option<i64>) -> Result<i64> {
        let entry = {
            let db = self.db();
            let position = match position {
                Some(p) => p,
                None => db.head(repo)?.ok_or_else(|| anyhow!("no line"))?.position,
            };
            db.line_at(repo, position)?
        };
        self.full_suite_after(repo, entry.position, entry.tree.clone(), false, None);
        Ok(entry.position)
    }

    /// The environments avp.toml declares, read from the head of the line.
    pub fn env_config(&self, repo: &str) -> Result<std::collections::BTreeMap<String, crate::check::EnvConfig>> {
        Ok(self.config(repo)?.envs)
    }

    /// avp.toml as the head of the line has it.
    pub fn config(&self, repo: &str) -> Result<crate::check::AvpConfig> {
        let head = { self.db().head(repo)? };
        let Some(head) = head else { return Ok(Default::default()) };
        let tree = self.objects.get_tree(&head.tree)?;
        read_config(&tree, &self.objects)
    }

    /// The GitHub bridge found the run of a promotion's deploy (`deploy.dispatched`): where an
    /// agent follows it, and what a server that restarts follows again instead of dispatching twice.
    pub fn github_dispatched(&self, repo: &str, name: &str, p: &Promotion, workflow: &str, run: u64, url: &str) -> Result<()> {
        let db = self.db();
        let task = db.line_at(repo, p.position)?.task;
        db.emit(repo, "deploy.dispatched", task.as_deref(), None, serde_json::json!({ "env": name, "position": p.position, "by": p.by, "workflow": workflow, "run": run, "url": url }))
    }

    /// The run of a promotion's GitHub deploy is green and nothing else says what the environment
    /// serves (no `serves` URL): it serves the position now, as when a `run` deploy exits 0.
    pub fn github_deployed(&self, repo: &str, name: &str, p: &Promotion, url: &str) -> Result<Environment> {
        let db = self.db();
        let env = db.set_env(repo, name, p.position)?;
        let task = db.line_at(repo, p.position)?.task;
        let rollback = Self::rollback_from(&db, repo, name, p.position)?;
        self.emit_told(&db, repo, "env.moved", task.as_deref(), rolled_back(serde_json::json!({ "env": name, "position": p.position, "by": p.by, "run": url }), rollback))?;
        drop(db);
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).remove(&(repo.to_string(), name.to_string()));
        self.follow_full(repo)?;
        Ok(env)
    }

    /// The run of a promotion's GitHub deploy failed: the environment stays, `reason` names the job.
    pub fn github_failed(&self, repo: &str, name: &str, p: &Promotion, reason: &str, url: &str) -> Result<()> {
        let db = self.db();
        let task = db.line_at(repo, p.position)?.task;
        let rollback = Self::rollback_from(&db, repo, name, p.position)?;
        self.emit_told(&db, repo, "env.failed", task.as_deref(), rolled_back(serde_json::json!({ "env": name, "position": p.position, "by": p.by, "reason": reason, "run": url }), rollback))
    }

    /// Every environment: what it serves (-1 when nothing yet), its last promotion, its deploy.
    pub fn envs_view(&self, repo: &str) -> Result<Vec<Environment>> {
        let cfg = self.env_config(repo)?;
        let db = self.db();
        let mut out: std::collections::BTreeMap<String, Environment> = db.envs(repo)?.into_iter().map(|e| (e.name.clone(), e)).collect();
        for name in cfg.keys() {
            out.entry(name.clone()).or_insert_with(|| Environment { name: name.clone(), position: -1, moved_at: 0, wanted: None, deploy: None, progress: None });
        }
        for (name, e) in out.iter_mut() {
            // A promotion the environment went past since, by other means (a deploy run from
            // GitHub), is no longer asked for: carried out now, it would take it back. One made
            // after the environment moved is a rollback, and stays asked for.
            e.wanted = db.promotion(repo, name)?.filter(|w| !(e.moved_at > w.at && e.position > w.position));
            let deploy = cfg.get(name).and_then(|c| c.deploy.as_ref());
            // For the bridge, which carries out the GitHub kind only: AVP runs the other itself.
            e.deploy = deploy.and_then(Deploy::github).cloned();
            // Only for the promotion still deploying, and only while the bridge keeps reading it. A
            // deploy AVP runs needs no reading: its last line stays, a failure's until the next
            // promotion, and a server that restarted reads it back from the database.
            let ours = matches!(deploy, Some(Deploy::Run(_)));
            let fresh = now_ms() - 5 * 60_000;
            let live = self.progress.lock().unwrap_or_else(|p| p.into_inner()).get(&(repo.to_string(), name.clone())).cloned();
            let kept = match (&live, ours) {
                (None, true) => db.last_deploy(repo, name)?.filter(|d| !d.step.is_empty()).map(|d| DeployProgress { position: d.promotion.position, step: d.step, since: d.step_at, url: None, at: d.step_at }),
                _ => None,
            };
            e.progress = live.or(kept).filter(|p| (ours || p.at >= fresh) && e.wanted.as_ref().is_some_and(|w| w.position == p.position && w.position != e.position));
        }
        Ok(out.into_values().collect())
    }

    /// The bridge's latest reading of a deploy's step.
    pub fn deploy_progress(&self, repo: &str, name: &str, mut p: DeployProgress) -> Result<DeployProgress> {
        p.at = now_ms();
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).insert((repo.to_string(), name.to_string()), p.clone());
        Ok(p)
    }

    /// Promotes a position to an environment, by the rules avp.toml sets for it: a green full suite
    /// (unless forced), what `after` serves, one of `approvers`. With a GitHub deploy, the promotion
    /// is recorded and AVP's bridge carries it out (the environment moves when it is observed serving
    /// it); with a `run` deploy, AVP runs it and answers once it is over, as a land answers once its
    /// proof is; without one, the environment moves at once. No position: what `after` serves, or
    /// the head.
    pub fn promote(&self, repo: &str, name: &str, position: Option<i64>, force: bool, person: &str, agent: &str) -> Result<Environment> {
        self.promote_as(repo, name, position, force, person, agent, true, None)
    }

    /// Takes an environment back to the position it served before the one it serves now, through
    /// the same deploy as a promotion, recorded as a rollback: its `env.promoted` and `env.moved`
    /// say `rollback` and the position it came `from`. What an emergency needs at once: the full
    /// suite and `after` are not asked again (that position passed them to be served), `approvers`
    /// are. A second rollback takes it back where the first one found it.
    pub fn rollback(&self, repo: &str, name: &str, person: &str, agent: &str) -> Result<Environment> {
        let (serves, before) = {
            let db = self.db();
            let serves = db.env(repo, name)?.map(|e| e.position).filter(|p| *p >= 0).ok_or_else(|| anyhow!("{name} serves nothing yet: there is nothing to roll back"))?;
            let before = Self::served(&db, repo, name)?.into_iter().rev().find(|p| *p != serves);
            (serves, before.ok_or_else(|| anyhow!("{name} has served only position {serves}: there is nothing to roll back to"))?)
        };
        self.promote_as(repo, name, Some(before), true, person, agent, true, Some(serves))
    }

    /// The positions an environment moved to, oldest first, as its `env.moved` events say.
    fn served(db: &Db, repo: &str, name: &str) -> Result<Vec<i64>> {
        Ok(db.events_since(repo, 0, i64::MAX)?.into_iter().filter(|e| e.kind == "env.moved" && e.data["env"] == name).filter_map(|e| e.data["position"].as_i64()).collect())
    }

    /// The position the newest promotion of `name` rolls back from, when it is a rollback to
    /// `position`: what its deploy says once it is over, however long after (a restart between).
    fn rollback_from(db: &Db, repo: &str, name: &str, position: i64) -> Result<Option<i64>> {
        let promoted = db.events_since(repo, 0, i64::MAX)?.into_iter().rev().find(|e| e.kind == "env.promoted" && e.data["env"] == name);
        Ok(promoted.filter(|e| e.data["position"] == position && e.data["rollback"] == true).and_then(|e| e.data["from"].as_i64()))
    }

    /// `wait` false: a `run` deploy goes on in the background and the promotion answers at once.
    /// `rollback`: the position a rollback takes the environment back from.
    #[allow(clippy::too_many_arguments)]
    fn promote_as(&self, repo: &str, name: &str, position: Option<i64>, force: bool, person: &str, agent: &str, wait: bool, rollback: Option<i64>) -> Result<Environment> {
        let cfg = self.env_config(repo)?.remove(name).unwrap_or_default();
        let db = self.db();
        let serves = |env: &str| -> Result<Option<i64>> { Ok(db.env(repo, env)?.map(|e| e.position).filter(|p| *p >= 0)) };
        let position = match (position, &cfg.after) {
            (Some(p), _) => p,
            (None, Some(after)) => serves(after)?.ok_or_else(|| anyhow!("{after} serves nothing yet: nothing to promote to {name}"))?,
            (None, None) => db.head(repo)?.ok_or_else(|| anyhow!("no line"))?.position,
        };
        let entry = db.line_at(repo, position)?;
        if !force {
            // Flaky is green on its retry: it promotes as green does.
            anyhow::ensure!(matches!(entry.full, Some(Verdict::Green | Verdict::Flaky)), "position {position} has no green full suite yet (full = {:?}): --force to promote it anyway", entry.full);
            if let Some(after) = &cfg.after {
                let s = serves(after)?;
                anyhow::ensure!(s == Some(position), "{name} takes what {after} serves ({}), not position {position}", s.map(|p| format!("position {p}")).unwrap_or_else(|| "nothing yet".into()));
            }
        }
        anyhow::ensure!(cfg.approvers.is_empty() || cfg.approvers.iter().any(|a| a == person), "{name} is promoted by {} only, not {person}", cfg.approvers.join(" or "));
        let Some(deploy) = cfg.deploy else {
            let env = db.set_env(repo, name, position)?;
            db.emit(repo, "env.moved", entry.task.as_deref(), None, rolled_back(serde_json::json!({ "env": name, "position": position, "by": person }), rollback))?;
            return Ok(env);
        };
        let p = Promotion { position, git: entry.git.clone(), by: person.to_string(), agent: agent.to_string(), at: now_ms() };
        db.set_promotion(repo, name, &p)?;
        db.emit(repo, "env.promoted", entry.task.as_deref(), None, rolled_back(serde_json::json!({ "env": name, "position": position, "by": person, "agent": agent, "git": entry.git }), rollback))?;
        let mut env = db.env(repo, name)?.unwrap_or(Environment { name: name.to_string(), position: -1, moved_at: 0, wanted: None, deploy: None, progress: None });
        env.wanted = Some(p.clone());
        let run = match deploy {
            Deploy::GitHub(d) => {
                env.deploy = Some(d);
                return Ok(env);
            }
            // Recorded with the promotion: a server that restarts before its turn still runs it.
            Deploy::Run(command) => db.insert_deploy(repo, name, &p, &command)?,
        };
        drop(db);
        if wait {
            return self.run_deploy(&run);
        }
        self.run_deploy_behind(run);
        Ok(env)
    }

    fn run_deploy_behind(&self, d: DeployRun) {
        let e = self.clone();
        std::thread::spawn(move || {
            if let Err(err) = e.run_deploy(&d) {
                eprintln!("{err:#}");
            }
        });
    }

    /// The deploys a server left unfinished when it stopped, settled by the next one as it starts:
    /// a deploy that waited for its turn runs now; one that was running is cut (`env.interrupted`)
    /// and runs again once, from the start, while its promotion is still the one asked for: the
    /// environment is still asked to serve that position. Cut a second time, it is left failed,
    /// with the reason (`env.failed`). Every unfinished deploy is this server's to settle: a data
    /// folder is served by one server at a time.
    pub fn resume_deploys(&self) -> Result<()> {
        let left = self.db().unfinished_deploys()?;
        for d in left {
            let db = self.db();
            let p = &d.promotion;
            let wanted = db.promotion(&d.repo, &d.env)?.filter(|w| (w.position, w.at) == (p.position, p.at));
            let task = db.line_at(&d.repo, p.position).ok().and_then(|l| l.task);
            let again = wanted.is_some() && d.attempt < 2;
            if d.started_at.is_some() {
                db.emit(&d.repo, "env.interrupted", task.as_deref(), None, serde_json::json!({ "env": d.env, "position": p.position, "by": p.by, "attempt": d.attempt, "again": again, "step": d.step }))?;
            }
            if wanted.is_none() {
                db.deploy_finished(&d.id, None, Some("a newer promotion goes instead"), "")?;
                continue;
            }
            if d.started_at.is_some() && !again {
                let reason = format!("a restart of the server cut it {} times", d.attempt);
                db.deploy_finished(&d.id, None, Some(&reason), &format!("failed: {reason}"))?;
                self.emit_told(&db, &d.repo, "env.failed", task.as_deref(), serde_json::json!({ "env": d.env, "position": p.position, "by": p.by, "reason": reason }))?;
                continue;
            }
            let mut d = d;
            if d.started_at.is_some() {
                d.step = "cut by a restart of the server: runs again from the start".into();
                db.deploy_again(&d.id, &d.step)?;
                d.attempt += 1;
                d.started_at = None;
            }
            drop(db);
            self.run_deploy_behind(d);
        }
        Ok(())
    }

    /// Deploys AVP runs that are not over, running or waiting for their turn (`GET /work`).
    pub fn deploys_in_flight(&self) -> Result<usize> {
        Ok(self.db().unfinished_deploys()?.len())
    }

    /// What the server has in hand (`GET /work`): what an autoscaler follows, and what a restart now
    /// would cut, which the server's updater waits for (server-update.sh drains, then waits for 0).
    pub fn work(&self) -> Result<Work> {
        let deploys = self.db().unfinished_deploys()?;
        let (waiting, running) = crate::check::local();
        let left = crate::check::draining();
        let mut work = Work {
            workers: None,
            waiting,
            running,
            demand: None,
            deploys: deploys.len(),
            deploying: deploys.iter().filter(|d| d.started_at.is_some()).count(),
            draining: left.is_some(),
            drain_left_s: left.map(|l| l.as_secs()),
        };
        match crate::workers::dispatcher() {
            Some(d) => {
                let q = d.depth();
                work.waiting += q.waiting;
                work.running += q.running;
                work.demand = Some(q.demand);
            }
            None => work.workers = Some("off".into()),
        }
        Ok(work)
    }

    /// Carries out a promotion whose deploy is `run`: the position laid out in the environment's
    /// own folder (what `keep` names stays from one deploy to the next, as from one proof to the
    /// next), the command run there as a proof's steps are, with what AVP_DEPLOY_ENV lists beside
    /// (its values masked in the output), each line it prints the deploy's progress. Green, the
    /// environment moves; red, it stays where it was, and the event, the progress and the answer
    /// say why. Its output is kept as a proof (`avp proof <id>`). One deploy at a time per
    /// environment: a promotion made while this one waited goes instead. Whatever ends it, the
    /// database has its end.
    fn run_deploy(&self, d: &DeployRun) -> Result<Environment> {
        let key = (d.repo.clone(), d.env.clone());
        let lock = self.deploy_locks.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_default().clone();
        let _one = lock.lock().unwrap_or_else(|e| e.into_inner());
        let out = self.deploy_in_turn(d);
        if let Err(e) = &out {
            let _ = self.db().deploy_finished(&d.id, None, Some(&format!("{e:#}")), "");
        }
        out
    }

    fn deploy_in_turn(&self, d: &DeployRun) -> Result<Environment> {
        let (repo, name, command, p) = (d.repo.as_str(), d.env.as_str(), d.command.as_str(), &d.promotion);
        // A server that drains starts no deploy: this one waits, recorded, and the server that
        // takes its place runs it if the drain ends in a restart.
        crate::check::wait_drain(&format!("deploy {repo}/{name}"));
        if let Some(w) = self.db().promotion(repo, name)?.filter(|w| (w.position, w.at) != (p.position, p.at)) {
            anyhow::bail!("{name} was promoted to position {} by {} while the deploy of position {} waited: that one goes instead", w.position, w.by, p.position);
        }
        // Read now: the mirror may have written the position's commit since it was promoted.
        let entry = self.db().line_at(repo, p.position)?;
        self.db().deploy_started(&d.id, &format!("lays out position {}", p.position))?;
        // Every line in memory, at most one a second in the database: a deploy may print thousands.
        let written = std::cell::Cell::new(now_ms());
        let say = |step: String| {
            if now_ms() - written.get() >= 1000 {
                written.set(now_ms());
                let _ = self.db().deploy_step(&d.id, &step);
            }
            let _ = self.deploy_progress(repo, name, DeployProgress { position: p.position, step, since: now_ms(), url: None, at: 0 });
        };
        let colors = regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap();
        let ran = || -> Result<Step> {
            anyhow::ensure!(safe_path(name) && !name.contains('/'), "{name:?} cannot name the deploy's folder");
            let mut run = command.replace("{position}", &p.position.to_string());
            if run.contains("{git}") {
                let git = entry.git.as_deref().ok_or_else(|| anyhow!("the deploy takes {{git}}, and position {} has no git commit yet", p.position))?;
                run = run.replace("{git}", git);
            }
            say(format!("lays out position {}", p.position));
            let tree = self.objects.get_tree(&entry.tree)?;
            let keep = read_config(&tree, &self.objects).map(|c| c.check.keep).unwrap_or_default();
            let dir = self.data.join("deploy").join(repo).join(name);
            crate::check::materialize(&self.objects, &tree, &dir, &keep)?;
            say(format!("runs {run}"));
            let mut watch = |line: &str| {
                let line = colors.replace_all(line, "");
                if !line.trim().is_empty() {
                    say(line.trim().chars().take(200).collect());
                }
            };
            Ok(crate::check::run_deploy(&dir, &run, &mut watch))
        };
        let mut step = ran().unwrap_or_else(|e| Step { name: String::new(), command: command.to_string(), exit: -1, seconds: 0.0, output: format!("{e:#}") });
        step.name = format!("deploy {name}");
        let exit = step.exit;
        let plain = colors.replace_all(&step.output, "");
        let tail = plain.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        let last = tail.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(200).collect::<String>();
        let proof = Proof { id: new_id("proof"), task: None, base_position: p.position, candidate_tree: entry.tree.clone(), files: vec![], steps: vec![step], verdict: if exit == 0 { Verdict::Green } else { Verdict::Red }, blamed: vec![], created_at: now_ms() };
        let db = self.db();
        db.insert_proof(repo, &proof)?;
        let rollback = Self::rollback_from(&db, repo, name, p.position)?;
        if exit != 0 {
            let failed = format!("failed (exit {exit}): {last}");
            db.deploy_finished(&d.id, Some(exit), None, &failed)?;
            self.emit_told(&db, repo, "env.failed", entry.task.as_deref(), rolled_back(serde_json::json!({ "env": name, "position": p.position, "by": p.by, "exit": exit, "proof": proof.id, "output": tail }), rollback))?;
            let serves = db.env(repo, name)?.map(|e| e.position).filter(|s| *s >= 0);
            drop(db);
            say(failed);
            let stays = serves.map(|s| format!("still serves position {s}")).unwrap_or_else(|| "serves nothing yet".into());
            anyhow::bail!("the deploy of position {} to {name} failed (exit {exit}), {name} {stays}. `avp proof {}` has its output, which ends with:\n{tail}", p.position, proof.id);
        }
        db.deploy_finished(&d.id, Some(0), None, &format!("serves position {}", p.position))?;
        let env = db.set_env(repo, name, p.position)?;
        self.emit_told(&db, repo, "env.moved", entry.task.as_deref(), rolled_back(serde_json::json!({ "env": name, "position": p.position, "by": p.by, "proof": proof.id }), rollback))?;
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).remove(&(repo.to_string(), name.to_string()));
        Ok(env)
    }

    // ---- notifications ----

    /// Emits an event, and tells the webhooks of `[notify]` when a person must hear of it.
    fn emit_told(&self, db: &Db, repo: &str, kind: &str, task: Option<&str>, data: serde_json::Value) -> Result<()> {
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

    // ---- scheduled suites ----

    /// Starts the scheduled suites (`[[check.schedule]]`) as their time comes: a look every 20
    /// seconds, for as long as the server runs.
    pub fn run_schedules(&self) {
        let e = self.clone();
        std::thread::spawn(move || loop {
            if let Err(err) = e.scheduled(now_ms()) {
                eprintln!("scheduled suites: {err:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(20));
        });
    }

    /// Starts, on the head of each repo, the suites its avp.toml schedules whose time has come at
    /// `now` (ms): once a day, within the hour after its `at` (UTC), so that a server down at that
    /// minute still runs it once it is back. A `line.scheduled` event says it started; its red is told
    /// like any red. Returns the repos and positions it started on.
    pub fn scheduled(&self, now: i64) -> Result<Vec<(String, i64)>> {
        const DAY: i64 = 86_400_000;
        let mut started = Vec::new();
        // Read first: the loop takes the database again.
        let repos = self.db().repos()?;
        for repo in repos {
            let Some(head) = self.db().head(&repo.id)? else { continue };
            // An avp.toml that does not read: every proof of the repo says why.
            let Ok(cfg) = read_config(&self.objects.get_tree(&head.tree)?, &self.objects) else { continue };
            for s in cfg.check.schedule {
                let today = now - now.rem_euclid(DAY) + s.minute * 60_000;
                let due = if today > now { today - DAY } else { today };
                if now - due >= 3_600_000 {
                    continue;
                }
                let key = (repo.id.clone(), format!("{} {}", s.at, s.shards.join(",")));
                let mut ran = SCHEDULED.lock().unwrap_or_else(|e| e.into_inner());
                // Read back once per server: one that started it today before a restart did run it.
                let last = match ran.get(&key) {
                    Some(at) => *at,
                    None => self.db().events_since(&repo.id, 0, i64::MAX)?.into_iter().rev().find(|e| e.kind == "line.scheduled" && e.data["at"] == s.at.as_str() && e.data["shards"] == serde_json::json!(s.shards)).map_or(i64::MIN, |e| e.at),
                };
                ran.insert(key, if last >= due { last } else { now });
                drop(ran);
                if last >= due {
                    continue;
                }
                self.db().emit(&repo.id, "line.scheduled", head.task.as_deref(), None, serde_json::json!({ "position": head.position, "at": s.at, "shards": s.shards }))?;
                self.full_suite_after(&repo.id, head.position, head.tree.clone(), false, Some(s));
                started.push((repo.id.clone(), head.position));
            }
        }
        Ok(started)
    }
}

/// An environment's event, marked as a rollback's when it is one, with the position it came from.
fn rolled_back(mut data: serde_json::Value, from: Option<i64>) -> serde_json::Value {
    if let Some(from) = from {
        data["rollback"] = true.into();
        data["from"] = from.into();
    }
    data
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
