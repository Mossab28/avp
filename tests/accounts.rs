//! People, tokens and rights on a team server, through its HTTP surface: a server without users as
//! before, then each person's role on the repo deciding what they may do, and the person on what
//! they do coming from their token.
#![cfg(feature = "cloud")]

use avp::engine::Engine;
use avp::model::*;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{json, Value};

/// A repo whose avp.toml lets bob promote to staging and names nobody for production; the server
/// on a port of its own. The runtime and the folder live as long as the test.
fn served() -> (String, tokio::runtime::Runtime, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let e = Engine::open(&dir.path.join("data")).unwrap();
    let r = e.create_repo("web", LeasePolicy::Advisory).unwrap().id;
    let toml = "[envs.staging]\npromoters = [\"bob\"]\n\n[envs.production]\n";
    e.import(&r, &[ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml.as_bytes()).unwrap(), mode: 0o100644 }], "imported").unwrap();
    e.create_repo("secret-project", LeasePolicy::Advisory).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    rt.spawn(async move { axum::serve(listener, avp::server::router(e).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    (base, rt, dir)
}

/// A request as from another machine, through the proxy in front: the server's own machine has
/// every right, and these tests are about people.
fn ask(r: RequestBuilder, token: Option<&str>) -> RequestBuilder {
    let r = r.header("x-forwarded-for", "203.0.113.7");
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
    assert!(r.status().is_success(), "{}", r.status());
    r.json().unwrap()
}

#[test]
fn a_server_without_users_answers_as_before() {
    let (base, _rt, _d) = served();
    let http = Client::new();
    assert_eq!(status(ask(http.get(format!("{base}/repos/web/line")), None)), 200, "no token asked");
    let me = json_of(ask(http.get(format!("{base}/me")), None));
    assert_eq!((&me["accounts"], &me["admin"]), (&json!(false), &json!(true)), "{me}");
}

#[test]
fn each_role_opens_what_it_names_and_the_person_comes_from_the_token() {
    let (base, _rt, _d) = served();
    let http = Client::new();
    let add = |name: &str, by: Option<&str>| json_of(ask(http.post(format!("{base}/users")), by).json(&json!({ "name": name })));
    let ada = add("ada", None);
    assert_eq!(ada["user"]["admin"], true, "the first person is an admin of the server: {ada}");
    let ada = ada["token"]["token"].as_str().unwrap().to_string();
    assert_eq!(status(ask(http.get(format!("{base}/repos/web/line")), None)), 401, "accounts are on: a request carries a token");
    assert_eq!(status(ask(http.post(format!("{base}/users")), None).json(&json!({ "name": "eve" }))), 401);
    let bob = add("bob", Some(&ada))["token"]["token"].as_str().unwrap().to_string();
    let carol = add("carol", Some(&ada))["token"]["token"].as_str().unwrap().to_string();
    assert_eq!(status(ask(http.post(format!("{base}/users")), Some(&bob)).json(&json!({ "name": "eve" }))), 403, "people are a server admin's");

    // No role: the repo answers as one that does not exist, and the list leaves it out.
    assert_eq!(status(ask(http.get(format!("{base}/repos/web/line")), Some(&bob))), 404);
    let role = |who: &str, role: &str, by: &str| status(ask(http.post(format!("{base}/repos/web/roles")), Some(by)).json(&json!({ "person": who, "role": role })));
    assert_eq!(role("bob", "read", &ada), 200);
    assert_eq!(role("carol", "read", &bob), 403, "roles are a repo admin's to give");
    assert_eq!(status(ask(http.get(format!("{base}/repos/web/line")), Some(&bob))), 200, "read reads the line");
    let names: Vec<Value> = json_of(ask(http.get(format!("{base}/repos")), Some(&bob))).as_array().unwrap().iter().map(|r| r["name"].clone()).collect();
    assert_eq!(names, vec![json!("web")], "a person sees the repos they have a role on");

    let open = |token: &str| ask(http.post(format!("{base}/repos/web/tasks")), Some(token)).json(&json!({ "goal": "g", "actor": { "person": "mallory", "agent": "codex", "session": "s" }, "scope": [] }));
    assert_eq!(status(open(&bob)), 403, "read does not open a task");
    assert_eq!(role("bob", "write", &ada), 200);
    let opened = json_of(open(&bob));
    assert_eq!(opened["actor"]["person"], "bob", "the token's person, whatever the client says: {opened}");
    assert_eq!(status(ask(http.post(format!("{base}/repos/web/guarded")), Some(&bob)).json(&json!({ "add": ["src/"] }))), 403, "guards are an admin's");
    assert_eq!(status(ask(http.get(format!("{base}/repos/web/secrets")), Some(&bob))), 403);
    assert_eq!(status(ask(http.post(format!("{base}/repos/web/guarded")), Some(&ada)).json(&json!({ "add": ["src/"] }))), 200, "a server admin has every right");

    // Promotions: the environment's promoters, else an admin of the repo; the engine checks the
    // person, which is the token's.
    let promote = |env: &str, token: &str| ask(http.post(format!("{base}/repos/web/envs/{env}")), Some(token)).json(&json!({ "force": true, "person": "bob" }));
    assert_eq!(status(promote("production", &bob)), 403, "production names no promoters: the repo's admins");
    assert_eq!(status(promote("staging", &bob)), 200, "bob is staging's promoter");
    assert_eq!(role("carol", "write", &ada), 200);
    let r = promote("staging", &carol).send().unwrap();
    assert_eq!(r.status().as_u16(), 400, "carol says she is bob");
    assert!(r.text().unwrap().contains("not carol"));

    // A revoked token stops at once; every change was an event, with who made it.
    let tokens = json_of(ask(http.get(format!("{base}/tokens")), Some(&bob)));
    let id = tokens[0]["id"].as_str().unwrap();
    assert_eq!(status(ask(http.post(format!("{base}/tokens/{id}/revoke")), Some(&carol))), 404, "another person's token is none of carol's");
    assert_eq!(status(ask(http.post(format!("{base}/tokens/{id}/revoke")), Some(&bob))), 200);
    assert_eq!(status(ask(http.get(format!("{base}/repos/web/line")), Some(&bob))), 401);
    let log = json_of(ask(http.get(format!("{base}/accounts/events")), Some(&ada)));
    let said: Vec<String> = log.as_array().unwrap().iter().map(|e| format!("{} {} by {}", e["kind"].as_str().unwrap(), e["data"]["person"].as_str().or(e["data"]["name"].as_str()).unwrap_or(""), e["data"]["by"].as_str().unwrap())).collect();
    for want in ["user.added ada by someone", "user.added bob by ada", "role.set bob by ada", "token.revoked bob by bob"] {
        assert!(said.iter().any(|s| s == want), "{want} in {said:?}");
    }
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
