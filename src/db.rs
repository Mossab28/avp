//! The relational half: tasks, writes, leases, proofs, the line, events, environments. SQLite,
//! one connection behind a mutex. Objects live next door (`objects.rs`).

use crate::model::*;
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;

pub struct Db {
    pub conn: Connection,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS repos (
  id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, policy TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS line (
  repo TEXT NOT NULL, position INTEGER NOT NULL, task TEXT, tree TEXT NOT NULL,
  landed_at INTEGER NOT NULL, proof TEXT, full TEXT, note TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (repo, position)
);
CREATE TABLE IF NOT EXISTS tasks (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL, goal TEXT NOT NULL, actor TEXT NOT NULL, scope TEXT NOT NULL,
  base_position INTEGER NOT NULL, base_tree TEXT NOT NULL, state TEXT NOT NULL,
  created_at INTEGER NOT NULL, landed_position INTEGER
);
CREATE TABLE IF NOT EXISTS writes (
  task TEXT NOT NULL, seq INTEGER NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL, blob TEXT,
  mode INTEGER NOT NULL, at INTEGER NOT NULL, undone INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (task, seq)
);
CREATE INDEX IF NOT EXISTS writes_path ON writes (task, path);
CREATE TABLE IF NOT EXISTS leases (
  repo TEXT NOT NULL, path TEXT NOT NULL, task TEXT NOT NULL,
  taken_at INTEGER NOT NULL, renewed_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
  PRIMARY KEY (repo, path)
);
CREATE TABLE IF NOT EXISTS proofs (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL, task TEXT, base_position INTEGER NOT NULL,
  candidate_tree TEXT NOT NULL, files TEXT NOT NULL, steps TEXT NOT NULL, verdict TEXT NOT NULL,
  blamed TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, at INTEGER NOT NULL, kind TEXT NOT NULL,
  task TEXT, path TEXT, data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_repo ON events (repo, id);
CREATE INDEX IF NOT EXISTS events_kind ON events (repo, kind, id);
CREATE TABLE IF NOT EXISTS approvals (
  task TEXT PRIMARY KEY, person TEXT NOT NULL, approved INTEGER NOT NULL, reason TEXT NOT NULL, at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS guarded (
  repo TEXT NOT NULL, pattern TEXT NOT NULL, PRIMARY KEY (repo, pattern)
);
CREATE TABLE IF NOT EXISTS envs (
  repo TEXT NOT NULL, name TEXT NOT NULL, position INTEGER NOT NULL, moved_at INTEGER NOT NULL,
  PRIMARY KEY (repo, name)
);
CREATE TABLE IF NOT EXISTS promotions (
  repo TEXT NOT NULL, env TEXT NOT NULL, body TEXT NOT NULL,
  PRIMARY KEY (repo, env)
);
CREATE TABLE IF NOT EXISTS deploys (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL, env TEXT NOT NULL, promotion TEXT NOT NULL, command TEXT NOT NULL,
  attempt INTEGER NOT NULL, started_at INTEGER, step TEXT NOT NULL, step_at INTEGER NOT NULL,
  finished_at INTEGER, exit INTEGER, reason TEXT
);
CREATE INDEX IF NOT EXISTS deploys_env ON deploys (repo, env);
CREATE TABLE IF NOT EXISTS before_avp (
  repo TEXT PRIMARY KEY, body TEXT NOT NULL, saved_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tags (
  repo TEXT NOT NULL, name TEXT NOT NULL, position INTEGER NOT NULL, person TEXT NOT NULL, at INTEGER NOT NULL,
  git TEXT, deleted INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (repo, name)
);
-- People, tokens and roles: AVP Cloud's accounts. Every server has the tables, so a data folder
-- moves between a free server and a cloud one as it is.
CREATE TABLE IF NOT EXISTS users (
  name TEXT PRIMARY KEY, email TEXT, github TEXT, admin INTEGER NOT NULL, created_at INTEGER NOT NULL, created_by TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS users_github ON users (github);
CREATE TABLE IF NOT EXISTS tokens (
  id TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, person TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
  created_at INTEGER NOT NULL, expires_at INTEGER, revoked_at INTEGER, used_at INTEGER
);
CREATE INDEX IF NOT EXISTS tokens_person ON tokens (person);
CREATE TABLE IF NOT EXISTS roles (
  repo TEXT NOT NULL, person TEXT NOT NULL, role TEXT NOT NULL, PRIMARY KEY (repo, person)
);
"#;

/// A name for a position of the line (`v1.2`): what a release is called, shown by `avp log` and AVP
/// Live, written as a git tag on the position's commit by the mirror, promoted by name.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Tag {
    pub name: String,
    pub position: i64,
    /// Who named it, when.
    pub person: String,
    pub at: i64,
    /// The position's goal.
    #[serde(default)]
    pub note: String,
    /// The position's git commit, once the mirror wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The commit the mirror last wrote the git tag on: a tag moved or deleted since is moved or
    /// deleted in git by the next mirror pass, and git never loses a tag AVP did not write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// Deleted (`avp tag -d`), kept until the mirror has deleted its git tag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
}

/// A deploy AVP runs itself (`run`), from its promotion to its end. Kept in the database, not only
/// in memory: a server that restarts finds the deploys the one before it left unfinished.
#[derive(Clone, Debug)]
pub struct DeployRun {
    pub id: String,
    pub repo: String,
    pub env: String,
    pub promotion: Promotion,
    pub command: String,
    /// 1, then 2 once a restart of the server cut the first run.
    pub attempt: i64,
    /// None while it waits for the deploy before it.
    pub started_at: Option<i64>,
    pub step: String,
    pub step_at: i64,
    pub finished_at: Option<i64>,
    pub exit: Option<i32>,
    /// Why it ended without an exit of its own: cut by restarts, gone past by a newer promotion.
    pub reason: Option<String>,
}

fn deploy_row(r: &Row) -> rusqlite::Result<DeployRun> {
    Ok(DeployRun {
        id: r.get(0)?,
        repo: r.get(1)?,
        env: r.get(2)?,
        promotion: serde_json::from_str(&r.get::<_, String>(3)?).map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?,
        command: r.get(4)?,
        attempt: r.get(5)?,
        started_at: r.get(6)?,
        step: r.get(7)?,
        step_at: r.get(8)?,
        finished_at: r.get(9)?,
        exit: r.get(10)?,
        reason: r.get(11)?,
    })
}
const DEPLOY_COLS: &str = "id, repo, env, promotion, command, attempt, started_at, step, step_at, finished_at, exit, reason";

fn policy_str(p: LeasePolicy) -> &'static str {
    match p {
        LeasePolicy::Advisory => "advisory",
        LeasePolicy::Strict => "strict",
    }
}
fn policy_of(s: &str) -> LeasePolicy {
    if s == "strict" { LeasePolicy::Strict } else { LeasePolicy::Advisory }
}
fn landing_str(r: LandingRule) -> &'static str {
    match r {
        LandingRule::Proof => "proof",
        LandingRule::ProofAndApproval => "proof_and_approval",
    }
}
fn landing_of(s: &str) -> LandingRule {
    if s == "proof_and_approval" { LandingRule::ProofAndApproval } else { LandingRule::Proof }
}
fn state_str(s: TaskState) -> &'static str {
    match s {
        TaskState::Open => "open",
        TaskState::Landed => "landed",
        TaskState::Abandoned => "abandoned",
    }
}
fn state_of(s: &str) -> TaskState {
    match s {
        "landed" => TaskState::Landed,
        "abandoned" => TaskState::Abandoned,
        _ => TaskState::Open,
    }
}
fn kind_str(k: WriteKind) -> &'static str {
    match k {
        WriteKind::Put => "put",
        WriteKind::Delete => "delete",
        WriteKind::Conflict => "conflict",
    }
}
fn kind_of(s: &str) -> WriteKind {
    match s {
        "delete" => WriteKind::Delete,
        "conflict" => WriteKind::Conflict,
        _ => WriteKind::Put,
    }
}
pub fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Green => "green",
        Verdict::Red => "red",
        Verdict::Blocked => "blocked",
        Verdict::Flaky => "flaky",
    }
}
fn verdict_of(s: &str) -> Verdict {
    match s {
        "green" => Verdict::Green,
        "blocked" => Verdict::Blocked,
        "flaky" => Verdict::Flaky,
        _ => Verdict::Red,
    }
}

fn task_row(r: &Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        repo: r.get(1)?,
        goal: r.get(2)?,
        actor: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
        scope: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        base_position: r.get(5)?,
        base_tree: r.get(6)?,
        state: state_of(&r.get::<_, String>(7)?),
        created_at: r.get(8)?,
        landed_position: r.get(9)?,
        transcript: r.get(10)?,
        similar: vec![],
        on: r.get(11)?,
    })
}
const TASK_COLS: &str = "id, repo, goal, actor, scope, base_position, base_tree, state, created_at, landed_position, transcript, on_task";

fn write_row(r: &Row) -> rusqlite::Result<Write> {
    Ok(Write {
        task: r.get(0)?,
        seq: r.get(1)?,
        path: r.get(2)?,
        kind: kind_of(&r.get::<_, String>(3)?),
        blob: r.get(4)?,
        mode: r.get(5)?,
        at: r.get(6)?,
        undone: r.get::<_, i64>(7)? != 0,
    })
}
const WRITE_COLS: &str = "task, seq, path, kind, blob, mode, at, undone";

fn proof_row(r: &Row) -> rusqlite::Result<Proof> {
    Ok(Proof {
        id: r.get(0)?,
        task: r.get(1)?,
        base_position: r.get(2)?,
        candidate_tree: r.get(3)?,
        files: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        steps: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
        verdict: verdict_of(&r.get::<_, String>(6)?),
        blamed: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
        created_at: r.get(8)?,
    })
}
const PROOF_COLS: &str = "id, task, base_position, candidate_tree, files, steps, verdict, blamed, created_at";

fn line_row(r: &Row) -> rusqlite::Result<LineEntry> {
    Ok(LineEntry {
        position: r.get(0)?,
        task: r.get(1)?,
        tree: r.get(2)?,
        landed_at: r.get(3)?,
        proof: r.get(4)?,
        full: r.get::<_, Option<String>>(5)?.map(|s| verdict_of(&s)),
        note: r.get(6)?,
        git: r.get(7)?,
        reverted: r.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str(&s).ok()),
    })
}
const LINE_COLS: &str = "position, task, tree, landed_at, proof, full, note, git, reverted";

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        // Older databases: the landing rule column.
        let _ = conn.execute("ALTER TABLE repos ADD COLUMN landing TEXT NOT NULL DEFAULT 'proof'", []);
        // Older databases: the git commit of a position (import, export).
        let _ = conn.execute("ALTER TABLE line ADD COLUMN git TEXT", []);
        // Older databases: where a task's conversation lives.
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN transcript TEXT", []);
        // Older databases: what took a position back, when AVP reverted it.
        let _ = conn.execute("ALTER TABLE line ADD COLUMN reverted TEXT", []);
        // Older databases: the task a task is stacked on.
        let _ = conn.execute("ALTER TABLE tasks ADD COLUMN on_task TEXT", []);
        conn.execute("CREATE INDEX IF NOT EXISTS tasks_on ON tasks (on_task)", [])?;
        Ok(Self { conn })
    }

    /// Records the git commit whose tree is this position. Idempotent: the same sha again is fine,
    /// another sha is refused (the export is deterministic, so two exporters agree).
    pub fn set_line_git(&self, repo: &str, position: i64, sha: &str) -> Result<()> {
        let current: Option<Option<String>> = self
            .conn
            .query_row("SELECT git FROM line WHERE repo = ?1 AND position = ?2", params![repo, position], |r| r.get(0))
            .optional()?;
        match current {
            None => Err(anyhow!("no position {position}")),
            Some(Some(s)) if s == sha => Ok(()),
            Some(Some(s)) => Err(anyhow!("position {position} is already git commit {s}, not {sha}")),
            Some(None) => {
                self.conn.execute("UPDATE line SET git = ?3 WHERE repo = ?1 AND position = ?2", params![repo, position, sha])?;
                Ok(())
            }
        }
    }

    /// The position whose git commit is `sha` (full or a prefix of at least 7).
    pub fn position_of_git(&self, repo: &str, sha: &str) -> Result<Option<i64>> {
        anyhow::ensure!(sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()), "not a git sha: {sha}");
        Ok(self
            .conn
            .query_row("SELECT position FROM line WHERE repo = ?1 AND git LIKE ?2 || '%' ORDER BY position DESC LIMIT 1", params![repo, sha], |r| r.get(0))
            .optional()?)
    }

    /// The last position at or below `at` that has a git commit.
    pub fn last_git(&self, repo: &str, at: i64) -> Result<Option<LineEntry>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {LINE_COLS} FROM line WHERE repo = ?1 AND position <= ?2 AND git IS NOT NULL ORDER BY position DESC LIMIT 1"), params![repo, at], line_row)
            .optional()?)
    }

    pub fn set_landing(&self, repo: &str, rule: LandingRule) -> Result<()> {
        self.conn.execute("UPDATE repos SET landing = ?2 WHERE id = ?1", params![repo, landing_str(rule)])?;
        Ok(())
    }

    pub fn set_approval(&self, a: &Approval) -> Result<()> {
        self.conn.execute(
            "INSERT INTO approvals (task, person, approved, reason, at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (task) DO UPDATE SET person = excluded.person, approved = excluded.approved, reason = excluded.reason, at = excluded.at",
            params![a.task, a.person, a.approved as i64, a.reason, a.at],
        )?;
        Ok(())
    }

    pub fn approval(&self, task: &str) -> Result<Option<Approval>> {
        Ok(self
            .conn
            .query_row("SELECT task, person, approved, reason, at FROM approvals WHERE task = ?1", params![task], |r| {
                Ok(Approval { task: r.get(0)?, person: r.get(1)?, approved: r.get::<_, i64>(2)? != 0, reason: r.get(3)?, at: r.get(4)? })
            })
            .optional()?)
    }

    // ---- guarded paths ----
    pub fn guarded(&self, repo: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT pattern FROM guarded WHERE repo = ?1 ORDER BY pattern")?;
        let rows = st.query_map(params![repo], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn guard(&self, repo: &str, add: &[String], remove: &[String]) -> Result<()> {
        for p in add {
            self.conn.execute("INSERT OR IGNORE INTO guarded (repo, pattern) VALUES (?1, ?2)", params![repo, p])?;
        }
        for p in remove {
            self.conn.execute("DELETE FROM guarded WHERE repo = ?1 AND pattern = ?2", params![repo, p])?;
        }
        Ok(())
    }

    // ---- repos ----
    pub fn create_repo(&self, name: &str, policy: LeasePolicy) -> Result<Repo> {
        let repo = Repo { id: new_id("repo"), name: name.to_string(), policy, landing: LandingRule::Proof, created_at: now_ms() };
        self.conn.execute(
            "INSERT INTO repos (id, name, policy, landing, created_at) VALUES (?1, ?2, ?3, 'proof', ?4)",
            params![repo.id, repo.name, policy_str(policy), repo.created_at],
        )?;
        Ok(repo)
    }

    /// By id or by name.
    pub fn repo(&self, key: &str) -> Result<Repo> {
        self.conn
            .query_row(
                "SELECT id, name, policy, created_at, landing FROM repos WHERE id = ?1 OR name = ?1",
                params![key],
                |r| Ok(Repo { id: r.get(0)?, name: r.get(1)?, policy: policy_of(&r.get::<_, String>(2)?), created_at: r.get(3)?, landing: landing_of(&r.get::<_, String>(4)?) }),
            )
            .optional()?
            .ok_or_else(|| anyhow!("no repo {key}"))
    }

    pub fn repos(&self) -> Result<Vec<Repo>> {
        let mut st = self.conn.prepare("SELECT id, name, policy, created_at, landing FROM repos ORDER BY created_at")?;
        let rows = st.query_map([], |r| Ok(Repo { id: r.get(0)?, name: r.get(1)?, policy: policy_of(&r.get::<_, String>(2)?), created_at: r.get(3)?, landing: landing_of(&r.get::<_, String>(4)?) }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- line ----
    pub fn head(&self, repo: &str) -> Result<Option<LineEntry>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {LINE_COLS} FROM line WHERE repo = ?1 ORDER BY position DESC LIMIT 1"), params![repo], line_row)
            .optional()?)
    }

    pub fn line_at(&self, repo: &str, position: i64) -> Result<LineEntry> {
        self.conn
            .query_row(&format!("SELECT {LINE_COLS} FROM line WHERE repo = ?1 AND position = ?2"), params![repo, position], line_row)
            .optional()?
            .ok_or_else(|| anyhow!("no position {position}"))
    }

    pub fn line(&self, repo: &str, from: i64, limit: i64) -> Result<Vec<LineEntry>> {
        let mut st = self.conn.prepare(&format!("SELECT {LINE_COLS} FROM line WHERE repo = ?1 AND position <= ?2 ORDER BY position DESC LIMIT ?3"))?;
        let rows = st.query_map(params![repo, from, limit], line_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Appends a position; `expected_head` is the compare-and-swap.
    pub fn append_line(&self, repo: &str, expected_head: i64, task: Option<&str>, tree: &str, proof: Option<&str>, note: &str) -> Result<LineEntry> {
        let head = self.head(repo)?.map(|h| h.position).unwrap_or(-1);
        anyhow::ensure!(head == expected_head, "line moved: head is {head}, expected {expected_head}");
        let entry = LineEntry { position: head + 1, task: task.map(String::from), tree: tree.to_string(), landed_at: now_ms(), proof: proof.map(String::from), full: None, note: note.to_string(), git: None, reverted: None };
        self.conn.execute(
            "INSERT INTO line (repo, position, task, tree, landed_at, proof, full, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
            params![repo, entry.position, entry.task, entry.tree, entry.landed_at, entry.proof, entry.note],
        )?;
        Ok(entry)
    }

    /// The newest position whose whole suite is green, or flaky (green on its retry).
    pub fn newest_green(&self, repo: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT MAX(position) FROM line WHERE repo = ?1 AND full IN ('green', 'flaky')", params![repo], |r| r.get(0))?)
    }

    pub fn set_full(&self, repo: &str, position: i64, verdict: Verdict, proof: &str) -> Result<()> {
        self.conn.execute("UPDATE line SET full = ?3, proof = COALESCE(proof, ?4) WHERE repo = ?1 AND position = ?2", params![repo, position, verdict_str(verdict), proof])?;
        Ok(())
    }

    /// AVP took the position back: what did, and why.
    pub fn set_reverted(&self, repo: &str, position: i64, r: &Reverted) -> Result<()> {
        self.conn.execute("UPDATE line SET reverted = ?3 WHERE repo = ?1 AND position = ?2", params![repo, position, serde_json::to_string(r)?])?;
        Ok(())
    }

    // ---- tasks ----
    pub fn insert_task(&self, task: &Task) -> Result<()> {
        self.conn.execute(
            &format!("INSERT INTO tasks ({TASK_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"),
            params![task.id, task.repo, task.goal, serde_json::to_string(&task.actor)?, serde_json::to_string(&task.scope)?, task.base_position, task.base_tree, state_str(task.state), task.created_at, task.landed_position, task.transcript, task.on],
        )?;
        Ok(())
    }

    pub fn task(&self, id: &str) -> Result<Task> {
        self.conn
            .query_row(&format!("SELECT {TASK_COLS} FROM tasks WHERE id = ?1"), params![id], task_row)
            .optional()?
            .ok_or_else(|| anyhow!("no task {id}"))
    }

    pub fn tasks(&self, repo: &str, state: Option<TaskState>) -> Result<Vec<Task>> {
        let mut st = self.conn.prepare(&format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ?1 AND (?2 IS NULL OR state = ?2) ORDER BY created_at"))?;
        let rows = st.query_map(params![repo, state.map(state_str)], task_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The open tasks stacked on this one, oldest first.
    pub fn stacked_on(&self, task: &str) -> Result<Vec<Task>> {
        let mut st = self.conn.prepare(&format!("SELECT {TASK_COLS} FROM tasks WHERE on_task = ?1 AND state = 'open' ORDER BY created_at"))?;
        let rows = st.query_map(params![task], task_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stacks a task on another: it stands on that one's tree from now on.
    pub fn stack_task(&self, id: &str, on: &str, tree: &str) -> Result<()> {
        self.conn.execute("UPDATE tasks SET on_task = ?2, base_tree = ?3 WHERE id = ?1", params![id, on, tree])?;
        Ok(())
    }

    pub fn rebase_task(&self, id: &str, position: i64, tree: &str) -> Result<()> {
        self.conn.execute("UPDATE tasks SET base_position = ?2, base_tree = ?3 WHERE id = ?1", params![id, position, tree])?;
        Ok(())
    }

    pub fn set_task_goal(&self, id: &str, goal: &str) -> Result<()> {
        self.conn.execute("UPDATE tasks SET goal = ?2 WHERE id = ?1", params![id, goal])?;
        Ok(())
    }

    pub fn set_task_state(&self, id: &str, state: TaskState, landed: Option<i64>) -> Result<()> {
        self.conn.execute("UPDATE tasks SET state = ?2, landed_position = COALESCE(?3, landed_position) WHERE id = ?1", params![id, state_str(state), landed])?;
        Ok(())
    }

    // ---- writes ----
    pub fn append_write(&self, task: &str, path: &str, kind: WriteKind, blob: Option<&str>, mode: u32) -> Result<Write> {
        let seq: i64 = self.conn.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM writes WHERE task = ?1", params![task], |r| r.get(0))?;
        let w = Write { task: task.to_string(), seq, path: path.to_string(), kind, blob: blob.map(String::from), mode, at: now_ms(), undone: false };
        self.conn.execute(
            "INSERT INTO writes (task, seq, path, kind, blob, mode, at, undone) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![w.task, w.seq, w.path, kind_str(kind), w.blob, w.mode, w.at],
        )?;
        Ok(w)
    }

    pub fn writes(&self, task: &str) -> Result<Vec<Write>> {
        let mut st = self.conn.prepare(&format!("SELECT {WRITE_COLS} FROM writes WHERE task = ?1 ORDER BY seq"))?;
        let rows = st.query_map(params![task], write_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Marks every live write on one path undone: the path goes back to the base.
    pub fn undo_path(&self, task: &str, path: &str) -> Result<usize> {
        Ok(self.conn.execute("UPDATE writes SET undone = 1 WHERE task = ?1 AND path = ?2 AND undone = 0", params![task, path])?)
    }

    /// Marks every write after `seq` undone. `seq = 0` undoes everything.
    pub fn undo_after(&self, task: &str, seq: i64) -> Result<usize> {
        Ok(self.conn.execute("UPDATE writes SET undone = 1 WHERE task = ?1 AND seq > ?2 AND undone = 0", params![task, seq])?)
    }

    // ---- leases ----
    pub fn lease(&self, repo: &str, path: &str) -> Result<Option<Lease>> {
        Ok(self
            .conn
            .query_row("SELECT path, task, taken_at, renewed_at, expires_at FROM leases WHERE repo = ?1 AND path = ?2", params![repo, path], |r| {
                Ok(Lease { path: r.get(0)?, task: r.get(1)?, taken_at: r.get(2)?, renewed_at: r.get(3)?, expires_at: r.get(4)? })
            })
            .optional()?)
    }

    pub fn leases_of(&self, task: &str) -> Result<Vec<Lease>> {
        let mut st = self.conn.prepare("SELECT path, task, taken_at, renewed_at, expires_at FROM leases WHERE task = ?1 ORDER BY path")?;
        let rows = st.query_map(params![task], |r| Ok(Lease { path: r.get(0)?, task: r.get(1)?, taken_at: r.get(2)?, renewed_at: r.get(3)?, expires_at: r.get(4)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_lease(&self, repo: &str, path: &str, task: &str, ttl_ms: i64) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO leases (repo, path, task, taken_at, renewed_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?4, ?5)
             ON CONFLICT (repo, path) DO UPDATE SET task = excluded.task, taken_at = CASE WHEN leases.task = excluded.task THEN leases.taken_at ELSE excluded.taken_at END, renewed_at = excluded.renewed_at, expires_at = excluded.expires_at",
            params![repo, path, task, now, now + ttl_ms],
        )?;
        Ok(())
    }

    pub fn release_leases(&self, task: &str, paths: Option<&[String]>) -> Result<usize> {
        match paths {
            None => Ok(self.conn.execute("DELETE FROM leases WHERE task = ?1", params![task])?),
            Some(paths) => {
                let mut n = 0;
                for p in paths {
                    n += self.conn.execute("DELETE FROM leases WHERE task = ?1 AND path = ?2", params![task, p])?;
                }
                Ok(n)
            }
        }
    }

    // ---- proofs ----
    pub fn insert_proof(&self, repo: &str, proof: &Proof) -> Result<()> {
        self.conn.execute(
            &format!("INSERT INTO proofs (repo, {PROOF_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
            params![repo, proof.id, proof.task, proof.base_position, proof.candidate_tree, serde_json::to_string(&proof.files)?, serde_json::to_string(&proof.steps)?, verdict_str(proof.verdict), serde_json::to_string(&proof.blamed)?, proof.created_at],
        )?;
        Ok(())
    }

    pub fn proof(&self, id: &str) -> Result<Option<Proof>> {
        Ok(self.conn.query_row(&format!("SELECT {PROOF_COLS} FROM proofs WHERE id = ?1"), params![id], proof_row).optional()?)
    }

    /// A proof of this repo; one of another repo is not found, as one that does not exist.
    pub fn proof_in(&self, repo: &str, id: &str) -> Result<Option<Proof>> {
        Ok(self.conn.query_row(&format!("SELECT {PROOF_COLS} FROM proofs WHERE id = ?1 AND repo = ?2"), params![id, repo], proof_row).optional()?)
    }

    pub fn last_proof(&self, task: &str) -> Result<Option<Proof>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {PROOF_COLS} FROM proofs WHERE task = ?1 ORDER BY created_at DESC LIMIT 1"), params![task], proof_row)
            .optional()?)
    }

    // ---- events ----
    pub fn emit(&self, repo: &str, kind: &str, task: Option<&str>, path: Option<&str>, data: serde_json::Value) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (repo, at, kind, task, path, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![repo, now_ms(), kind, task, path, data.to_string()],
        )?;
        Ok(())
    }

    pub fn events_since(&self, repo: &str, since: i64, limit: i64) -> Result<Vec<Event>> {
        let mut st = self.conn.prepare("SELECT id, at, kind, task, path, data FROM events WHERE repo = ?1 AND id > ?2 ORDER BY id LIMIT ?3")?;
        let rows = st.query_map(params![repo, since, limit], |r| {
            Ok(Event { id: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, task: r.get(3)?, path: r.get(4)?, data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null) })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- stats ----
    /// The positions tasks landed in [since, until), oldest first: imports are not landings.
    pub fn landed_between(&self, repo: &str, since: i64, until: i64) -> Result<Vec<LineEntry>> {
        let mut st = self.conn.prepare(&format!("SELECT {LINE_COLS} FROM line WHERE repo = ?1 AND task IS NOT NULL AND landed_at >= ?2 AND landed_at < ?3 ORDER BY position"))?;
        let rows = st.query_map(params![repo, since, until], line_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The proofs made in [since, until), for what they cost: each step's name, exit and seconds,
    /// read inside SQLite without its output (a full suite's weighs megabytes), save a split step's,
    /// which holds the time of each of its shares. Files and blame come back empty.
    pub fn proof_times(&self, repo: &str, since: i64, until: i64) -> Result<Vec<Proof>> {
        let mut st = self.conn.prepare(
            "SELECT id, task, base_position, verdict, created_at, (SELECT json_group_array(json_object('name', json_extract(s.value, '$.name'), 'command', '', \
             'exit', json_extract(s.value, '$.exit'), 'seconds', json_extract(s.value, '$.seconds'), 'output', \
             CASE WHEN json_extract(s.value, '$.output') GLOB '[0-9]* jobs side by side*' THEN json_extract(s.value, '$.output') ELSE '' END)) FROM json_each(proofs.steps) s) \
             FROM proofs WHERE repo = ?1 AND created_at >= ?2 AND created_at < ?3 ORDER BY created_at",
        )?;
        let rows = st.query_map(params![repo, since, until], |r| {
            Ok(Proof {
                id: r.get(0)?,
                task: r.get(1)?,
                base_position: r.get(2)?,
                candidate_tree: String::new(),
                files: vec![],
                steps: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                verdict: verdict_of(&r.get::<_, String>(3)?),
                blamed: vec![],
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The events of these kinds from `since` on, oldest first.
    pub fn events_of(&self, repo: &str, kinds: &[&str], since: i64) -> Result<Vec<Event>> {
        let mut st = self.conn.prepare("SELECT id, at, kind, task, path, data FROM events WHERE repo = ?1 AND at >= ?2 AND kind IN (SELECT value FROM json_each(?3)) ORDER BY id")?;
        let rows = st.query_map(params![repo, since, serde_json::to_string(kinds)?], |r| {
            Ok(Event { id: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, task: r.get(3)?, path: r.get(4)?, data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null) })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- environments ----
    pub fn envs(&self, repo: &str) -> Result<Vec<Environment>> {
        let mut st = self.conn.prepare("SELECT name, position, moved_at FROM envs WHERE repo = ?1 ORDER BY name")?;
        let rows = st.query_map(params![repo], |r| Ok(Environment { name: r.get(0)?, position: r.get(1)?, moved_at: r.get(2)?, wanted: None, deploy: None, progress: None }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn env(&self, repo: &str, name: &str) -> Result<Option<Environment>> {
        Ok(self.conn.query_row("SELECT name, position, moved_at FROM envs WHERE repo = ?1 AND name = ?2", params![repo, name], |r| Ok(Environment { name: r.get(0)?, position: r.get(1)?, moved_at: r.get(2)?, wanted: None, deploy: None, progress: None })).optional()?)
    }

    pub fn promotion(&self, repo: &str, env: &str) -> Result<Option<Promotion>> {
        let body: Option<String> = self.conn.query_row("SELECT body FROM promotions WHERE repo = ?1 AND env = ?2", params![repo, env], |r| r.get(0)).optional()?;
        Ok(match body { Some(b) => Some(serde_json::from_str(&b)?), None => None })
    }

    pub fn set_promotion(&self, repo: &str, env: &str, p: &Promotion) -> Result<()> {
        self.conn.execute(
            "INSERT INTO promotions (repo, env, body) VALUES (?1, ?2, ?3) ON CONFLICT (repo, env) DO UPDATE SET body = excluded.body",
            params![repo, env, serde_json::to_string(p)?],
        )?;
        Ok(())
    }

    // ---- the before AVP: one per repo, the last saved ----
    pub fn before(&self, repo: &str) -> Result<Option<crate::stats::Before>> {
        let body: Option<String> = self.conn.query_row("SELECT body FROM before_avp WHERE repo = ?1", params![repo], |r| r.get(0)).optional()?;
        Ok(match body { Some(b) => Some(serde_json::from_str(&b)?), None => None })
    }

    pub fn set_before(&self, repo: &str, b: &crate::stats::Before) -> Result<()> {
        self.conn.execute(
            "INSERT INTO before_avp (repo, body, saved_at) VALUES (?1, ?2, ?3) ON CONFLICT (repo) DO UPDATE SET body = excluded.body, saved_at = excluded.saved_at",
            params![repo, serde_json::to_string(b)?, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_env(&self, repo: &str, name: &str, position: i64) -> Result<Environment> {
        let env = Environment { name: name.to_string(), position, moved_at: now_ms(), wanted: None, deploy: None, progress: None };
        self.conn.execute(
            "INSERT INTO envs (repo, name, position, moved_at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (repo, name) DO UPDATE SET position = excluded.position, moved_at = excluded.moved_at",
            params![repo, env.name, env.position, env.moved_at],
        )?;
        Ok(env)
    }

    // ---- tags ----
    /// The repo's tags, newest position first; with `deleted`, those deleted whose git tag the
    /// mirror has not deleted yet too.
    pub fn tags(&self, repo: &str, deleted: bool) -> Result<Vec<Tag>> {
        let mut st = self.conn.prepare(
            "SELECT t.name, t.position, t.person, t.at, COALESCE(l.note, ''), l.git, t.git, t.deleted FROM tags t LEFT JOIN line l ON l.repo = t.repo AND l.position = t.position \
             WHERE t.repo = ?1 AND (?2 OR t.deleted = 0) ORDER BY t.position DESC, t.name",
        )?;
        let rows = st.query_map(params![repo, deleted], |r| {
            Ok(Tag { name: r.get(0)?, position: r.get(1)?, person: r.get(2)?, at: r.get(3)?, note: r.get(4)?, commit: r.get(5)?, git: r.get(6)?, deleted: r.get::<_, i64>(7)? != 0 })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Names a position, or moves the name (a deleted one comes back): the commit its git tag was
    /// written on stays, for the mirror to move it from there.
    pub fn set_tag(&self, repo: &str, name: &str, position: i64, person: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tags (repo, name, position, person, at) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (repo, name) DO UPDATE SET position = excluded.position, person = excluded.person, at = excluded.at, deleted = 0",
            params![repo, name, position, person, now_ms()],
        )?;
        Ok(())
    }

    /// Deletes a tag: at once when git never had it, else once the mirror has deleted its git tag.
    pub fn delete_tag(&self, repo: &str, name: &str) -> Result<()> {
        self.conn.execute("DELETE FROM tags WHERE repo = ?1 AND name = ?2 AND git IS NULL", params![repo, name])?;
        self.conn.execute("UPDATE tags SET deleted = 1 WHERE repo = ?1 AND name = ?2", params![repo, name])?;
        Ok(())
    }

    /// The mirror wrote the tag's git tag on `sha`, or deleted it (None): a deleted tag is gone then.
    pub fn set_tag_git(&self, repo: &str, name: &str, sha: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE tags SET git = ?3 WHERE repo = ?1 AND name = ?2", params![repo, name, sha])?;
        self.conn.execute("DELETE FROM tags WHERE repo = ?1 AND name = ?2 AND deleted = 1 AND git IS NULL", params![repo, name])?;
        Ok(())
    }

    // ---- deploys AVP runs ----
    /// A promotion's deploy, recorded with the promotion: waiting for its turn until it starts.
    pub fn insert_deploy(&self, repo: &str, env: &str, p: &Promotion, command: &str) -> Result<DeployRun> {
        let d = DeployRun { id: new_id("deploy"), repo: repo.into(), env: env.into(), promotion: p.clone(), command: command.into(), attempt: 1, started_at: None, step: String::new(), step_at: now_ms(), finished_at: None, exit: None, reason: None };
        self.conn.execute(
            &format!("INSERT INTO deploys ({DEPLOY_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, 1, NULL, '', ?6, NULL, NULL, NULL)"),
            params![d.id, repo, env, serde_json::to_string(p)?, command, d.step_at],
        )?;
        Ok(d)
    }

    pub fn deploy_started(&self, id: &str, step: &str) -> Result<()> {
        let now = now_ms();
        self.conn.execute("UPDATE deploys SET started_at = ?2, step = ?3, step_at = ?2 WHERE id = ?1", params![id, now, step])?;
        Ok(())
    }

    pub fn deploy_step(&self, id: &str, step: &str) -> Result<()> {
        self.conn.execute("UPDATE deploys SET step = ?2, step_at = ?3 WHERE id = ?1 AND finished_at IS NULL", params![id, step, now_ms()])?;
        Ok(())
    }

    /// Its end: the command's exit, or the reason it ended without one. A deploy already over
    /// stays as it ended.
    pub fn deploy_finished(&self, id: &str, exit: Option<i32>, reason: Option<&str>, step: &str) -> Result<()> {
        let now = now_ms();
        self.conn.execute("UPDATE deploys SET finished_at = ?2, exit = ?3, reason = ?4, step = ?5, step_at = ?2 WHERE id = ?1 AND finished_at IS NULL", params![id, now, exit, reason, step])?;
        Ok(())
    }

    /// The deploy runs again from the start, its next attempt, once its turn comes.
    pub fn deploy_again(&self, id: &str, step: &str) -> Result<()> {
        self.conn.execute("UPDATE deploys SET attempt = attempt + 1, started_at = NULL, step = ?2, step_at = ?3 WHERE id = ?1", params![id, step, now_ms()])?;
        Ok(())
    }

    /// Every deploy not over yet, running or waiting for its turn, oldest first.
    pub fn unfinished_deploys(&self) -> Result<Vec<DeployRun>> {
        let mut st = self.conn.prepare(&format!("SELECT {DEPLOY_COLS} FROM deploys WHERE finished_at IS NULL ORDER BY rowid"))?;
        let rows = st.query_map([], deploy_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The environment's newest deploy: the one of its last promotion.
    pub fn last_deploy(&self, repo: &str, env: &str) -> Result<Option<DeployRun>> {
        Ok(self.conn.query_row(&format!("SELECT {DEPLOY_COLS} FROM deploys WHERE repo = ?1 AND env = ?2 ORDER BY rowid DESC LIMIT 1"), params![repo, env], deploy_row).optional()?)
    }
}
