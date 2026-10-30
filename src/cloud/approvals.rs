//! A person in the loop: the repo's landing rule (`proof`, or `proof_and_approval`), its guarded
//! paths (avp.toml always among them), a person's word on a task, and the gate they put on a
//! landing. A task whose avp.toml names secrets the line's does not give is proven without them
//! until a person approves it as written.

use crate::check::read_config;
use crate::engine::{Engine, CHECK_FILES};
use crate::model::*;
use crate::server::{blocking, ok, own_task, rid, verified_person, R, S};
use anyhow::Result;
use axum::extract::{Path as P, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

pub(crate) fn routes() -> Router<Engine> {
    Router::new()
        .route("/repos/{repo}/tasks/{task}/approve", post(approve))
        .route("/repos/{repo}/landing", post(set_landing))
        .route("/repos/{repo}/guarded", post(guard).get(guarded))
}

/// What holds a landing back (the seam's `landing_gate`): a red proof made without secrets its
/// checks name and the line's do not give, until a person approves the task as written; a green
/// one, the repo's landing rule and its guarded paths. None: the core goes on (a red proof is red).
pub(crate) fn landing_gate(e: &Engine, task: &Task, writes: &[Write], candidate: &Tree, proof: &Proof) -> Result<Option<NotLanded>> {
    let withheld = if proof.verdict == Verdict::Red { e.withheld_secrets(task, candidate)? } else { vec![] };
    if !withheld.is_empty() && !e.approved_as_written(&task.id)? {
        // Red, proven without secrets its checks name and the line's do not give: a person's
        // word gives them, and `approve` proves it again with them.
        let note = format!("proven without {}, which its avp.toml names and the line's does not give: a person's approval gives them, and it is proven again", withheld.join(", "));
        let db = e.db();
        e.emit_told(&db, &task.repo, "task.awaiting_approval", Some(&task.id), serde_json::json!({ "proof": proof.id, "goal": task.goal, "secrets": withheld }))?;
        return Ok(Some(NotLanded { guarded: CHECK_FILES.iter().map(|f| f.to_string()).filter(|f| candidate.get(f.as_str()).is_some()).collect(), reason: "approval".into(), conflicts: vec![], proof: Some(proof.clone()), waits_for: vec![], word: None, note: Some(note), stack: vec![] }));
    }
    if proof.verdict != Verdict::Green {
        return Ok(None);
    }
    // The landing rule: a green proof may still need a person's word, for the whole repo or
    // because the task touches a guarded path (kept on the server, not in the tree: a task
    // cannot lift its own guard).
    let (rule, word, patterns) = {
        let db = e.db();
        (db.repo(&task.repo)?.landing, db.approval(&task.id)?, db.guarded(&task.repo)?)
    };
    // The checks themselves are always guarded: a task proven by its own checks may change
    // them, but only a person makes that change count.
    let base = e.objects.get_tree(&task.base_tree)?;
    let guarded: Vec<String> = Engine::effective_writes(writes)
        .keys()
        .filter(|p| patterns.iter().any(|g| path_matches(g, p)) || (CHECK_FILES.contains(&p.as_str()) && base.get(*p) != candidate.get(*p)))
        .cloned()
        .collect();
    if rule == LandingRule::ProofAndApproval || !guarded.is_empty() {
        match &word {
            Some(w) if w.approved && w.at >= proof.created_at.min(task.created_at) => {}
            Some(w) if !w.approved => {
                return Ok(Some(NotLanded { guarded: vec![], reason: "refused".into(), conflicts: vec![], proof: Some(proof.clone()), waits_for: vec![], word: word.clone(), note: None, stack: vec![] }));
            }
            _ => {
                let db = e.db();
                e.emit_told(&db, &task.repo, "task.awaiting_approval", Some(&task.id), serde_json::json!({ "proof": proof.id, "goal": task.goal, "guarded": guarded }))?;
                return Ok(Some(NotLanded { guarded: guarded.clone(), reason: "approval".into(), conflicts: vec![], proof: Some(proof.clone()), waits_for: vec![], word: None, note: None, stack: vec![] }));
            }
        }
    }
    Ok(None)
}

impl Engine {
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

    /// The secrets a task that changes the checks may give its steps: those the checks of the
    /// position it stands on give theirs, which its code already reaches, until a person approves
    /// it as it now is; then every one its checks name (avp migrate's first avp.toml names the
    /// secrets the tests need, and no line gave them yet). None when it leaves them alone (its
    /// avp.toml is the line's) or once approved.
    pub(crate) fn line_secrets(&self, task: &Task, tree: &Tree) -> Result<Option<Vec<String>>> {
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
}

#[derive(Deserialize)]
struct Guard {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

async fn guard(State(e): S, P(repo): P<String>, Json(req): Json<Guard>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.guard(&r, &req.add, &req.remove) }).await?)
}

async fn guarded(State(e): S, P(repo): P<String>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.db.lock().unwrap().guarded(&r) }).await?)
}

#[derive(Deserialize)]
struct Word {
    person: String,
    approved: bool,
    #[serde(default)]
    reason: String,
}

async fn approve(State(e): S, P((repo, task)): P<(String, String)>, headers: HeaderMap, Json(mut req): Json<Word>) -> R {
    if let Some(person) = verified_person(&headers) {
        req.person = person;
    }
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.approve(&task, &req.person, req.approved, &req.reason) }).await?)
}

#[derive(Deserialize)]
struct Landing {
    landing: LandingRule,
}

async fn set_landing(State(e): S, P(repo): P<String>, Json(req): Json<Landing>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.set_landing(&r, req.landing)?; e.db.lock().unwrap().repo(&r) }).await?)
}
