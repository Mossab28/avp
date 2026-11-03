//! A repo's settings on AVP Cloud through its HTTP surface, one test per rule that protects
//! something: who may change what, a token made for one repo, an old name, an archived, a public and
//! a deleted repo.
#![cfg(feature = "cloud")]

use avp::engine::Engine;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{json, Value};

/// Every test of this file runs AVP Cloud open to sign-ups, with a free plan of one minute.
fn cloud() {
    static ON: std::sync::Once = std::sync::Once::new();
    ON.call_once(|| {
        std::env::set_var("AVP_CLOUD", "1");
        std::env::set_var("AVP_SIGNUP", "open");
        std::env::set_var("AVP_FREE_MINUTES", "1");
    });
}

/// The server on a port of its own, and its engine. The runtime and the folder live as long as the test.
fn served() -> (String, Engine, tokio::runtime::Runtime, tempdir::Dir) {
    cloud();
    let dir = tempdir::Dir::new();
    let e = Engine::open(&dir.path.join("data")).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = e.clone();
    rt.spawn(async move { axum::serve(listener, avp::server::router(served).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    (base, e, rt, dir)
}

/// A request from an address of the internet, through the proxy in front (the server's own machine
/// has every right).
fn from(r: RequestBuilder, address: &str, token: Option<&str>) -> RequestBuilder {
    let r = r.header("x-forwarded-for", address);
    match token {
        Some(t) => r.bearer_auth(t),
        None => r,
    }
}

fn status(r: RequestBuilder) -> u16 {
    r.send().unwrap().status().as_u16()
}

fn json_of(r: RequestBuilder) -> Value {
    let r = r.send().unwrap();
    let s = r.status();
    let body: Value = r.json().unwrap();
    assert!(s.is_success(), "{s}: {body}");
    body
}

/// Signs a person up from an address of their own; their token.
fn sign_up(http: &Client, base: &str, name: &str, address: &str) -> String {
    let made = json_of(from(http.post(format!("{base}/signup")), address, None).json(&json!({ "name": name, "email": format!("{name}@example.com"), "password": "correct horse battery" })));
    assert_eq!(made["user"]["admin"], false, "one who signs up is no admin, even the first: {made}");
    made["token"]["token"].as_str().unwrap().to_string()
}

fn open_task(http: &Client, base: &str, repo: &str, token: &str, address: &str) -> reqwest::blocking::Response {
    from(http.post(format!("{base}/repos/{repo}/tasks")), address, Some(token)).json(&json!({ "goal": "g", "actor": { "person": "p", "agent": "codex", "session": "s" }, "scope": [] })).send().unwrap()
}

fn error_of(r: reqwest::blocking::Response) -> (u16, String) {
    let s = r.status().as_u16();
    // git's routes answer in plain text, which git shows the person; the API's in JSON.
    let text = r.text().unwrap();
    (s, serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text))
}

#[test]
fn a_writer_cannot_delete_a_reader_cannot_invite_and_a_deleted_repo_refuses_clone() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada", "203.0.113.31");
    let bob = sign_up(&http, &base, "bob", "203.0.113.32");
    let carol = sign_up(&http, &base, "carol", "203.0.113.33");
    let as_ = |token: &str, r: RequestBuilder| from(r, "203.0.113.39", Some(token));
    json_of(as_(&ada, http.post(format!("{base}/repos"))).json(&json!({ "name": "shop" })));
    json_of(as_(&ada, http.post(format!("{base}/repos/ada/shop/import"))).json(&json!({ "entries": [], "note": "empty" })));
    let settings = |rest: &str| format!("{base}/repos/ada/shop/settings{rest}");

    // At once, by name or by email.
    let bob_in = json_of(as_(&ada, http.post(settings("/collaborators"))).json(&json!({ "person": "bob", "role": "write" })));
    assert!(bob_in["said"].as_str().unwrap().contains("bob has write"), "{bob_in}");
    json_of(as_(&ada, http.post(settings("/collaborators"))).json(&json!({ "person": "Carol@Example.com", "role": "read" })));
    assert_eq!(status(as_(&carol, http.get(format!("{base}/repos/ada/shop/head")))), 200);

    assert_eq!(status(as_(&carol, http.post(settings("/collaborators"))).json(&json!({ "person": "bob", "role": "admin" }))), 403, "a reader invites no one");
    assert_eq!(status(as_(&bob, http.post(settings("/delete"))).json(&json!({ "confirm": "shop" }))), 403, "a writer deletes nothing");
    assert_eq!(status(as_(&bob, http.get(settings("")))), 403, "nor reads the settings");
    let (code, said) = error_of(as_(&ada, http.post(settings("/collaborators"))).json(&json!({ "person": "ada", "role": "write" })).send().unwrap());
    assert_eq!(code, 400, "the last admin stays: {said}");
    assert_eq!(status(as_(&ada, http.post(settings("/delete"))).json(&json!({ "confirm": "shpo" }))), 400, "its name, typed");

    json_of(as_(&ada, http.post(settings("/delete"))).json(&json!({ "confirm": "shop" })));
    let (code, said) = error_of(as_(&bob, http.get(format!("{base}/ada/shop.git/info/refs?service=git-upload-pack"))).send().unwrap());
    assert_eq!(code, 404, "a deleted repo refuses clone");
    assert!(said.contains("deleted") && said.contains("avp repo restore"), "{said}");
    assert_eq!(open_task(&http, &base, "ada/shop", &ada, "203.0.113.39").status().as_u16(), 404, "and every task");
    assert!(json_of(as_(&ada, http.get(format!("{base}/repos")))).as_array().unwrap().is_empty(), "nor is it listed");
    assert_eq!(status(as_(&bob, http.post(settings("/restore")))), 403, "a writer restores nothing");
    json_of(as_(&ada, http.post(settings("/restore"))));
    assert_eq!(status(as_(&bob, http.get(format!("{base}/repos/ada/shop/head")))), 200, "restored as it was");

    let log = json_of(as_(&ada, http.get(settings("/audit"))));
    let kinds: Vec<&str> = log.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(&kinds[..4], ["repo.restored", "repo.deleted", "role.set", "role.set"], "{log}");
    assert_eq!(log[0]["by"], "ada");
}

#[test]
fn a_repo_token_reaches_its_repo_alone_to_its_role_and_never_approves() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let dan = sign_up(&http, &base, "dan", "203.0.113.41");
    let as_ = |token: &str, r: RequestBuilder| from(r, "203.0.113.49", Some(token));
    for name in ["api", "web"] {
        json_of(as_(&dan, http.post(format!("{base}/repos"))).json(&json!({ "name": name })));
        json_of(as_(&dan, http.post(format!("{base}/repos/dan/{name}/import"))).json(&json!({ "entries": [], "note": "empty" })));
    }
    let made = |role: &str| json_of(as_(&dan, http.post(format!("{base}/repos/dan/api/settings/tokens"))).json(&json!({ "name": "ci", "role": role })));
    let read = made("read");
    assert!(read["said"].as_str().unwrap().contains("Shown once"), "{read}");
    let ci = read["token"].as_str().unwrap();
    assert_eq!(status(as_(ci, http.get(format!("{base}/repos/dan/api/head")))), 200);
    assert_eq!(open_task(&http, &base, "dan/api", ci, "203.0.113.49").status().as_u16(), 403, "read opens no task");
    assert_eq!(status(as_(ci, http.get(format!("{base}/repos/dan/web/head")))), 403, "its person's other repo is out of reach");
    assert_eq!(status(as_(ci, http.get(format!("{base}/orgs")))), 403);

    let write = made("write");
    let agent = write["token"].as_str().unwrap();
    let task = open_task(&http, &base, "dan/api", agent, "203.0.113.49");
    assert_eq!(task.status().as_u16(), 200, "write opens a task");
    let task = task.json::<Value>().unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(status(as_(agent, http.post(format!("{base}/repos/dan/api/tasks/{task}/approve"))).json(&json!({ "person": "dan", "approved": true }))), 403, "an approval is a person's");
    assert_eq!(status(as_(agent, http.get(format!("{base}/repos/dan/api/settings")))), 403, "settings are a person's");
    let swap = http.post(format!("{base}/session")).header("x-avp-live", "1").header("x-forwarded-for", "203.0.113.49").json(&json!({ "token": agent })).send().unwrap();
    assert_eq!(swap.status().as_u16(), 401, "a repo's token opens no session of its person");
    assert!(swap.headers().get("set-cookie").is_none());

    let listed = json_of(as_(&dan, http.get(format!("{base}/repos/dan/api/settings"))));
    let scoped: Vec<&Value> = listed["tokens"].as_array().unwrap().iter().filter(|t| t["revocable"] == true).collect();
    assert_eq!(scoped.len(), 2, "{listed}");
    assert!(scoped.iter().all(|t| t["scope"].as_str().unwrap().starts_with("dan/api: ") && t.get("token").is_none()), "never the token again");
    let id = read["info"]["id"].as_str().unwrap();
    assert_eq!(status(as_(&dan, http.post(format!("{base}/repos/dan/web/settings/tokens/{id}/revoke")))), 404, "revoked from its own repo only");
    json_of(as_(&dan, http.post(format!("{base}/repos/dan/api/settings/tokens/{id}/revoke"))));
    assert_eq!(status(as_(ci, http.get(format!("{base}/repos/dan/api/head")))), 401);
}

#[test]
fn an_old_name_still_answers_an_archived_repo_lands_nothing_and_a_public_one_reads_signed_out() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let eve = sign_up(&http, &base, "eve", "203.0.113.51");
    let as_ = |r: RequestBuilder| from(r, "203.0.113.59", Some(&eve));
    let made = json_of(as_(http.post(format!("{base}/repos"))).json(&json!({ "name": "blog" })));
    json_of(as_(http.post(format!("{base}/repos/eve/blog/import"))).json(&json!({ "entries": [], "note": "empty" })));

    json_of(as_(http.post(format!("{base}/repos/eve/blog/settings/general"))).json(&json!({ "name": "notes", "description": "what I learn" })));
    let old = json_of(as_(http.get(format!("{base}/repos/eve/blog"))));
    assert_eq!((&old["id"], &old["name"]), (&made["id"], &json!("notes")), "the old name reaches the renamed repo");
    let shown = json_of(as_(http.get(format!("{base}/repos/eve/notes/settings"))));
    assert_eq!((&shown["description"], &shown["names_before"]), (&json!("what I learn"), &json!(["eve/blog"])));

    json_of(as_(http.post(format!("{base}/repos/eve/notes/settings/archive"))).json(&json!({ "archived": true })));
    let (code, said) = error_of(open_task(&http, &base, "eve/notes", &eve, "203.0.113.59"));
    assert_eq!(code, 403);
    assert!(said.contains("eve/notes is archived") && said.contains("archive --off"), "{said}");
    assert_eq!(status(as_(http.get(format!("{base}/repos/eve/notes/head")))), 200, "it still reads");
    json_of(as_(http.post(format!("{base}/repos/eve/notes/settings/archive"))).json(&json!({ "archived": false })));

    let signed_out = |path: &str| from(http.get(format!("{base}{path}")), "203.0.113.60", None).send().unwrap().status().as_u16();
    assert_eq!(signed_out("/repos/eve/notes/head"), 401, "private by default");
    let refused = as_(http.post(format!("{base}/repos/eve/notes/settings/general"))).json(&json!({ "visibility": "public" })).send().unwrap();
    assert!(refused.status().is_client_error(), "no public repo unless the server opens them");
    std::env::set_var("AVP_PUBLIC_REPOS", "1");
    json_of(as_(http.post(format!("{base}/repos/eve/notes/settings/general"))).json(&json!({ "visibility": "public" })));
    assert_eq!(signed_out("/repos/eve/notes/head"), 200, "public: anyone reads");
    assert_eq!(signed_out("/repos/eve/notes/settings"), 401, "not its settings");
    assert_eq!(from(http.post(format!("{base}/repos/eve/notes/tasks")), "203.0.113.60", None).json(&json!({ "goal": "g", "actor": { "person": "x", "agent": "codex", "session": "s" }, "scope": [] })).send().unwrap().status().as_u16(), 401, "nobody writes without a role");
}

mod tempdir {
    pub struct Dir {
        pub path: std::path::PathBuf,
    }
    impl Dir {
        pub fn new() -> Self {
            let path = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
