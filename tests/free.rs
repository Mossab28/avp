//! The free AVP, built without `--features cloud`: a route of AVP Cloud answers 404 with what it
//! is, and the CLI's client says what to do; nothing gates a landing, nothing follows it.
#![cfg(not(feature = "cloud"))]

use avp::client::Client;
use avp::engine::{Engine, LandOutcome};
use avp::model::*;

#[test]
fn a_route_of_avp_cloud_says_so_and_what_to_do() {
    let dir = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let e = Engine::open(&dir).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    rt.spawn(async move { axum::serve(listener, avp::server::router(e).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });

    let r = reqwest::blocking::Client::new().post(format!("{base}/work/claim")).json(&serde_json::json!({ "wait_s": 1 })).send().unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert_eq!(r.json::<serde_json::Value>().unwrap()["error"], "proof workers are part of AVP Cloud");

    let err = Client::new(&base).get::<serde_json::Value>("/drain").unwrap_err();
    assert!(avp::client::cloud_only(&err), "{err:#}");
    assert!(format!("{err:#}").contains("point AVP_SERVER at yours"), "{err:#}");
    let err = Client::new(&base).post::<serde_json::Value>("/repos/app/envs/production", &serde_json::json!({})).unwrap_err();
    assert!(format!("{err:#}").starts_with("environments are part of AVP Cloud"), "{err:#}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A task that changes the checks lands on its own green proof, with no person's word, and no
/// full suite runs on the position it lands at.
#[test]
fn a_change_to_the_checks_lands_and_nothing_follows() {
    let dir = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let e = Engine::open(&dir).unwrap();
    let r = e.create_repo("app", LeasePolicy::Advisory).unwrap().id;
    let toml = |step: &str| format!("[check]\nfull = \"true\"\n[[check.steps]]\nname = \"{step}\"\nrun = \"true\"\n");
    e.import(&r, &[ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml("lint").as_bytes()).unwrap(), mode: 0o100644 }], "imported").unwrap();
    let t = e.open_task(&r, OpenTask { goal: "lint becomes test".into(), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    e.write(&t.id, PutWrite { path: "avp.toml".into(), kind: WriteKind::Put, blob: Some(e.objects.put(toml("test").as_bytes()).unwrap()), mode: 0o100644 }).unwrap();
    let position = match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("not landed: {}", n.reason),
    };
    std::thread::sleep(std::time::Duration::from_secs(1));
    let db = e.db.lock().unwrap();
    assert_eq!(db.line_at(&r, position).unwrap().full, None, "a full suite ran");
    let kinds: Vec<String> = db.events_since(&r, 0, 1000).unwrap().into_iter().map(|ev| ev.kind).collect();
    assert!(!kinds.iter().any(|k| ["line.green", "line.red", "line.flaky", "task.awaiting_approval"].contains(&k.as_str())), "{kinds:?}");
    drop(db);
    let _ = std::fs::remove_dir_all(dir);
}
