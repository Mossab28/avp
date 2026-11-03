//! Who may do what on AVP Cloud, through its HTTP surface, one test per rule a team lead found
//! open: moving a repo out of its space, a member taken out of an organization, the free plan's
//! seats, approving one's own task, a repo out of reach answering as a missing one, the audit
//! naming who did it, and what a refused request says.
#![cfg(feature = "cloud")]

use avp::engine::Engine;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{json, Value};

fn cloud() {
    static ON: std::sync::Once = std::sync::Once::new();
    ON.call_once(|| {
        std::env::set_var("AVP_CLOUD", "1");
        std::env::set_var("AVP_SIGNUP", "open");
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
fn as_(token: &str, r: RequestBuilder) -> RequestBuilder {
    r.header("x-forwarded-for", "203.0.113.99").bearer_auth(token)
}

fn json_of(r: RequestBuilder) -> Value {
    let r = r.send().unwrap();
    let s = r.status();
    let body: Value = r.json().unwrap();
    assert!(s.is_success(), "{s}: {body}");
    body
}

fn error_of(r: RequestBuilder) -> (u16, String) {
    let r = r.send().unwrap();
    let s = r.status().as_u16();
    let text = r.text().unwrap();
    (s, serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text))
}

/// Signs a person up, each from an address of their own (sign-ups are limited per address); their token.
fn sign_up(http: &Client, base: &str, name: &str) -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let address = format!("198.18.{}.1", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let made = json_of(http.post(format!("{base}/signup")).header("x-forwarded-for", address).json(&json!({ "name": name, "email": format!("{name}@example.com"), "password": "correct horse battery" })));
    made["token"]["token"].as_str().unwrap().to_string()
}

fn repo(http: &Client, base: &str, token: &str, name: &str) {
    json_of(as_(token, http.post(format!("{base}/repos"))).json(&json!({ "name": name })));
    json_of(as_(token, http.post(format!("{base}/repos/{name}/import"))).json(&json!({ "entries": [], "note": "empty" })));
}

fn open_task(http: &Client, base: &str, token: &str, repo: &str) -> String {
    let t = json_of(as_(token, http.post(format!("{base}/repos/{repo}/tasks"))).json(&json!({ "goal": "g", "actor": { "person": "p", "agent": "codex", "session": "s" }, "scope": [] })));
    t["id"].as_str().unwrap().to_string()
}

fn kinds(log: &Value) -> Vec<String> {
    log.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_string()).collect()
}

fn join(http: &Client, base: &str, owner: &str, org: &str, who: &str, token: &str) {
    let i = json_of(as_(owner, http.post(format!("{base}/orgs/{org}/invites"))).json(&json!({ "to": who })));
    json_of(as_(token, http.post(format!("{base}/invites/{}", i["id"].as_str().unwrap()))).json(&json!({ "accept": true })));
}

#[test]
fn a_repo_admin_moves_no_repo_out_of_its_space_nor_demotes_its_owner() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada");
    let bob = sign_up(&http, &base, "bob");
    json_of(as_(&ada, http.post(format!("{base}/orgs"))).json(&json!({ "name": "acme" })));
    repo(&http, &base, &ada, "acme/shop");
    repo(&http, &base, &ada, "ada/notes");
    for r in ["acme/shop", "ada/notes"] {
        json_of(as_(&ada, http.post(format!("{base}/repos/{r}/settings/collaborators"))).json(&json!({ "person": "bob", "role": "admin" })));
        let (code, said) = error_of(as_(&bob, http.post(format!("{base}/repos/{r}/settings/transfer"))).json(&json!({ "to": "bob" })));
        assert_eq!(code, 403, "{r}: {said}");
        assert!(said.contains("an admin role on the repo is not enough"), "{said}");
    }
    let (code, said) = error_of(as_(&bob, http.post(format!("{base}/repos/ada/notes/settings/collaborators"))).json(&json!({ "person": "ada", "role": "write" })));
    assert_eq!(code, 403, "the space's owner stays: {said}");
    let (code, _) = error_of(as_(&bob, http.post(format!("{base}/repos/acme/shop/settings/collaborators"))).json(&json!({ "person": "ada", "role": "read" })));
    assert_eq!(code, 403, "an org admin's role is the org's admins'");

    json_of(as_(&ada, http.post(format!("{base}/repos/acme/shop/settings/transfer"))).json(&json!({ "to": "ada" })));
    let org_log = json_of(as_(&ada, http.get(format!("{base}/orgs/acme/events"))));
    assert_eq!(kinds(&org_log).last().map(String::as_str), Some("org.repo.out"), "{org_log}");
    let audit = json_of(as_(&ada, http.get(format!("{base}/repos/ada/shop/settings/audit"))));
    let moved = audit.as_array().unwrap().iter().find(|e| e["kind"] == "repo.transferred").unwrap();
    assert_eq!(moved["by"], "ada", "{audit}");
    assert_eq!((&audit[0]["kind"], &audit[0]["data"]["person"]), (&json!("role.set"), &json!("ada")), "its new owner runs it: {audit}");
}

#[test]
fn a_member_taken_out_of_an_org_loses_the_roles_its_repos_gave_them_and_is_told_so() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada");
    let bob = sign_up(&http, &base, "bob");
    json_of(as_(&ada, http.post(format!("{base}/orgs"))).json(&json!({ "name": "acme" })));
    join(&http, &base, &ada, "acme", "bob", &bob);
    json_of(as_(&ada, http.post(format!("{base}/orgs/acme/members"))).json(&json!({ "person": "bob", "role": "admin" })));
    repo(&http, &base, &bob, "acme/api");
    let shown = json_of(as_(&ada, http.get(format!("{base}/repos/acme/api/settings"))));
    let by_bob: Vec<&Value> = shown["collaborators"].as_array().unwrap().iter().filter(|c| c["person"] == "bob").collect();
    assert_eq!(by_bob[0]["via"], "org", "its creator holds no direct admin role: {shown}");
    repo(&http, &base, &ada, "acme/shop");
    json_of(as_(&ada, http.post(format!("{base}/repos/acme/shop/settings/collaborators"))).json(&json!({ "person": "bob", "role": "admin" })));
    open_task(&http, &base, &bob, "acme/api");

    json_of(as_(&ada, http.post(format!("{base}/orgs/acme/members"))).json(&json!({ "person": "bob", "role": "none" })));
    for r in ["acme/shop", "acme/api"] {
        let (code, said) = error_of(as_(&bob, http.post(format!("{base}/repos/{r}/tasks"))).json(&json!({ "goal": "g", "actor": { "person": "p", "agent": "codex", "session": "s" }, "scope": [] })));
        assert_eq!(code, 403, "{r}: {said}");
        assert!(said.contains(&format!("your access to {r} was removed")), "{said}");
    }
}

#[test]
fn the_free_plan_seats_three_people_and_refuses_a_fourth() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada");
    sign_up(&http, &base, "bob");
    sign_up(&http, &base, "carol");
    sign_up(&http, &base, "dan");
    json_of(as_(&ada, http.post(format!("{base}/orgs"))).json(&json!({ "name": "acme" })));
    repo(&http, &base, &ada, "acme/shop");
    json_of(as_(&ada, http.post(format!("{base}/orgs/acme/invites"))).json(&json!({ "to": "bob" })));
    json_of(as_(&ada, http.post(format!("{base}/repos/acme/shop/settings/collaborators"))).json(&json!({ "person": "carol@example.com", "role": "read" })));
    assert_eq!(error_of(as_(&ada, http.post(format!("{base}/orgs/acme/invites"))).json(&json!({ "to": "zoe@example.org" }))).0, 400, "an email invited takes a seat");
    let (code, said) = error_of(as_(&ada, http.post(format!("{base}/orgs/acme/invites"))).json(&json!({ "to": "dan" })));
    assert_eq!(code, 400);
    assert!(said.contains("3 of 3 seats") && said.contains("Team plan ($10/person/month)"), "{said}");
    let (code, _) = error_of(as_(&ada, http.post(format!("{base}/repos/acme/shop/settings/collaborators"))).json(&json!({ "person": "dan", "role": "read" })));
    assert_eq!(code, 400, "a collaborator who is no member takes a seat too");
    let org = json_of(as_(&ada, http.get(format!("{base}/orgs/acme"))));
    assert_eq!((&org["seats"], &org["seats_said"]), (&json!(3), &json!("3 of 3 seats")), "{org}");
}

#[test]
fn an_approval_is_a_persons_never_a_tokens_and_not_the_authors_unless_nobody_else_may() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada");
    let bob = sign_up(&http, &base, "bob");
    repo(&http, &base, &ada, "ada/solo");
    // Passwords are checked from an address of their own: sign-ins are limited per address.
    let approve = |token: &str, task: &str, password: Option<&str>| {
        as_(token, http.post(format!("{base}/repos/ada/solo/tasks/{task}/approve"))).header("x-forwarded-for", "198.51.100.7").json(&json!({ "person": "x", "approved": true, "password": password }))
    };
    let alone = open_task(&http, &base, &ada, "ada/solo");
    let (code, said) = error_of(approve(&ada, &alone, None));
    assert_eq!(code, 403, "a token alone, the person's own, never approves");
    assert!(said.starts_with("an approval is a person's: approve it on http") && said.contains(&format!("/r/ada/solo/tasks/{alone}")) && said.contains("`avp approve`"), "{said}");
    let (code, said) = error_of(approve(&ada, &alone, Some("wrong horse battery")));
    assert_eq!(code, 403, "a wrong password gives no word: {said}");
    let said = json_of(approve(&ada, &alone, Some("correct horse battery")));
    assert!(said["said"].as_str().unwrap().contains("approved their own task"), "a single person approves, and is told why: {said}");
    let log = json_of(as_(&ada, http.get(format!("{base}/repos/ada/solo/events?task={alone}"))));
    assert!(log.as_array().unwrap().iter().any(|e| e["kind"] == "task.approved" && e["data"]["reason"] == "self-approved (single person)"), "the audit says so: {log}");

    json_of(as_(&ada, http.post(format!("{base}/repos/ada/solo/settings/collaborators"))).json(&json!({ "person": "bob", "role": "write" })));
    let task = open_task(&http, &base, &ada, "ada/solo");
    let (code, said) = error_of(approve(&ada, &task, Some("correct horse battery")));
    assert_eq!(code, 400);
    assert!(said.contains("second person") && said.contains("bob"), "{said}");
    assert_eq!(json_of(approve(&bob, &task, Some("correct horse battery")))["person"], "bob");
}

#[test]
fn a_repo_out_of_reach_answers_as_a_missing_one_by_the_name_asked() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada");
    let eve = sign_up(&http, &base, "eve");
    repo(&http, &base, &ada, "ada/secret");
    let asked = |path: &str| error_of(as_(&eve, http.get(format!("{base}/repos/{path}/head"))));
    assert_eq!(asked("ada/secret"), (404, "no repo ada/secret".into()));
    assert_eq!(asked("ada/nothing"), (404, "no repo ada/nothing".into()));
    assert_eq!(asked("ada%2Fsecret"), (404, "no repo ada/secret".into()));
    json_of(as_(&ada, http.post(format!("{base}/repos/ada/secret/settings/delete"))).json(&json!({ "confirm": "secret" })));
    assert_eq!(asked("ada/secret"), (404, "no repo ada/secret".into()), "nothing of who deleted it, nor when");
}

#[test]
fn the_audit_names_who_guarded_and_created_and_the_org_log_follows_its_repos() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada");
    json_of(as_(&ada, http.post(format!("{base}/orgs"))).json(&json!({ "name": "acme" })));
    repo(&http, &base, &ada, "acme/web");
    json_of(as_(&ada, http.post(format!("{base}/repos/acme/web/guarded"))).json(&json!({ "add": ["deploy/"] })));
    json_of(as_(&ada, http.post(format!("{base}/repos/acme/web/landing"))).json(&json!({ "landing": "proof_and_approval" })));
    let audit = json_of(as_(&ada, http.get(format!("{base}/repos/acme/web/settings/audit"))));
    let by: Vec<(&str, &str)> = audit.as_array().unwrap().iter().map(|e| (e["kind"].as_str().unwrap(), e["by"].as_str().unwrap_or("someone"))).collect();
    assert_eq!(by, [("repo.landing", "ada"), ("repo.guarded", "ada"), ("repo.created", "ada")], "{audit}");

    json_of(as_(&ada, http.post(format!("{base}/repos/acme/web/settings/delete"))).json(&json!({ "confirm": "web" })));
    let org_log = json_of(as_(&ada, http.get(format!("{base}/orgs/acme/events"))));
    assert_eq!(&kinds(&org_log)[2..], ["org.repo.created", "org.repo.deleted"], "{org_log}");
    let org = json_of(as_(&ada, http.get(format!("{base}/orgs/acme"))));
    assert_eq!(org["repos"], json!([]), "{org}");
    assert_eq!(org["deleted"][0]["name"], "web", "its admins see it, restorable: {org}");
}

#[test]
fn a_repo_token_shows_its_repo_alone_and_once_revoked_says_how_to_make_another() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let ada = sign_up(&http, &base, "ada");
    repo(&http, &base, &ada, "ada/api");
    repo(&http, &base, &ada, "ada/web");
    let made = json_of(as_(&ada, http.post(format!("{base}/repos/ada/api/settings/tokens"))).json(&json!({ "name": "ci", "role": "write" })));
    let ci = made["token"].as_str().unwrap();
    let me = json_of(as_(ci, http.get(format!("{base}/me"))));
    assert_eq!(me["roles"], json!([{ "repo": "ada/api", "role": "write" }]), "{me}");
    assert_eq!(me["scope"]["repo"], "ada/api");

    json_of(as_(&ada, http.post(format!("{base}/repos/ada/api/settings/tokens/{}/revoke", made["info"]["id"].as_str().unwrap()))));
    let (code, said) = error_of(as_(ci, http.get(format!("{base}/repos/ada/api/head"))));
    assert_eq!(code, 401);
    assert!(said.contains("made for ada/api") && said.contains("avp repo token create"), "{said}");
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
