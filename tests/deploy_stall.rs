//! A deploy AVP runs that prints nothing for AVP_DEPLOY_STALL_S says so in `avp envs` and to the
//! webhooks (`env.stalled`), and goes on. Alone in its binary: it sets the variable for the process.
#![cfg(feature = "cloud")]

use avp::engine::Engine;
use avp::model::*;
use std::time::{Duration, Instant};

#[test]
fn a_silent_deploy_is_said_stalled_and_goes_on() {
    std::env::set_var("AVP_DEPLOY_STALL_S", "1");
    let dir = std::env::temp_dir().join(format!("avp-deploy-stall-{}", ulid::Ulid::new()));
    let e = Engine::open(&dir.join("data")).unwrap();
    let r = e.create_repo("app", LeasePolicy::Advisory).unwrap().id;
    let toml = "[envs.staging]\ndeploy = { run = \"echo migrating; sleep 4; echo done\" }\n";
    e.import(&r, &[ImportEntry { path: "avp.toml".into(), blob: e.objects.put(toml.as_bytes()).unwrap(), mode: 0o100644 }], "imported").unwrap();
    let promoting = {
        let (e, r) = (e.clone(), r.clone());
        std::thread::spawn(move || e.promote(&r, "staging", Some(0), true, "ada", "test"))
    };
    let stalled = || e.events_for(&r, None, 0, 1000).unwrap().into_iter().find(|v| v.kind == "env.stalled");
    let deadline = Instant::now() + Duration::from_secs(60);
    while stalled().is_none() {
        assert!(Instant::now() < deadline, "never said stalled");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(stalled().unwrap().data["step"], "migrating");
    let step = e.envs_view(&r).unwrap().into_iter().find(|v| v.name == "staging").and_then(|v| v.progress).map(|p| p.step).unwrap_or_default();
    assert!(step.starts_with("stalled? nothing printed for ") && step.ends_with(", last: migrating"), "{step}");
    assert_eq!(promoting.join().unwrap().unwrap().position, 0, "it went on, and served");
    let _ = std::fs::remove_dir_all(&dir);
}
