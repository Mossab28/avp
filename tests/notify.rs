//! What AVP tells people through the webhooks of `[notify]`. Alone in its file: the test sets the
//! server's environment, which no other test of the process may read meanwhile.
#![cfg(feature = "cloud")]

use avp::engine::{Engine, LandOutcome};
use avp::model::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

/// A webhook on a port of its own, which hands each post it gets to the receiver with its number.
/// It answers the first one 503, as a chat service down for a moment does, and the others 200.
fn webhook() -> (String, mpsc::Receiver<(usize, serde_json::Value)>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hooks/s3cret-token", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (n, conn) in listener.incoming().enumerate() {
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
            let status = if n == 0 { "503 Service Unavailable" } else { "200 OK" };
            let _ = c.write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes());
            let _ = tx.send((n, serde_json::from_slice(&body).unwrap_or_default()));
        }
    });
    (url, rx)
}

fn open(e: &Engine, repo: &str, path: &str) -> Task {
    let t = e.open_task(repo, OpenTask { goal: format!("write {path}"), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    let blob = e.objects.put(b"x\n").unwrap();
    e.write(&t.id, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
    t
}

/// The full suite red on its retry and a task that waits for a person reach the webhook, each with
/// a sentence that says what to do, once each, the webhook down at first; the URL, named in avp.toml
/// as a variable of the server's environment, is in no event and no proof.
#[test]
fn a_red_suite_and_an_approval_asked_reach_the_webhook_and_its_token_stays_out() {
    let (url, posts) = webhook();
    let var = format!("AVP_NOTIFY_TEST_{}", ulid::Ulid::new());
    std::env::set_var(&var, &url);
    let dir = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let e = Engine::open(&dir.join("data")).unwrap();
    let r = e.create_repo("app", LeasePolicy::Advisory).unwrap().id;
    // The suite prints its environment: the webhook's variable is not in it. Red, it stays red: no
    // revert of AVP's to tell of besides.
    let toml = format!("[check]\nfull = \"env; exit 1\"\n[line]\nauto_revert = false\n[notify]\nwebhooks = [\"${var}\"]\n");
    e.import(&r, &[ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml.as_bytes()).unwrap(), mode: 0o100644 }], "imported").unwrap();
    assert!(matches!(e.land(&open(&e, &r, "a.txt").id).unwrap(), LandOutcome::Landed(l) if l.position == 1));
    e.set_landing(&r, LandingRule::ProofAndApproval).unwrap();
    let t = open(&e, &r, "b.txt");
    for _ in 0..2 {
        assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::NotLanded(n) if n.reason == "approval"));
    }
    let mut got: Vec<(usize, serde_json::Value)> = Vec::new();
    let taken = |got: &[(usize, serde_json::Value)], kind: &str| got.iter().any(|(n, b)| *n > 0 && b["event"] == kind);
    while !(taken(&got, "line.red") && taken(&got, "task.awaiting_approval")) {
        got.push(posts.recv_timeout(Duration::from_secs(60)).expect("both posts within a minute"));
    }
    assert!(posts.recv_timeout(Duration::from_millis(500)).is_err(), "told once each, the refused post tried again: {got:?}");
    assert_eq!(got.len(), 3, "{got:?}");
    let body = |kind: &str| got.iter().rev().find(|(_, b)| b["event"] == kind).unwrap().1.clone();
    let red = body("line.red");
    assert_eq!((red["repo"].as_str(), red["position"].as_i64()), (Some("app"), Some(1)));
    let text = red["text"].as_str().unwrap();
    assert!(text.starts_with("app: the full suite is red on position 1 (write a.txt), on its retry too. `avp proof proof_"), "{text}");
    let asked = body("task.awaiting_approval");
    let text = asked["text"].as_str().unwrap();
    assert!(text.contains(&format!("{} waits for a person's approval (write b.txt)", t.id)) && text.contains(&format!("`avp approve {}` lands it", t.id)), "{text}");
    let said = serde_json::to_string(&e.events_for(&r, None, 0, 1000).unwrap()).unwrap();
    let proof = red["proof"].as_str().unwrap();
    let output = serde_json::to_string(&e.db.lock().unwrap().proof(proof).unwrap()).unwrap();
    assert!(output.contains("CI=true") && !output.contains("s3cret-token") && !said.contains("s3cret-token"), "{output}");
    let _ = std::fs::remove_dir_all(&dir);
}
