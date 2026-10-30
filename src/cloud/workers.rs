//! Proof workers, the server's side. With AVP_WORKERS=1 the server does not run a proof itself: it
//! queues the candidate, a `avp worker` (src/worker.rs) claims it and sends its steps back. One proof
//! per worker, so a proof never shares its machine with another one, and capacity is the number of
//! workers. If no worker claims a proof in time, or a worker goes silent, the server runs the proof
//! itself, as before. Here too the drain (`POST /drain`), what the server's own machine asks before
//! it takes a new version, and `GET /work`, what an autoscaler follows.

use crate::engine::Engine;
use crate::model::*;
use crate::server::{blocking, err, ok, R, S};
use crate::worker::{Claimed, Job};
use super::auth::token_user;
use anyhow::Result;
use axum::extract::{Path as P, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{mpsc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) fn routes() -> Router<Engine> {
    Router::new()
        .route("/work/claim", post(work_claim))
        .route("/work/{id}/result", post(work_result))
        .route("/work/{id}/alive", post(work_alive))
        .route("/work", get(work_depth))
        .route("/drain", get(drain_state).post(drain).delete(undrain))
}

/// Where a proof runs (the seam's `prove_away`): on a worker when the server has them, None when
/// it proves it itself, or when no worker answers in time. A speculative proof goes to a worker or
/// nowhere: the server does not spend itself on a guess.
pub(crate) fn prove_away(job: Job, speculative: bool) -> Result<Option<Vec<Step>>> {
    let Some(d) = dispatcher() else { return Ok(None) };
    let name = job.name.clone();
    // Listed until a worker claims it: a land the drain holds in the queue hears why.
    let _queued = crate::check::queued(&job.id, &name);
    if let Some(steps) = d.run(job) {
        return Ok(Some(steps));
    }
    anyhow::ensure!(!speculative, "no worker took the speculative proof");
    eprintln!("no worker answered for {name}: proving here");
    Ok(None)
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Job>,
    /// Claimed jobs and the last time their worker gave a sign of life.
    claimed: HashMap<String, (Instant, Job)>,
    replies: HashMap<String, mpsc::Sender<Vec<Step>>>,
    /// Claimed jobs whose tree is no longer the task's: their worker hears it at its next sign of
    /// life and stops.
    cancelled: HashSet<String>,
}

/// What a proof stopped halfway gives back: one step, cancelled.
fn cancelled_steps() -> Vec<Step> {
    vec![Step { name: "cancelled".into(), command: String::new(), exit: crate::check::CANCELLED, seconds: 0.0, output: "cancelled: the task's files changed, a newer tree is being proven".into() }]
}

pub struct Dispatcher {
    q: Mutex<Queue>,
    cv: Condvar,
}

#[derive(Serialize)]
pub struct Depth {
    pub waiting: usize,
    pub running: usize,
    /// Workers wanted right now (waiting + running): what an autoscaler follows.
    pub demand: usize,
}

static DISPATCHER: OnceLock<Option<Dispatcher>> = OnceLock::new();

/// The queue, when the server runs with AVP_WORKERS=1.
pub fn dispatcher() -> Option<&'static Dispatcher> {
    DISPATCHER
        .get_or_init(|| (std::env::var("AVP_WORKERS").ok().as_deref() == Some("1")).then(|| Dispatcher { q: Mutex::new(Queue::default()), cv: Condvar::new() }))
        .as_ref()
}

fn secs(var: &str, default: u64) -> Duration {
    Duration::from_secs(std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
}

impl Dispatcher {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.q.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues the job and waits for a worker's steps. A worker silent for AVP_WORKER_LOST_S
    /// (default 30: it beats every 2 s) loses the job, which goes back to the front of the queue;
    /// a worker can die mid-proof, or before its claim even reached it. None when nobody claimed
    /// it within AVP_WORKER_CLAIM_S (default 120), after 3 lost workers, or past
    /// AVP_WORKER_RUN_S (default 2400): the caller then proves it itself.
    pub fn run(&self, job: Job) -> Option<Vec<Step>> {
        let (claim_within, run_within, lost_after) = (secs("AVP_WORKER_CLAIM_S", 120), secs("AVP_WORKER_RUN_S", 2400), secs("AVP_WORKER_LOST_S", 30));
        let mut lost = 0;
        let (tx, rx) = mpsc::channel();
        let id = job.id.clone();
        {
            let mut q = self.lock();
            q.replies.insert(id.clone(), tx);
            q.waiting.push_back(job);
        }
        self.cv.notify_all();
        let start = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(steps) => return Some(steps),
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let mut q = self.lock();
                    if q.claimed.get(&id).map(|(t, _)| t.elapsed() > lost_after).unwrap_or(false) {
                        let (_, job) = q.claimed.remove(&id).unwrap();
                        lost += 1;
                        eprintln!("{id}: worker lost ({lost}), back in the queue");
                        q.waiting.push_front(job);
                        self.cv.notify_all();
                    }
                    let unclaimed = q.waiting.iter().any(|j| j.id == id);
                    if (unclaimed && start.elapsed() > claim_within) || lost >= 3 || start.elapsed() > run_within {
                        q.waiting.retain(|j| j.id != id);
                        q.claimed.remove(&id);
                        q.replies.remove(&id);
                        return None;
                    }
                }
            }
        }
    }

    /// Stops the proofs of `name` (a task) that prove another tree than `tree_hash`: a waiting one
    /// leaves the queue, a running one is let go here and its worker told to stop. Either way the
    /// caller waiting on it gets a cancelled step at once.
    pub fn cancel_other(&self, name: &str, tree_hash: &str) {
        let mut q = self.lock();
        let stale = |j: &Job| j.name == name && j.tree_hash != tree_hash;
        let waiting: Vec<String> = q.waiting.iter().filter(|j| stale(j)).map(|j| j.id.clone()).collect();
        let running: Vec<String> = q.claimed.iter().filter(|(_, (_, j))| stale(j)).map(|(id, _)| id.clone()).collect();
        q.waiting.retain(|j| !stale(j));
        for id in waiting.iter().chain(running.iter()) {
            q.claimed.remove(id);
            if let Some(tx) = q.replies.remove(id) {
                let _ = tx.send(cancelled_steps());
            }
        }
        q.cancelled.extend(running);
    }

    /// Stops the other jobs of `name` on `tree_hash`: one part of the proof is red, the rest of
    /// it no longer counts. Their callers get a cancelled step at once.
    pub fn cancel_tree(&self, name: &str, tree_hash: &str) {
        let mut q = self.lock();
        let moot = |j: &Job| j.name == name && j.tree_hash == tree_hash;
        let waiting: Vec<String> = q.waiting.iter().filter(|j| moot(j)).map(|j| j.id.clone()).collect();
        let running: Vec<String> = q.claimed.iter().filter(|(_, (_, j))| moot(j)).map(|(id, _)| id.clone()).collect();
        q.waiting.retain(|j| !moot(j));
        for id in waiting.iter().chain(running.iter()) {
            q.claimed.remove(id);
            if let Some(tx) = q.replies.remove(id) {
                let _ = tx.send(cancelled_steps());
            }
        }
        q.cancelled.extend(running);
    }

    /// The oldest waiting job this worker can run, waiting up to `wait` for one: a worker from
    /// before parts never gets one, it would run the whole proof.
    pub fn claim(&self, wait: Duration, parts: bool) -> Option<Job> {
        let deadline = Instant::now() + wait;
        let mut q = self.lock();
        loop {
            if let Some(i) = q.waiting.iter().position(|j| parts || j.part.is_none()) {
                let job = q.waiting.remove(i).unwrap();
                q.claimed.insert(job.id.clone(), (Instant::now(), job.clone()));
                return Some(job);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            q = self.cv.wait_timeout(q, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// A worker's sign of life on a job it runs: (still its own, to be stopped).
    pub fn alive(&self, id: &str) -> (bool, bool) {
        let mut q = self.lock();
        if q.cancelled.remove(id) {
            return (false, true);
        }
        match q.claimed.get_mut(id) {
            Some(c) => {
                c.0 = Instant::now();
                (true, false)
            }
            None => (false, false),
        }
    }

    /// A worker's steps for a job. False when nobody waits for it any more (the server gave up
    /// and proved it itself).
    pub fn complete(&self, id: &str, steps: Vec<Step>) -> bool {
        let mut q = self.lock();
        q.claimed.remove(id);
        q.cancelled.remove(id);
        match q.replies.remove(id) {
            Some(tx) => tx.send(steps).is_ok(),
            None => false,
        }
    }

    pub fn depth(&self) -> Depth {
        let q = self.lock();
        Depth { waiting: q.waiting.len(), running: q.claimed.len(), demand: q.waiting.len() + q.claimed.len() }
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

impl Engine {
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
        match dispatcher() {
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
}

#[derive(Deserialize)]
struct Claim {
    #[serde(default)]
    wait_s: Option<u64>,
    /// The worker runs parts of a proof (AVP 0.4.7 and later).
    #[serde(default)]
    parts: bool,
}
/// A worker asks for a proof to run; answers 204 after `wait_s` (at most 55 s) if there is none,
/// or while the server drains.
async fn work_claim(State(e): S, Json(req): Json<Claim>) -> R {
    let Some(d) = dispatcher() else { return Err(err(anyhow::anyhow!("workers are off on this server (AVP_WORKERS=1 turns them on)"))) };
    let wait = std::time::Duration::from_secs(req.wait_s.unwrap_or(25).min(55));
    match blocking(move || Ok(claim_undrained(d, wait, req.parts).map(|job| with_secrets(&e, job)))).await? {
        Some(claimed) => ok(claimed),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// The job with the values of the secrets its steps name, read from the store as it is handed
/// over. A store that cannot be read sends none: the steps that name one say it did not reach them.
fn with_secrets(e: &Engine, job: Job) -> Claimed {
    let named = crate::check::read_config(&job.tree, &e.objects).map(|c| crate::check::secrets_named(&c.check, job.steps_override.as_deref(), job.part.as_ref())).unwrap_or_default();
    let secrets = if named.is_empty() {
        Default::default()
    } else {
        super::secrets::SecretStore::new(&e.data).values(&job.repo, &named).unwrap_or_else(|err| {
            eprintln!("{}: secrets not read: {err:#}", job.id);
            Default::default()
        })
    };
    Claimed { job, secrets }
}

/// The oldest job this worker can run, within `wait`, none while the server drains. It waits by
/// slices of a second, each one after a look at the drain: a drain that begins while a claim waits
/// lets that claim's job go within a second at most, a job that then runs and counts as running,
/// which the updater waits for.
fn claim_undrained(d: &Dispatcher, wait: std::time::Duration, parts: bool) -> Option<Job> {
    let deadline = std::time::Instant::now() + wait;
    let left = || deadline.saturating_duration_since(std::time::Instant::now());
    while !left().is_zero() && crate::check::lifted_within(left()) {
        if let Some(job) = d.claim(left().min(std::time::Duration::from_secs(1)), parts) {
            crate::check::claimed(&job.id);
            return Some(job);
        }
    }
    None
}
#[derive(Deserialize)]
struct WorkResult {
    steps: Vec<Step>,
    /// Each step's whole log and artifacts, by `check::kept_key`: kept before the steps are handed
    /// on, so that the proof they make has them. A worker from before sends none.
    #[serde(default)]
    kept: HashMap<String, crate::check::KeptStep>,
}
async fn work_result(State(e): S, P(id): P<String>, Json(req): Json<WorkResult>) -> R {
    let Some(d) = dispatcher() else { return Err(err(anyhow::anyhow!("workers are off"))) };
    let kept = crate::check::Kept::new(&e.data);
    let accepted = blocking(move || {
        for (key, step) in &req.kept {
            if let Err(err) = kept.put(key, step, None) {
                eprintln!("{id}: a step's log not kept: {err:#}");
            }
        }
        Ok(d.complete(&id, req.steps))
    })
    .await?;
    ok(serde_json::json!({ "accepted": accepted }))
}
async fn work_alive(P(id): P<String>) -> R {
    let Some(d) = dispatcher() else { return Err(err(anyhow::anyhow!("workers are off"))) };
    let (alive, cancel) = d.alive(&id);
    ok(serde_json::json!({ "alive": alive, "cancel": cancel }))
}
/// The proofs waiting and running, here and on workers, the deploys AVP runs, and whether the
/// server drains (see Engine::work).
async fn work_depth(State(e): S) -> R {
    ok(blocking(move || e.work()).await?)
}

#[derive(Deserialize)]
struct DrainQuery {
    /// How long the drain lasts at most, in minutes: 10 by default, 60 at most.
    minutes: Option<f64>,
    /// The task whose proof the asker waits for.
    task: Option<String>,
}

fn drain_answer(task: Option<&str>) -> R {
    let left = crate::check::draining();
    ok(serde_json::json!({ "draining": left.is_some(), "left_s": left.map(|l| l.as_secs()), "held": task.is_some_and(crate::check::held) }))
}

/// Whether the server drains, and with `?task=` whether that task's proof waits for it: what a land
/// that waits tells its agent.
async fn drain_state(Query(q): Query<DrainQuery>) -> R {
    drain_answer(q.task.as_deref())
}

/// The server starts no new work until `DELETE /drain`, the bound, or its restart: what runs ends,
/// what comes waits (see check::drain). Its own machine asks for it, before it takes a new version.
async fn drain(Query(q): Query<DrainQuery>, req: axum::extract::Request) -> R {
    from_host(&req)?;
    let minutes = q.minutes.unwrap_or(10.0);
    if !(minutes > 0.0 && minutes <= 60.0) {
        return Err(err(anyhow::anyhow!("minutes: more than 0, 60 at most")));
    }
    crate::check::drain(std::time::Duration::from_secs_f64(minutes * 60.0));
    eprintln!("draining for {minutes} min: no new work starts");
    drain_answer(None)
}

async fn undrain(req: axum::extract::Request) -> R {
    from_host(&req)?;
    crate::check::undrain();
    eprintln!("drain lifted: the work it held starts");
    drain_answer(None)
}

/// What only the server's own machine may ask (the drain stops everyone's proofs): a request from a
/// loopback address that no proxy forwarded (the one in front of a shared server runs on the same
/// machine, and says whom it forwards), one with the server's token, or one of a server admin's.
fn from_host(req: &axum::extract::Request) -> Result<(), Response> {
    if req.extensions().get::<super::auth::Who>().is_some_and(|w| matches!(w, super::auth::Who::Person(u) if u.admin)) {
        return Ok(());
    }
    let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok());
    if std::env::var("AVP_TOKEN").ok().filter(|t| !t.is_empty()).is_some_and(|t| token_user(auth, &t).is_some()) {
        return Ok(());
    }
    let loopback = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().is_some_and(|c| c.0.ip().to_canonical().is_loopback());
    let forwarded = ["forwarded", "x-forwarded-for", "x-real-ip", "x-avp-user"].iter().any(|h| req.headers().contains_key(*h));
    if loopback && !forwarded {
        return Ok(());
    }
    Err((StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "only the server's own machine (127.0.0.1, no proxy) or its token may drain it" }))).into_response())
}
