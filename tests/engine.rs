//! The engine on a toy repo whose checks are shell commands: no pnpm, no node.

use std::collections::BTreeMap;
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

fn engine() -> (Engine, tempdir::Dir) {
    // The proof slots belong to the process, which runs these tests side by side: more than they
    // ever take at once, whatever AVP_CHECK_SLOTS says, so that none waits for another's.
    avp::check::set_check_slots(64);
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path).unwrap(), dir)
}

/// Polls until `done` holds, for up to a minute: what a busy machine may take (on 26/10, a dozen
/// agents proving at once), and what a calm one never waits, the loop ending as soon as it holds.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what}: not within 60 s");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How long a step lasts when a test needs it stopped: awaited instead, the test takes at least
/// this long, and a stopped one is bounded by half of it, tens of seconds a busy machine does not
/// add to a stop. A calm machine never waits for it.
const LONG: u64 = 60;

fn repo(e: &Engine, policy: LeasePolicy) -> String {
    let r = e.create_repo("toy", policy).unwrap();
    let mut entries = vec![];
    for (p, c) in [("avp.toml", TOML), ("ok.txt", "ok\n"), ("src/a.txt", "a1\na2\na3\n"), ("src/b.txt", "b\n")] {
        let blob = e.objects.put(c.as_bytes()).unwrap();
        entries.push(ImportEntry { path: p.into(), blob, mode: 0o100644 });
    }
    e.import(&r.id, &entries, "imported").unwrap();
    r.id
}

fn open(e: &Engine, repo: &str, goal: &str, scope: &[&str]) -> Task {
    e.open_task(repo, OpenTask { goal: goal.into(), actor: Actor::default(), scope: scope.iter().map(|s| s.to_string()).collect(), transcript: None }).unwrap()
}

fn put(e: &Engine, task: &str, path: &str, content: &str) -> WriteOutcome {
    let blob = e.objects.put(content.as_bytes()).unwrap();
    e.write(task, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap()
}

fn content(e: &Engine, repo: &str, position: i64, path: &str) -> String {
    let tree = e.tree_at(repo, position).unwrap();
    String::from_utf8(e.objects.get(&tree[path].blob).unwrap()).unwrap()
}

#[test]
fn open_write_check_land() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let t = open(&e, &r, "first", &["src/a.txt"]);
    assert_eq!(put(&e, &t.id, "src/a.txt", "a1\nA2\na3\n").lease, "held");
    let proof = e.check(&t.id).unwrap();
    assert_eq!(proof.verdict, Verdict::Green);
    match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => {
            assert_eq!(l.position, 1);
            assert_eq!(l.proof, proof.id, "the green proof is reused, not rerun");
        }
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
    assert_eq!(content(&e, &r, 1, "src/a.txt"), "a1\nA2\na3\n");
    assert_eq!(e.status(&t.id).unwrap().task.state, TaskState::Landed);
    assert!(e.status(&t.id).unwrap().leases.is_empty(), "landing releases leases");
}

#[test]
fn red_proof_does_not_land() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let t = open(&e, &r, "bad", &[]);
    put(&e, &t.id, "src/b.txt", "FORBIDDEN\n");
    match e.land(&t.id).unwrap() {
        LandOutcome::NotLanded(n) => {
            assert_eq!(n.reason, "red");
            assert_eq!(n.proof.unwrap().verdict, Verdict::Red);
        }
        LandOutcome::Landed(_) => panic!("landed a red task"),
    }
    assert_eq!(e.head(&r).unwrap().position, 0);
}

#[test]
fn moved_line_merges_disjoint_hunks_and_flags_overlap() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let first = open(&e, &r, "first line", &[]);
    let second = open(&e, &r, "third line", &[]);
    put(&e, &first.id, "src/a.txt", "A1\na2\na3\n");
    put(&e, &second.id, "src/a.txt", "a1\na2\nA3\n");
    assert!(matches!(e.land(&first.id).unwrap(), LandOutcome::Landed(_)));
    match e.land(&second.id).unwrap() {
        LandOutcome::Landed(l) => {
            assert!(l.replayed);
            assert_eq!(content(&e, &r, 2, "src/a.txt"), "A1\na2\nA3\n", "both intents kept");
        }
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
    // Same hunk on both sides: a conflict comes back to the task with markers.
    let third = open(&e, &r, "second line, mine", &[]);
    let fourth = open(&e, &r, "second line, theirs", &[]);
    put(&e, &third.id, "src/a.txt", "A1\nmine\nA3\n");
    put(&e, &fourth.id, "src/a.txt", "A1\ntheirs\nA3\n");
    assert!(matches!(e.land(&third.id).unwrap(), LandOutcome::Landed(_)));
    match e.land(&fourth.id).unwrap() {
        LandOutcome::NotLanded(n) => {
            assert_eq!(n.reason, "conflict");
            assert_eq!(n.conflicts, vec!["src/a.txt".to_string()]);
            let st = e.status(&fourth.id).unwrap();
            let live = Engine::effective_writes(&st.writes);
            let w = live["src/a.txt"];
            assert_eq!(w.kind, WriteKind::Conflict);
            let text = String::from_utf8(e.objects.get(w.blob.as_ref().unwrap()).unwrap()).unwrap();
            assert!(text.contains("<<<<<<<") && text.contains("mine") && text.contains("theirs"));
            assert_eq!(st.task.base_position, 3, "the task now sits on the moved head");
        }
        LandOutcome::Landed(_) => panic!("landed over a conflict"),
    }
    // Settled: a plain write replaces the conflict and the task lands.
    put(&e, &fourth.id, "src/a.txt", "A1\nmine and theirs\nA3\n");
    assert!(matches!(e.land(&fourth.id).unwrap(), LandOutcome::Landed(_)));
    assert_eq!(content(&e, &r, 4, "src/a.txt"), "A1\nmine and theirs\nA3\n");
}

#[test]
fn leases_advisory_tell_and_strict_refuse() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let a = open(&e, &r, "a", &["src"]);
    let b = open(&e, &r, "b", &[]);
    let out = put(&e, &b.id, "src/a.txt", "x\n");
    assert_eq!(out.lease, "taken_over");
    assert_eq!(out.previous_holder.as_deref(), Some(a.id.as_str()));
    let seen = e.events_for(&r, Some(&a.id), 0, 100).unwrap();
    assert!(seen.iter().any(|ev| ev.kind == "lease.taken_over" && ev.task.as_deref() == Some(&a.id)), "the holder is told");

    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Strict);
    let a = open(&e, &r, "a", &["src/a.txt"]);
    let b = open(&e, &r, "b", &[]);
    let out = put(&e, &b.id, "src/a.txt", "x\n");
    assert_eq!(out.lease, "refused");
    assert_eq!(out.previous_holder.as_deref(), Some(a.id.as_str()));
    assert!(Engine::effective_writes(&e.status(&b.id).unwrap().writes).is_empty(), "a refused write is not recorded live");
}

#[test]
fn undo_and_why() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let t = open(&e, &r, "two writes", &[]);
    put(&e, &t.id, "src/b.txt", "b2\n");
    put(&e, &t.id, "src/b.txt", "b3\n");
    assert_eq!(e.undo(&t.id, 1, None).unwrap(), 1);
    let st = e.status(&t.id).unwrap();
    let live = Engine::effective_writes(&st.writes);
    assert_eq!(live["src/b.txt"].seq, 1);
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
    assert_eq!(content(&e, &r, 1, "src/b.txt"), "b2\n");
    let why = e.why(&r, "src/b.txt", None).unwrap();
    assert_eq!(why.position, 1);
    assert_eq!(why.task.unwrap().goal, "two writes");
    assert_eq!(e.why(&r, "src/a.txt", None).unwrap().position, 0, "untouched since the import");
    let _: BTreeMap<String, Entry> = e.tree_at(&r, 1).unwrap();
}

/// Save points: back to one, forward to a later one, and after the task was replayed on the line,
/// the save's changes merged with what the line changed since.
#[test]
fn save_points_go_back_forward_and_follow_the_line() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let t = open(&e, &r, "save points", &["src"]);
    let tree = || {
        let st = e.status(&t.id).unwrap();
        e.candidate(&st.task, &st.writes).unwrap()
    };
    let text = |tree: &Tree, p: &str| String::from_utf8(e.objects.get(&tree[p].blob).unwrap()).unwrap();
    put(&e, &t.id, "src/a.txt", "a1\na2\nA3\n");
    assert_eq!(e.save(&t.id, "A3").unwrap().n, 1);
    put(&e, &t.id, "src/a.txt", "a1\na2\nA3\nA4\n");
    put(&e, &t.id, "src/new.txt", "new\n");
    assert_eq!(e.save(&t.id, "A4 and new").unwrap().n, 2);

    assert_eq!(e.restore(&t.id, "1").unwrap().files, vec!["src/a.txt", "src/new.txt"]);
    assert_eq!(text(&tree(), "src/a.txt"), "a1\na2\nA3\n");
    assert!(!Engine::effective_writes(&e.status(&t.id).unwrap().writes).contains_key("src/new.txt"), "a file the save did not have is no write at all");
    e.restore(&t.id, "A4 and new").unwrap();
    assert_eq!(text(&tree(), "src/a.txt"), "a1\na2\nA3\nA4\n", "forward, by its message");
    assert_eq!(text(&tree(), "src/new.txt"), "new\n");

    let other = open(&e, &r, "the line moves", &[]);
    put(&e, &other.id, "src/a.txt", "A1\na2\na3\n");
    put(&e, &other.id, "src/b.txt", "B\n");
    assert!(matches!(e.land(&other.id).unwrap(), LandOutcome::Landed(_)));
    e.catch_up(&t.id).unwrap();
    let back = e.restore(&t.id, "1").unwrap();
    assert_eq!((back.replayed_on, back.conflicts.len()), (Some(1), 0));
    assert_eq!(text(&tree(), "src/a.txt"), "A1\na2\nA3\n", "the save's change merged with the line's");
    assert_eq!(text(&tree(), "src/b.txt"), "B\n", "what only the line changed stays");
    assert!(matches!(e.restore(&t.id, "9"), Err(err) if err.to_string().contains("1 (A3), 2 (A4 and new)")));
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

#[test]
fn landing_rule_waits_for_a_person() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    e.set_landing(&r, LandingRule::ProofAndApproval).unwrap();
    let t = open(&e, &r, "needs a yes", &[]);
    put(&e, &t.id, "src/b.txt", "b2\n");
    match e.land(&t.id).unwrap() {
        LandOutcome::NotLanded(n) => assert_eq!(n.reason, "approval"),
        LandOutcome::Landed(_) => panic!("landed without a word"),
    }
    e.approve(&t.id, "mossab", false, "rename it").unwrap();
    match e.land(&t.id).unwrap() {
        LandOutcome::NotLanded(n) => {
            assert_eq!(n.reason, "refused");
            assert_eq!(n.word.unwrap().reason, "rename it");
        }
        LandOutcome::Landed(_) => panic!("landed after a refusal"),
    }
    e.approve(&t.id, "mossab", true, "").unwrap();
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
}

#[test]
fn an_empty_task_does_not_land() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let t = open(&e, &r, "nothing", &[]);
    match e.land(&t.id).unwrap() {
        LandOutcome::NotLanded(n) => assert_eq!(n.reason, "empty"),
        LandOutcome::Landed(_) => panic!("landed nothing"),
    }
    assert_eq!(e.head(&r).unwrap().position, 0);
}

#[test]
fn the_same_change_twice_lands_once_and_a_landed_task_stays_landed() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let first = open(&e, &r, "fix", &[]);
    let again = open(&e, &r, "the same fix", &[]);
    put(&e, &first.id, "src/b.txt", "B\n");
    put(&e, &again.id, "src/b.txt", "B\n");
    assert!(matches!(e.land(&first.id).unwrap(), LandOutcome::Landed(l) if l.position == 1));
    match e.land(&again.id).unwrap() {
        LandOutcome::NotLanded(n) => assert_eq!(n.reason, "already"),
        LandOutcome::Landed(l) => panic!("landed an empty position {}", l.position),
    }
    assert_eq!(e.head(&r).unwrap().position, 1);
    let refused = e.abandon(&first.id).unwrap_err().to_string();
    assert!(refused.contains("landed at position 1"), "{refused}");
    assert_eq!(e.status(&first.id).unwrap().task.state, TaskState::Landed);
}

#[test]
fn promotions_follow_the_rules_of_avp_toml() {
    let (e, _d) = engine();
    let r = e.create_repo("envs", LeasePolicy::Advisory).unwrap().id;
    let toml = r#"
[check]
full = "test -f ok.txt"
[envs.staging]
follow = "full"
deploy = { github_workflow = "deploy-staging.yml", inputs = { source_sha = "{git}" } }
[envs.production]
after = "staging"
approvers = ["mossab"]
deploy = { github_workflow = "deploy-production.yml", approve_environment = "production" }
"#;
    let mut entries = vec![];
    for (p, c) in [("avp.toml", toml), ("ok.txt", "ok\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    assert!(e.promote(&r, "staging", Some(0), false, "mossab", "human").is_err(), "no green full suite on the import");
    let t = open(&e, &r, "a change", &[]);
    put(&e, &t.id, "two.txt", "2\n");
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
    // The full suite runs after the landing, on its own thread; staging follows it when green.
    let mut wanted = None;
    until("staging follows the green full suite", || {
        wanted = e.db.lock().unwrap().promotion(&r, "staging").unwrap();
        wanted.is_some()
    });
    let w = wanted.unwrap();
    assert_eq!((w.position, w.by.as_str()), (1, "avp"));
    let err = e.promote(&r, "production", None, false, "gabriel", "human").unwrap_err().to_string();
    assert!(err.contains("staging serves nothing yet"), "{err}");
    e.db.lock().unwrap().set_env(&r, "staging", 1).unwrap(); // what the bridge observes once deployed
    let err = e.promote(&r, "production", None, false, "gabriel", "human").unwrap_err().to_string();
    assert!(err.contains("promoted by mossab only"), "{err}");
    let p = e.promote(&r, "production", None, false, "mossab", "claude-code").unwrap();
    let w = p.wanted.unwrap();
    assert_eq!((w.position, w.by.as_str(), w.agent.as_str()), (1, "mossab", "claude-code"));
    assert_eq!(p.deploy.unwrap().approve_environment.as_deref(), Some("production"));
    let view = e.envs_view(&r).unwrap();
    assert_eq!(view.iter().map(|e| (e.name.as_str(), e.position)).collect::<Vec<_>>(), [("production", -1), ("staging", 1)]);
    // Production deployed past that promotion by other means (GitHub's button): the promotion is
    // no longer asked for, or a bridge carrying it out would take production back to 1.
    let t = open(&e, &r, "another change", &[]);
    put(&e, &t.id, "three.txt", "3\n");
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
    e.set_git(&r, 2, &"b".repeat(40)).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    e.observe_env(&r, "production", &"b".repeat(40)).unwrap();
    let prod = e.envs_view(&r).unwrap().into_iter().find(|e| e.name == "production").unwrap();
    assert_eq!((prod.position, prod.wanted.is_none()), (2, true));
    // Promoted back to 1 after that: a rollback, asked for until production serves it.
    std::thread::sleep(Duration::from_millis(5));
    e.promote(&r, "production", Some(1), false, "mossab", "human").unwrap();
    let prod = e.envs_view(&r).unwrap().into_iter().find(|e| e.name == "production").unwrap();
    assert_eq!(prod.wanted.map(|w| w.position), Some(1));
}

#[test]
fn avp_up_guesses_the_checks_of_a_python_project() {
    let base = std::env::temp_dir().join(format!("avp-test-py-{}", ulid::Ulid::new()));
    let uv = base.join("uv");
    std::fs::create_dir_all(uv.join("tests")).unwrap();
    std::fs::write(uv.join("pyproject.toml"), "[project]\nname = \"x\"\n[dependency-groups]\ndev = [\"pytest\", \"ruff\"]\n").unwrap();
    std::fs::write(uv.join("uv.lock"), "").unwrap();
    let (toml, names) = avp::up::guess_checks(&uv).expect("a uv project");
    assert_eq!(names, ["ruff", "pytest"]);
    // The setup has only pyproject.toml and uv.lock: it installs the dependencies, not the project.
    assert!(toml.contains("uv sync --frozen --all-extras --no-install-project") && toml.contains("uv run --frozen pytest -q") && toml.contains(r#"setup_links = [".venv"]"#), "{toml}");
    let req = base.join("req");
    std::fs::create_dir_all(&req).unwrap();
    std::fs::write(req.join("requirements.txt"), "requests\n").unwrap();
    std::fs::write(req.join("requirements-dev.txt"), "pytest\n").unwrap();
    let (toml, names) = avp::up::guess_checks(&req).expect("a requirements project");
    assert_eq!(names, ["pytest"]);
    assert!(toml.contains("-r requirements.txt -r requirements-dev.txt") && toml.contains(".venv/bin/python -m pytest -q"), "{toml}");
    let parsed: toml::Value = toml::from_str(&toml).unwrap();
    assert_eq!(parsed["check"]["setup_keys"].as_array().unwrap().len(), 2, "only the files that exist key the setup");
    std::fs::remove_dir_all(&base).unwrap();
}

/// A proof started in the background while the agent works: a check of the same tree takes its
/// verdict instead of proving again, and a newer tree stops it instead of waiting for it.
#[test]
fn a_speculative_proof_is_reused_or_stopped() {
    let (e, d) = engine();
    let r = e.create_repo("slow", LeasePolicy::Advisory).unwrap();
    // Each run writes down the tree it proves: what was proven twice is counted, not timed.
    let runs = d.path.join("runs");
    let toml = format!("[check]\n[[check.steps]]\nname = \"slow\"\nrun = \"cat src/a.txt >> '{}'; case $(cat src/a.txt) in slow) sleep {LONG};; *) sleep 1;; esac\"\n", runs.display());
    let mut entries = vec![];
    for (p, c) in [("avp.toml", toml.as_str()), ("src/a.txt", "a\n")] {
        let blob = e.objects.put(c.as_bytes()).unwrap();
        entries.push(ImportEntry { path: p.into(), blob, mode: 0o100644 });
    }
    e.import(&r.id, &entries, "imported").unwrap();
    let t = open(&e, &r.id, "speculate", &["src/a.txt"]);
    let ran = || std::fs::read_to_string(&runs).unwrap_or_default().lines().map(String::from).collect::<Vec<_>>();

    put(&e, &t.id, "src/a.txt", "a2\n");
    assert!(e.speculate(&t.id));
    until("the speculative proof runs", || ran() == ["a2"]);
    assert_eq!(e.check(&t.id).unwrap().verdict, Verdict::Green);
    assert_eq!(ran(), ["a2"], "the same tree is not proven twice");

    put(&e, &t.id, "src/a.txt", "slow\n");
    assert!(e.speculate(&t.id));
    until("the speculative proof of the slow tree runs", || ran() == ["a2", "slow"]);
    put(&e, &t.id, "src/a.txt", "a4\n");
    let t1 = Instant::now();
    assert_eq!(e.check(&t.id).unwrap().verdict, Verdict::Green);
    assert!(t1.elapsed().as_secs() < LONG / 2, "the proof of the older tree is stopped, not awaited ({:?})", t1.elapsed());
    assert_eq!(ran(), ["a2", "slow", "a4"]);
}

/// No write, import or layout may reach outside the folder a tree is laid out in.
#[test]
fn paths_stay_inside_the_tree() {
    for bad in ["../x", "a/../../x", "a/..", "..", "/etc/passwd", "a/./b", "a//b", "a\\b", ""] {
        assert!(!safe_path(bad), "{bad:?} is refused");
    }
    for good in ["a", "src/a.ts", ".github/workflows/ci.yml", "a/.b/..c"] {
        assert!(safe_path(good), "{good:?} is accepted");
    }
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let t = open(&e, &r, "escape", &[]);
    let blob = e.objects.put(b"x").unwrap();
    assert!(e.write(&t.id, PutWrite { path: "src/../../x".into(), kind: WriteKind::Put, blob: Some(blob.clone()), mode: 0o100644 }).is_err());
    assert!(e.import(&r, &[ImportEntry { path: "../x".into(), blob, mode: 0o100644 }], "escape").is_err());
}

/// A test that reads a stylesheet by its name is related to it: a task that only touches the
/// stylesheet still runs that test, and not the tests that do not name it.
#[test]
fn a_test_naming_a_touched_file_is_related() {
    let (e, _d) = engine();
    let r = e.create_repo("named", LeasePolicy::Advisory).unwrap();
    let toml = "[check]\n[[check.steps]]\nname = \"related\"\nrun = \"echo {files}\"\nneeds_files = true\n";
    let mut entries = vec![];
    for (p, c) in [
        ("avp.toml", toml),
        ("src/hero.css", ".hero { width: 26vw }\n"),
        ("src/hero.test.mjs", "readFileSync('src/hero.css')\n"),
        ("src/other.test.mjs", "readFileSync('src/footer.css')\n"),
    ] {
        let blob = e.objects.put(c.as_bytes()).unwrap();
        entries.push(ImportEntry { path: p.into(), blob, mode: 0o100644 });
    }
    e.import(&r.id, &entries, "imported").unwrap();
    let t = open(&e, &r.id, "css only", &["src/hero.css"]);
    put(&e, &t.id, "src/hero.css", ".hero { width: 24vw }\n");
    let proof = e.check(&t.id).unwrap();
    let out = &proof.steps.iter().find(|s| s.name == "related").expect("the step ran").output;
    assert!(out.contains("src/hero.test.mjs"), "{out}");
    assert!(!out.contains("src/other.test.mjs"), "{out}");
}

/// With AVP_TOKEN, the server knows who asks from the name given with the token, and nothing else.
#[test]
fn the_token_names_the_person() {
    use base64::Engine as _;
    let basic = |s: &str| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(s));
    let t = "s3cret-token";
    assert_eq!(avp::server::token_user(Some(&basic("gabriel:s3cret-token")), t).as_deref(), Some("gabriel"));
    assert_eq!(avp::server::token_user(Some("Bearer s3cret-token"), t).as_deref(), Some("token"));
    assert_eq!(avp::server::token_user(Some(&basic("gabriel:wrong")), t), None);
    assert_eq!(avp::server::token_user(Some(&basic(":s3cret-token")), t), None);
    assert_eq!(avp::server::token_user(Some("Bearer s3cret-toke"), t), None);
    assert_eq!(avp::server::token_user(None, t), None);
}

/// Six agents land at once, each on its own line of the same file: every one lands (the late ones
/// replayed on the moved line), the line grows by six, and the file holds all six changes.
#[test]
fn six_agents_landing_at_once_all_land() {
    let (e, _d) = engine();
    let r = e.create_repo("crowd", LeasePolicy::Advisory).unwrap();
    let lines: Vec<String> = (0..80).map(|i| format!("line {i}")).collect();
    let original = lines.join("\n") + "\n";
    let mut entries = vec![];
    for (p, c) in [("avp.toml", "[check]\n"), ("src/shared.txt", original.as_str())] {
        let blob = e.objects.put(c.as_bytes()).unwrap();
        entries.push(ImportEntry { path: p.into(), blob, mode: 0o100644 });
    }
    let start = e.import(&r.id, &entries, "imported").unwrap().position;
    let tasks: Vec<Task> = (0..6).map(|k| open(&e, &r.id, &format!("agent {k}"), &[])).collect();
    for (k, t) in tasks.iter().enumerate() {
        let mut mine = lines.clone();
        mine[5 + 12 * k] = format!("line {} changed by agent {k}", 5 + 12 * k);
        put(&e, &t.id, "src/shared.txt", &(mine.join("\n") + "\n"));
    }
    let handles: Vec<_> = tasks.iter().map(|t| { let (e, id) = (e.clone(), t.id.clone()); std::thread::spawn(move || e.land(&id).unwrap()) }).collect();
    for h in handles {
        match h.join().unwrap() {
            LandOutcome::Landed(_) => {}
            LandOutcome::NotLanded(n) => panic!("not landed: {} {:?}", n.reason, n.conflicts),
        }
    }
    let head = start + 6;
    let text = content(&e, &r.id, head, "src/shared.txt");
    for k in 0..6 {
        assert!(text.contains(&format!("changed by agent {k}")), "agent {k} is missing:\n{text}");
    }
    assert_eq!(text.lines().count(), 80);
}

/// A [tool.ruff] section is a configuration, not an installation: ruff is a step only when the
/// project lists it as a dependency (here poetry's dev group).
#[test]
fn ruff_configured_is_not_ruff_installed() {
    let base = std::env::temp_dir().join(format!("avp-test-ruff-{}", ulid::Ulid::new()));
    let p = base.join("configured");
    std::fs::create_dir_all(p.join("tests")).unwrap();
    std::fs::write(p.join("pyproject.toml"), "[project]\nname = \"x\"\n[dependency-groups]\ndev = [\"pytest >= 8\"]\n[tool.ruff]\nline-length = 100\n").unwrap();
    std::fs::write(p.join("poetry.lock"), "").unwrap();
    let (_, names) = avp::up::guess_checks(&p).expect("a poetry project");
    assert_eq!(names, ["pytest"]);
    let q = base.join("declared");
    std::fs::create_dir_all(q.join("tests")).unwrap();
    std::fs::write(q.join("pyproject.toml"), "[tool.poetry.group.dev.dependencies]\nruff = \"^0.6\"\npytest = \"^8\"\n").unwrap();
    std::fs::write(q.join("poetry.lock"), "").unwrap();
    let (_, names) = avp::up::guess_checks(&q).expect("a poetry project");
    assert_eq!(names, ["ruff", "pytest"]);
    std::fs::remove_dir_all(&base).unwrap();
}

/// In a monorepo the root tsconfig may cover nothing: the project's own `typecheck` script is the
/// step, and without one, every package's.
#[test]
fn a_monorepo_typechecks_its_packages() {
    let base = std::env::temp_dir().join(format!("avp-test-mono-{}", ulid::Ulid::new()));
    let a = base.join("with-script");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(a.join("package.json"), r#"{"scripts":{"typecheck":"turbo typecheck"},"devDependencies":{"typescript":"5"}}"#).unwrap();
    std::fs::write(a.join("pnpm-lock.yaml"), "").unwrap();
    std::fs::write(a.join("pnpm-workspace.yaml"), "packages:\n  - cli\n").unwrap();
    std::fs::write(a.join("tsconfig.json"), "{}").unwrap();
    let (toml, _) = avp::up::guess_checks(&a).expect("a pnpm monorepo");
    assert!(toml.contains(r#"run = "pnpm run typecheck""#), "{toml}");
    let b = base.join("without-script");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join("package.json"), r#"{"devDependencies":{"typescript":"5"}}"#).unwrap();
    std::fs::write(b.join("pnpm-lock.yaml"), "").unwrap();
    std::fs::write(b.join("pnpm-workspace.yaml"), "packages:\n  - cli\n").unwrap();
    let (toml, _) = avp::up::guess_checks(&b).expect("a pnpm monorepo");
    assert!(toml.contains("pnpm -r --if-present run typecheck"), "{toml}");
    std::fs::remove_dir_all(&base).unwrap();
}

/// A step with `split` runs as several jobs side by side when the task reaches more related tests
/// than one job's share, beside one job for the other steps. The first part to go red stops the
/// others: the agent reads the failure without waiting for the rest.
#[test]
fn a_split_step_runs_side_by_side_and_stops_at_the_first_red() {
    let (e, _d) = engine();
    let r = e.create_repo("split", LeasePolicy::Advisory).unwrap().id;
    let green = "[check]\n[[check.steps]]\nname = \"lint\"\nrun = \"true\"\n[[check.steps]]\nname = \"tests\"\nrun = \"echo ran {shard}\"\nsplit = 1\n";
    let mut entries = vec![];
    for (p, c) in [("avp.toml", green), ("src/a.ts", "export const a = 1;\n"), ("src/one.test.ts", "import { a } from \"./a\";\n"), ("src/two.test.ts", "import { a } from \"./a\";\n"), ("src/three.test.ts", "import { a } from \"./a\";\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    let t = open(&e, &r, "touch a", &[]);
    put(&e, &t.id, "src/a.ts", "export const a = 2;\n");
    let p = e.check(&t.id).unwrap();
    assert_eq!(p.verdict, Verdict::Green, "{:?}", p.steps);
    assert_eq!(p.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["lint", "tests"]);
    for shard in ["1/3", "2/3", "3/3"] {
        assert!(p.steps[1].output.contains(&format!("ran {shard}")), "{}", p.steps[1].output);
    }
    let red = format!("[check]\n[[check.steps]]\nname = \"lint\"\nrun = \"sleep {LONG}\"\n[[check.steps]]\nname = \"tests\"\nrun = \"case {{shard}} in 2/3) echo boom; exit 1;; *) sleep {LONG};; esac\"\nsplit = 1\n");
    // The red checks go on the line first, the next task proven under them.
    let mut entries = vec![];
    for (p, c) in [("avp.toml", red.as_str()), ("src/a.ts", "export const a = 1;\n"), ("src/one.test.ts", "import { a } from \"./a\";\n"), ("src/two.test.ts", "import { a } from \"./a\";\n"), ("src/three.test.ts", "import { a } from \"./a\";\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "red checks").unwrap();
    let t = open(&e, &r, "touch a again", &[]);
    put(&e, &t.id, "src/a.ts", "export const a = 3;\n");
    let started = Instant::now();
    let p = e.check(&t.id).unwrap();
    assert_eq!(p.verdict, Verdict::Red);
    assert!(started.elapsed().as_secs() < LONG / 2, "the red share stops the others: {:?}", started.elapsed());
    assert_eq!(p.steps.iter().map(|s| (s.name.as_str(), s.exit)).collect::<Vec<_>>(), [("tests", 1)]);
    assert!(p.steps[0].output.contains("boom"));
}

/// A task is proven by its own avp.toml, a change to its checks included, but its steps get no
/// secret the line's checks do not give: it does not hand itself one before a person reads it.
/// Landed, the checks give it.
#[test]
fn a_change_to_the_checks_gets_no_secret_the_lines_do_not_give() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    avp::check::SecretStore::new(&e.data).set(&r, "STRIPE_KEY", "pay-sandbox-value-5c71e2").unwrap();
    let t = open(&e, &r, "the pay step gets the key", &[]);
    put(&e, &t.id, "avp.toml", "[check]\n[[check.steps]]\nname = \"pay\"\nrun = \"test -z \\\"$STRIPE_KEY\\\"\"\nsecrets = [\"STRIPE_KEY\"]\n");
    let p = e.check(&t.id).unwrap();
    assert_eq!((p.verdict, p.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()), (Verdict::Green, vec!["pay"]), "its own step, without the key: {:?}", p.steps);
    e.approve(&t.id, "mossab", true, "").unwrap();
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
    let t = open(&e, &r, "proven under them", &[]);
    put(&e, &t.id, "src/a.txt", "a\n");
    assert_eq!(e.check(&t.id).unwrap().verdict, Verdict::Red, "on the line, the pay step gets the key");
}

/// Staging follows the green full suite one deploy at a time: a newer green position waits until
/// staging serves the one it was promoted to, then goes at once. Promoted again mid-deploy, it
/// would start a deploy that takes the place of the running one, and never settle.
#[test]
fn staging_takes_the_newest_green_once_its_deploy_is_served() {
    let (e, _d) = engine();
    let r = e.create_repo("follow", LeasePolicy::Advisory).unwrap().id;
    let toml = "[check]\nfull = \"test -f ok.txt\"\n[envs.staging]\nfollow = \"full\"\ndeploy = { github_workflow = \"deploy-staging.yml\" }\n";
    let mut entries = vec![];
    for (p, c) in [("avp.toml", toml), ("ok.txt", "ok\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    let green = |position: i64| until(&format!("a green full suite at {position}"), || e.db.lock().unwrap().line_at(&r, position).unwrap().full == Some(Verdict::Green));
    let wanted = || e.db.lock().unwrap().promotion(&r, "staging").unwrap().map(|p| p.position);
    for (n, file) in [(1, "one.txt"), (2, "two.txt")] {
        let t = open(&e, &r, file, &[]);
        put(&e, &t.id, file, "x\n");
        assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(_)));
        green(n);
        if n == 1 {
            until("staging follows the green full suite", || wanted() == Some(1));
        }
        // The suite's thread follows once it has said green, when it gets there: followed here too,
        // the rule has been applied to position n whatever that thread's pace on a busy machine.
        e.follow_full(&r).unwrap();
        assert_eq!(wanted(), Some(1), "position {n}: staging still deploys 1");
    }
    e.set_git(&r, 1, &"a".repeat(40)).unwrap();
    e.observe_env(&r, "staging", &"a".repeat(40)).unwrap();
    assert_eq!(wanted(), Some(2), "served 1: staging takes the newest green");
}

/// A position reads as what it changed against the one before, over HTTP as AVP Live asks for it:
/// a file modified, one added, one deleted, and nothing of the positions after it.
#[test]
fn a_position_reads_as_its_diff() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let t = open(&e, &r, "a2 says more, c arrives, b goes", &[]);
    put(&e, &t.id, "src/a.txt", "a1\nA2\na3\n");
    put(&e, &t.id, "src/c.txt", "c\n");
    e.write(&t.id, PutWrite { path: "src/b.txt".into(), kind: WriteKind::Delete, blob: None, mode: 0o100644 }).unwrap();
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(l) if l.position == 1));
    let t = open(&e, &r, "a3 too", &[]);
    put(&e, &t.id, "src/a.txt", "a1\nA2\nA3\n");
    assert!(matches!(e.land(&t.id).unwrap(), LandOutcome::Landed(l) if l.position == 2));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let client = avp::client::Client::new(&format!("http://{}", listener.local_addr().unwrap()));
    rt.spawn(async move { axum::serve(listener, avp::server::router(e)).await });
    let files: Vec<FileDiff> = client.get("/repos/toy/line/1/diff").unwrap();
    let got: Vec<(&str, &str)> = files.iter().map(|f| (f.path.as_str(), f.change.as_str())).collect();
    assert_eq!(got, [("src/a.txt", "modified"), ("src/b.txt", "deleted"), ("src/c.txt", "added")]);
    assert!(files[0].patch.contains("-a2\n+A2\n a3") && !files[0].patch.contains("A3"), "{}", files[0].patch);
    assert!(files[1].patch.contains("-b") && files[2].patch.contains("+c"));
    let err = client.get::<Vec<FileDiff>>("/repos/toy/line/0/diff").unwrap_err().to_string();
    assert!(err.contains("where the line starts"), "{err}");
    let err = client.get::<Vec<FileDiff>>("/repos/toy/line/9/diff").unwrap_err().to_string();
    assert!(err.starts_with("404") && err.contains("no position 9"), "{err}");
}
