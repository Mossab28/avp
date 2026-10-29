//! A line that never stays red: a full suite red twice after a landing finds the position that
//! broke it, halving the batch, and AVP takes it back in a task of its own, proven and landed. What
//! AVP cannot settle that way stays red for a person.

use avp::engine::{Engine, LandOutcome};
use avp::model::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn engine() -> (Engine, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path.join("data")).unwrap(), dir)
}

/// A repo whose position 0 holds `avp.toml` only.
fn repo(e: &Engine, toml: &str) -> String {
    let r = e.create_repo("green", LeasePolicy::Advisory).unwrap().id;
    e.import(&r, &[ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml.as_bytes()).unwrap(), mode: 0o100644 }], "imported").unwrap();
    r
}

fn land(e: &Engine, repo: &str, path: &str) -> i64 {
    let actor = Actor { person: "ada".into(), agent: "codex".into(), session: String::new() };
    let t = e.open_task(repo, OpenTask { goal: format!("write {path}"), actor, scope: vec![], transcript: None }).unwrap();
    e.write(&t.id, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(e.objects.put(b"x\n").unwrap()), mode: 0o100644 }).unwrap();
    match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
}

/// Waits a minute at most: suites and a revert's proof run one after the other.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{what}: not within 60 s");
}

fn entry(e: &Engine, repo: &str, position: i64) -> Option<LineEntry> {
    e.db.lock().unwrap().line_at(repo, position).ok()
}

fn full(e: &Engine, repo: &str, position: i64) -> Verdict {
    until(&format!("the full suite of {position}"), || entry(e, repo, position).is_some_and(|l| l.full.is_some()));
    entry(e, repo, position).unwrap().full.unwrap()
}

fn events(e: &Engine, repo: &str, kind: &str) -> Vec<serde_json::Value> {
    e.events_for(repo, None, 0, 1000).unwrap().into_iter().filter(|v| v.kind == kind).map(|v| v.data).collect()
}

/// A full suite that, the first time it runs, waits for `go` to exist: positions land meanwhile, and
/// the next suite covers them as one batch. Red where `broken.txt` is.
fn gated(dir: &Path) -> String {
    let (started, go) = (dir.join("started"), dir.join("go"));
    format!(
        "[check]\nfull = \"if [ ! -f '{0}' ]; then touch '{0}'; while [ ! -f '{1}' ]; do sleep 0.05; done; fi; test ! -f broken.txt || {{ echo ' FAIL  src/b.test.ts > b > breaks'; exit 1; }}\"\n",
        started.display(),
        go.display()
    )
}

/// Lands `first`, then the others while its suite waits: their suite runs once, on the last.
fn batch(e: &Engine, repo: &str, dir: &Path, first: &str, others: &[&str]) -> i64 {
    let p = land(e, repo, first);
    until("the first suite starts", || dir.join("started").exists());
    let last = others.iter().map(|f| land(e, repo, f)).last().unwrap();
    std::fs::write(dir.join("go"), "").unwrap();
    assert_eq!(full(e, repo, p), Verdict::Green);
    last
}

/// Positions 2 and 3 land while 1's suite runs: 3's suite is red, and the shard runs on 2, red, so 2
/// broke it, not 3. AVP reverts 2 on the head; 3 stays, and the line is green again.
#[test]
fn a_batch_is_halved_down_to_the_position_that_broke_it() {
    let (e, d) = engine();
    let r = repo(&e, &gated(&d.path));
    let last = batch(&e, &r, &d.path, "one.txt", &["broken.txt", "three.txt"]);
    assert_eq!(last, 3);
    assert_eq!(full(&e, &r, 3), Verdict::Red);
    assert_eq!(full(&e, &r, 4), Verdict::Green, "the revert lands at 4, and the line is green");
    let tree = e.tree_at(&r, 4).unwrap();
    assert_eq!(tree.keys().map(String::as_str).collect::<Vec<_>>(), ["avp.toml", "one.txt", "three.txt"]);
    assert!(entry(&e, &r, 4).unwrap().note.starts_with("Revert position 2: write broken.txt (full suite red twice, shard full: FAIL  src/b.test.ts > b > breaks)"));
    let culprit = entry(&e, &r, 2).unwrap();
    assert_eq!(culprit.full, Some(Verdict::Red), "the shard read red on it");
    assert_eq!(culprit.reverted.map(|v| (v.by, v.shard)), Some((4, "full".to_string())));
    let said = events(&e, &r, "line.reverted");
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!((said[0]["position"].as_i64(), said[0]["red"].as_i64(), said[0]["person"].as_str(), said[0]["agent"].as_str()), (Some(2), Some(3), Some("ada"), Some("codex")));
    assert_eq!(said[0]["probes"], serde_json::json!([{ "position": 2, "red": true }]));
    let revert = e.status(entry(&e, &r, 4).unwrap().task.as_deref().unwrap()).unwrap().task;
    assert_eq!((revert.actor.person.as_str(), revert.actor.agent.as_str()), ("avp", "avp"));
    let err = e.promote(&r, "production", Some(3), false, "ada", "test").unwrap_err().to_string();
    assert!(err.contains("no green full suite"), "{err}");
}

/// The revert of 2 removes what 3 builds on: its proof is red, it does not land, and the line is
/// left to a person, who hears why.
#[test]
fn a_revert_that_does_not_land_green_is_left_to_a_person() {
    let (e, d) = engine();
    let toml = format!("{}[[check.steps]]\nname = \"uses\"\nrun = \"test -f broken.txt || test ! -f uses-broken.txt\"\n", gated(&d.path));
    let r = repo(&e, &toml);
    batch(&e, &r, &d.path, "one.txt", &["broken.txt", "uses-broken.txt"]);
    assert_eq!(full(&e, &r, 3), Verdict::Red);
    until("the revert is given up", || !events(&e, &r, "line.revert_failed").is_empty());
    let failed = events(&e, &r, "line.revert_failed");
    assert_eq!(failed[0]["position"].as_i64(), Some(3));
    assert!(failed[0]["reason"].as_str().unwrap().contains("the revert of position 2 does not land: its proof is red, uses ends with"), "{failed:?}");
    assert_eq!(e.head(&r).unwrap().position, 3, "nothing landed");
    assert!(events(&e, &r, "line.reverted").is_empty());
    let mine = e.db.lock().unwrap().tasks(&r, Some(TaskState::Abandoned)).unwrap();
    assert_eq!(mine.iter().map(|t| t.actor.person.as_str()).collect::<Vec<_>>(), ["avp"], "AVP's task is abandoned");
}

/// The suite goes red on the revert too (here: on everything after its third run): AVP never
/// reverts its own revert, and leaves the line to a person.
#[test]
fn a_red_revert_is_not_reverted_again() {
    let (e, d) = engine();
    let runs = d.path.join("runs");
    let toml = format!(
        "[check]\nfull = \"echo run >> '{0}'; [ $(wc -l < '{0}') -lt 4 ] || {{ echo ' FAIL  late'; exit 1; }}; test ! -f broken.txt || {{ echo ' FAIL  broken'; exit 1; }}\"\n",
        runs.display()
    );
    let r = repo(&e, &toml);
    land(&e, &r, "one.txt");
    assert_eq!(full(&e, &r, 1), Verdict::Green);
    land(&e, &r, "broken.txt");
    assert_eq!(full(&e, &r, 2), Verdict::Red);
    assert_eq!(full(&e, &r, 3), Verdict::Red, "the revert lands at 3, red too");
    until("the revert's red is left to a person", || !events(&e, &r, "line.revert_failed").is_empty());
    assert!(events(&e, &r, "line.revert_failed")[0]["reason"].as_str().unwrap().contains("AVP's own revert"));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(e.head(&r).unwrap().position, 3);
}

/// A webhook on a port of its own that takes every post and hands its body to the receiver. Named
/// by its URL in avp.toml: no variable of the process to set, which other tests read meanwhile.
fn webhook() -> (String, mpsc::Receiver<serde_json::Value>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hooks/avp", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { continue };
            let mut r = BufReader::new(c.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = tx.send(serde_json::from_slice(&body).unwrap_or_default());
        }
    });
    (url, rx)
}

/// AVP's revert reaches the webhooks of `[notify]` as a red suite does: the position it took back,
/// the position that took it back and the shard that was red, with a sentence a person reads.
#[test]
fn a_revert_reaches_the_webhook() {
    let (e, _d) = engine();
    let (url, posts) = webhook();
    let r = repo(&e, &format!("[check]\nfull = \"test ! -f broken.txt || {{ echo ' FAIL  broken'; exit 1; }}\"\n[notify]\nwebhooks = [\"{url}\"]\n"));
    land(&e, &r, "one.txt");
    assert_eq!(full(&e, &r, 1), Verdict::Green);
    land(&e, &r, "broken.txt");
    let told = loop {
        let body = posts.recv_timeout(Duration::from_secs(60)).expect("the revert told within a minute");
        if body["event"] == "line.reverted" {
            break body;
        }
    };
    assert_eq!((told["repo"].as_str(), told["position"].as_i64(), told["by"].as_i64(), told["shard"].as_str()), (Some("green"), Some(2), Some(3), Some("full")));
    let text = told["text"].as_str().unwrap();
    assert!(text.starts_with("green: AVP reverted position 2 (write broken.txt) at position 3: it turned shard full red twice (FAIL  broken)."), "{text}");
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
