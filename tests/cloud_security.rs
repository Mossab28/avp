//! AVP Cloud as a stranger with a free account would attack it (docs/internal/SECURITY-AUDIT.md):
//! another organization's file read through a hash put in one's own tree, a deploy run on the
//! server's machine, a `.git` path laid out in a checkout, a page without its CSP, a password
//! guessed from many addresses, a deploy link that runs script.
#![cfg(feature = "cloud")]

use avp::engine::Engine;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{json, Value};

fn served() -> (String, Engine, tokio::runtime::Runtime, tempdir::Dir) {
    static ON: std::sync::Once = std::sync::Once::new();
    ON.call_once(|| {
        std::env::set_var("AVP_CLOUD", "1");
        std::env::set_var("AVP_SIGNUP", "open");
    });
    let dir = tempdir::Dir::new();
    let e = Engine::open(&dir.path.join("data")).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = e.clone();
    rt.spawn(async move { axum::serve(listener, avp::server::router(served).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    (base, e, rt, dir)
}

/// From an address of the internet, through the proxy in front.
fn from(r: RequestBuilder, token: &str) -> RequestBuilder {
    r.header("x-forwarded-for", "203.0.113.9").bearer_auth(token)
}

fn sign_up(http: &Client, base: &str, name: &str) -> String {
    let r: Value = http.post(format!("{base}/signup")).header("x-forwarded-for", format!("198.51.100.{}", name.len())).json(&json!({ "name": name, "email": format!("{name}@example.com"), "password": "correct horse battery" })).send().unwrap().json().unwrap();
    r["token"]["token"].as_str().unwrap().to_string()
}

/// A repo of `token`'s person holding `files` at position 0; the blobs' hashes.
fn repo(http: &Client, base: &str, token: &str, name: &str, files: &[(&str, &[u8])]) -> Vec<String> {
    let ok = |r: RequestBuilder| {
        let r = r.send().unwrap();
        assert!(r.status().is_success(), "{}", r.text().unwrap());
    };
    ok(from(http.post(format!("{base}/repos")), token).json(&json!({ "name": name })));
    let mut entries = vec![];
    for (path, bytes) in files {
        let blob = avp::objects::hash_bytes(bytes);
        ok(from(http.put(format!("{base}/repos/{name}/objects/{blob}")), token).body(bytes.to_vec()));
        entries.push(json!({ "path": path, "blob": blob, "mode": 0o100644 }));
    }
    ok(from(http.post(format!("{base}/repos/{name}/import")), token).json(&json!({ "entries": entries, "note": "first" })));
    entries.iter().map(|x| x["blob"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_hash_known_puts_no_file_of_another_org_in_ones_own_tree() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let (ada, eve) = (sign_up(&http, &base, "ada"), sign_up(&http, &base, "eve"));
    let secret = repo(&http, &base, &ada, "plans", &[("PLAN.md", b"the launch date is a secret")]).remove(0);
    repo(&http, &base, &eve, "mine", &[("a", b"a")]);
    let entry = json!({ "path": "stolen.md", "blob": secret, "mode": 0o100644 });
    let r = from(http.post(format!("{base}/repos/mine/import")), &eve).json(&json!({ "entries": [entry], "note": "x" })).send().unwrap();
    assert_eq!(r.status().as_u16(), 404, "an import of a blob the repo never held");
    let task: Value = from(http.post(format!("{base}/repos/mine/tasks")), &eve).json(&json!({ "goal": "g", "actor": { "person": "", "agent": "", "session": "" } })).send().unwrap().json().unwrap();
    let id = task["id"].as_str().unwrap();
    let w = json!({ "path": "stolen.md", "kind": "put", "blob": secret, "mode": 0o100644 });
    assert_eq!(from(http.post(format!("{base}/repos/mine/tasks/{id}/writes")), &eve).json(&w).send().unwrap().status().as_u16(), 404, "nor a write of one");
    assert_eq!(from(http.get(format!("{base}/repos/mine/objects/{secret}")), &eve).send().unwrap().status().as_u16(), 404);
    // The same bytes sent first are hers to write, as the CLI does.
    let own = json!({ "path": "copy.md", "kind": "put", "blob": secret, "mode": 0o100644 });
    assert_eq!(from(http.put(format!("{base}/repos/mine/objects/{secret}")), &eve).body(&b"the launch date is a secret"[..]).send().unwrap().status().as_u16(), 200);
    assert_eq!(from(http.post(format!("{base}/repos/mine/tasks/{id}/writes")), &eve).json(&own).send().unwrap().status().as_u16(), 200);
}

#[test]
fn avp_cloud_runs_no_deploy_on_the_servers_machine() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let eve = sign_up(&http, &base, "mallory");
    repo(&http, &base, &eve, "dep", &[("avp.toml", b"[check]\nsteps = []\n[envs.prod]\ndeploy = { run = \"id\" }\n")]);
    let r = from(http.post(format!("{base}/repos/dep/envs/prod")), &eve).json(&json!({ "position": 0, "force": true })).send().unwrap();
    assert_eq!(r.status().as_u16(), 400);
    assert!(r.text().unwrap().contains("github_workflow"), "it says what a deploy is here");
}

#[test]
fn no_path_of_a_tree_lands_in_a_checkouts_git_folder() {
    for bad in [".git/hooks/pre-commit", "a/.git/config", ".GIT/config", ".git./x", "GIT~1/config", ".g\u{200c}it/config"] {
        assert!(!avp::model::safe_path(bad), "{bad}");
    }
    for good in [".github/workflows/ci.yml", ".gitignore", "src/git/mod.rs", ".avp-notes.md"] {
        assert!(avp::model::safe_path(good), "{good}");
    }
}

#[test]
fn every_answer_says_its_type_and_avp_live_runs_only_the_servers_script() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let live = http.get(format!("{base}/live")).send().unwrap();
    let csp = live.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(csp.contains("default-src 'self'") && !csp.contains("script-src"), "scripts of this server alone: {csp}");
    let page = live.text().unwrap();
    assert!(page.contains("<script src=\"/live.js\"></script>") && !page.contains("<script>"), "no inline script left");
    assert!(http.get(format!("{base}/live.js")).send().unwrap().text().unwrap().contains("use strict"));
    let api = http.get(format!("{base}/me")).header("x-forwarded-for", "203.0.113.9").send().unwrap();
    assert_eq!(api.headers()["x-content-type-options"], "nosniff");
    assert!(api.headers()["content-security-policy"].to_str().unwrap().contains("sandbox"), "a raw answer runs nothing");
}

#[test]
fn an_account_takes_so_many_wrong_passwords_from_every_address_together() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    sign_up(&http, &base, "victim");
    let try_from = |i: usize, password: &str| http.post(format!("{base}/session")).header("x-forwarded-for", format!("192.0.2.{i}")).json(&json!({ "name": "victim", "password": password })).send().unwrap().status().as_u16();
    for i in 0..20 {
        assert_eq!(try_from(i, "wrong guess"), 401);
    }
    assert_eq!(try_from(99, "correct horse battery"), 429, "a 21st try waits, from a fresh address too");
}

#[test]
fn a_deploys_link_is_a_web_page() {
    let (base, _e, _rt, _d) = served();
    let http = Client::new();
    let eve = sign_up(&http, &base, "trent");
    repo(&http, &base, &eve, "web", &[("a", b"a")]);
    let at = |url: &str| from(http.post(format!("{base}/repos/web/envs/prod/progress")), &eve).json(&json!({ "position": 0, "step": "s", "since": 0, "url": url })).send().unwrap().status().as_u16();
    assert_eq!(at("javascript:alert(1)"), 400);
    assert_eq!(at("https://ci.example.com/run/1"), 200);
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
