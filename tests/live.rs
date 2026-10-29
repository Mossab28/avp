//! The routes AVP Live reads besides the verbs': the line page by page with who landed each position
//! and its proof, the events of some kinds, the head's avp.toml, the agents seen on the line.

use avp::engine::{Engine, LandOutcome};
use avp::model::*;
use serde_json::{json, Value};

/// A repo whose position 0 is avp.toml, and position 1 a task Cursor landed for ada; the server on a
/// port of its own. The runtime and the folder live as long as the test.
fn served() -> (String, tokio::runtime::Runtime, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let e = Engine::open(&dir.path.join("data")).unwrap();
    let r = e.create_repo("live", LeasePolicy::Advisory).unwrap().id;
    let toml = "[check]\nkeep = [\"target\"]\n\n[[check.steps]]\nname = \"ok\"\nrun = \"true\"\n\n[envs.staging]\nfollow = \"full\"\n";
    e.import(&r, &[ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml.as_bytes()).unwrap(), mode: 0o100644 }], "imported").unwrap();
    let actor = Actor { person: "ada".into(), agent: "cursor".into(), session: "s".into() };
    let t = e.open_task(&r, OpenTask { goal: "say hello".into(), actor, scope: vec![], transcript: Some("https://example.com/chat".into()) }).unwrap();
    e.write(&t.id, PutWrite { path: "hello.txt".into(), kind: WriteKind::Put, blob: Some(e.objects.put(b"hello\n").unwrap()), mode: 0o100644 }).unwrap();
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}/repos/live", listener.local_addr().unwrap());
    rt.spawn(async move { axum::serve(listener, avp::server::router(e).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    (base, rt, dir)
}

fn get(url: &str) -> Value {
    let r = reqwest::blocking::get(url).unwrap();
    assert!(r.status().is_success(), "{url}: {}", r.status());
    r.json().unwrap()
}

#[test]
fn a_page_of_the_line_says_who_landed_each_position_and_its_proof() {
    let (base, _rt, _d) = served();
    let page = get(&format!("{base}/positions?limit=10"));
    let p = &page[0];
    assert_eq!((&p["position"], &p["note"], &p["actor"]), (&json!(1), &json!("say hello"), &json!({ "person": "ada", "agent": "cursor" })), "{p}");
    assert_eq!((&p["verdict"], &p["base_position"], &p["transcript"]), (&json!("green"), &json!(0), &json!("https://example.com/chat")), "{p}");
    assert!(p["opened_at"].as_i64().is_some_and(|o| o <= p["landed_at"].as_i64().unwrap()), "{p}");
    assert_eq!(p["steps"][0]["name"], "ok", "{p}");
    assert!(p["steps"][0].get("output").is_none(), "a page carries no step output: {p}");
    assert_eq!((&page[1]["position"], &page[1]["actor"]), (&json!(0), &Value::Null), "an import has no task");
    assert_eq!(get(&format!("{base}/positions?from=0&limit=10")).as_array().unwrap().len(), 1, "from reads back from a position");
}

#[test]
fn events_of_some_kinds_and_the_newest_ones() {
    let (base, _rt, _d) = served();
    let all = get(&format!("{base}/events?since=0&limit=1000"));
    let some = get(&format!("{base}/events?kinds=task.,line.imported"));
    let kinds: Vec<&str> = some.as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"task.opened") && kinds.contains(&"task.landed") && kinds.contains(&"line.imported"), "{kinds:?}");
    assert!(kinds.iter().all(|k| k.starts_with("task.") || *k == "line.imported"), "{kinds:?}");
    let ids = |v: &Value| v.as_array().unwrap().iter().map(|e| e["id"].as_i64().unwrap()).collect::<Vec<_>>();
    let all = ids(&all);
    assert_eq!(ids(&get(&format!("{base}/events?last=2"))), all[all.len() - 2..], "the newest two, oldest first");
}

#[test]
fn the_heads_avp_toml_reads_as_json() {
    let (base, _rt, _d) = served();
    let c = get(&format!("{base}/config"));
    assert_eq!((&c["file"], &c["position"]), (&json!("avp.toml"), &json!(1)), "{c}");
    assert_eq!((&c["config"]["check"]["steps"][0]["run"], &c["config"]["check"]["keep"]), (&json!("true"), &json!(["target"])), "{c}");
    assert_eq!(c["config"]["envs"]["staging"]["follow"], "full", "{c}");
    assert_eq!(c["always_guarded"], json!(["avp.toml", "trame.toml"]), "what AVP guards by itself: {c}");
}

#[test]
#[cfg(feature = "cloud")]
fn a_saved_before_comes_with_the_stats() {
    let (base, _rt, _d) = served();
    assert_eq!((get(&format!("{base}/before")), &get(&format!("{base}/stats"))["before"]), (Value::Null, &Value::Null), "nothing saved yet");
    let post = |b: &Value| reqwest::blocking::Client::new().post(format!("{base}/before")).json(b).send().unwrap();
    // The numbers only: a field the before does not have is not kept.
    let before = json!({ "repo": "acme/shop", "since": "2026-10-01T00:00:00Z", "until": "2026-10-08T00:00:00Z", "pull_requests": 12, "opened_to_merged": { "count": 12, "median_s": 5460.0, "p90_s": 20000.0 }, "job_minutes": 900.0, "extra": "not kept" });
    let saved: Value = post(&before).json().unwrap();
    assert!(saved.get("extra").is_none() && saved["pull_requests"] == 12, "{saved}");
    let s = get(&format!("{base}/stats?since=1d"));
    assert_eq!((&s["landings"], &s["before"]["repo"], &s["before"]["job_minutes"]), (&json!(1), &json!("acme/shop"), &json!(900.0)), "{s}");
    assert_eq!(get(&format!("{base}/before"))["opened_to_merged"]["median_s"], 5460.0);
    assert!(!post(&json!({ "repo": "acme/shop", "since": "2026-10-08", "until": "2026-10-01" })).status().is_success(), "a window that ends before it starts");
}

#[test]
fn every_agent_seen_on_the_line() {
    let (base, _rt, _d) = served();
    let a = get(&format!("{base}/agents"));
    assert_eq!(a, json!([{ "agent": "cursor", "tasks": 1, "landed": 1, "open": 0, "people": ["ada"], "first_at": a[0]["first_at"], "last_at": a[0]["last_at"] }]));
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
