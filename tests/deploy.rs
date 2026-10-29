//! Deploys AVP runs itself: `run = "<command>"` under [envs.<name>.deploy], on a toy repo.

use avp::check::{AvpConfig, Deploy};
use avp::engine::{Engine, LandOutcome};
use avp::model::*;
use std::path::Path;
use std::time::{Duration, Instant};

fn engine() -> (Engine, tempdir::Dir) {
    // The proof slots belong to the process, which runs these tests side by side: more than they
    // ever take at once, whatever AVP_CHECK_SLOTS says, so that none waits for another's.
    avp::check::set_check_slots(64);
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path.join("data")).unwrap(), dir)
}

/// A repo whose position 0 holds `avp.toml` and `app.txt`.
fn repo(e: &Engine, toml: &str) -> String {
    let r = e.create_repo("app", LeasePolicy::Advisory).unwrap().id;
    let mut entries = vec![];
    for (p, c) in [("avp.toml", toml), ("app.txt", "v0\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    r
}

fn land(e: &Engine, repo: &str, path: &str, content: &str) -> i64 {
    let t = e.open_task(repo, OpenTask { goal: format!("write {path}"), actor: Actor::default(), scope: vec![], transcript: None }).unwrap();
    let blob = e.objects.put(content.as_bytes()).unwrap();
    e.write(&t.id, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
    match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
}

fn env(e: &Engine, repo: &str, name: &str) -> Environment {
    e.envs_view(repo).unwrap().into_iter().find(|v| v.name == name).unwrap()
}

fn events(e: &Engine, repo: &str, kind: &str) -> Vec<serde_json::Value> {
    e.events_for(repo, None, 0, 1000).unwrap().into_iter().filter(|v| v.kind == kind).map(|v| v.data).collect()
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

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn a_deploy_is_a_command_or_a_github_workflow() {
    let cfg: AvpConfig = toml::from_str("[envs.a]\ndeploy = { run = \"./deploy.sh {position}\" }\n[envs.b]\ndeploy = { github_workflow = \"d.yml\", inputs = { sha = \"{git}\" } }\n").unwrap();
    assert!(matches!(&cfg.envs["a"].deploy, Some(Deploy::Run(c)) if c == "./deploy.sh {position}"));
    let b = cfg.envs["b"].deploy.as_ref().and_then(Deploy::github).expect("the GitHub kind");
    assert_eq!((b.github_workflow.as_str(), b.inputs["sha"].as_str()), ("d.yml", "{git}"));
    let both = toml::from_str::<AvpConfig>("[envs.a]\ndeploy = { run = \"x\", github_workflow = \"d.yml\" }\n").unwrap_err().to_string();
    assert!(both.contains("not both"), "{both}");
    let neither = toml::from_str::<AvpConfig>("[envs.a]\ndeploy = { inputs = { a = \"b\" } }\n").unwrap_err().to_string();
    assert!(neither.contains("a deploy needs `run`"), "{neither}");
}

#[test]
fn a_run_deploy_moves_the_environment_once_it_succeeds() {
    let (e, d) = engine();
    let served = d.path.join("served");
    let r = repo(&e, &format!("[envs.staging]\ndeploy = {{ run = \"cp app.txt '{}' && echo deployed {{position}}\" }}\n", served.display()));
    let s = e.promote(&r, "staging", Some(0), true, "ada", "human").unwrap();
    assert_eq!(s.position, 0, "the answer comes once the deploy is over");
    assert_eq!(read(&served), "v0\n", "run in the promoted position laid out");
    let view = env(&e, &r, "staging");
    assert_eq!((view.position, view.wanted.map(|w| w.by)), (0, Some("ada".into())));
    assert!(view.deploy.is_none(), "the bridge has nothing to carry out");
    assert!(view.progress.is_none(), "nothing deploys any more");
    let moved = events(&e, &r, "env.moved");
    let proof = moved.last().unwrap()["proof"].as_str().unwrap().to_string();
    let proof = e.db.lock().unwrap().proof(&proof).unwrap().unwrap();
    assert_eq!(proof.verdict, Verdict::Green);
    assert!(proof.steps[0].output.contains("deployed 0"), "{}", proof.steps[0].output);

    let p = land(&e, &r, "app.txt", "v1\n");
    assert_eq!(e.promote(&r, "staging", Some(p), true, "ada", "human").unwrap().position, p);
    assert_eq!(read(&served), "v1\n");
    // A rollback is a promotion like another.
    assert_eq!(e.promote(&r, "staging", Some(0), true, "ada", "human").unwrap().position, 0);
    assert_eq!(read(&served), "v0\n");
}

#[test]
fn a_failed_run_deploy_leaves_the_environment_and_says_why() {
    let (e, _d) = engine();
    let r = repo(&e, "[envs.staging]\ndeploy = { run = \"echo pushing; echo cannot reach the cluster >&2; exit 3\" }\n[envs.sha]\ndeploy = { run = \"echo {git}\" }\n");
    let err = e.promote(&r, "staging", Some(0), true, "ada", "human").unwrap_err().to_string();
    assert!(err.contains("failed (exit 3)") && err.contains("staging serves nothing yet") && err.contains("cannot reach the cluster"), "{err}");
    let view = env(&e, &r, "staging");
    assert_eq!((view.position, view.wanted.map(|w| w.position)), (-1, Some(0)), "it stays, the promotion stays asked for");
    assert_eq!(view.progress.map(|p| p.step).as_deref(), Some("failed (exit 3): cannot reach the cluster"));
    let failed = events(&e, &r, "env.failed");
    assert_eq!((failed[0]["env"].as_str(), failed[0]["exit"].as_i64()), (Some("staging"), Some(3)));
    assert!(failed[0]["output"].as_str().unwrap().contains("pushing"));
    assert!(events(&e, &r, "env.moved").is_empty());

    let err = e.promote(&r, "sha", Some(0), true, "ada", "human").unwrap_err().to_string();
    assert!(err.contains("position 0 has no git commit yet"), "{err}");
    e.set_git(&r, 0, &"c".repeat(40)).unwrap();
    assert_eq!(e.promote(&r, "sha", Some(0), true, "ada", "human").unwrap().position, 0);
}

#[test]
fn a_run_deploy_streams_its_output_as_progress() {
    let (e, d) = engine();
    let go = d.path.join("go");
    let r = repo(&e, &format!("[envs.staging]\ndeploy = {{ run = \"printf 'building images\\\\n\\\\033[32mpushing\\\\033[0m'; while [ ! -f '{}' ]; do sleep 0.05; done; echo done\" }}\n", go.display()));
    let deploying = {
        let (e, r) = (e.clone(), r.clone());
        std::thread::spawn(move || e.promote(&r, "staging", Some(0), true, "ada", "human"))
    };
    let step = || env(&e, &r, "staging").progress.map(|p| p.step).unwrap_or_default();
    until("the line it printed is the deploy's step", || step() == "building images");
    assert_eq!(env(&e, &r, "staging").position, -1);
    std::fs::write(&go, "").unwrap();
    assert_eq!(deploying.join().unwrap().unwrap().position, 0);
    let proof = events(&e, &r, "env.moved")[0]["proof"].as_str().unwrap().to_string();
    let out = e.db.lock().unwrap().proof(&proof).unwrap().unwrap().steps[0].output.clone();
    assert!(out.contains("pushing") && out.ends_with("done\n"), "{out}");
}

#[test]
fn an_environment_that_follows_the_full_suite_runs_its_deploy() {
    let (e, d) = engine();
    let served = d.path.join("served");
    let r = repo(&e, &format!("[check]\nfull = \"true\"\n[envs.staging]\nfollow = \"full\"\ndeploy = {{ run = \"cp app.txt '{}'\" }}\n", served.display()));
    let p = land(&e, &r, "app.txt", "v1\n");
    until("staging serves the green position", || env(&e, &r, "staging").position == p);
    assert_eq!(read(&served), "v1\n");
    assert_eq!(env(&e, &r, "staging").wanted.map(|w| w.by), Some("avp".into()));
}

#[test]
fn a_deploy_a_restart_cut_runs_again_once() {
    let (e, d) = engine();
    let data = d.path.join("data");
    let ran = |env: &str| d.path.join(format!("{env}-runs"));
    let command = |env: &str| format!("echo run >> '{}' && cp app.txt '{}'", ran(env).display(), d.path.join(format!("{env}-served")).display());
    let r = repo(&e, &["staging", "production", "preview"].map(|env| format!("[envs.{env}]\ndeploy = {{ run = \"{}\" }}\n", command(env))).concat());
    // What a server stopped in the middle leaves: promotions, and their deploys as it last wrote
    // them. Staging's had started, production's had started a second time, preview's waited.
    let left = |env: &str, starts: usize| {
        let db = e.db.lock().unwrap();
        let p = Promotion { position: 0, git: None, by: "ada".into(), agent: "human".into(), at: now_ms() };
        db.set_promotion(&r, env, &p).unwrap();
        let run = db.insert_deploy(&r, env, &p, &command(env)).unwrap();
        for i in 0..starts {
            if i > 0 {
                db.deploy_again(&run.id, "").unwrap();
            }
            db.deploy_started(&run.id, "runs").unwrap();
        }
    };
    left("staging", 1);
    left("production", 2);
    left("preview", 0);
    assert_eq!(e.deploys_in_flight().unwrap(), 3, "what GET /work counts");
    drop(e);

    let e = Engine::open(&data).unwrap();
    until("staging and preview serve the position they were promoted to", || env(&e, &r, "staging").position == 0 && env(&e, &r, "preview").position == 0);
    assert_eq!((read(&ran("staging")), read(&ran("preview"))), ("run\n".into(), "run\n".into()), "each ran once, from the start");
    let cut = events(&e, &r, "env.interrupted");
    let of = |env: &str| cut.iter().find(|c| c["env"] == env).cloned();
    assert_eq!(of("staging").map(|c| (c["again"].as_bool(), c["attempt"].as_i64())), Some((Some(true), Some(1))));
    assert!(of("preview").is_none(), "a deploy that waited for its turn was not cut");

    // Cut a second time: left failed, with the reason, and not run a third time.
    assert_eq!(of("production").map(|c| c["again"].as_bool()), Some(Some(false)));
    assert_eq!(read(&ran("production")), "");
    let failed = events(&e, &r, "env.failed");
    assert_eq!((failed[0]["env"].as_str(), failed[0]["reason"].as_str()), (Some("production"), Some("a restart of the server cut it 2 times")));
    let view = env(&e, &r, "production");
    assert_eq!((view.position, view.progress.map(|p| p.step).as_deref()), (-1, Some("failed: a restart of the server cut it 2 times")));
    until("nothing in flight", || e.deploys_in_flight().unwrap() == 0);

    // Opened again, nothing is left to settle.
    drop(e);
    let e = Engine::open(&data).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((read(&ran("staging")), read(&ran("production"))), ("run\n".into(), "".into()));
    assert_eq!(events(&e, &r, "env.interrupted").len(), 2);
}

/// `avp rollback`: what the environment served before, deployed as a promotion is, recorded as a
/// rollback. The full suite is not asked again, the approvers are; twice, it is back where it was.
#[test]
fn a_rollback_deploys_what_the_environment_served_before() {
    let (e, d) = engine();
    let served = d.path.join("served");
    let r = repo(&e, &format!("[envs.staging]\napprovers = [\"ada\"]\ndeploy = {{ run = \"cp app.txt '{}'\" }}\n", served.display()));
    let err = e.rollback(&r, "staging", "ada", "human").unwrap_err().to_string();
    assert!(err.contains("staging serves nothing yet"), "{err}");
    e.promote(&r, "staging", Some(0), true, "ada", "human").unwrap();
    let err = e.rollback(&r, "staging", "ada", "human").unwrap_err().to_string();
    assert!(err.contains("staging has served only position 0"), "{err}");
    let one = land(&e, &r, "app.txt", "v1\n");
    e.promote(&r, "staging", Some(one), true, "ada", "human").unwrap();
    assert_eq!(read(&served), "v1\n");
    let err = e.rollback(&r, "staging", "bob", "human").unwrap_err().to_string();
    assert!(err.contains("promoted by ada only"), "{err}");
    let back = e.rollback(&r, "staging", "ada", "claude-code").unwrap();
    assert_eq!((back.position, read(&served).as_str()), (0, "v0\n"), "deployed as a promotion is, no full suite asked");
    let (promoted, moved) = (events(&e, &r, "env.promoted"), events(&e, &r, "env.moved"));
    for said in [promoted.last().unwrap(), moved.last().unwrap()] {
        assert_eq!((said["rollback"].as_bool(), said["from"].as_i64(), said["position"].as_i64()), (Some(true), Some(1), Some(0)), "{said}");
    }
    assert!(moved[1].get("rollback").is_none(), "a promotion is no rollback: {}", moved[1]);
    assert_eq!(e.rollback(&r, "staging", "ada", "human").unwrap().position, 1, "twice: back where the first one found it");
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
