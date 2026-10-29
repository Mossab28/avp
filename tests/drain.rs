//! The drain (`POST /drain`): while it lasts the server starts no new work, what runs goes to its
//! end, and it lifts when asked or at its bound. Proofs here run in the server's own slots; the
//! workers' side is tests/drain_workers.rs.

use avp::engine::Engine;
use avp::model::*;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The drain belongs to the process: these tests take turns.
static ONE: Mutex<()> = Mutex::new(());

/// A test that fails while the server drains does not leave the next one held.
struct Lift;
impl Drop for Lift {
    fn drop(&mut self) {
        avp::check::undrain();
    }
}

fn engine() -> (Engine, tempdir::Dir) {
    avp::check::set_check_slots(64);
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path.join("data")).unwrap(), dir)
}

/// A repo whose one step writes its task's name (`who.txt`) in `started`, then waits for
/// `go-<name>`, a minute at most.
fn repo(e: &Engine, dir: &Path, envs: &str) -> String {
    let d = dir.display();
    let toml = format!("[[check.steps]]\nname = \"gate\"\nrun = \"echo $(cat who.txt) >> '{d}/started'; for i in $(seq 1 1200); do [ -f '{d}/go-'$(cat who.txt) ] && exit 0; sleep 0.05; done; exit 1\"\n{envs}");
    let r = e.create_repo("app", LeasePolicy::Advisory).unwrap().id;
    let mut entries = vec![];
    for (p, c) in [("avp.toml", toml.as_str()), ("who.txt", "nobody\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    r
}

/// A task named `who`, its proof started in the background: its verdict comes on the channel.
fn prove(e: &Engine, repo: &str, who: &str) -> (String, std::sync::mpsc::Receiver<Verdict>) {
    let t = e.open_task(repo, OpenTask { goal: format!("task {who}"), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    let blob = e.objects.put(format!("{who}\n").as_bytes()).unwrap();
    e.write(&t.id, PutWrite { path: "who.txt".into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (e, id) = (e.clone(), t.id.clone());
    std::thread::spawn(move || tx.send(e.check(&id).map(|p| p.verdict).unwrap_or(Verdict::Red)).unwrap());
    (t.id, rx)
}

fn started(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("started")).unwrap_or_default()
}

fn go(dir: &Path, who: &str) {
    std::fs::write(dir.join(format!("go-{who}")), "").unwrap();
}

/// Polls until `done` holds, for up to a minute: what a busy machine may take, and what a calm one
/// never waits, the loop ending as soon as it holds.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what}: not within 60 s");
        std::thread::sleep(Duration::from_millis(50));
    }
}

const MINUTE: Duration = Duration::from_secs(60);

#[test]
fn a_drain_starts_no_proof_and_lets_the_running_one_end() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let _lift = Lift;
    let (e, d) = engine();
    let r = repo(&e, &d.path, "");
    let (a, a_verdict) = prove(&e, &r, "a");
    until("a's proof runs", || started(&d.path) == "a\n");
    let w = e.work().unwrap();
    assert_eq!((w.workers.as_deref(), w.running, w.draining), (Some("off"), 1, false), "a proof in a slot here counts as running: {w:?}");

    avp::check::drain(10 * MINUTE);
    let (b, b_verdict) = prove(&e, &r, "b");
    until("b waits for the drain", || e.work().unwrap().waiting == 1);
    assert!(avp::check::held(&b) && !avp::check::held(&a), "b waits for the drain, a runs");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(started(&d.path), "a\n", "no slot is taken while the server drains");

    go(&d.path, "a");
    assert_eq!(a_verdict.recv_timeout(MINUTE).unwrap(), Verdict::Green, "what ran goes to its end");
    until("nothing runs", || e.work().unwrap().running == 0);
    let w = e.work().unwrap();
    assert_eq!((w.waiting, w.running, w.draining), (1, 0, true), "{w:?}");
    assert!(w.drain_left_s.is_some_and(|s| s > 500), "{w:?}");
    assert_eq!(started(&d.path), "a\n", "b still waits");

    avp::check::undrain();
    until("b's proof runs once the drain is lifted", || started(&d.path) == "a\nb\n");
    assert!(!avp::check::held(&b));
    go(&d.path, "b");
    assert_eq!(b_verdict.recv_timeout(MINUTE).unwrap(), Verdict::Green);
    let w = e.work().unwrap();
    assert_eq!((w.waiting, w.running, w.draining, w.drain_left_s), (0, 0, false, None), "{w:?}");
}

#[test]
fn a_drain_lifts_by_itself_at_its_bound() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let _lift = Lift;
    let (e, d) = engine();
    let r = repo(&e, &d.path, "");
    go(&d.path, "c");
    let drained = Instant::now();
    avp::check::drain(Duration::from_secs(3));
    let (_, verdict) = prove(&e, &r, "c");
    until("c's proof runs once the bound is past", || started(&d.path) == "c\n");
    assert!(drained.elapsed() >= Duration::from_secs(3), "the drain held it until its bound");
    assert_eq!(verdict.recv_timeout(MINUTE).unwrap(), Verdict::Green);
    assert!(avp::check::draining().is_none() && !e.work().unwrap().draining);
}

#[test]
fn a_drain_starts_no_deploy() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let _lift = Lift;
    let (e, d) = engine();
    let deployed = d.path.join("deployed");
    let r = repo(&e, &d.path, &format!("[envs.staging]\ndeploy = {{ run = \"echo {{position}} >> '{}'\" }}\n", deployed.display()));
    avp::check::drain(10 * MINUTE);
    let (tx, rx) = std::sync::mpsc::channel();
    let (e2, r2) = (e.clone(), r.clone());
    std::thread::spawn(move || tx.send(e2.promote(&r2, "staging", Some(0), true, "ada", "human").map(|env| env.position).ok()).unwrap());
    until("the deploy is recorded", || e.work().unwrap().deploys == 1);
    std::thread::sleep(Duration::from_millis(300));
    let w = e.work().unwrap();
    assert_eq!((w.deploys, w.deploying), (1, 0), "recorded, not started: a restart now cuts nothing, the next server runs it");
    assert!(!deployed.exists(), "the deploy waits for the drain");
    avp::check::undrain();
    assert_eq!(rx.recv_timeout(MINUTE).unwrap(), Some(0), "it runs once the drain is lifted");
    assert_eq!(std::fs::read_to_string(&deployed).unwrap(), "0\n");
    assert_eq!(e.work().unwrap().deploys, 0);
}

/// The HTTP side: only the server's own machine drains it, and GET /work and GET /drain say it.
#[test]
fn the_drain_is_the_servers_own_machines() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let _lift = Lift;
    let (e, _d) = engine();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    rt.spawn(async move { axum::serve(listener, avp::server::router(e).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let http = reqwest::blocking::Client::new();
    let json = |r: reqwest::blocking::Response| -> (u16, serde_json::Value) { (r.status().as_u16(), r.json().unwrap_or_default()) };

    let (status, _) = json(http.post(format!("{base}/drain")).header("x-forwarded-for", "203.0.113.9").send().unwrap());
    assert_eq!(status, 403, "a request a proxy forwarded is not the machine's own");
    let (status, _) = json(http.post(format!("{base}/drain")).header("x-avp-user", "mallory").send().unwrap());
    assert_eq!(status, 403);
    assert!(avp::check::draining().is_none());
    for bad in ["0", "61", "x"] {
        assert_eq!(http.post(format!("{base}/drain?minutes={bad}")).send().unwrap().status().as_u16(), 400, "minutes={bad}");
    }

    let (status, body) = json(http.post(format!("{base}/drain?minutes=5")).send().unwrap());
    assert_eq!((status, &body["draining"]), (200, &serde_json::json!(true)), "{body}");
    assert!(body["left_s"].as_u64().is_some_and(|s| s > 290 && s <= 300), "{body}");
    let (_, work) = json(http.get(format!("{base}/work")).send().unwrap());
    assert_eq!((&work["draining"], &work["workers"], &work["running"], &work["deploying"]), (&serde_json::json!(true), &serde_json::json!("off"), &serde_json::json!(0), &serde_json::json!(0)), "{work}");
    let (_, state) = json(http.get(format!("{base}/drain?task=task_none")).send().unwrap());
    assert_eq!((&state["draining"], &state["held"]), (&serde_json::json!(true), &serde_json::json!(false)), "{state}");

    let (status, _) = json(http.delete(format!("{base}/drain")).header("forwarded", "for=203.0.113.9").send().unwrap());
    assert_eq!(status, 403, "nor is lifting it");
    let (status, body) = json(http.delete(format!("{base}/drain")).send().unwrap());
    assert_eq!((status, &body["draining"]), (200, &serde_json::json!(false)), "{body}");
    let (_, work) = json(http.get(format!("{base}/work")).send().unwrap());
    assert_eq!(work["draining"], serde_json::json!(false), "{work}");
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
