//! The drain with proof workers (AVP_WORKERS=1, which the queue reads once per process: this test
//! has its binary to itself): while the server drains, a proof waits in the queue and no worker's
//! claim gets it; once the drain is lifted, the next claim does.

use avp::engine::Engine;
use avp::model::*;
use std::time::{Duration, Instant};

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what}: not within 60 s");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn no_worker_claims_a_proof_while_the_server_drains() {
    // Before the queue exists. Unclaimed, a proof would be proven here after AVP_WORKER_CLAIM_S:
    // longer than this test, so that it stays in the queue.
    std::env::set_var("AVP_WORKERS", "1");
    std::env::set_var("AVP_WORKER_CLAIM_S", "600");
    let dir = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let e = Engine::open(&dir).unwrap();
    let r = e.create_repo("app", LeasePolicy::Advisory).unwrap().id;
    let toml = "[[check.steps]]\nname = \"lint\"\nrun = \"true\"\n";
    let entries = vec![ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml.as_bytes()).unwrap(), mode: 0o100644 }];
    e.import(&r, &entries, "imported").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = e.clone();
    rt.spawn(async move { axum::serve(listener, avp::server::router(served).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let http = reqwest::blocking::Client::new();
    let get = |path: &str| -> serde_json::Value { http.get(format!("{base}{path}")).send().unwrap().json().unwrap() };
    let claim = || http.post(format!("{base}/work/claim")).json(&serde_json::json!({ "wait_s": 2, "parts": true })).send().unwrap();

    assert_eq!(http.post(format!("{base}/drain")).send().unwrap().status().as_u16(), 200);
    let t = e.open_task(&r, OpenTask { goal: "a".into(), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    let blob = e.objects.put(b"a\n").unwrap();
    e.write(&t.id, PutWrite { path: "a.txt".into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (e2, id) = (e.clone(), t.id.clone());
    std::thread::spawn(move || tx.send(e2.check(&id).map(|p| p.verdict).ok()).unwrap());
    until("the proof waits in the queue", || get("/work")["waiting"] == 1);
    assert_eq!(get(&format!("/drain?task={}", t.id))["held"], true, "a land waiting for it hears why");

    let none = claim();
    assert_eq!(none.status().as_u16(), 204, "no claim gets a job while the server drains");
    let work = get("/work");
    assert_eq!((&work["waiting"], &work["running"], &work["draining"]), (&serde_json::json!(1), &serde_json::json!(0), &serde_json::json!(true)), "{work}");

    assert_eq!(http.delete(format!("{base}/drain")).send().unwrap().status().as_u16(), 200);
    let got = claim();
    assert_eq!(got.status().as_u16(), 200, "lifted, the next claim gets it");
    let job: avp::workers::Job = got.json().unwrap();
    assert_eq!(job.name, t.id);
    let work = get("/work");
    assert_eq!((&work["waiting"], &work["running"], &work["draining"]), (&serde_json::json!(0), &serde_json::json!(1), &serde_json::json!(false)), "{work}");
    assert_eq!(get(&format!("/drain?task={}", t.id))["held"], false);

    let steps = vec![Step { name: "lint".into(), command: "true".into(), exit: 0, seconds: 0.1, output: String::new() }];
    let done: serde_json::Value = http.post(format!("{base}/work/{}/result", job.id)).json(&serde_json::json!({ "steps": steps })).send().unwrap().json().unwrap();
    assert_eq!(done["accepted"], true);
    assert_eq!(rx.recv_timeout(Duration::from_secs(60)).unwrap(), Some(Verdict::Green));
    let _ = std::fs::remove_dir_all(&dir);
}
