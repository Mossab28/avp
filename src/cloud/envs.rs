//! Environments: what each one serves, promotions and rollbacks by the rules avp.toml sets (a green
//! full suite, `after`, `approvers`), the deploys AVP runs itself (`run`) or GitHub's bridge carries
//! out (`github_workflow`), what an environment is observed serving, and those that follow the
//! full suite.

use crate::check::{read_config, Deploy};
use crate::db::{Db, DeployRun};
use crate::engine::Engine;
use crate::model::*;
use crate::server::{blocking, kept_alive, ok, rid, verified_person, R, S};
use anyhow::{anyhow, Result};
use axum::extract::{Path as P, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) fn routes() -> Router<Engine> {
    Router::new()
        .route("/repos/{repo}/envs", get(envs))
        .route("/repos/{repo}/envs/{name}", post(promote))
        .route("/repos/{repo}/envs/{name}/rollback", post(rollback))
        .route("/repos/{repo}/envs/{name}/observed", post(observe_env))
        .route("/repos/{repo}/envs/{name}/progress", post(env_progress))
}

/// What the deploys keep in memory, beside the engine (`seam::State`).
#[derive(Default)]
pub struct Deploys {
    /// The step each environment's deploy is at: as the bridge last read it (it sends it again
    /// every half minute), or the last line a deploy AVP runs printed. A deploy AVP runs keeps its
    /// state in the database too (`deploys`), its step written at most once a second.
    progress: Mutex<HashMap<(String, String), DeployProgress>>,
    /// The deploys AVP runs itself (`run`): one at a time per environment.
    deploy_locks: Mutex<HashMap<(String, String), Arc<Mutex<()>>>>,
}

impl Engine {
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

    /// The environments avp.toml declares, read from the head of the line.
    pub fn env_config(&self, repo: &str) -> Result<std::collections::BTreeMap<String, crate::check::EnvConfig>> {
        Ok(self.config(repo)?.envs)
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
        self.cloud.deploys.progress.lock().unwrap_or_else(|e| e.into_inner()).remove(&(repo.to_string(), name.to_string()));
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
            let live = self.cloud.deploys.progress.lock().unwrap_or_else(|p| p.into_inner()).get(&(repo.to_string(), name.clone())).cloned();
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
        self.cloud.deploys.progress.lock().unwrap_or_else(|e| e.into_inner()).insert((repo.to_string(), name.to_string()), p.clone());
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
        let lock = self.cloud.deploys.deploy_locks.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_default().clone();
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
        self.cloud.deploys.progress.lock().unwrap_or_else(|e| e.into_inner()).remove(&(repo.to_string(), name.to_string()));
        Ok(env)
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

#[derive(Deserialize)]
struct Observed {
    sha: String,
}

async fn observe_env(State(e): S, P((repo, name)): P<(String, String)>, Json(req): Json<Observed>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.observe_env(&r, &name, &req.sha) }).await?)
}

async fn env_progress(State(e): S, P((repo, name)): P<(String, String)>, Json(req): Json<DeployProgress>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.deploy_progress(&r, &name, req) }).await?)
}

async fn envs(State(e): S, P(repo): P<String>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.envs_view(&r) }).await?)
}

#[derive(Deserialize)]
struct Promote {
    /// None: what the environment's `after` serves, or the head.
    #[serde(default)]
    position: Option<i64>,
    #[serde(default)]
    force: bool,
    /// The field's name before `force`: false forced.
    #[serde(default = "yes")]
    require_full: bool,
    #[serde(default)]
    person: Option<String>,
    #[serde(default)]
    agent: Option<String>,
}

fn yes() -> bool {
    true
}

/// Answers once a deploy AVP runs is over, which may take longer than a proxy waits on a silent
/// connection: kept alive as a land is.
async fn promote(State(e): S, P((repo, name)): P<(String, String)>, headers: HeaderMap, Json(req): Json<Promote>) -> Response {
    let person = verified_person(&headers).or(req.person).unwrap_or_else(|| "someone".into());
    let agent = req.agent.unwrap_or_else(|| "human".into());
    let force = req.force || !req.require_full;
    kept_alive(&headers, move || {
        let r = rid(&e, &repo)?;
        Ok((StatusCode::OK, serde_json::to_value(e.promote(&r, &name, req.position, force, &person, &agent)?)?))
    })
    .await
}

#[derive(Deserialize)]
struct Rollback {
    #[serde(default)]
    person: Option<String>,
    #[serde(default)]
    agent: Option<String>,
}

/// Back to the position the environment served before the one it serves now, kept alive as a
/// promotion is. A route of its own: a server from before it answers 404, not a promotion.
async fn rollback(State(e): S, P((repo, name)): P<(String, String)>, headers: HeaderMap, Json(req): Json<Rollback>) -> Response {
    let person = verified_person(&headers).or(req.person).unwrap_or_else(|| "someone".into());
    let agent = req.agent.unwrap_or_else(|| "human".into());
    kept_alive(&headers, move || {
        let r = rid(&e, &repo)?;
        Ok((StatusCode::OK, serde_json::to_value(e.rollback(&r, &name, &person, &agent)?)?))
    })
    .await
}
