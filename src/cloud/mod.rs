//! What a team's server adds to the core: proof workers, accounts and tokens, the full suite,
//! approvals, secrets, environments, stats, AVP Live, AVP Cloud's organizations and plans. The
//! `cloud` feature, on in every default build and release binary; `--no-default-features` leaves
//! it out. The core reaches it through `crate::seam` alone, and this module is that seam's cloud
//! side: the core never names what is in this folder, so the folder moves as one.

pub mod approvals;
pub mod auth;
pub mod envs;
pub mod full;
pub mod github;
pub mod orgs;
pub mod secrets;
pub mod settings;
pub mod stats;
pub mod workers;

pub(crate) use stats::left_out;

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
        .route("/live", get(|headers: axum::http::HeaderMap, uri: axum::http::Uri| async move { live_page(&headers, uri.query()) }))
        .route("/live.js", get(|| async { ([(axum::http::header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (axum::http::header::CACHE_CONTROL, "no-cache")], live_parts().1) }))
        .merge(workers::routes())
        .merge(full::routes())
        .merge(approvals::routes())
        .merge(secrets::routes())
        .merge(envs::routes())
        .merge(stats::routes())
        .merge(auth::routes())
        .merge(orgs::routes())
        .merge(settings::routes())
}

/// AVP Live's page and its script apart: the page then runs scripts of this server only, so a goal,
/// a name or a deploy's link that holds markup never runs in it, signed in as whoever reads it.
fn live_parts() -> &'static (String, &'static str) {
    static PARTS: std::sync::OnceLock<(String, &'static str)> = std::sync::OnceLock::new();
    PARTS.get_or_init(|| {
        let page = include_str!("live.html");
        match page.split_once("<script>").and_then(|(head, rest)| rest.split_once("</script>").map(|(script, tail)| (head, script, tail))) {
            Some((head, script, tail)) => (format!("{head}<script src=\"/live.js\"></script>{tail}"), script),
            None => (page.to_string(), ""),
        }
    })
}

/// AVP Live is a section of the website now: a browser signed in to it goes to the repo's Activity
/// (`/live?repo=acme/shop`), or to the home and its feed across repos. Signed out, on a server
/// without accounts, or for a script, `/live` is still the page itself.
fn live_moved(cookies: &str, query: Option<&str>) -> Option<String> {
    if !cookies.split(';').any(|kv| kv.trim().starts_with("avp_session=")) {
        return None;
    }
    let url = reqwest::Url::parse(&format!("http://avp/live?{}", query.unwrap_or(""))).ok()?;
    let repo = url.query_pairs().find(|(k, _)| k == "repo").map(|(_, v)| v.into_owned()).unwrap_or_default();
    // A name as repos have them, so the address it sends to is always one of this site's.
    let named = !repo.is_empty() && repo.split('/').all(|p| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)));
    Some(if named { format!("/r/{repo}/activity") } else { "/".into() })
}

fn live_page(headers: &axum::http::HeaderMap, query: Option<&str>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let cookies = headers.get_all(axum::http::header::COOKIE).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(";");
    if let Some(to) = live_moved(&cookies, query) {
        return (axum::http::StatusCode::SEE_OTHER, [(axum::http::header::LOCATION, to)]).into_response();
    }
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CONTENT_SECURITY_POLICY, "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'"),
        ],
        live_parts().0.as_str(),
    )
        .into_response()
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
    if let Err(e) = orgs::settle_spaces(&engine.db.lock().unwrap()) {
        eprintln!("repos from before spaces left in none: {e:#}");
    }
    if let Err(e) = settings::tables(&engine.db.lock().unwrap()) {
        eprintln!("repo settings unavailable: {e:#}");
    }
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
    // Deleted repos whose days are up go, now and every hour.
    let e = engine.clone();
    std::thread::spawn(move || loop {
        match settings::purge(&e) {
            Ok(gone) => gone.iter().for_each(|r| eprintln!("{r}: deleted {} days ago, its data is gone", settings::KEEP_DAYS)),
            Err(x) => eprintln!("deleted repos not purged: {x:#}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(3600));
    });
    // GitHub's side of the deploys and of avp/full, when the server has AVP_GITHUB_BRIDGE=1.
    github::start(engine.clone());
}

pub(crate) fn readable(db: &Db, repos: Vec<Repo>, asker: &Extensions) -> Result<Vec<Repo>> {
    auth::readable(db, repos, asker.get::<auth::Who>())
}

pub(crate) fn created(e: &Engine, repo: &Repo, asker: &Extensions) -> Result<()> {
    auth::created(e, repo, asker.get::<auth::Who>())
}

pub(crate) fn create_pushed(e: &Engine, key: &str, asker: &Extensions) -> Result<Repo> {
    match asker.get::<auth::Who>() {
        Some(who) if orgs::on() => orgs::create_repo(e, key, LeasePolicy::Advisory, who),
        who => {
            let repo = e.create_repo(key, LeasePolicy::Advisory)?;
            auth::created(e, &repo, who)?;
            Ok(repo)
        }
    }
}

pub(crate) fn proof_progress(name: &str) -> Option<String> {
    workers::progress(name)
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

pub(crate) fn waits_for_word(e: &Engine, task: &Task, writes: &[Write], candidate: &Tree) -> Result<Option<Vec<String>>> {
    approvals::waits_for_word(e, task, writes, candidate)
}

pub(crate) fn after_landing(e: &Engine, task: &Task, position: i64, tree_hash: &str) {
    full::after_landing(e, task, position, tree_hash)
}

#[cfg(test)]
mod tests {
    use super::live_moved;

    #[test]
    fn live_sends_a_signed_in_browser_to_the_repos_activity() {
        assert_eq!(live_moved("a=1; avp_session=x", Some("repo=acme%2Fshop")).as_deref(), Some("/r/acme/shop/activity"));
        assert_eq!(live_moved("avp_session=x", None).as_deref(), Some("/"));
        assert_eq!(live_moved("avp_session=x", Some("repo=..%2Fx")).as_deref(), Some("/"), "no address outside the repo's pages");
        assert_eq!(live_moved("avp_session=x", Some("repo=%2F%2Fevil.example")).as_deref(), Some("/"));
        assert_eq!(live_moved("", Some("repo=shop")), None, "signed out, the page itself");
    }
}
