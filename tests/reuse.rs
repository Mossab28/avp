//! A shard of the full suite that says what it reads (`inputs`): green on a position, it does not
//! run again on one with the same inputs; `{changed}` names the inputs that changed; `audit` runs
//! and tells; a red behind a green that stood is told with the files that changed outside.
#![cfg(feature = "cloud")]

use avp::engine::{Engine, LandOutcome};
use avp::model::*;
use std::path::Path;
use std::time::{Duration, Instant};

fn engine() -> (Engine, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path.join("data")).unwrap(), dir)
}

/// A repo whose position 0 holds `avp.toml` and one file of each folder the shards read.
fn repo(e: &Engine, toml: &str) -> String {
    let r = e.create_repo("reuse", LeasePolicy::Advisory).unwrap().id;
    let file = |path: &str, text: &str| ImportEntry { path: path.into(), blob: e.objects.put(text.as_bytes()).unwrap(), mode: 0o100644 };
    e.import(&r, &[file("avp.toml", toml), file("web/a.ts", "a\n"), file("daemon/b.ts", "b\n"), file("docs/c.md", "c\n")], "imported").unwrap();
    r
}

fn land(e: &Engine, repo: &str, path: &str, text: &str) -> i64 {
    let t = e.open_task(repo, OpenTask { goal: format!("write {path}"), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    e.write(&t.id, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(e.objects.put(text.as_bytes()).unwrap()), mode: 0o100644 }).unwrap();
    let landed = match e.land(&t.id).unwrap() {
        // avp.toml is guarded: proven, it waits for a person.
        LandOutcome::NotLanded(n) if n.reason == "approval" => e.approve(&t.id, "ada", true, "").and_then(|_| e.land(&t.id)).unwrap(),
        other => other,
    };
    match landed {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
}

/// The verdict of the suite of `position`, and its event.
fn full(e: &Engine, repo: &str, position: i64) -> (Verdict, serde_json::Value) {
    let read = || e.db.lock().unwrap().line_at(repo, position).unwrap().full;
    let event = || e.events_for(repo, None, 0, 1000).unwrap().into_iter().rev().find(|v| matches!(v.kind.as_str(), "line.green" | "line.red") && v.data["position"] == position);
    let start = Instant::now();
    while read().is_none() || event().is_none() {
        assert!(start.elapsed() < Duration::from_secs(60), "the full suite of {position}: not within 60 s");
        std::thread::sleep(Duration::from_millis(50));
    }
    (read().unwrap(), event().unwrap().data)
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(String::from).collect()
}

fn shard(name: &str, run: &str, more: &str) -> String {
    format!("[[check.full_shards]]\nname = \"{name}\"\nrun = \"{run}\"\n{more}\n")
}

/// The common day on Murmell's line: a change to the web app, then one to the daemon. Each shard
/// runs on the positions that change what it reads, and takes its green from before on the others.
#[test]
fn a_shard_runs_only_where_its_inputs_changed() {
    let (e, d) = engine();
    let (web, daemon, all) = (d.path.join("web"), d.path.join("daemon"), d.path.join("all"));
    let toml = format!(
        "[check]\n{}{}{}",
        shard("web", &format!("echo run >> '{}'", web.display()), "inputs = [\"web/\"]"),
        shard("daemon", &format!("echo run >> '{}'", daemon.display()), "inputs = [\"**\", \"!web/\", \"!**/*.md\"]"),
        shard("all", &format!("echo run >> '{}'", all.display()), ""),
    );
    let r = repo(&e, &toml);
    let p1 = land(&e, &r, "web/a.ts", "a2\n");
    assert_eq!(full(&e, &r, p1).0, Verdict::Green);
    let p2 = land(&e, &r, "daemon/b.ts", "b2\n");
    let (verdict, event) = full(&e, &r, p2);
    assert_eq!(verdict, Verdict::Green);
    assert_eq!(event["reused"], serde_json::json!(["web"]), "{event}");
    let p3 = land(&e, &r, "docs/c.md", "c2\n");
    let (verdict, event) = full(&e, &r, p3);
    assert_eq!((verdict, &event["reused"]), (Verdict::Green, &serde_json::json!(["web", "daemon"])));
    assert_eq!((lines(&web).len(), lines(&daemon).len(), lines(&all).len()), (1, 2, 3), "a shard without inputs runs on every position");
    let step = e.db.lock().unwrap().suite_step(&r, p3, "daemon").unwrap().unwrap();
    assert!(step.exit == 0 && step.output.starts_with(&format!("reused: its inputs are those of position {p2}")), "{step:?}");
    // Back to what position 1 held: the run of then stands, whatever ran since.
    let p4 = land(&e, &r, "daemon/b.ts", "b\n");
    assert_eq!(full(&e, &r, p4).1["reused"], serde_json::json!(["web", "daemon"]));
    // Run again by hand, every shard runs: what stood is compared with it, never taken.
    e.rerun_full(&r, Some(p4)).unwrap();
    let start = Instant::now();
    while lines(&web).len() < 2 || lines(&daemon).len() < 3 {
        assert!(start.elapsed() < Duration::from_secs(60), "the suite run again by hand runs every shard");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A scanner that judges each file alone: after its first whole run, it is given the inputs that
/// changed, and the position it counts from. A change to its own command runs it whole again.
#[test]
fn changed_names_the_inputs_that_changed_since_the_last_green() {
    let (e, d) = engine();
    let seen = d.path.join("seen");
    let scan = |word: &str| format!("[check]\n{}", shard("scan", &format!("echo \\\"{word} ${{AVP_CHANGED_SINCE:-whole}}: {{changed}}\\\" >> '{}'", seen.display()), "inputs = [\"**\", \"!avp.toml\"]\nwhole_when = [\"docs/rules.md\"]"));
    let r = repo(&e, &scan("v1"));
    let p1 = land(&e, &r, "web/a.ts", "a2\n");
    full(&e, &r, p1);
    let p2 = land(&e, &r, "daemon/b.ts", "b2\n");
    assert_eq!(full(&e, &r, p2).1["changed_only"], serde_json::json!(["scan"]));
    let p3 = land(&e, &r, "docs/new.md", "n\n");
    full(&e, &r, p3);
    assert_eq!(lines(&seen), ["v1 whole: ".to_string(), format!("v1 {p1}: daemon/b.ts"), format!("v1 {p2}: docs/new.md")]);
    // The file that says how it scans changed: every file is judged again.
    let p = land(&e, &r, "docs/rules.md", "stricter\n");
    full(&e, &r, p);
    assert_eq!(lines(&seen).last().map(String::as_str), Some("v1 whole: "));
    let p4 = land(&e, &r, "avp.toml", &scan("v2"));
    full(&e, &r, p4);
    assert_eq!(lines(&seen).last().map(String::as_str), Some("v2 whole: "), "another command is another shard: whole");
}

/// `audit` on a shard whose inputs miss a file it reads: it runs on every position, says what
/// `on` would have reused, and the red found behind a green that stood names the file.
#[test]
fn audit_runs_anyway_and_tells_the_input_a_shard_did_not_say() {
    let (e, d) = engine();
    let runs = d.path.join("runs");
    // Reads docs/c.md, says web/ only.
    let run = format!("echo run >> '{}'; grep -q c docs/c.md", runs.display());
    let toml = format!("[check]\n[line]\nauto_revert = false\n{}", shard("web", &run, "inputs = [\"web/\"]\nreuse = \"audit\""));
    let r = repo(&e, &toml);
    let p1 = land(&e, &r, "web/a.ts", "a2\n");
    assert_eq!(full(&e, &r, p1).0, Verdict::Green);
    let p2 = land(&e, &r, "daemon/b.ts", "b2\n");
    let (verdict, event) = full(&e, &r, p2);
    assert_eq!((verdict, &event["reusable"], &event["reused"]), (Verdict::Green, &serde_json::json!(["web"]), &serde_json::Value::Null));
    let p3 = land(&e, &r, "docs/c.md", "broken\n");
    assert_eq!(full(&e, &r, p3).0, Verdict::Red);
    assert_eq!(lines(&runs).len(), 4, "every position ran it, the red one twice");
    let missed: Vec<_> = e.events_for(&r, None, 0, 1000).unwrap().into_iter().filter(|v| v.kind == "line.reuse_missed").map(|v| v.data).collect();
    assert_eq!(missed.len(), 1, "{missed:?}");
    assert_eq!((missed[0]["shard"].as_str(), missed[0]["green_at"].as_i64(), &missed[0]["changed_outside_inputs"]), (Some("web"), Some(p2), &serde_json::json!(["docs/c.md"])));
    // Nothing stands for those inputs any more: fixed, the shard runs and is green by its own run.
    let p4 = land(&e, &r, "docs/c.md", "c again\n");
    let (verdict, event) = full(&e, &r, p4);
    assert_eq!((verdict, &event["reusable"]), (Verdict::Green, &serde_json::Value::Null));
}

/// A shard reused on the position that broke it (it reads a file its inputs do not name) goes red
/// later, on a position that changes its inputs. That one is not to blame: the bisection starts
/// from the last position the shard ran green on, finds the one that broke it, and reverts it.
#[test]
fn a_red_behind_reused_greens_blames_the_position_that_broke_it() {
    let (e, _d) = engine();
    let r = repo(&e, &format!("[check]\n{}", shard("web", "grep -q c docs/c.md", "inputs = [\"web/\"]")));
    let p1 = land(&e, &r, "web/a.ts", "a2\n");
    assert_eq!(full(&e, &r, p1).0, Verdict::Green);
    let p2 = land(&e, &r, "daemon/b.ts", "b2\n");
    assert_eq!(full(&e, &r, p2).0, Verdict::Green);
    let broke = land(&e, &r, "docs/c.md", "broken\n");
    assert_eq!(full(&e, &r, broke).1["reused"], serde_json::json!(["web"]));
    let innocent = land(&e, &r, "web/a.ts", "a3\n");
    assert_eq!(full(&e, &r, innocent).0, Verdict::Red);
    let reverted = || e.events_for(&r, None, 0, 1000).unwrap().into_iter().find(|v| v.kind == "line.reverted").map(|v| v.data);
    let start = Instant::now();
    while reverted().is_none() {
        assert!(start.elapsed() < Duration::from_secs(60), "no revert: {:?}", e.events_for(&r, None, 0, 1000).unwrap().into_iter().filter(|v| v.kind.starts_with("line.")).map(|v| (v.kind, v.data)).collect::<Vec<_>>());
        std::thread::sleep(Duration::from_millis(50));
    }
    let said = reverted().unwrap();
    assert_eq!(said["position"].as_i64(), Some(broke), "{said}");
    assert_eq!(full(&e, &r, said["by"].as_i64().unwrap()).0, Verdict::Green, "the revert's own suite runs the shard, and is green");
}

#[test]
fn inputs_that_cannot_be_right_are_refused_or_never_reused() {
    let bad = |more: &str| toml::from_str::<avp::check::AvpConfig>(&format!("[check]\n{}", shard("web", "true", more))).unwrap().check.validate().unwrap_err().to_string();
    assert!(bad("reuse = \"audit\"").contains("no `inputs`"));
    assert!(bad("inputs = [\"!docs/\"]").contains("only take paths out"));
    assert!(bad("inputs = [\"web/\"]\nreuse_hours = 0").contains("1 to 168"));
    assert!(bad("inputs = [\"web/\"]\nartifacts = [\"out/\"]").contains("artifacts"));
    // A typo in a pattern: the shard runs on every position, and says why.
    let (e, d) = engine();
    let runs = d.path.join("runs");
    let r = repo(&e, &format!("[check]\n{}", shard("web", &format!("echo run >> '{}'", runs.display()), "inputs = [\"web/\", \"wbe/\"]")));
    let p1 = land(&e, &r, "docs/c.md", "c2\n");
    full(&e, &r, p1);
    let p2 = land(&e, &r, "docs/c.md", "c3\n");
    assert_eq!(full(&e, &r, p2).1["reused"], serde_json::Value::Null);
    assert_eq!(lines(&runs).len(), 2);
    let step = e.db.lock().unwrap().suite_step(&r, p2, "web").unwrap().unwrap();
    assert!(step.output.contains("\"wbe/\" of this shard matches no file"), "{}", step.output);
}

mod tempdir {
    pub struct Dir {
        pub path: std::path::PathBuf,
    }
    impl Dir {
        pub fn new() -> Self {
            let path = std::env::temp_dir().join(format!("avp-reuse-{}", ulid::Ulid::new()));
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
