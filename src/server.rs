//! HTTP surface. JSON in, JSON out, blobs as raw bytes. Every route is one engine verb.

use crate::engine::{Engine, LandOutcome};
use crate::model::*;
use axum::body::Bytes;
use axum::extract::{Path as P, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::HashMap;

pub(crate) type S = State<Engine>;
pub(crate) type R = Result<Response, Response>;

/// Routes take a repo id or its name.
pub(crate) fn rid(e: &Engine, key: &str) -> anyhow::Result<String> {
    Ok(e.db.lock().unwrap().repo(key)?.id)
}

/// A task of this repo. One of another repo answers as one that does not exist: "no task", never
/// a hint that the id is real.
pub(crate) fn own_task(e: &Engine, repo: &str, task: &str) -> anyhow::Result<()> {
    let r = rid(e, repo)?;
    let db = e.db.lock().unwrap();
    anyhow::ensure!(db.task(task)?.repo == r, "no task {task}");
    Ok(())
}

pub(crate) fn err(e: anyhow::Error) -> Response {
    let (code, body) = err_parts(&e);
    (code, Json(body)).into_response()
}

pub(crate) fn err_parts(e: &anyhow::Error) -> (StatusCode, serde_json::Value) {
    let msg = format!("{e:#}");
    let code = if msg.starts_with("no ") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
    (code, serde_json::json!({ "error": msg }))
}

/// An answer that may take minutes (a proof). A client that sends `x-avp-keepalive` gets a space
/// every 15 s while it is worked out, so that nothing between (a load balancer, a NAT) cuts a
/// connection it takes for idle, then `{"status": <code>, "body": <answer>}`. Any other client gets
/// the plain answer with its status. The work goes on to its end if the client leaves.
pub(crate) async fn kept_alive(headers: &HeaderMap, work: impl FnOnce() -> anyhow::Result<(StatusCode, serde_json::Value)> + Send + 'static) -> Response {
    let job = tokio::task::spawn_blocking(work);
    let parts = |out: Result<anyhow::Result<(StatusCode, serde_json::Value)>, tokio::task::JoinError>| match out {
        Ok(Ok(answer)) => answer,
        Ok(Err(e)) => err_parts(&e),
        Err(e) => err_parts(&anyhow::anyhow!(e)),
    };
    if !headers.contains_key("x-avp-keepalive") {
        let (status, body) = parts(job.await);
        return (status, Json(body)).into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    tokio::spawn(async move {
        let mut job = job;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        tick.tick().await;
        let out = loop {
            tokio::select! {
                out = &mut job => break out,
                _ = tick.tick() => { let _ = tx.send(Bytes::from_static(b" ")).await; }
            }
        };
        let (status, body) = parts(out);
        let _ = tx.send(Bytes::from(serde_json::json!({ "status": status.as_u16(), "body": body }).to_string())).await;
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx)) });
    Response::builder().header("content-type", "application/json").body(axum::body::Body::from_stream(stream)).unwrap()
}

pub(crate) fn ok<T: serde::Serialize>(v: T) -> R {
    Ok(Json(v).into_response())
}

pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(f).await.map_err(|e| err(anyhow::anyhow!(e)))?.map_err(err)
}

#[derive(Deserialize)]
struct CreateRepo {
    name: String,
    #[serde(default = "advisory")]
    policy: LeasePolicy,
}
fn advisory() -> LeasePolicy {
    LeasePolicy::Advisory
}

/// On a server with accounts, the person who creates a repo is its admin.
async fn create_repo(State(e): S, asker: axum::http::Extensions, Json(req): Json<CreateRepo>) -> R {
    ok(blocking(move || {
        let repo = e.create_repo(&req.name, req.policy)?;
        crate::seam::created(&e, &repo, &asker)?;
        Ok(repo)
    })
    .await?)
}
/// The repos the asker may read: on a server with accounts, those they have a role on.
async fn repos(State(e): S, asker: axum::http::Extensions) -> R {
    ok(blocking(move || {
        let db = e.db.lock().unwrap();
        crate::seam::readable(&db, db.repos()?, &asker)
    })
    .await?)
}
async fn repo(State(e): S, P(repo): P<String>) -> R {
    ok(blocking(move || e.db.lock().unwrap().repo(&repo)).await?)
}

async fn has_object(State(e): S, P((_, hash)): P<(String, String)>) -> StatusCode {
    if e.objects.has(&hash) { StatusCode::OK } else { StatusCode::NOT_FOUND }
}
async fn put_object(State(e): S, P((_, hash)): P<(String, String)>, body: Bytes) -> R {
    let got = blocking(move || e.objects.put(&body)).await?;
    if got != hash {
        return Err(err(anyhow::anyhow!("hash mismatch: body is {got}")));
    }
    ok(serde_json::json!({ "hash": got }))
}
async fn get_object(State(e): S, P((_, hash)): P<(String, String)>) -> R {
    let bytes = blocking(move || e.objects.get(&hash)).await?;
    Ok(bytes.into_response())
}

#[derive(Deserialize)]
struct Hashes {
    hashes: Vec<String>,
}

/// Several objects in one answer, in the order asked: `<hash> <length>\n` then the bytes, for each.
/// Laying a view out one object per request costs a round trip per file.
async fn get_objects(State(e): S, P(_): P<String>, Json(q): Json<Hashes>) -> R {
    let body = blocking(move || {
        anyhow::ensure!(q.hashes.len() <= 1000, "at most 1000 objects at once");
        let mut out = Vec::new();
        for h in &q.hashes {
            let bytes = e.objects.get(h)?;
            out.extend_from_slice(format!("{h} {}\n", bytes.len()).as_bytes());
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    })
    .await?;
    Ok(body.into_response())
}

#[derive(Deserialize)]
struct Import {
    entries: Vec<ImportEntry>,
    #[serde(default)]
    note: String,
    /// The git commit this tree comes from, when it comes from one.
    #[serde(default)]
    git: Option<String>,
}
async fn import(State(e): S, P(repo): P<String>, Json(req): Json<Import>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.import_git(&r, &req.entries, &req.note, req.git.as_deref()) }).await?)
}

#[derive(Deserialize)]
struct GitSha {
    sha: String,
}
async fn set_git(State(e): S, P((repo, position)): P<(String, i64)>, Json(req): Json<GitSha>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.set_git(&r, position, &req.sha) }).await?)
}
#[derive(Deserialize)]
struct MirrorEvent {
    blocked: bool,
    branch: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    position: i64,
}
async fn mirror_event(State(e): S, P(repo): P<String>, Json(req): Json<MirrorEvent>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.mirror_event(&r, req.blocked, &req.branch, req.commit.as_deref(), req.position)?; Ok(serde_json::json!({ "ok": true })) }).await?)
}

#[derive(Deserialize)]
struct TagsQuery {
    /// Those deleted whose git tag the mirror has yet to delete, too.
    #[serde(default)]
    deleted: bool,
}
async fn tags(State(e): S, P(repo): P<String>, Query(q): Query<TagsQuery>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.tags(&r, q.deleted) }).await?)
}
/// A tag's name rides in the body: `release/1.2` is one name, not two path segments.
#[derive(Deserialize)]
struct TagBody {
    name: String,
    #[serde(default)]
    position: Option<i64>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    person: Option<String>,
}
async fn tag(State(e): S, P(repo): P<String>, headers: HeaderMap, Json(b): Json<TagBody>) -> R {
    let person = verified_person(&headers).or(b.person).unwrap_or_else(|| "someone".into());
    ok(blocking(move || { let r = rid(&e, &repo)?; e.tag(&r, &b.name, b.position, b.force, &person) }).await?)
}
async fn untag(State(e): S, P(repo): P<String>, headers: HeaderMap, Json(b): Json<TagBody>) -> R {
    let person = verified_person(&headers).or(b.person).unwrap_or_else(|| "someone".into());
    ok(blocking(move || { let r = rid(&e, &repo)?; e.untag(&r, &b.name, &person) }).await?)
}
#[derive(Deserialize)]
struct TagGit {
    name: String,
    sha: Option<String>,
}
/// The mirror wrote a tag's git tag on a commit, or deleted it (`sha` null).
async fn tag_git(State(e): S, P(repo): P<String>, Json(b): Json<TagGit>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.tag_git(&r, &b.name, b.sha.as_deref())?; Ok(serde_json::json!({ "ok": true })) }).await?)
}
async fn last_git(State(e): S, P((repo, position)): P<(String, i64)>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.last_git(&r, position) }).await?)
}

#[derive(Deserialize)]
struct LineQuery {
    from: Option<i64>,
    limit: Option<i64>,
}
async fn line(State(e): S, P(repo): P<String>, Query(q): Query<LineQuery>) -> R {
    ok(blocking(move || {
        let repo = rid(&e, &repo)?;
        let db = e.db.lock().unwrap();
        let from = match q.from {
            Some(f) => f,
            None => db.head(&repo)?.map(|h| h.position).unwrap_or(-1),
        };
        db.line(&repo, from, q.limit.unwrap_or(30))
    })
    .await?)
}
async fn head(State(e): S, P(repo): P<String>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.head(&r) }).await?)
}
async fn tree(State(e): S, P((repo, position)): P<(String, i64)>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.tree_at(&r, position) }).await?)
}
async fn line_diff(State(e): S, P((repo, position)): P<(String, i64)>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.position_diff(&r, position) }).await?)
}
#[derive(Deserialize)]
struct DiffAsk {
    /// Each a position, `head`, or with a task, `base` and `task`.
    from: String,
    to: String,
    task: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    context: Option<usize>,
}
/// `avp diff` and avp_diff: from one tree to another (see Engine::diff_trees).
async fn diff(State(e): S, P(repo): P<String>, Json(a): Json<DiffAsk>) -> R {
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        if let Some(t) = &a.task {
            own_task(&e, &repo, t)?;
        }
        e.diff_trees(&r, a.task.as_deref(), &a.from, &a.to, &a.paths, a.context.unwrap_or(3))
    })
    .await?)
}

/// The line as a person reads it (AVP Live): each position with its task's person, agent, opening
/// and conversation, and its proof's verdict and steps without their output. One query per page, so
/// a line of thousands of positions does not cost a request per position.
async fn positions(State(e): S, P(repo): P<String>, Query(q): Query<LineQuery>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; positions_of(&e, &r, q.from.unwrap_or(i64::MAX), q.limit.unwrap_or(100).clamp(1, 500)) }).await?)
}

fn positions_of(e: &Engine, repo: &str, from: i64, limit: i64) -> anyhow::Result<Vec<serde_json::Value>> {
    #[derive(Deserialize)]
    struct StepHead {
        name: String,
        exit: i32,
        seconds: f64,
    }
    let db = e.db.lock().unwrap();
    let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
    for t in db.tags(repo, false)? {
        tags.entry(t.position).or_default().push(t.name);
    }
    // The steps' output stays inside SQLite: a full suite's weighs megabytes.
    let mut st = db.conn.prepare(
        "SELECT l.position, l.task, l.landed_at, l.proof, l.full, l.note, l.git, t.actor, t.created_at, t.transcript, p.verdict, p.base_position, \
         (SELECT json_group_array(json_object('name', json_extract(s.value, '$.name'), 'exit', json_extract(s.value, '$.exit'), 'seconds', json_extract(s.value, '$.seconds'))) FROM json_each(p.steps) s) \
         FROM line l LEFT JOIN tasks t ON t.id = l.task LEFT JOIN proofs p ON p.id = l.proof \
         WHERE l.repo = ?1 AND l.position <= ?2 ORDER BY l.position DESC LIMIT ?3",
    )?;
    let rows = st.query_map(rusqlite::params![repo, from, limit], |r| {
        let steps: Vec<StepHead> = r.get::<_, Option<String>>(12)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let actor = r.get::<_, Option<String>>(7)?.and_then(|a| serde_json::from_str::<Actor>(&a).ok());
        let position = r.get::<_, i64>(0)?;
        Ok(serde_json::json!({
            "position": position,
            "tags": tags.get(&position).cloned().unwrap_or_default(),
            "task": r.get::<_, Option<String>>(1)?,
            "landed_at": r.get::<_, i64>(2)?,
            "proof": r.get::<_, Option<String>>(3)?,
            "full": r.get::<_, Option<String>>(4)?,
            "note": r.get::<_, String>(5)?,
            "git": r.get::<_, Option<String>>(6)?,
            "actor": actor.map(|a| serde_json::json!({ "person": a.person, "agent": a.agent })),
            "opened_at": r.get::<_, Option<i64>>(8)?,
            "transcript": r.get::<_, Option<String>>(9)?,
            "verdict": r.get::<_, Option<String>>(10)?,
            "base_position": r.get::<_, Option<i64>>(11)?,
            "seconds": steps.iter().map(|s| s.seconds).sum::<f64>(),
            "steps": steps.iter().map(|s| serde_json::json!({ "name": s.name, "exit": s.exit, "seconds": s.seconds })).collect::<Vec<_>>(),
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The head's avp.toml as JSON, for a person reading how this line proves and deploys: its checks,
/// services and environments, whatever keys a newer AVP reads there. And what AVP guards by itself,
/// whatever the repo guards: the check files (a change to them waits for a person).
async fn config(State(e): S, P(repo): P<String>) -> R {
    use crate::engine::CHECK_FILES;
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        let head = e.db.lock().unwrap().head(&r)?;
        let Some(head) = head else { return Ok(serde_json::json!({ "position": null, "file": null, "config": {}, "always_guarded": CHECK_FILES })) };
        let tree = e.objects.get_tree(&head.tree)?;
        let Some((file, entry)) = CHECK_FILES.iter().find_map(|n| tree.get(*n).map(|x| (*n, x))) else {
            return Ok(serde_json::json!({ "position": head.position, "file": null, "config": {}, "always_guarded": CHECK_FILES }));
        };
        let config: toml::Value = toml::from_str(&String::from_utf8(e.objects.get(&entry.blob)?)?).map_err(|x| anyhow::anyhow!("{file} at position {}: {x}", head.position))?;
        Ok(serde_json::json!({ "position": head.position, "file": file, "config": config, "always_guarded": CHECK_FILES }))
    })
    .await?)
}

/// Every agent seen on the line, as each one names itself (`AVP_AGENT`, its MCP client): its tasks,
/// those landed and open, the people it worked for, when it was first and last seen. Most landed first.
async fn agents(State(e): S, P(repo): P<String>) -> R {
    ok(blocking(move || {
        #[derive(Default)]
        struct Seen {
            tasks: u64,
            landed: u64,
            open: u64,
            people: std::collections::BTreeSet<String>,
            first: i64,
            last: i64,
        }
        let r = rid(&e, &repo)?;
        let db = e.db.lock().unwrap();
        let mut st = db.conn.prepare("SELECT actor, state, created_at FROM tasks WHERE repo = ?1")?;
        let rows = st.query_map(rusqlite::params![r], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        let mut by: HashMap<String, Seen> = HashMap::new();
        for row in rows {
            let (actor, state, at) = row?;
            let actor: Actor = serde_json::from_str(&actor).unwrap_or_default();
            let s = by.entry(if actor.agent.is_empty() { "human".into() } else { actor.agent }).or_insert_with(|| Seen { first: at, ..Default::default() });
            s.tasks += 1;
            s.landed += (state == "landed") as u64;
            s.open += (state == "open") as u64;
            s.first = s.first.min(at);
            s.last = s.last.max(at);
            if !actor.person.is_empty() {
                s.people.insert(actor.person);
            }
        }
        let mut out: Vec<(String, Seen)> = by.into_iter().collect();
        out.sort_by(|a, b| b.1.landed.cmp(&a.1.landed).then(b.1.last.cmp(&a.1.last)));
        Ok(out.into_iter().map(|(agent, s)| serde_json::json!({ "agent": agent, "tasks": s.tasks, "landed": s.landed, "open": s.open, "people": s.people, "first_at": s.first, "last_at": s.last })).collect::<Vec<_>>())
    })
    .await?)
}

/// The person the request is known to come from (`X-Avp-User`): the one whose token it carries on a
/// server with accounts (AVP Cloud's gate sets it), or the one the proxy in front authenticated (Caddy's
/// basic auth), never taken from the client. It wins over the person a client declares, so a task,
/// an approval and a promotion carry who really sent them.
pub(crate) fn verified_person(h: &HeaderMap) -> Option<String> {
    h.get("x-avp-user").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

#[derive(Deserialize)]
struct OpenAt {
    /// The position the task starts from (none: the head), refused when the line does not hold it.
    base: Option<i64>,
    /// The task it is stacked on (`avp open --on`): its tree, not the line's.
    on: Option<String>,
}
async fn open_task(State(e): S, P(repo): P<String>, Query(at): Query<OpenAt>, headers: HeaderMap, Json(mut req): Json<OpenTask>) -> R {
    if let Some(person) = verified_person(&headers) {
        req.actor.person = person;
    }
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        // Read before the task takes its leases, which would hide the tasks holding them until now.
        // The line's memory helps the agent; it never stops a task from opening.
        let context = crate::blame::context(&e, &r, &req.scope, None).unwrap_or_else(|err| {
            eprintln!("context of a task opening on {r}: {err:#}");
            vec![]
        });
        let task = match at.on.filter(|o| !o.is_empty()) {
            Some(on) => e.open_stacked(&r, req, &on)?,
            None => e.open_task_at(&r, req, at.base)?,
        };
        Ok(crate::blame::Opened { task, context })
    })
    .await?)
}
#[derive(Deserialize)]
struct TasksQuery {
    state: Option<TaskState>,
    /// Those of one agent (`git`), or of the agents a prefix names (`pr-*`): what the Git follows
    /// ask at every look, a few tasks rather than the repo's whole history.
    agent: Option<String>,
}
async fn tasks(State(e): S, P(repo): P<String>, Query(q): Query<TasksQuery>) -> R {
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        let mut ts = e.db.lock().unwrap().tasks(&r, q.state)?;
        if let Some(a) = q.agent.as_deref() {
            ts.retain(|t| a.strip_suffix('*').map_or(t.actor.agent == a, |p| t.actor.agent.starts_with(p)));
        }
        Ok(ts)
    })
    .await?)
}
async fn status(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.status(&task) }).await?)
}
async fn write(State(e): S, P((repo, task)): P<(String, String)>, Json(req): Json<PutWrite>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.write(&task, req) }).await?)
}
#[derive(Deserialize)]
struct Paths {
    paths: Option<Vec<String>>,
}
async fn claim(State(e): S, P((repo, task)): P<(String, String)>, Json(req): Json<Paths>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.claim(&task, &req.paths.unwrap_or_default()) }).await?)
}
async fn release(State(e): S, P((repo, task)): P<(String, String)>, Json(req): Json<Paths>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.release(&task, req.paths.as_deref()) }).await.map(|n| serde_json::json!({ "released": n }))?)
}
#[derive(Deserialize)]
struct Undo {
    #[serde(default)]
    to: i64,
    path: Option<String>,
}
async fn undo(State(e): S, P((repo, task)): P<(String, String)>, Json(req): Json<Undo>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.undo(&task, req.to, req.path.as_deref()) }).await.map(|n| serde_json::json!({ "undone": n }))?)
}
#[derive(Deserialize)]
struct SaveBody {
    message: String,
}
async fn save(State(e): S, P((repo, task)): P<(String, String)>, Json(b): Json<SaveBody>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.save(&task, &b.message) }).await?)
}
async fn saves(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.saves(&task) }).await?)
}
#[derive(Deserialize)]
struct RestoreBody {
    /// A save point's number, or its message.
    to: String,
}
async fn restore(State(e): S, P((repo, task)): P<(String, String)>, Json(b): Json<RestoreBody>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.restore(&task, &b.to) }).await?)
}
#[derive(Deserialize)]
struct PickBody {
    position: i64,
}
/// A position's change brought into the task, three ways (`avp pick`).
async fn pick(State(e): S, P((repo, task)): P<(String, String)>, Json(b): Json<PickBody>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.pick(&task, b.position) }).await?)
}
#[derive(serde::Deserialize)]
struct GoalBody {
    goal: String,
}

async fn set_goal(State(e): S, P((repo, task)): P<(String, String)>, Json(b): Json<GoalBody>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.set_goal(&task, &b.goal) }).await.map(|_| serde_json::json!({ "ok": true }))?)
}

/// Says the tasks stacked on it, which stay open.
async fn abandon(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    ok(blocking(move || {
        own_task(&e, &repo, &task)?;
        e.abandon(&task)?;
        let stacked: Vec<String> = e.db.lock().unwrap().stacked_on(&task)?.into_iter().map(|t| t.id).collect();
        Ok(serde_json::json!({ "ok": true, "stacked": stacked }))
    })
    .await?)
}
async fn candidate(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    ok(blocking(move || {
        own_task(&e, &repo, &task)?;
        let (t, w) = {
            let db = e.db.lock().unwrap();
            (db.task(&task)?, db.writes(&task)?)
        };
        e.candidate(&t, &w)
    })
    .await?)
}
async fn check(State(e): S, headers: HeaderMap, P((repo, task)): P<(String, String)>) -> Response {
    kept_alive(&headers, move || {
        own_task(&e, &repo, &task)?;
        Ok((StatusCode::OK, serde_json::to_value(e.check(&task)?)?))
    })
    .await
}
/// Starts a proof of the task's current tree and answers at once (see Engine::speculate).
async fn task_diff(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.diff(&task) }).await?)
}
async fn catch_up(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.catch_up(&task) }).await?)
}
async fn speculate(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    let (e2, repo2, task2) = (e.clone(), repo, task.clone());
    blocking(move || own_task(&e2, &repo2, &task2)).await?;
    ok(serde_json::json!({ "started": e.speculate(&task) }))
}
#[derive(Deserialize)]
struct LandQuery {
    /// The open tasks it is stacked on land first (`avp land --stack`).
    #[serde(default)]
    stack: bool,
    /// Open tasks to land with it, as one position proven whole (`avp land --with`), comma separated.
    #[serde(default)]
    with: String,
}
async fn land(State(e): S, headers: HeaderMap, P((repo, task)): P<(String, String)>, Query(q): Query<LandQuery>) -> Response {
    kept_alive(&headers, move || {
        own_task(&e, &repo, &task)?;
        let with: Vec<String> = q.with.split(',').map(str::trim).filter(|w| !w.is_empty()).map(String::from).collect();
        Ok(match if !with.is_empty() { e.land_with(&task, &with)? } else if q.stack { e.land_stack(&task)? } else { e.land(&task)? } {
            LandOutcome::Landed(l) => (StatusCode::OK, serde_json::to_value(l)?),
            LandOutcome::NotLanded(n) => (StatusCode::CONFLICT, serde_json::to_value(n)?),
        })
    })
    .await
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    task: Option<String>,
    limit: Option<i64>,
    /// Only these kinds, comma separated; one that ends with a dot is a family (`env.`).
    kinds: Option<String>,
    /// The newest this many, oldest first, instead of the oldest after `since`.
    last: Option<i64>,
}
#[derive(Deserialize)]
struct WaitBody {
    #[serde(default)]
    tasks: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
}
/// An open task's `avp wait` looks (Engine::waiting): what it waits on, remembered while it runs.
async fn waits(State(e): S, P((repo, task)): P<(String, String)>, Json(b): Json<WaitBody>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.waiting(&task, &b.tasks, &b.paths) }).await?)
}
/// The reverts a task opening should hear of (Engine::reverted_for).
async fn reverted(State(e): S, P((repo, task)): P<(String, String)>) -> R {
    ok(blocking(move || { own_task(&e, &repo, &task)?; e.reverted_for(&task) }).await?)
}
async fn events(State(e): S, P(repo): P<String>, Query(q): Query<EventsQuery>) -> R {
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        if q.kinds.is_some() || q.last.is_some() {
            anyhow::ensure!(q.task.is_none(), "kinds and last read the repo's events, not a task's");
            return events_of(&e, &r, q.kinds.as_deref().unwrap_or(""), q.since.unwrap_or(0), q.last, q.limit.unwrap_or(100));
        }
        if let Some(t) = q.task.as_deref() {
            own_task(&e, &r, t)?;
        }
        e.events_for(&r, q.task.as_deref(), q.since.unwrap_or(0), q.limit.unwrap_or(100))
    })
    .await?)
}

/// The repo's events of some kinds after `since`: the oldest `limit`, or with `last` the newest that
/// many, oldest first. A page that opens on a line of thousands of positions reads the flakes and
/// deploys of the last days this way, without every write since the line began.
fn events_of(e: &Engine, repo: &str, kinds: &str, since: i64, last: Option<i64>, limit: i64) -> anyhow::Result<Vec<Event>> {
    let kinds: Vec<&str> = kinds.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    anyhow::ensure!(kinds.len() <= 32, "at most 32 kinds at once");
    let mut sql = String::from("SELECT id, at, kind, task, path, data FROM events WHERE repo = ?1 AND id > ?2");
    if !kinds.is_empty() {
        let any: Vec<String> = kinds.iter().enumerate().map(|(i, k)| if k.ends_with('.') { format!("substr(kind, 1, {}) = ?{}", k.chars().count(), i + 3) } else { format!("kind = ?{}", i + 3) }).collect();
        sql.push_str(&format!(" AND ({})", any.join(" OR ")));
    }
    sql.push_str(&format!(" ORDER BY id{} LIMIT {}", if last.is_some() { " DESC" } else { "" }, last.unwrap_or(limit).clamp(1, 10_000)));
    let db = e.db.lock().unwrap();
    let mut st = db.conn.prepare(&sql)?;
    let mut args: Vec<&dyn rusqlite::ToSql> = vec![&repo, &since];
    args.extend(kinds.iter().map(|k| k as &dyn rusqlite::ToSql));
    let rows = st.query_map(args.as_slice(), |r| {
        Ok(Event { id: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, task: r.get(3)?, path: r.get(4)?, data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null) })
    })?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if last.is_some() {
        out.reverse();
    }
    Ok(out)
}

async fn why(State(e): S, P(repo): P<String>, Query(q): Query<HashMap<String, String>>) -> R {
    let path = q.get("path").cloned().unwrap_or_default();
    let at = q.get("at").and_then(|s| s.parse().ok());
    let line: Option<usize> = q.get("line").and_then(|s| s.parse().ok());
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        match line {
            Some(n) => e.why_line(&r, &path, n, at),
            None => e.why(&r, &path, at),
        }
    })
    .await?)
}

#[derive(Deserialize)]
struct FileLogQuery {
    path: String,
    /// The position to read back from (the `more` of the page before); none: the head.
    from: Option<i64>,
    limit: Option<usize>,
}
async fn file_log(State(e): S, P(repo): P<String>, Query(q): Query<FileLogQuery>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; crate::blame::log(&e, &r, &q.path, q.from, q.limit.unwrap_or(30)) }).await?)
}
#[derive(Deserialize)]
struct BlameQuery {
    path: String,
    /// The position whose file to read; none: the head.
    at: Option<i64>,
}
async fn blame(State(e): S, P(repo): P<String>, Query(q): Query<BlameQuery>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; crate::blame::blame(&e, &r, &q.path, q.at) }).await?)
}
#[derive(Deserialize)]
struct ContextQuery {
    paths: Vec<String>,
    /// The task asking: its own leases are not named.
    task: Option<String>,
}
/// A POST, as `/objects`: the paths are many, and go whole in a body.
async fn context(State(e): S, P(repo): P<String>, Json(q): Json<ContextQuery>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; crate::blame::context(&e, &r, &q.paths, q.task.as_deref()) }).await?)
}

/// A proof, each step with how many lines its whole log keeps (`log`) and what its artifacts hold
/// (`artifacts`), when they are kept.
async fn proof(State(e): S, P((repo, id)): P<(String, String)>) -> R {
    ok(blocking(move || {
        let p = proof_of(&e, &repo, &id)?;
        let kept = crate::check::Kept::new(&e.data);
        let mut v = serde_json::to_value(&p)?;
        for (st, out) in p.steps.iter().zip(v["steps"].as_array_mut().into_iter().flatten()) {
            let key = crate::check::kept_key(st);
            if let Some(lines) = kept.lines(&key) {
                out["log"] = lines.into();
            }
            if let Some(a) = kept.artifacts(&key) {
                out["artifacts"] = serde_json::to_value(a)?;
            }
        }
        Ok(v)
    })
    .await?)
}

fn proof_of(e: &Engine, repo: &str, id: &str) -> anyhow::Result<Proof> {
    let r = rid(e, repo)?;
    e.db.lock().unwrap().proof_in(&r, id)?.ok_or_else(|| anyhow::anyhow!("no proof {id}"))
}

/// A step of a proof: the one named, or the first red one, or the last.
fn step_of<'a>(p: &'a Proof, name: Option<&str>) -> anyhow::Result<&'a Step> {
    match name {
        Some(n) => p.steps.iter().find(|s| s.name == n).ok_or_else(|| anyhow::anyhow!("no step {n:?} in {}: its steps are {}", p.id, p.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "))),
        None => p.steps.iter().find(|s| s.exit != 0 && s.exit != crate::check::CANCELLED).or(p.steps.last()).ok_or_else(|| anyhow::anyhow!("no step in {}", p.id)),
    }
}

#[derive(Deserialize)]
struct StepQuery {
    step: Option<String>,
    /// Only this many lines, from a few before the first failing one (or the last ones).
    window: Option<usize>,
}

/// A step's whole log, each line with the milliseconds since the step started; the output kept in
/// the proof, undated, when the log is not (a proof from before, a deploy, past 14 days). `first`
/// is the line where it first says what failed, `from` the index of the first line given.
async fn proof_log(State(e): S, P((repo, id)): P<(String, String)>, Query(q): Query<StepQuery>) -> R {
    ok(blocking(move || {
        let p = proof_of(&e, &repo, &id)?;
        let st = step_of(&p, q.step.as_deref())?;
        let kept = crate::check::Kept::new(&e.data).log(&crate::check::kept_key(st));
        let log = kept.clone().unwrap_or_else(|| crate::check::Log { lines: st.output.lines().map(|l| (0, l.to_string())).collect(), ..Default::default() });
        let total = log.lines.len();
        let (from, to) = match q.window {
            Some(n) => crate::check::around_failure(&log, n.max(1)),
            None => (0, total),
        };
        let first = (st.exit != 0).then(|| crate::check::first_failure(log.lines.iter().map(|(_, l)| l.as_str()))).flatten();
        Ok(serde_json::json!({
            "proof": p.id, "step": st.name, "command": st.command, "exit": st.exit, "seconds": st.seconds,
            "kept": kept.is_some(), "started": (log.started > 0).then_some(log.started),
            "total": total, "from": from, "first": first, "cut": log.cut,
            "lines": &log.lines[from..to],
        }))
    })
    .await?)
}

/// A step's artifacts, packed (`.tar.gz`), to download.
async fn proof_artifacts(State(e): S, P((repo, id)): P<(String, String)>, Query(q): Query<StepQuery>) -> R {
    let (name, bytes) = blocking(move || {
        let p = proof_of(&e, &repo, &id)?;
        let st = step_of(&p, q.step.as_deref())?;
        let pack = crate::check::Kept::new(&e.data).pack(&crate::check::kept_key(st)).ok_or_else(|| anyhow::anyhow!("no artifacts kept for {} in {id}: none named, none made, or past 14 days", st.name))?;
        let file: String = format!("{id}-{}.tar.gz", st.name).chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
        Ok((file, std::fs::read(pack)?))
    })
    .await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/gzip".to_string()), (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))], bytes).into_response())
}

/// The binary this server runs and its hash, read once.
fn own_binary() -> anyhow::Result<&'static (Vec<u8>, String)> {
    static BIN: std::sync::OnceLock<(Vec<u8>, String)> = std::sync::OnceLock::new();
    if let Some(b) = BIN.get() {
        return Ok(b);
    }
    let bytes = std::fs::read(crate::own_exe()?)?;
    let hash = crate::objects::hash_bytes(&bytes);
    Ok(BIN.get_or_init(|| (bytes, hash)))
}

/// What this server runs: its clients take it as theirs (`avp update`). The hash, not the version,
/// says whether a client has it: a commit that kept the version number still reaches them.
async fn client_info() -> R {
    let (_, hash) = own_binary().map_err(err)?;
    Ok(Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "platform": crate::platform(), "hash": hash })).into_response())
}

/// The binary this server runs, for `avp update`: a release reaches every client by reaching the server.
async fn client_binary() -> R {
    let (bytes, _) = own_binary().map_err(err)?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes.clone()).into_response())
}

pub fn router(engine: Engine) -> Router {
    let router: Router<Engine> = Router::new()
        .route("/", get(|| async { "avp" }))
        .route("/client", get(client_info))
        .route("/client/avp", get(client_binary))
        .route("/repos", post(create_repo).get(repos))
        .route("/repos/{repo}", get(repo))
        .route("/repos/{repo}/objects/{hash}", put(put_object).get(get_object).head(has_object))
        .route("/repos/{repo}/objects", post(get_objects))
        .route("/repos/{repo}/import", post(import))
        .route("/repos/{repo}/line/{position}/git", post(set_git))
        .route("/repos/{repo}/mirror", post(mirror_event))
        .route("/repos/{repo}/line/{position}/last-git", get(last_git))
        .route("/repos/{repo}/tags", get(tags).post(tag))
        .route("/repos/{repo}/tags/remove", post(untag))
        .route("/repos/{repo}/tags/git", post(tag_git))
        .route("/repos/{repo}/line/{position}/diff", get(line_diff))
        .route("/repos/{repo}/diff", post(diff))
        .route("/repos/{repo}/line", get(line))
        .route("/repos/{repo}/positions", get(positions))
        .route("/repos/{repo}/config", get(config))
        .route("/repos/{repo}/agents", get(agents))
        .route("/repos/{repo}/head", get(head))
        .route("/repos/{repo}/tree/{position}", get(tree))
        .route("/repos/{repo}/tasks", post(open_task).get(tasks))
        .route("/repos/{repo}/tasks/{task}", get(status))
        .route("/repos/{repo}/tasks/{task}/writes", post(write))
        .route("/repos/{repo}/tasks/{task}/claim", post(claim))
        .route("/repos/{repo}/tasks/{task}/release", post(release))
        .route("/repos/{repo}/tasks/{task}/undo", post(undo))
        .route("/repos/{repo}/tasks/{task}/saves", post(save).get(saves))
        .route("/repos/{repo}/tasks/{task}/restore", post(restore))
        .route("/repos/{repo}/tasks/{task}/pick", post(pick))
        .route("/repos/{repo}/tasks/{task}/abandon", post(abandon))
        .route("/repos/{repo}/tasks/{task}/goal", post(set_goal))
        .route("/repos/{repo}/tasks/{task}/tree", get(candidate))
        .route("/repos/{repo}/tasks/{task}/check", post(check))
        .route("/repos/{repo}/tasks/{task}/speculate", post(speculate))
        .route("/repos/{repo}/tasks/{task}/catch-up", post(catch_up))
        .route("/repos/{repo}/tasks/{task}/diff", get(task_diff))
        .route("/repos/{repo}/tasks/{task}/land", post(land))
        .route("/repos/{repo}/tasks/{task}/waits", post(waits))
        .route("/repos/{repo}/tasks/{task}/reverted", get(reverted))
        .route("/repos/{repo}/events", get(events))
        .route("/repos/{repo}/why", get(why))
        .route("/repos/{repo}/history/log", get(file_log))
        .route("/repos/{repo}/history/blame", get(blame))
        .route("/repos/{repo}/history/context", post(context))
        .route("/repos/{repo}/proofs/{id}", get(proof))
        .route("/repos/{repo}/proofs/{id}/log", get(proof_log))
        .route("/repos/{repo}/proofs/{id}/artifacts", get(proof_artifacts))
        .merge(crate::seam::routes())
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024));
    // Who asks and whether they may, for every route: AVP Cloud's accounts and tokens.
    crate::seam::access(router, &engine).with_state(engine)
}

pub async fn serve(engine: Engine, addr: &str) -> anyhow::Result<()> {
    let host = addr.rsplit_once(':').map(|(h, _)| h.trim_matches(|c| c == '[' || c == ']')).unwrap_or(addr);
    let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false);
    let open = std::env::var("AVP_INSECURE").is_ok_and(|v| v == "1");
    if !loopback && !open {
        crate::seam::may_listen(addr)?;
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("avp serving on http://{addr}  data: {}", engine.data.display());
    // What runs beside the routes: AVP Cloud's scheduled suites, deploys and GitHub bridge.
    crate::seam::start(&engine);
    // `[git]` of avp.toml: the server reaches itself on loopback, whatever address it listens on.
    let mut local = listener.local_addr()?;
    if local.ip().is_unspecified() {
        local.set_ip(if local.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    crate::gitbridge::serve_follows(engine.clone(), format!("http://{local}"));
    // The peer's address: some routes are the server's own machine's (from_host).
    axum::serve(listener, router(engine).into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}
