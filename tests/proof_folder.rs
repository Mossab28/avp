//! A proof folder reused from one proof to the next. Alone in their binary, and one at a time: the
//! check slots belong to the process, and a test running beside another would take the slot whose
//! folder it reuses.

use avp::engine::Engine;
use avp::model::*;
use std::sync::Mutex;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A proof folder is reused from one proof to the next. What a step left there, a tracked file a
/// step wrote into, and a dependency the setup gained since must not make a proof differ from one
/// on a fresh checkout: the store behind the tree is never touched either.
#[test]
fn a_reused_proof_folder_is_a_fresh_checkout() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let e = Engine::open(&d).unwrap();
    let toml = r#"
[check]
setup = "mkdir -p node_modules && for p in $(cat deps.txt); do mkdir -p node_modules/$p; done"
setup_keys = ["deps.txt"]
setup_links = ["node_modules"]
keep = ["cache"]
[[check.steps]]
name = "build"
run = "test \"$(cat src/x.txt)\" = x && test ! -e out/x.txt && mkdir -p out && cp src/x.txt out/x.txt && echo more >> out/x.txt && echo built >> src/x.txt && mkdir -p cache && echo run >> cache/runs"
[[check.steps]]
name = "deps"
run = "for p in $(cat deps.txt); do test -d node_modules/$p || exit 1; done"
"#;
    let tree = |deps: &str| -> Tree {
        let mut t = Tree::new();
        for (p, c) in [("avp.toml", toml), ("deps.txt", deps), ("src/x.txt", "x\n")] {
            t.insert(p.into(), Entry { blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
        }
        t
    };
    let ws = avp::check::Workspace::new(&d, "r");
    let (a, b) = (tree("a\n"), tree("a\nb\n"));
    let mut dir = std::path::PathBuf::new();
    for (round, t) in [(1, &a), (2, &a), (3, &b)] {
        let (steps, d) = ws.check(&e.objects, t, &format!("{round}"), "line-0", &[], None, None).unwrap();
        for s in &steps {
            assert_eq!(s.exit, 0, "round {round}, {}: {}", s.name, s.output);
        }
        dir = d;
    }
    assert_eq!(e.objects.get(&a["src/x.txt"].blob).unwrap(), b"x\n");
    // What the project keeps, like a CI cache, stays: here every round added a line.
    assert_eq!(std::fs::read_to_string(dir.join("cache/runs")).unwrap(), "run\nrun\nrun\n");
    // Cargo's `target` is such a cache.
    let rust = std::env::temp_dir().join(format!("avp-test-rust-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&rust).unwrap();
    std::fs::write(rust.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let (toml, _) = avp::up::guess_checks(&rust).expect("a crate");
    assert!(toml.contains(r#"keep = ["target"]"#), "{toml}");
    std::fs::remove_dir_all(&rust).unwrap();
    std::fs::remove_dir_all(&d).unwrap();
}

/// Murmell's full suite at position 89 (24/10): a test built the web app into `.work/`, its bundler
/// copying `apps/web/public` there as `fs.copyFile` does, mode and all, and the shard went red on
/// `EACCES: permission denied, copyfile` in a folder an earlier run had built in. A tracked file
/// laid read-only gives a read-only copy, and a copy made once cannot be made again over itself.
/// Here a proof, then the full suite in the same slot, each copy a laid-out file twice into a folder
/// of their own, the way the bundler did; and laying the same tree again rewrites nothing.
#[test]
fn a_step_copies_a_laid_out_file_twice() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let objects = avp::objects::Objects::open(&d.join("objects")).unwrap();
    // `cp -p` keeps the mode, as `fs.copyFile` does; over a read-only copy it fails as the bundler did.
    let bundle = "{ test ! -e .work || { echo an earlier proof left .work; exit 1; }; } && mkdir -p .work/build && for i in 1 2; do cp -p apps/web/public/signal.svg bin/run.sh .work/build/ || exit 1; done && echo hashed >> .work/build/signal.svg";
    let toml = format!("[check]\nfull = {bundle:?}\n[[check.steps]]\nname = \"bundle\"\nrun = {bundle:?}\n");
    let mut tree = Tree::new();
    for (p, c, mode) in [("avp.toml", toml.as_str(), 0o100644), ("apps/web/public/signal.svg", "<svg/>\n", 0o100644), ("bin/run.sh", "#!/bin/sh\n", 0o100755)] {
        tree.insert(p.into(), Entry { blob: objects.put(c.as_bytes()).unwrap(), mode });
    }
    let ws = avp::check::Workspace::new(&d, "r");
    let mut inodes = vec![];
    for (round, full) in [(1, None), (2, Some(bundle))] {
        let (steps, dir) = ws.check(&objects, &tree, "t", "line-12", &[], full, None).unwrap();
        for s in &steps {
            assert_eq!(s.exit, 0, "round {round}, {}: {}", s.name, s.output);
        }
        for (p, e) in &tree {
            let laid = std::fs::metadata(dir.join(p)).unwrap();
            let object = std::fs::metadata(objects.path_of(&e.blob)).unwrap();
            // A file of its own, as on a fresh checkout: writable, never the store's read-only object.
            assert_ne!(laid.ino(), object.ino(), "{p} is the store's object");
            assert_eq!(laid.nlink(), 1, "{p} is linked elsewhere");
            assert_eq!(laid.permissions().mode() & 0o777, if e.mode & 0o111 != 0 { 0o755 } else { 0o644 }, "{p}");
        }
        inodes.push(tree.keys().map(|p| std::fs::metadata(dir.join(p)).unwrap().ino()).collect::<Vec<_>>());
    }
    // Fast: what the first proof laid and no step changed is not laid again.
    assert_eq!(inodes[0], inodes[1]);
    assert_eq!(objects.get(&tree["apps/web/public/signal.svg"].blob).unwrap(), b"<svg/>\n");
    std::fs::remove_dir_all(&d).unwrap();
}

/// A step that leaves a folder of the tree read-only (a test that locks its fixtures, stopped before
/// it gives the mode back: AVP kills a superseded proof) leaves nothing behind either. The next
/// proof in the slot removes what that step wrote there, lays a changed file in it, and its own
/// steps write there, as on a fresh checkout.
#[test]
fn a_folder_a_step_left_read_only_is_writable_again() {
    use std::os::unix::fs::PermissionsExt;
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let d = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let objects = avp::objects::Objects::open(&d.join("objects")).unwrap();
    let toml = "[[check.steps]]\nname = \"test\"\nrun = \"test ! -e fixtures/tmp && touch fixtures/tmp && chmod 555 fixtures\"\n";
    let tree = |fixture: &str| -> Tree {
        [("avp.toml", toml), ("fixtures/a.txt", fixture)].iter().map(|(p, c)| (p.to_string(), Entry { blob: objects.put(c.as_bytes()).unwrap(), mode: 0o100644 })).collect()
    };
    let ws = avp::check::Workspace::new(&d, "r");
    let mut dir = std::path::PathBuf::new();
    for (round, t) in [(1, tree("a\n")), (2, tree("a\n")), (3, tree("b\n"))] {
        let (steps, laid) = ws.check(&objects, &t, &format!("{round}"), "t", &[], None, None).unwrap_or_else(|e| panic!("round {round}: {e:#}"));
        for s in &steps {
            assert_eq!(s.exit, 0, "round {round}, {}: {}", s.name, s.output);
        }
        dir = laid;
    }
    assert_eq!(std::fs::read_to_string(dir.join("fixtures/a.txt")).unwrap(), "b\n");
    std::fs::set_permissions(dir.join("fixtures"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(&d).unwrap();
}
