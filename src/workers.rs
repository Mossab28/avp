//! Proof workers. With AVP_WORKERS=1 the server does not run a proof itself: it queues the
//! candidate, a `avp worker` (any machine that reaches the server) claims it, lays it out with
//! the same code, runs the same steps and sends the steps back. One proof per worker, so a proof
//! never shares its machine with another one, and capacity is the number of workers. If no worker
//! claims a proof in time, or a worker goes silent, the server runs the proof itself, as before.
//! What a step leaves beside its output (its whole log, its artifacts) goes back with the steps.

use crate::check::{Kept, KeptStep, Workspace};
use crate::client::Client;
use crate::model::*;
use crate::objects::Objects;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{mpsc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// One proof to run: the candidate tree in full, so a worker needs nothing else but its blobs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub repo: String,
    pub tree: Tree,
    pub tree_hash: Hash,
    /// Whose proof it is: the task id, or `line-N` for a shard of the whole suite.
    pub name: String,
    pub files: Vec<String>,
    pub steps_override: Option<String>,
    /// One part of a task's proof, when its steps run as several jobs side by side. Only a
    /// worker that says it knows parts claims such a job.
    #[serde(default)]
    pub part: Option<crate::check::Part>,
}

/// A job as a worker claims it: with the values of the secrets its steps name (`[check] secrets`),
/// which the server adds as it hands the job over and never keeps in the queue. The worker gives
/// them to those steps and writes them nowhere. A worker from before secrets reads a plain job.
#[derive(Serialize, Deserialize)]
pub struct Claimed {
    #[serde(flatten)]
    pub job: Job,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
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

/// `avp worker`: claim a proof, fetch the blobs this machine lacks, run it with the same code
/// the server would, send the steps back, again. Errors become a failed step, never a lost job.
pub fn work(client: &Client, data: &Path) -> Result<()> {
    let objects = Objects::open(&data.join("objects"))?;
    eprintln!("avp worker: claiming proofs from {}", client.shown());
    // Asked to stop (a scale-down, a deploy): the file $AVP_WORKER_STOP appears, the worker claims
    // nothing more and leaves once the proof in hand is delivered. A container's stop hook turns
    // its SIGTERM into that file.
    let stop = std::env::var("AVP_WORKER_STOP").ok().map(std::path::PathBuf::from);
    loop {
        if stop.as_ref().is_some_and(|p| p.exists()) {
            eprintln!("avp worker: asked to stop, no proof in hand: leaving");
            return Ok(());
        }
        let claimed: Option<Claimed> = match client.post_maybe("/work/claim", &serde_json::json!({ "wait_s": 25, "parts": true })) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("claim: {e:#}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let Some(Claimed { job, secrets }) = claimed else { continue };
        let started = Instant::now();
        let run = || -> Result<Vec<Step>> {
            // A worker that just started (a node scaled up) lacks the whole tree: by batches.
            let missing: Vec<String> = job.tree.values().map(|e| e.blob.clone()).filter(|b| !objects.has(b)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            for chunk in missing.chunks(200) {
                for bytes in client.get_objects(&job.repo, chunk)? {
                    objects.put(&bytes)?;
                }
            }
            if !missing.is_empty() {
                eprintln!("{}: {} blobs fetched", job.id, missing.len());
            }
            let ws = Workspace::new(data, &job.repo).with_secrets(secrets.clone());
            Ok(ws.check(&objects, &job.tree, &job.tree_hash, &job.name, &job.files, job.steps_override.as_deref(), job.part.as_ref())?.0)
        };
        let done = std::sync::atomic::AtomicBool::new(false);
        let steps = std::thread::scope(|scope| {
            // A sign of life every 2 s while the proof runs, or the server gives the job away; its
            // answer is also how the worker hears that the proof is no longer wanted.
            scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let beat = client.post::<serde_json::Value>(&format!("/work/{}/alive", job.id), &serde_json::json!({}));
                    // The task's files changed: this tree is not worth finishing.
                    if beat.is_ok_and(|b| b["cancel"].as_bool() == Some(true)) {
                        crate::check::cancel_other(&job.name, "");
                        eprintln!("{}: cancelled by the server", job.id);
                    }
                    for _ in 0..4 {
                        if done.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            });
            let r = run();
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            r
        });
        let steps = steps.unwrap_or_else(|e| vec![Step { name: "worker".into(), command: String::new(), exit: 1, seconds: started.elapsed().as_secs_f64(), output: format!("the worker could not run this proof: {e:#}") }]);
        let verdict = if steps.iter().any(|s| s.exit == crate::check::CANCELLED) { "cancelled" } else if steps.iter().any(|s| s.exit != 0) { "red" } else { "green" };
        // Out of this machine's store before it is sent: the server keeps them from now on.
        let kept: BTreeMap<String, KeptStep> = steps.iter().map(crate::check::kept_key).filter_map(|k| Kept::new(data).take(&k).map(|s| (k, s))).collect();
        if let Err(e) = client.post::<serde_json::Value>(&format!("/work/{}/result", job.id), &serde_json::json!({ "steps": steps, "kept": kept })) {
            eprintln!("{}: result not delivered: {e:#}", job.id);
        }
        eprintln!("{} {verdict} in {:.0}s", job.id, started.elapsed().as_secs_f64());
    }
}
