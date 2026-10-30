//! `avp worker`: the worker's side of proof workers, a client of the API as any verb is. A server
//! started with AVP_WORKERS=1 does not run a proof itself: it queues the candidate, a `avp worker`
//! (any machine that reaches the server: your own runners) claims it, lays it out with the same
//! code, runs the same steps and sends the steps back. What a step leaves beside its output (its
//! whole log, its artifacts) goes back with the steps. The queue is the server's
//! (src/cloud/workers.rs); a server built without the cloud feature answers that workers are left
//! out of it, and the worker stops there.

use crate::check::{Kept, KeptStep, Workspace};
use crate::client::Client;
use crate::model::*;
use crate::objects::Objects;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
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
            // A server built without the cloud has no queue: claiming again will not make one.
            Err(e) if crate::client::cloud_only(&e) => return Err(e),
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
