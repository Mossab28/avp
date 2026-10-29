//! Tasks that need each other (`avp land --with`): each red alone, a wait sees the cycle, and they
//! land together as one position proven whole.

use avp::engine::{Engine, LandOutcome};
use avp::model::*;

// Each section's tests need the other's.
const TOML: &str = "[[check.steps]]\nname = \"tests\"\nrun = \"test -f src/customers.txt && test -f src/invoices.txt\"\n";

fn engine() -> (Engine, tempdir::Dir) {
    avp::check::set_check_slots(64);
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path).unwrap(), dir)
}

fn repo(e: &Engine) -> String {
    let r = e.create_repo("together", LeasePolicy::Advisory).unwrap().id;
    let mut entries = vec![];
    for (p, c) in [("avp.toml", TOML), ("src/app.txt", "a1\na2\na3\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    r
}

fn open(e: &Engine, r: &str, goal: &str) -> String {
    e.open_task(r, OpenTask { goal: goal.into(), actor: Actor::default(), scope: vec![], transcript: None }).unwrap().id
}

fn put(e: &Engine, task: &str, path: &str, content: &str) {
    let blob = e.objects.put(content.as_bytes()).unwrap();
    e.write(task, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap();
}

fn read(e: &Engine, tree: &Tree, path: &str) -> Option<String> {
    tree.get(path).map(|x| String::from_utf8(e.objects.get(&x.blob).unwrap()).unwrap())
}

#[test]
fn tasks_that_need_each_other_see_it_and_land_as_one_position() {
    let (e, _d) = engine();
    let r = repo(&e);
    let (a, b) = (open(&e, &r, "customers"), open(&e, &r, "invoices"));
    put(&e, &a, "src/customers.txt", "c\n");
    put(&e, &b, "src/invoices.txt", "i\n");
    assert!(matches!(e.land(&a).unwrap(), LandOutcome::NotLanded(n) if n.reason == "red"), "red alone");
    // The first to wait waits; the second sees that the one it waits on waits on it.
    assert!(e.waiting(&a, &[b.clone()], &[]).unwrap().cycle.is_empty());
    let seen = e.waiting(&b, &[a[..12].to_string()], &[]).unwrap();
    assert_eq!(seen.cycle, vec![b.clone(), a.clone()]);
    assert_eq!(seen.needs, vec![format!("{b} waits for {a}"), format!("{a} waits for {b}")]);
    let LandOutcome::Landed(l) = e.land_with(&a, &[b.clone()]).unwrap() else { panic!("not landed together") };
    assert_eq!((l.position, l.with.clone()), (1, vec![b.clone()]));
    let line = e.tree_at(&r, 1).unwrap();
    assert_eq!((read(&e, &line, "src/customers.txt"), read(&e, &line, "src/invoices.txt")), (Some("c\n".into()), Some("i\n".into())));
    // Each task is landed at that position, and says with which.
    let st = e.status(&b).unwrap();
    assert_eq!((st.task.state, st.task.landed_position, st.with), (TaskState::Landed, Some(1), vec![a.clone()]));
    let seen = e.waiting(&b, &[a.clone()], &[]).unwrap();
    assert_eq!((seen.landed, seen.with), (Some(1), vec![a.clone()]));
    assert!(matches!(e.land(&b).unwrap(), LandOutcome::Landed(l) if l.position == 1 && l.with == vec![a.clone()]));
    // The position's record names both, and each file the task that wrote it.
    let entry = e.head(&r).unwrap();
    assert_eq!(entry.note, format!("customers; together with {b}: invoices"));
    assert_eq!(e.why(&r, "src/invoices.txt", None).unwrap().task.map(|t| t.id), Some(b));
}

#[test]
fn a_longer_cycle_is_seen_too() {
    let (e, _d) = engine();
    let r = repo(&e);
    let (a, b, c) = (open(&e, &r, "a"), open(&e, &r, "b"), open(&e, &r, "c"));
    put(&e, &a, "src/a.txt", "a\n");
    e.waiting(&a, &[b.clone()], &[]).unwrap();
    assert!(e.waiting(&b, &[c.clone()], &[]).unwrap().cycle.is_empty());
    // c waits on the path a writes: a on b, b on c, c on a.
    assert_eq!(e.waiting(&c, &[], &["src/a.txt".into()]).unwrap().cycle, vec![c, a, b]);
}

#[test]
fn a_conflict_between_them_lands_nothing_and_changes_no_task() {
    let (e, _d) = engine();
    let r = repo(&e);
    let (a, b) = (open(&e, &r, "customers"), open(&e, &r, "invoices"));
    put(&e, &a, "src/app.txt", "a1\nA\na3\n");
    put(&e, &b, "src/app.txt", "a1\nB\na3\n");
    let LandOutcome::NotLanded(n) = e.land_with(&a, &[b.clone()]).unwrap() else { panic!("landed") };
    assert_eq!((n.reason.as_str(), n.conflicts.clone()), ("conflict", vec!["src/app.txt".to_string()]));
    assert!(n.note.unwrap().contains(&format!("tasks {a} and {b} change the same lines of src/app.txt")));
    assert_eq!(e.head(&r).unwrap().position, 0);
    assert_eq!(e.status(&b).unwrap().writes.len(), 1, "no markers written");
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
