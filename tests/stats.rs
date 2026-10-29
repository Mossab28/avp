//! `avp stats` read from a real server's database: the window, the proofs' times without their
//! output, the full suite that follows a landing.

use std::time::{Duration, Instant};
use avp::engine::{Engine, LandOutcome};
use avp::model::*;

const TOML: &str = r#"
[check]
full = "test -f ok.txt"
[[check.steps]]
name = "lint"
run = "! grep -rq FORBIDDEN src"
"#;

fn engine() -> (Engine, Dir) {
    avp::check::set_check_slots(64);
    let dir = Dir(std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new())));
    std::fs::create_dir_all(&dir.0).unwrap();
    (Engine::open(&dir.0).unwrap(), dir)
}

struct Dir(std::path::PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn land(e: &Engine, repo: &str, person: &str, goal: &str, path: &str, content: &str) -> i64 {
    let t = e.open_task(repo, OpenTask { goal: goal.into(), actor: Actor { person: person.into(), agent: "claude-code".into(), session: String::new() }, scope: vec![], transcript: None }).unwrap();
    let blob = e.objects.put(content.as_bytes()).unwrap();
    e.write(&t.id, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
    match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
}

#[test]
fn stats_of_a_line() {
    let (e, _d) = engine();
    let r = e.create_repo("toy", LeasePolicy::Advisory).unwrap().id;
    let mut entries = vec![];
    for (p, c) in [("avp.toml", TOML), ("ok.txt", "ok\n"), ("src/a.txt", "a\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    let start = now_ms();
    land(&e, &r, "ada", "say A", "src/a.txt", "A\n");
    let last = land(&e, &r, "bob", "Revert position 1: say A", "src/a.txt", "a\n");
    // The full suite runs after the landings, in the background.
    let deadline = Instant::now() + Duration::from_secs(60);
    while e.db.lock().unwrap().line_at(&r, last).unwrap().full.is_none() {
        assert!(Instant::now() < deadline, "no full suite verdict within 60 s");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A split step's output holds its shares' times: kept. Any other output stays in the database.
    let split = "2 jobs side by side (2 shares of the related tests)\n\n== share 1/2, 30s, exit 0\nok\n\n== share 2/2, 12s, exit 0\nok\n";
    let steps = vec![Step { name: "build".into(), command: "make".into(), exit: 0, seconds: 5.0, output: "a long log".into() }, Step { name: "tests".into(), command: "t".into(), exit: 0, seconds: 30.0, output: split.into() }];
    e.db.lock().unwrap().insert_proof(&r, &Proof { id: new_id("proof"), task: Some("task_x".into()), base_position: 0, candidate_tree: String::new(), files: vec!["src/a.txt".into()], steps, verdict: Verdict::Green, blamed: vec![], created_at: now_ms() }).unwrap();
    let read = e.db.lock().unwrap().proof_times(&r, start, now_ms() + 1).unwrap();
    let mine = read.iter().find(|p| p.task.as_deref() == Some("task_x")).unwrap();
    assert_eq!((mine.steps[0].output.as_str(), mine.steps[1].output.as_str(), mine.steps[1].seconds), ("", split, 30.0));

    let s = avp::stats::stats(&e, &r, start, now_ms() + 1).unwrap();
    assert_eq!((s.repo.as_str(), s.landings, s.reverts, s.by_person["ada"], s.by_agent["claude-code"]), ("toy", 2, 1, 1, 2));
    assert_eq!((s.opened_to_landed.count, s.landed_to_full.count), (2, 2), "both landings have their time to the line and to a full suite verdict");
    assert!(s.full_suites >= 1 && s.full_red == 0 && s.proofs == 3, "{s:?}");
    assert!(s.machine_minutes.proofs >= 0.8, "the split step's shares add up: 47 s at least, {s:?}");
    // The import is not a landing, and a window before the line is empty.
    assert_eq!(avp::stats::stats(&e, &r, start - 60_000, start).unwrap().landings, 0);
}

/// GitHub's answers, recorded in tests/fixtures/github: no network.
fn recorded(path: &str) -> anyhow::Result<serde_json::Value> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/github");
    let file = if path.starts_with("/repos/acme/shop/pulls?") {
        assert!(path.contains("state=closed") && path.contains("page=1"), "{path}");
        "pulls.json".to_string()
    } else if path.starts_with("/repos/acme/shop/actions/runs?created=2026-10-22T00:00:00Z..2026-10-24T00:00:00Z") {
        "runs.json".to_string()
    } else {
        let run = path.strip_prefix("/repos/acme/shop/actions/runs/").and_then(|p| p.split_once("/jobs?")).map(|(id, _)| id).unwrap_or_else(|| panic!("not recorded: {path}"));
        format!("jobs-{run}.json")
    };
    Ok(serde_json::from_str(&std::fs::read_to_string(dir.join(file))?)?)
}

#[test]
fn the_before_read_from_github() {
    let (since, until) = avp::stats::range("2026-10-22..2026-10-24", now_ms()).unwrap();
    let b = avp::stats::github_before(&recorded, "acme/shop", since, until).unwrap();
    // #101 and #100 were merged in the window; #99 was closed unmerged, #98 merged the day before.
    assert_eq!((b.pull_requests, b.opened_to_merged.median_s, b.opened_to_merged.p90_s), (2, Some(2550.0), Some(3390.0)));
    // Run 14 starts at the window's end: out. 10 + 20 min of CI, 2 and 6 of deploys; a skipped job
    // and one not finished count nothing.
    assert_eq!((b.runs, b.job_minutes, b.deploy_runs, b.deploy_runs_cancelled), (3, 38.0, 2, 1));
    assert!(avp::stats::github_before(&recorded, "acme/shop?x=1", since, until).is_err(), "owner/name only");

    let after = avp::stats::Stats { repo: "shop".into(), since: "2026-10-25T00:00:00Z".into(), until: "2026-10-27T00:00:00Z".into(), landings: 6, opened_to_landed: avp::stats::Spread::of(vec![255.0]), machine_minutes: avp::stats::MachineMinutes { proofs: 4.0, full_suites: 5.5, total: 9.5 }, ..Default::default() };
    let table = avp::stats::side_by_side(&b, &after);
    for said in ["Pull requests and CI", "4.2 min (median), 10.0x faster", "2 pull requests", "6 landings, 3.0x more", "38 min", "9.5 min, 75 % less", "50 % (1 of 2)"] {
        assert!(table.contains(said), "{said:?} not in:\n{table}");
    }
}
