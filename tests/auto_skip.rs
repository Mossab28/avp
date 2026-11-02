//! `[check] auto_skip`: a slow test file that never fails leaves the proofs (the step gets it as
//! `{skip_files}`), and comes back once a full suite sees it red.
#![cfg(feature = "cloud")]

use avp::check::{AutoSkip, AvpConfig};
use avp::engine::{Engine, LandOutcome};
use avp::model::*;
use std::time::{Duration, Instant};

#[test]
fn auto_skip_is_on_by_default_and_said_three_ways() {
    let rule = |toml: &str| toml::from_str::<AvpConfig>(toml).map(|c| c.check.auto_skip);
    assert_eq!(rule("[check]\n").unwrap(), Some(AutoSkip { after: 100, min_seconds: 5.0 }));
    assert_eq!(rule("[check]\nauto_skip = false\n").unwrap(), None);
    assert_eq!(rule("[check]\nauto_skip = { after = 30 }\n").unwrap(), Some(AutoSkip { after: 30, min_seconds: 5.0 }));
    assert!(rule("[check]\nauto_skip = { after = 0 }\n").unwrap_err().to_string().contains("after = 0 is 1 to 1000 runs"));
}

#[test]
fn a_slow_file_that_never_fails_leaves_the_proofs_until_a_full_suite_sees_it_red() {
    avp::check::set_check_slots(16);
    let dir = std::env::temp_dir().join(format!("avp-auto-skip-{}", ulid::Ulid::new()));
    let e = Engine::open(&dir.join("data")).unwrap();
    let toml = "[check]\nauto_skip = { after = 2, min_seconds = 1 }\n[[check.steps]]\nname = \"tests\"\nrun = \"echo '✓ test/slow.test.ts (1 test) 9s'; echo skipped:{skip_files}.\"\n";
    let r = e.create_repo("app", LeasePolicy::Advisory).unwrap().id;
    let entries = [("avp.toml", toml), ("test/slow.test.ts", "it()\n")].map(|(p, c)| ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    e.import(&r, &entries, "imported").unwrap();
    // What the tests step printed in the proof of a task that writes `path`.
    let prove = |path: &str| -> String {
        let t = e.open_task(&r, OpenTask { goal: format!("write {path}"), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
        e.write(&t.id, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(e.objects.put(b"x\n").unwrap()), mode: 0o100644 }).unwrap();
        assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
        let p = e.db.lock().unwrap().last_proof(&t.id).unwrap().unwrap();
        p.steps[0].output.lines().find(|l| l.starts_with("skipped:")).unwrap_or_default().to_string()
    };
    let rule = AutoSkip { after: 2, min_seconds: 1.0 };
    assert_eq!(prove("a.txt"), "skipped:.", "one green run is not two");
    prove("b.txt");
    let deadline = Instant::now() + Duration::from_secs(60);
    while avp::cloud::stats::left_out(&e, &r, &rule).is_empty() {
        assert!(Instant::now() < deadline, "the server never read the proofs");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(prove("c.txt"), "skipped:test/slow.test.ts.", "green twice and 9 s long: out of the proofs");
    assert_eq!(prove("test/slow.test.ts"), "skipped:.", "a task that writes the file runs it");
    // Red in a full suite: back in the proofs at once.
    let red = Proof { id: new_id("proof"), task: None, base_position: 1, candidate_tree: String::new(), files: vec![], steps: vec![Step { name: "full".into(), command: String::new(), exit: 1, seconds: 9.0, output: "× test/slow.test.ts (1 test | 1 failed) 9s\n".into() }], verdict: Verdict::Red, blamed: vec![], created_at: now_ms(), timing: None };
    e.db.lock().unwrap().insert_proof(&r, &red).unwrap();
    assert_eq!(prove("d.txt"), "skipped:.");
    let _ = std::fs::remove_dir_all(&dir);
}
