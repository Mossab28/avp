//! A flake in the full suite: a red shard runs again once on the same tree. Green the second time,
//! the position is flaky, green for promotions and `follow`, never red; red twice is red. A task's
//! proof never runs again.
#![cfg(feature = "cloud")]

use avp::engine::{Engine, LandOutcome};
use avp::model::*;
use std::path::Path;
use std::time::{Duration, Instant};

fn engine() -> (Engine, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path.join("data")).unwrap(), dir)
}

/// A repo whose position 0 holds `avp.toml` only.
fn repo(e: &Engine, toml: &str) -> String {
    let r = e.create_repo("flaky", LeasePolicy::Advisory).unwrap().id;
    e.import(&r, &[ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml.as_bytes()).unwrap(), mode: 0o100644 }], "imported").unwrap();
    r
}

fn open(e: &Engine, repo: &str, path: &str) -> Task {
    let t = e.open_task(repo, OpenTask { goal: format!("write {path}"), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    let blob = e.objects.put(b"x\n").unwrap();
    e.write(&t.id, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
    t
}

fn land(e: &Engine, repo: &str, path: &str) -> i64 {
    match e.land(&open(e, repo, path).id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
}

/// Waits a minute at most: the suite runs twice, on a machine other proofs may load.
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

fn full(e: &Engine, repo: &str, position: i64) -> Verdict {
    let read = || e.db.lock().unwrap().line_at(repo, position).unwrap().full;
    until(&format!("the full suite of {position}"), || read().is_some());
    read().unwrap()
}

fn events(e: &Engine, repo: &str, kind: &str) -> Vec<serde_json::Value> {
    e.events_for(repo, None, 0, 1000).unwrap().into_iter().filter(|v| v.kind == kind).map(|v| v.data).collect()
}

fn serves(e: &Engine, repo: &str, env: &str) -> Option<i64> {
    e.db.lock().unwrap().env(repo, env).unwrap().map(|v| v.position)
}

/// A shard that fails where `ran` does not exist yet, and passes once it does: a test that fails
/// under load, as vitest prints it. Each run adds a line to `runs`.
fn flaky_shard(ran: &Path, runs: &Path) -> String {
    format!(
        "echo run >> '{}'; if [ -f '{1}' ]; then echo all green; else touch '{1}'; echo ' FAIL  apps/daemon/src/codexActivity.test.ts > codexActivity > reads the log'; echo '   × codexActivity > reads the log 503ms'; exit 1; fi",
        runs.display(),
        ran.display()
    )
}

fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap_or_default().lines().count()
}

/// Positions 91 and 93 of Murmell's line (24/10): red on a test their changes did not reach, green on
/// the next run. Run again at once, the shard is green: the position is flaky, not red, staging
/// follows it, a person can promote it, and the event says which shard flaked on which tests.
#[test]
fn a_shard_green_on_its_retry_is_flaky_and_counts_as_green() {
    let (e, d) = engine();
    let (ran, runs) = (d.path.join("ran"), d.path.join("runs"));
    let toml = format!("[check]\n[[check.full_shards]]\nname = \"web\"\nrun = \"echo web green\"\n[[check.full_shards]]\nname = \"daemon\"\nrun = \"{}\"\n[envs.staging]\nfollow = \"full\"\n", flaky_shard(&ran, &runs));
    let r = repo(&e, &toml);
    let p = land(&e, &r, "a.txt");
    assert_eq!(full(&e, &r, p), Verdict::Flaky);
    assert_eq!(lines(&runs), 2, "the red shard ran once more, on the same tree");
    let flaky = events(&e, &r, "line.flaky");
    assert_eq!(flaky.len(), 1, "{flaky:?}");
    assert_eq!((flaky[0]["position"].as_i64(), flaky[0]["shard"].as_str()), (Some(p), Some("daemon")));
    assert_eq!(flaky[0]["tests"], serde_json::json!(["FAIL  apps/daemon/src/codexActivity.test.ts > codexActivity > reads the log", "× codexActivity > reads the log"]));
    assert!(events(&e, &r, "line.red").is_empty() && events(&e, &r, "line.green").is_empty(), "a flaky position has no other verdict");
    let proof = e.db.lock().unwrap().proof(flaky[0]["proof"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(proof.verdict, Verdict::Flaky);
    assert!(proof.blamed.is_empty(), "a flake blames no file");
    let daemon = proof.steps.iter().find(|s| s.name.starts_with("daemon")).unwrap();
    assert!(daemon.name.contains("flaky") && daemon.output.contains("reads the log") && daemon.output.contains("all green"), "{daemon:?}");
    until("staging follows the flaky position", || serves(&e, &r, "staging") == Some(p));
    e.promote(&r, "production", Some(p), false, "ada", "test").unwrap();
    assert_eq!(serves(&e, &r, "production"), Some(p), "a person promotes a flaky position as a green one");
}

/// Red twice is red, as it was: staging stays, a promotion needs --force. A shard that flaked beside
/// it still says so. With `auto_revert = false`, the red position stays on the line for a person.
#[test]
fn a_shard_red_twice_is_red() {
    let (e, d) = engine();
    let (ran, runs, red) = (d.path.join("ran"), d.path.join("runs"), d.path.join("red"));
    let toml = format!(
        "[check]\n[[check.full_shards]]\nname = \"web\"\nrun = \"echo run >> '{}'; echo ' FAIL  src/a.test.ts > a > sums'; exit 1\"\n[[check.full_shards]]\nname = \"daemon\"\nrun = \"{}\"\n[envs.staging]\nfollow = \"full\"\n[line]\nauto_revert = false\n",
        red.display(),
        flaky_shard(&ran, &runs)
    );
    let r = repo(&e, &toml);
    let p = land(&e, &r, "a.txt");
    assert_eq!(full(&e, &r, p), Verdict::Red);
    assert_eq!(lines(&red), 2, "red, then red again: twice, never more");
    assert_eq!(events(&e, &r, "line.red").len(), 1);
    let flaky = events(&e, &r, "line.flaky");
    assert_eq!(flaky.iter().map(|v| v["shard"].as_str().unwrap()).collect::<Vec<_>>(), ["daemon"]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(serves(&e, &r, "staging"), None, "staging does not follow a red position");
    assert_eq!((e.head(&r).unwrap().position, lines(&red)), (p, 2), "nothing reverted, nothing run again");
    let err = e.promote(&r, "production", Some(p), false, "ada", "test").unwrap_err().to_string();
    assert!(err.contains("no green full suite"), "{err}");
}

/// A task's proof gates its landing: a red step is the agent's to read, never run again to see.
#[test]
fn a_task_proof_never_runs_again() {
    let (e, d) = engine();
    let runs = d.path.join("runs");
    let toml = format!("[check]\n[[check.steps]]\nname = \"tests\"\nrun = \"echo run >> '{0}'; [ $(wc -l < '{0}') -ge 2 ]\"\n", runs.display());
    let r = repo(&e, &toml);
    let t = open(&e, &r, "a.txt");
    assert_eq!(e.check(&t.id).unwrap().verdict, Verdict::Red);
    assert_eq!(lines(&runs), 1, "the red step ran once");
}

/// `[[check.schedule]]`: at its time (UTC), the shards it names run on the head, once that day, the
/// next day again; its red counts for the position as any red does, and says it was scheduled.
#[test]
fn a_scheduled_suite_runs_its_shards_on_the_head_once_a_day() {
    let (e, _d) = engine();
    let r = repo(&e, "[[check.full_shards]]\nname = \"unit\"\nrun = \"echo unit\"\n[[check.full_shards]]\nname = \"nightly\"\nrun = \"echo nightly; exit 1\"\n[[check.schedule]]\nat = \"02:00\"\nshards = [\"nightly\"]\n");
    const DAY: i64 = 86_400_000;
    let at = |day: i64, h: i64, m: i64| (20_754 + day) * DAY + (h * 60 + m) * 60_000; // from 28/10/2026, UTC
    assert!(e.scheduled(at(0, 1, 59)).unwrap().is_empty(), "not yet");
    assert_eq!(e.scheduled(at(0, 2, 0)).unwrap(), [(r.clone(), 0)]);
    assert!(e.scheduled(at(0, 2, 30)).unwrap().is_empty(), "once a day");
    assert_eq!(full(&e, &r, 0), Verdict::Red);
    let red = events(&e, &r, "line.red");
    assert_eq!(red[0]["scheduled"], "02:00");
    let proof = e.db.lock().unwrap().proof(red[0]["proof"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(proof.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["nightly"], "only the shards it names");
    assert!(e.scheduled(at(1, 3, 0)).unwrap().is_empty(), "past the hour after its time, the day is missed");
    assert_eq!(e.scheduled(at(2, 2, 5)).unwrap().len(), 1, "and the next day it runs again");
    until("the second scheduled suite", || events(&e, &r, "line.red").len() == 2);
    let bad = toml::from_str::<avp::check::AvpConfig>("[[check.schedule]]\nat = \"2am\"\n").unwrap_err().to_string();
    assert!(bad.contains("\"HH:MM\""), "{bad}");
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
