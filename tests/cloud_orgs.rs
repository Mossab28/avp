//! AVP Cloud (`AVP_CLOUD=1`) through its HTTP surface: accounts anyone makes, organizations that do
//! not see each other's repos or objects, an org role that opens the org's repos, and the free
//! plan's proof minutes refusing a new proof once used up.
#![cfg(feature = "cloud")]

use avp::engine::Engine;
use avp::model::*;
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

#[test]
fn a_second_account_reaches_nothing_of_the_first_until_an_org_lets_it() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada", "203.0.113.1");
    let bob = sign_up(&http, &base, "bob", "203.0.113.2");
    let carol = sign_up(&http, &base, "carol", "203.0.113.3");
    let as_ = |token: &str, r: RequestBuilder| from(r, "203.0.113.9", Some(token));

    // Ada's repo, with a file on its line.
    json_of(as_(&ada, http.post(format!("{base}/repos"))).json(&json!({ "name": "ada-web" })));
    let secret = b"the launch date is a secret";
    let blob = avp::objects::hash_bytes(secret);
    assert_eq!(status(as_(&ada, http.put(format!("{base}/repos/ada-web/objects/{blob}"))).body(secret.to_vec())), 200);
    let head = json_of(as_(&ada, http.post(format!("{base}/repos/ada-web/import"))).json(&json!({ "entries": [{ "path": "PLAN.md", "blob": blob, "mode": 0o100644 }], "note": "plans" })));
    let tree = head["tree"].as_str().unwrap().to_string();
    assert_eq!(status(as_(&ada, http.get(format!("{base}/repos/ada-web/objects/{blob}")))), 200);
    assert_eq!(status(as_(&ada, http.post(format!("{base}/repos/ada-web/objects"))).json(&json!({ "hashes": [blob, tree] }))), 200, "a batch of the repo's own objects");

    // Bob cannot read, list, open a task on, or fetch an object of it, by its name or through his own repo.
    assert_eq!(status(as_(&bob, http.get(format!("{base}/repos/ada-web/line")))), 404);
    assert_eq!(status(as_(&bob, http.post(format!("{base}/repos/ada-web/tasks"))).json(&json!({ "goal": "g", "actor": { "person": "bob", "agent": "codex", "session": "s" }, "scope": [] }))), 404);
    assert_eq!(status(as_(&bob, http.get(format!("{base}/repos/ada-web/objects/{blob}")))), 404);
    json_of(as_(&bob, http.post(format!("{base}/repos"))).json(&json!({ "name": "bob-app" })));
    let names: Vec<Value> = json_of(as_(&bob, http.get(format!("{base}/repos")))).as_array().unwrap().iter().map(|r| r["name"].clone()).collect();
    assert_eq!(names, vec![json!("bob-app")], "bob lists his own repo alone");
    for hash in [&blob, &tree] {
        assert_eq!(status(as_(&bob, http.get(format!("{base}/repos/bob-app/objects/{hash}")))), 404, "a hash known is no right to read it");
        assert_eq!(status(as_(&bob, http.head(format!("{base}/repos/bob-app/objects/{hash}")))), 404, "nor to learn that it exists");
    }
    assert_eq!(status(as_(&bob, http.post(format!("{base}/repos/bob-app/objects"))).json(&json!({ "hashes": [blob] }))), 404, "nor in a batch");
    assert_eq!(status(as_(&bob, http.get(format!("{base}/work")))), 403, "the workers' queue is every organization's");

    // An organization: bob joins it by an invitation he accepts, and its repos open by his org role.
    json_of(as_(&ada, http.post(format!("{base}/orgs"))).json(&json!({ "name": "acme" })));
    assert_eq!(status(as_(&bob, http.post(format!("{base}/orgs"))).json(&json!({ "name": "ada" }))), 400, "a person's name is no organization's");
    let invite = json_of(as_(&ada, http.post(format!("{base}/orgs/acme/invites"))).json(&json!({ "to": "bob" })));
    assert_eq!(status(as_(&bob, http.get(format!("{base}/orgs/acme")))), 404, "not a member yet");
    let mine = json_of(as_(&bob, http.get(format!("{base}/invites"))));
    assert_eq!(mine[0]["org"], "acme", "{mine}");
    assert_eq!(status(as_(&carol, http.post(format!("{base}/invites/{}", invite["id"].as_str().unwrap()))).json(&json!({ "accept": true }))), 404, "an invitation is its person's");
    json_of(as_(&bob, http.post(format!("{base}/invites/{}", invite["id"].as_str().unwrap()))).json(&json!({ "accept": true })));
    assert_eq!(status(as_(&bob, http.post(format!("{base}/orgs/acme/repos"))).json(&json!({ "repo": "ada-web" }))), 403, "a member brings no repo in");
    assert_eq!(json_of(as_(&ada, http.post(format!("{base}/orgs/acme/repos"))).json(&json!({ "repo": "ada-web" })))["org"], "acme");
    assert_eq!(status(as_(&bob, http.get(format!("{base}/repos/ada-web/line")))), 200, "a member reads the org's repos");
    assert_eq!(status(as_(&bob, http.get(format!("{base}/repos/ada-web/objects/{blob}")))), 200, "and their objects");
    let opened = json_of(as_(&bob, http.post(format!("{base}/repos/ada-web/tasks"))).json(&json!({ "goal": "g", "actor": { "person": "bob", "agent": "codex", "session": "s" }, "scope": [] })));
    assert_eq!(opened["actor"]["person"], "bob", "member gives write");
    assert_eq!(status(as_(&bob, http.post(format!("{base}/repos/ada-web/guarded"))).json(&json!({ "add": ["src/"] }))), 403, "but not admin");
    assert_eq!(status(as_(&carol, http.get(format!("{base}/repos/ada-web/line")))), 404, "carol is in no org of it");
    assert_eq!(status(as_(&carol, http.get(format!("{base}/orgs/acme/usage")))), 404);
    let usage = json_of(as_(&bob, http.get(format!("{base}/orgs/acme/usage"))));
    assert_eq!((&usage["plan"], &usage["seats"], &usage["repos"]), (&json!("free"), &json!(2), &json!(["ada-web"])), "{usage}");

    let log = json_of(as_(&ada, http.get(format!("{base}/orgs/acme/events"))));
    let kinds: Vec<&str> = log.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["org.created", "org.member", "org.invited", "org.joined", "org.repo"]);
}

#[test]
fn the_free_plan_refuses_a_new_proof_past_its_minutes_and_says_how_to_go_further() {
    let (base, e, _rt, _d) = served();
    let http = Client::new();
    let dan = sign_up(&http, &base, "dan", "198.51.100.1");
    let as_ = |r: RequestBuilder| from(r, "198.51.100.9", Some(&dan));
    let repo: Repo = serde_json::from_value(json_of(as_(http.post(format!("{base}/repos"))).json(&json!({ "name": "dan-api" })))).unwrap();
    let blob = avp::objects::hash_bytes(b"api\n");
    assert_eq!(status(as_(http.put(format!("{base}/repos/dan-api/objects/{blob}"))).body(b"api\n".to_vec())), 200);
    json_of(as_(http.post(format!("{base}/repos/dan-api/import"))).json(&json!({ "entries": [{ "path": "README", "blob": blob, "mode": 0o100644 }], "note": "first" })));
    let task = json_of(as_(http.post(format!("{base}/repos/dan-api/tasks"))).json(&json!({ "goal": "g", "actor": { "person": "dan", "agent": "codex", "session": "s" }, "scope": [] })))["id"].as_str().unwrap().to_string();
    assert_eq!(status(as_(http.post(format!("{base}/repos/dan-api/tasks/{task}/speculate")))), 200, "minutes left");

    // Two minutes of proof this month: past the free plan's one.
    let step = Step { name: "tests".into(), command: String::new(), exit: 0, seconds: 120.0, output: String::new() };
    let proof = Proof { id: new_id("proof"), task: Some(task.clone()), base_position: 0, candidate_tree: String::new(), files: vec![], steps: vec![step], verdict: Verdict::Green, blamed: vec![], created_at: now_ms(), timing: None };
    e.db.lock().unwrap().insert_proof(&repo.id, &proof).unwrap();
    let r = as_(http.post(format!("{base}/repos/dan-api/tasks/{task}/check"))).send().unwrap();
    assert_eq!(r.status().as_u16(), 402);
    let said = r.json::<Value>().unwrap()["error"].as_str().unwrap().to_string();
    assert!(said.contains("dan used 2 of its 1 proof minutes") && said.contains("organization on the team plan"), "{said}");
    let usage = json_of(as_(http.get(format!("{base}/repos/dan-api/usage"))));
    assert_eq!((&usage["space"], &usage["minutes"], &usage["over"]), (&json!("dan"), &json!(2.0), &json!(true)), "what `avp status` says: {usage}");

    // Into an organization a server admin moves to the team plan: proofs run again.
    json_of(as_(http.post(format!("{base}/orgs"))).json(&json!({ "name": "dan-co" })));
    json_of(as_(http.post(format!("{base}/orgs/dan-co/repos"))).json(&json!({ "repo": "dan-api" })));
    assert_eq!(status(as_(http.post(format!("{base}/repos/dan-api/tasks/{task}/land")))), 402, "the organization's free plan is used up too");
    assert_eq!(status(as_(http.post(format!("{base}/orgs/dan-co/plan"))).json(&json!({ "plan": "team" }))), 403, "a plan is a server admin's");
    // The server's own machine: every right.
    assert_eq!(json_of(http.post(format!("{base}/orgs/dan-co/plan")).json(&json!({ "plan": "team" })))["plan"], "team");
    assert_eq!(status(as_(http.post(format!("{base}/repos/dan-api/tasks/{task}/speculate")))), 200, "no cap on the team plan");
}

#[test]
fn sign_ups_are_limited_per_address_and_a_password_signs_in() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    for i in 0..5 {
        sign_up(&http, &base, &format!("user{i}"), "192.0.2.50");
    }
    let sixth = from(http.post(format!("{base}/signup")), "192.0.2.50", None).json(&json!({ "name": "user5", "email": "user5@example.com", "password": "correct horse battery" }));
    assert_eq!(status(sixth), 429);
    let signin = |password: &str| from(http.post(format!("{base}/session")), "192.0.2.60", None).json(&json!({ "name": "user0@example.com", "password": password }));
    assert_eq!(status(signin("wrong horse battery")), 401);
    let token = json_of(signin("correct horse battery"))["token"]["token"].as_str().unwrap().to_string();
    let me = json_of(from(http.get(format!("{base}/me")), "192.0.2.60", Some(&token)));
    assert_eq!((&me["person"], &me["cloud"]), (&json!("user0"), &json!(true)), "{me}");
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

#[test]
fn a_repo_name_is_its_spaces_two_people_each_bring_hello() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let eve = sign_up(&http, &base, "eve", "203.0.113.21");
    let fay = sign_up(&http, &base, "fay", "203.0.113.22");
    let as_ = |token: &str, r: RequestBuilder| from(r, "203.0.113.9", Some(token));

    // Each brings `hello` into their own space: two repos, said `eve/hello` and `fay/hello`.
    let mine = json_of(as_(&eve, http.post(format!("{base}/repos"))).json(&json!({ "name": "hello" })));
    let theirs = json_of(as_(&fay, http.post(format!("{base}/repos"))).json(&json!({ "name": "hello" })));
    assert_eq!((mine["space"].as_str(), theirs["space"].as_str()), (Some("eve"), Some("fay")));
    assert_ne!(mine["id"], theirs["id"]);
    for (who, token, repo) in [("eve", &eve, &mine), ("fay", &fay, &theirs)] {
        // By `<space>/<name>`, by the name alone (the person's one `hello`), and by its id.
        for key in [format!("{who}/hello"), format!("{who}%2Fhello"), "hello".to_string(), repo["id"].as_str().unwrap().to_string()] {
            let got = json_of(as_(token, http.get(format!("{base}/repos/{key}"))));
            assert_eq!(got["id"], repo["id"], "{who} reads {key}");
        }
        let head = json_of(as_(token, http.post(format!("{base}/repos/{who}/hello/import"))).json(&json!({ "entries": [], "note": "empty" })));
        assert_eq!(head["position"], 0);
        assert_eq!(json_of(as_(token, http.get(format!("{base}/repos/{who}/hello/head"))))["position"], 0);
    }
    assert_eq!(status(as_(&eve, http.get(format!("{base}/repos/fay/hello/head")))), 404, "another's space answers as nothing");
    let listed = json_of(as_(&eve, http.get(format!("{base}/repos"))));
    assert_eq!(listed.as_array().unwrap().len(), 1, "eve lists her hello alone: {listed}");

    // The same name twice in one organization: a clear refusal, never SQL's.
    json_of(as_(&eve, http.post(format!("{base}/orgs"))).json(&json!({ "name": "hello-co" })));
    json_of(as_(&eve, http.post(format!("{base}/repos"))).json(&json!({ "name": "hello-co/hello" })));
    let again = as_(&eve, http.post(format!("{base}/repos"))).json(&json!({ "name": "hello-co/hello" })).send().unwrap();
    assert_eq!(again.status().as_u16(), 400);
    let said = again.json::<Value>().unwrap()["error"].as_str().unwrap().to_string();
    assert!(said.contains("hello-co/hello already exists") && !said.contains("UNIQUE"), "{said}");
    let moved = as_(&eve, http.post(format!("{base}/orgs/hello-co/repos"))).json(&json!({ "repo": "hello" })).send().unwrap();
    assert_eq!(moved.status().as_u16(), 400, "her own hello cannot join an org that has one");

    // Two `hello`s of hers now: the name alone asks which.
    let both = as_(&eve, http.get(format!("{base}/repos/hello"))).send().unwrap();
    assert_eq!(both.status().as_u16(), 409);
    let said = both.json::<Value>().unwrap()["error"].as_str().unwrap().to_string();
    assert!(said.contains("eve/hello") && said.contains("hello-co/hello") && !said.contains("fay"), "{said}");
    assert_eq!(status(as_(&fay, http.post(format!("{base}/repos"))).json(&json!({ "name": "hello-co/web" }))), 404, "an org fay is not in answers as none");
}

#[test]
fn a_database_from_before_spaces_keeps_its_repos_and_names_them_per_space() {
    let dir = tempdir::Dir::new();
    let path = dir.path.join("avp.db");
    {
        let old = rusqlite::Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE repos (id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, policy TEXT NOT NULL, created_at INTEGER NOT NULL);
             ALTER TABLE repos ADD COLUMN landing TEXT NOT NULL DEFAULT 'proof';
             ALTER TABLE repos ADD COLUMN org TEXT;
             ALTER TABLE repos ADD COLUMN owner TEXT;
             INSERT INTO repos (id, name, policy, created_at, owner) VALUES ('repo_a', 'hello', 'advisory', 1, 'ada');
             INSERT INTO repos (id, name, policy, created_at) VALUES ('repo_t', 'tools', 'advisory', 2);",
        )
        .unwrap();
    }
    let db = avp::db::Db::open(&path).unwrap();
    assert_eq!(db.repo("ada/hello").unwrap().id, "repo_a");
    assert_eq!(db.repo("tools").unwrap().space, None, "a repo of no space keeps its bare name");
    let bobs = db.create_repo_in("hello", LeasePolicy::Advisory, None, Some("bob")).unwrap();
    assert_eq!(bobs.full_name(), "bob/hello");
    let twice = db.create_repo("tools", LeasePolicy::Advisory).unwrap_err().to_string();
    assert_eq!(twice, "tools already exists: choose another name");
    assert!(db.repo("hello").unwrap_err().to_string().contains("ada/hello, bob/hello"));
}
