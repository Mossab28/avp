//! AVP's bridge to GitHub (`avp serve` with AVP_GITHUB_BRIDGE=1), against a fake GitHub on a port
//! of its own: no network.

use avp::engine::{Engine, LandOutcome};
use avp::github::{Bridge, GitHub};
use avp::model::*;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RUN: &str = "https://github.test/acme/shop/actions/runs/7";

/// What the fake GitHub was asked, and what it answers (the test sets it).
#[derive(Default)]
struct Fake {
    /// Method, path with its query, authorization header, body.
    calls: Vec<(String, String, String, Value)>,
    run: Value,
    jobs: Value,
    pending: Value,
    /// What `/served` answers: an environment's own page.
    served: String,
    /// Every call refused, the rate limit spent for an hour.
    limited: bool,
}

impl Fake {
    fn answer(&self, method: &str, path: &str) -> (&'static str, String, String) {
        if self.limited {
            let reset = now_ms() / 1000 + 3600;
            return ("403 Forbidden", format!("x-ratelimit-remaining: 0\r\nx-ratelimit-reset: {reset}\r\n"), json!({ "message": "API rate limit exceeded" }).to_string());
        }
        let body = match (method, path) {
            ("GET", "/served") => return ("200 OK", String::new(), self.served.clone()),
            ("POST", p) if p.ends_with("/dispatches") => json!({ "workflow_run_id": 7, "html_url": RUN }),
            ("GET", p) if p.contains("/actions/workflows/") => json!({ "workflow_runs": [] }),
            ("GET", p) if p.contains("/runs/7/jobs") => json!({ "jobs": self.jobs }),
            ("GET", p) if p.ends_with("/runs/7/pending_deployments") => self.pending.clone(),
            ("GET", p) if p.ends_with("/actions/runs/7") => self.run.clone(),
            _ => json!({}),
        };
        ("200 OK", String::new(), body.to_string())
    }

    fn count(&self, method: &str, ends: &str) -> usize {
        self.calls.iter().filter(|c| c.0 == method && c.1.ends_with(ends)).count()
    }
}

fn github() -> (String, Arc<Mutex<Fake>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fake = Arc::new(Mutex::new(Fake::default()));
    let f = fake.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { continue };
            let mut r = BufReader::new(c.try_clone().unwrap());
            let mut first = String::new();
            let _ = r.read_line(&mut first);
            let mut words = first.split_whitespace().map(String::from);
            let (method, path) = (words.next().unwrap_or_default(), words.next().unwrap_or_default());
            let (mut len, mut auth) = (0, String::new());
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap_or(("", ""));
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => len = value.trim().parse().unwrap_or(0),
                    "authorization" => auth = value.trim().to_string(),
                    _ => {}
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let mut f = f.lock().unwrap();
            f.calls.push((method.clone(), path.clone(), auth, serde_json::from_slice(&body).unwrap_or_default()));
            let (status, headers, text) = f.answer(&method, &path);
            let _ = c.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n{text}", text.len()).as_bytes());
        }
    });
    (url, fake)
}

fn engine() -> (Engine, tempdir::Dir) {
    avp::check::set_check_slots(64);
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path.join("data")).unwrap(), dir)
}

/// A repo whose position 0 holds `avp.toml` and `app.txt`, its git commit `a...a`.
fn repo(e: &Engine, toml: &str) -> String {
    let r = e.create_repo("shop", LeasePolicy::Advisory).unwrap().id;
    let mut entries = vec![];
    for (p, c) in [("avp.toml", toml), ("app.txt", "v0\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    e.set_git(&r, 0, &"a".repeat(40)).unwrap();
    r
}

fn land(e: &Engine, repo: &str, content: &str) -> i64 {
    let t = e.open_task(repo, OpenTask { goal: "write app.txt".into(), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    let blob = e.objects.put(content.as_bytes()).unwrap();
    e.write(&t.id, PutWrite { path: "app.txt".into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
    match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
}

fn env(e: &Engine, repo: &str) -> Environment {
    e.envs_view(repo).unwrap().into_iter().find(|v| v.name == "staging").unwrap()
}

fn step(e: &Engine, repo: &str) -> String {
    env(e, repo).progress.map(|p| p.step).unwrap_or_default()
}

fn events(e: &Engine, repo: &str, kind: &str) -> Vec<Value> {
    e.events_for(repo, None, 0, 1000).unwrap().into_iter().filter(|v| v.kind == kind).map(|v| v.data).collect()
}

#[test]
fn a_promotion_is_dispatched_approved_followed_and_moves_the_environment_once_its_run_is_green() {
    let (url, gh) = github();
    let (e, _d) = engine();
    let r = repo(&e, "[github]\nrepo = \"acme/shop\"\n[envs.staging]\ndeploy = { github_workflow = \"deploy.yml\", inputs = { sha = \"{git}\", n = \"{position}\" }, approve_environment = \"staging\" }\n");
    e.promote(&r, "staging", Some(0), true, "ada", "human").unwrap();
    {
        let mut f = gh.lock().unwrap();
        f.run = json!({ "id": 7, "status": "waiting", "html_url": RUN, "updated_at": "2026-10-28T10:00:00Z" });
        f.pending = json!([{ "environment": { "id": 99, "name": "staging" } }, { "environment": { "id": 98, "name": "production" } }]);
    }
    let mut bridge = Bridge::new(e.clone(), Some(GitHub::new(&url, "s3cret")));
    bridge.round(now_ms());
    {
        let f = gh.lock().unwrap();
        let dispatch = f.calls.iter().find(|c| c.1 == "/repos/acme/shop/actions/workflows/deploy.yml/dispatches").expect("dispatched");
        assert_eq!((&dispatch.3["ref"], &dispatch.3["inputs"]), (&json!("main"), &json!({ "sha": "a".repeat(40), "n": "0" })));
        let approval = f.calls.iter().find(|c| c.0 == "POST" && c.1.ends_with("/runs/7/pending_deployments")).expect("approved");
        assert_eq!(approval.3, json!({ "environment_ids": [99], "state": "approved", "comment": "AVP: position 0 promoted to staging by ada" }));
        assert!(f.calls.iter().all(|c| c.2 == "Bearer s3cret"), "the token goes in a header");
    }
    assert_eq!(step(&e, &r), "waiting for an approval");
    assert_eq!(env(&e, &r).progress.and_then(|p| p.url).as_deref(), Some(RUN));
    assert_eq!(events(&e, &r, "deploy.dispatched")[0]["run"], 7);

    {
        let mut f = gh.lock().unwrap();
        f.run = json!({ "id": 7, "status": "in_progress", "html_url": RUN });
        f.jobs = json!([
            { "name": "build", "status": "completed", "conclusion": "success" },
            { "name": "deploy", "status": "in_progress", "steps": [{ "name": "Set up job", "status": "completed" }, { "name": "helm upgrade", "status": "in_progress", "started_at": "2026-10-28T10:01:00Z" }] }
        ]);
    }
    bridge.round(now_ms());
    assert_eq!(step(&e, &r), "deploy: helm upgrade");
    assert_eq!(env(&e, &r).position, -1, "it moves once the run is green");

    gh.lock().unwrap().run = json!({ "id": 7, "status": "completed", "conclusion": "success", "html_url": RUN });
    bridge.round(now_ms());
    assert_eq!(env(&e, &r).position, 0);
    let moved = events(&e, &r, "env.moved");
    assert_eq!((moved[0]["by"].as_str(), moved[0]["run"].as_str()), (Some("ada"), Some(RUN)));
    bridge.round(now_ms());
    assert_eq!(gh.lock().unwrap().count("POST", "/dispatches"), 1, "dispatched once");
    let all = serde_json::to_string(&e.events_for(&r, None, 0, 1000).unwrap()).unwrap();
    assert!(!all.contains("s3cret"), "the token is in no event");
}

#[test]
fn a_failed_run_names_its_job_and_a_restarted_bridge_neither_dispatches_nor_tells_it_again() {
    let (url, gh) = github();
    let (e, _d) = engine();
    let r = repo(&e, "[github]\nrepo = \"acme/shop\"\n[envs.staging]\ndeploy = { github_workflow = \"deploy.yml\" }\n");
    e.promote(&r, "staging", Some(0), true, "ada", "human").unwrap();
    {
        let mut f = gh.lock().unwrap();
        f.run = json!({ "id": 7, "status": "completed", "conclusion": "failure", "html_url": RUN });
        f.jobs = json!([{ "name": "build", "conclusion": "success" }, { "name": "deploy", "conclusion": "failure" }, { "name": "smoke", "conclusion": "skipped" }]);
    }
    Bridge::new(e.clone(), Some(GitHub::new(&url, "t"))).round(now_ms());
    let failed = events(&e, &r, "env.failed");
    assert_eq!((failed[0]["reason"].as_str(), failed[0]["run"].as_str()), (Some("deploy.yml: job \"deploy\" failed"), Some(RUN)));
    assert_eq!((env(&e, &r).position, step(&e, &r).as_str()), (-1, "failed: deploy.yml: job \"deploy\" failed"));

    Bridge::new(e.clone(), Some(GitHub::new(&url, "t"))).round(now_ms());
    assert_eq!(gh.lock().unwrap().count("POST", "/dispatches"), 1);
    assert_eq!(events(&e, &r, "env.failed").len(), 1);
    assert_eq!(step(&e, &r), "failed: deploy.yml: job \"deploy\" failed");
}

#[test]
fn a_promotion_long_older_than_the_bridge_and_without_a_run_is_not_dispatched() {
    let (url, gh) = github();
    let (e, _d) = engine();
    let r = repo(&e, "[github]\nrepo = \"acme/shop\"\n[envs.staging]\ndeploy = { github_workflow = \"deploy.yml\" }\n");
    let p = Promotion { position: 0, git: None, by: "ada".into(), agent: "human".into(), at: now_ms() - 3 * 3600 * 1000 };
    e.db.lock().unwrap().set_promotion(&r, "staging", &p).unwrap();
    Bridge::new(e.clone(), Some(GitHub::new(&url, "t"))).round(now_ms());
    assert_eq!(gh.lock().unwrap().count("POST", "/dispatches"), 0);
    assert!(step(&e, &r).ends_with("no run of deploy.yml since: promote again"), "{}", step(&e, &r));
}

#[test]
fn the_full_suites_verdict_is_written_once_as_the_commit_status_avp_full() {
    let (url, gh) = github();
    let (e, _d) = engine();
    let r = repo(&e, "[github]\nrepo = \"acme/shop\"\n[check]\nfull = \"true\"\n");
    let p = land(&e, &r, "v1\n");
    let deadline = Instant::now() + Duration::from_secs(60);
    while e.db.lock().unwrap().line_at(&r, p).unwrap().full.is_none() {
        assert!(Instant::now() < deadline, "the full suite within 60 s");
        std::thread::sleep(Duration::from_millis(50));
    }
    let sha = "b".repeat(40);
    e.set_git(&r, p, &sha).unwrap();
    for _ in 0..2 {
        Bridge::new(e.clone(), Some(GitHub::new(&url, "t"))).round(now_ms());
    }
    let f = gh.lock().unwrap();
    let statuses: Vec<_> = f.calls.iter().filter(|c| c.1.starts_with("/repos/acme/shop/statuses/")).collect();
    assert_eq!(statuses.len(), 1, "once, a restart between: {:?}", f.calls);
    assert_eq!(statuses[0].1, format!("/repos/acme/shop/statuses/{sha}"));
    assert_eq!((&statuses[0].3["state"], &statuses[0].3["context"]), (&json!("success"), &json!("avp/full")));
    assert_eq!(statuses[0].3["description"], format!("AVP full suite, position {p}: green"));
}

#[test]
fn a_serves_url_moves_the_environment_and_a_green_run_waits_for_it() {
    let (url, gh) = github();
    let (e, _d) = engine();
    let r = repo(&e, &format!("[github]\nrepo = \"acme/shop\"\n[envs.staging]\ndeploy = {{ github_workflow = \"deploy.yml\" }}\nserves = \"{url}/served\"\n"));
    let p = land(&e, &r, "v1\n");
    let sha = "c".repeat(40);
    e.set_git(&r, p, &sha).unwrap();
    gh.lock().unwrap().served = "a".repeat(40);
    let mut bridge = Bridge::new(e.clone(), Some(GitHub::new(&url, "t")));
    bridge.round(now_ms());
    assert_eq!(env(&e, &r).position, 0, "what it serves, whoever deployed it");

    e.promote(&r, "staging", Some(p), true, "ada", "human").unwrap();
    gh.lock().unwrap().run = json!({ "id": 7, "status": "completed", "conclusion": "success", "html_url": RUN });
    bridge.round(now_ms());
    assert_eq!(env(&e, &r).position, 0, "green, but it does not serve it yet");
    assert_eq!(step(&e, &r), format!("deployed, waits for {url}/served to answer cccccccccccc"));

    gh.lock().unwrap().served = json!({ "sha": sha }).to_string();
    bridge.round(now_ms());
    assert_eq!(env(&e, &r).position, p);
}

#[test]
fn a_spent_rate_limit_stops_the_calls_until_it_resets() {
    let (url, gh) = github();
    gh.lock().unwrap().limited = true;
    let github = GitHub::new(&url, "t");
    let first = github.call(reqwest::Method::GET, "/repos/acme/shop", None).unwrap_err().to_string();
    assert!(first.contains("403") && first.contains("nothing more is asked of it for"), "{first}");
    let second = github.call(reqwest::Method::GET, "/repos/acme/shop", None).unwrap_err().to_string();
    assert!(second.contains("rate limit is spent"), "{second}");
    assert_eq!(gh.lock().unwrap().calls.len(), 1, "the second was not sent");
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
