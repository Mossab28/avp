//! The boundary between AVP's core and what a team's server adds: proof workers, accounts and
//! tokens, the full suite after landings, approvals and guarded paths, secrets, environments and
//! the GitHub deploy bridge, stats, AVP Live (src/cloud/, the `cloud` feature). Every default build
//! and every release binary carries it. The core calls the functions here and never names a cloud
//! module, so the cloud's folder moves as one. Built without the feature (`--no-default-features`),
//! each one does what the small avp does: proofs run on the server itself, nothing gates a
//! landing, nothing follows it, the server answers this machine alone, and the cloud's routes
//! answer that this avp left them out.

use crate::db::Db;
use crate::engine::Engine;
use crate::model::*;
use crate::worker::Job;
use anyhow::Result;
use axum::http::Extensions;
use axum::Router;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[cfg(feature = "cloud")]
use crate::cloud as side;
#[cfg(not(feature = "cloud"))]
use small as side;

/// How the small avp's answer on a route of the `cloud` feature ends, after what the route does: the
/// CLI and the MCP tools know it by this, and say what to do.
pub const CLOUD: &str = "left out of this avp: it was built without the cloud feature";

/// What the cloud keeps in memory beside the engine (the deploys in flight): nothing in the small avp.
pub use side::State;

/// The routes the cloud adds to the server's own.
pub fn routes() -> Router<Engine> {
    side::routes()
}

/// Who asks and whether they may, around every route: accounts and tokens.
pub fn access(router: Router<Engine>, engine: &Engine) -> Router<Engine> {
    side::access(router, engine)
}

/// Whether the server may listen on `addr`, which other machines reach: only where something
/// checks who is asking.
pub fn may_listen(addr: &str) -> Result<()> {
    side::may_listen(addr)
}

/// What the engine resumes as it opens: the deploys a restart cut.
pub fn opened(engine: &Engine) {
    side::opened(engine)
}

/// What runs beside the routes once the server starts: the scheduled suites, the deploys GitHub
/// carries out.
pub fn start(engine: &Engine) {
    side::start(engine)
}

/// The repos the asker may read: on a server with accounts, those they have a role on.
pub fn readable(db: &Db, repos: Vec<Repo>, asker: &Extensions) -> Result<Vec<Repo>> {
    side::readable(db, repos, asker)
}

/// A repo just created: on a server with accounts, the person who created it is its admin.
pub fn created(e: &Engine, repo: &Repo, asker: &Extensions) -> Result<()> {
    side::created(e, repo, asker)
}

/// Where a proof runs: its steps when it ran elsewhere (proof workers), None when this server runs
/// it itself.
pub fn prove_away(job: Job, speculative: bool) -> Result<Option<Vec<Step>>> {
    side::prove_away(job, speculative)
}

/// The proofs of `name` on another tree than `tree_hash` are worth nothing now: stopped wherever
/// they run away from this server.
pub fn cancel_other(name: &str, tree_hash: &str) {
    side::cancel_other(name, tree_hash)
}

/// One part of the proof of `name` on `tree_hash` is red: its other parts away from this server stop.
pub fn cancel_tree(name: &str, tree_hash: &str) {
    side::cancel_tree(name, tree_hash)
}

/// The values of the secrets a proof's steps name, from the server's store in `data`: the small avp
/// keeps none (a step that names one says it is not set).
pub fn secret_values(data: &Path, repo: &str, names: &BTreeSet<String>) -> Result<BTreeMap<String, String>> {
    side::secret_values(data, repo, names)
}

/// The secrets a task that changes the checks may give its steps, until a person approves it as
/// written: None, every one its checks name.
pub fn line_secrets(e: &Engine, task: &Task, tree: &Tree) -> Result<Option<Vec<String>>> {
    side::line_secrets(e, task, tree)
}

/// What holds back a task whose proof is done: a person's approval (the landing rule and guarded
/// paths). None: it lands when green.
pub fn landing_gate(e: &Engine, task: &Task, writes: &[Write], candidate: &Tree, proof: &Proof) -> Result<Option<NotLanded>> {
    side::landing_gate(e, task, writes, candidate, proof)
}

/// What follows a landing at `position`: the full suite, and the environments that follow it.
pub fn after_landing(e: &Engine, task: &Task, position: i64, tree_hash: &str) {
    side::after_landing(e, task, position, tree_hash)
}

#[cfg(not(feature = "cloud"))]
mod small {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::{any, MethodRouter};
    use axum::Json;

    #[derive(Default)]
    pub struct State;

    /// A route of the cloud, as the small avp answers it: 404, and what it is.
    fn cloud(what: &'static str) -> MethodRouter<Engine> {
        any(move || async move { (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": format!("{what} {CLOUD}") }))) })
    }

    pub(super) fn routes() -> Router<Engine> {
        let (accounts, envs) = (cloud("accounts and tokens are"), cloud("environments are"));
        Router::new()
            .route("/work", cloud("proof workers are"))
            .route("/work/{*rest}", cloud("proof workers are"))
            .route("/drain", cloud("draining the server is"))
            .route("/repos/{repo}/full", cloud("the full suite after a landing is"))
            .route("/repos/{repo}/tasks/{task}/approve", cloud("a person's approval is"))
            .route("/repos/{repo}/landing", cloud("the landing rule is"))
            .route("/repos/{repo}/guarded", cloud("guarded paths are"))
            .route("/repos/{repo}/secrets", cloud("secrets for tests are"))
            .route("/repos/{repo}/secrets/{*rest}", cloud("secrets for tests are"))
            .route("/repos/{repo}/envs", envs.clone())
            .route("/repos/{repo}/envs/{*rest}", envs)
            .route("/repos/{repo}/stats", cloud("stats are"))
            .route("/repos/{repo}/before", cloud("stats are"))
            .route("/repos/{repo}/roles", accounts.clone())
            .route("/live", cloud("AVP Live is"))
            .route("/me", accounts.clone())
            .route("/users", accounts.clone())
            .route("/users/{*rest}", accounts.clone())
            .route("/tokens", accounts.clone())
            .route("/tokens/{*rest}", accounts.clone())
            .route("/accounts/{*rest}", accounts.clone())
            .route("/session", accounts.clone())
            .route("/auth", accounts.clone())
            .route("/auth/{*rest}", accounts)
    }

    pub(super) fn access(router: Router<Engine>, _: &Engine) -> Router<Engine> {
        router
    }

    pub(super) fn may_listen(addr: &str) -> Result<()> {
        anyhow::bail!(
            "{addr} is reachable from other machines, and this avp checks nobody who asks (accounts and tokens are {CLOUD}): \
             run the release binaries (install.sh) or a default `cargo build`, which check AVP_TOKEN, listen on 127.0.0.1, \
             or set AVP_INSECURE=1 if this network is yours alone"
        )
    }

    pub(super) fn opened(_: &Engine) {}

    pub(super) fn start(_: &Engine) {
        if std::env::var("AVP_TOKEN").is_ok_and(|t| !t.is_empty()) {
            eprintln!("AVP_TOKEN is not checked here: accounts and tokens are {CLOUD}, and this server answers its own machine (the release binaries check it)");
        }
    }

    pub(super) fn readable(_: &Db, repos: Vec<Repo>, _: &Extensions) -> Result<Vec<Repo>> {
        Ok(repos)
    }

    pub(super) fn created(_: &Engine, _: &Repo, _: &Extensions) -> Result<()> {
        Ok(())
    }

    pub(super) fn prove_away(_: Job, _: bool) -> Result<Option<Vec<Step>>> {
        Ok(None)
    }

    pub(super) fn cancel_other(_: &str, _: &str) {}

    pub(super) fn cancel_tree(_: &str, _: &str) {}

    pub(super) fn secret_values(_: &Path, _: &str, _: &BTreeSet<String>) -> Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    pub(super) fn line_secrets(_: &Engine, _: &Task, _: &Tree) -> Result<Option<Vec<String>>> {
        Ok(None)
    }

    pub(super) fn landing_gate(_: &Engine, _: &Task, _: &[Write], _: &Tree, _: &Proof) -> Result<Option<NotLanded>> {
        Ok(None)
    }

    pub(super) fn after_landing(_: &Engine, _: &Task, _: i64, _: &str) {}
}
