//! What a team's server adds to the core: proof workers, accounts and tokens, the full suite,
//! approvals, secrets, environments, stats, AVP Live. The `cloud` feature, on in every default
//! build and release binary; `--no-default-features` leaves it out. The core reaches it through
//! `crate::seam` alone, and this module is that seam's cloud side: the core never names what is in
//! this folder, so the folder moves as one.

pub mod approvals;
pub mod auth;
pub mod envs;
pub mod full;
pub mod github;
pub mod secrets;
pub mod stats;
pub mod workers;

use crate::db::Db;
use crate::engine::Engine;
use crate::model::*;
use crate::worker::Job;
use anyhow::Result;
use axum::http::Extensions;
use axum::routing::get;
use axum::Router;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What the cloud keeps in memory beside the engine.
#[derive(Default)]
pub struct State {
    pub(crate) deploys: envs::Deploys,
}

pub(crate) fn routes() -> Router<Engine> {
    Router::new()
        // AVP Live: the line as it moves, read from this API by the browser (behind the same auth).
        .route("/live", get(|| async { ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], include_str!("live.html")) }))
        .merge(workers::routes())
        .merge(full::routes())
        .merge(approvals::routes())
        .merge(secrets::routes())
        .merge(envs::routes())
        .merge(stats::routes())
        .merge(auth::routes())
}

/// Who asks and whether they may, for every route (see auth::gate): with no user on the server,
/// its one token when it has one, as before accounts.
pub(crate) fn access(router: Router<Engine>, engine: &Engine) -> Router<Engine> {
    router.layer(axum::middleware::from_fn_with_state(auth::Gate::new(engine.clone()), auth::gate))
}

pub(crate) fn may_listen(addr: &str) -> Result<()> {
    anyhow::ensure!(
        std::env::var("AVP_TOKEN").is_ok_and(|t| !t.is_empty()),
        "{addr} is reachable from other machines, and nothing would check who is asking: set AVP_TOKEN \
         (clients use https://name:TOKEN@host), or listen on 127.0.0.1 behind a proxy that authenticates, \
         or set AVP_INSECURE=1 if this network is yours alone"
    );
    Ok(())
}

pub(crate) fn opened(engine: &Engine) {
    if let Err(e) = engine.resume_deploys() {
        eprintln!("deploys left unfinished: {e:#}");
    }
}

pub(crate) fn start(engine: &Engine) {
    // Secrets sealed with blake3 by a server from before XChaCha20-Poly1305, resealed before a proof opens one.
    match secrets::SecretStore::new(&engine.data).reseal_old() {
        Ok(said) => said.iter().for_each(|line| eprintln!("{line}")),
        Err(e) => eprintln!("secrets not resealed: {e:#}"),
    }
    engine.run_schedules();
    // GitHub's side of the deploys and of avp/full, when the server has AVP_GITHUB_BRIDGE=1.
    github::start(engine.clone());
}

pub(crate) fn readable(db: &Db, repos: Vec<Repo>, asker: &Extensions) -> Result<Vec<Repo>> {
    auth::readable(db, repos, asker.get::<auth::Who>())
}

pub(crate) fn created(e: &Engine, repo: &Repo, asker: &Extensions) -> Result<()> {
    auth::created(e, repo, asker.get::<auth::Who>())
}

pub(crate) fn prove_away(job: Job, speculative: bool) -> Result<Option<Vec<Step>>> {
    workers::prove_away(job, speculative)
}

pub(crate) fn cancel_other(name: &str, tree_hash: &str) {
    if let Some(d) = workers::dispatcher() {
        d.cancel_other(name, tree_hash);
    }
}

pub(crate) fn cancel_tree(name: &str, tree_hash: &str) {
    if let Some(d) = workers::dispatcher() {
        d.cancel_tree(name, tree_hash);
    }
}

pub(crate) fn secret_values(data: &Path, repo: &str, names: &BTreeSet<String>) -> Result<BTreeMap<String, String>> {
    secrets::SecretStore::new(data).values(repo, names)
}

pub(crate) fn line_secrets(e: &Engine, task: &Task, tree: &Tree) -> Result<Option<Vec<String>>> {
    e.line_secrets(task, tree)
}

pub(crate) fn landing_gate(e: &Engine, task: &Task, writes: &[Write], candidate: &Tree, proof: &Proof) -> Result<Option<NotLanded>> {
    approvals::landing_gate(e, task, writes, candidate, proof)
}

pub(crate) fn after_landing(e: &Engine, task: &Task, position: i64, tree_hash: &str) {
    full::after_landing(e, task, position, tree_hash)
}
