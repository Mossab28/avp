//! Stacked tasks (`avp open --on`): a task built on another open task's work lands after it, is
//! replayed on its latest changes, and stays open, without them, when that work goes.

use avp::engine::{Engine, LandOutcome};
use avp::model::*;

const TOML: &str = "[[check.steps]]\nname = \"lint\"\nrun = \"! grep -rq FORBIDDEN src\"\n";

fn engine() -> (Engine, tempdir::Dir) {
    avp::check::set_check_slots(64);
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path).unwrap(), dir)
}

fn repo(e: &Engine, policy: LeasePolicy) -> String {
    let r = e.create_repo("stack", policy).unwrap().id;
    let mut entries = vec![];
    for (p, c) in [("avp.toml", TOML), ("src/a.txt", "a1\na2\na3\na4\na5\n")] {
        entries.push(ImportEntry { path: p.into(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 });
    }
    e.import(&r, &entries, "imported").unwrap();
    r
}

fn req(goal: &str, scope: &[&str]) -> OpenTask {
    OpenTask { goal: goal.into(), actor: Actor::default(), scope: scope.iter().map(|s| s.to_string()).collect(), transcript: None }
}

fn put(e: &Engine, task: &str, path: &str, content: &str) -> WriteOutcome {
    let blob = e.objects.put(content.as_bytes()).unwrap();
    e.write(task, PutWrite { path: path.into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 }).unwrap()
}

/// A file of a task's tree (its base and its writes), or of a position.
fn read(e: &Engine, tree: &Tree, path: &str) -> Option<String> {
    tree.get(path).map(|x| String::from_utf8(e.objects.get(&x.blob).unwrap()).unwrap())
}

fn tree_of(e: &Engine, task: &str) -> Tree {
    let st = e.status(task).unwrap();
    e.candidate(&st.task, &st.writes).unwrap()
}

fn events(e: &Engine, repo: &str, task: &str, kind: &str) -> usize {
    e.events_for(repo, Some(task), 0, 1000).unwrap().into_iter().filter(|x| x.kind == kind && x.task.as_deref() == Some(task)).count()
}

#[test]
fn a_stacked_task_is_replayed_on_its_parent_and_lands_after_it() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let parent = e.open_task(&r, req("parser", &["src/a.txt"])).unwrap();
    put(&e, &parent.id, "src/a.txt", "A1\na2\na3\na4\na5\n");
    let child = e.open_stacked(&r, req("uses the parser", &[]), &parent.id[..12]).unwrap();
    assert_eq!(child.on.as_deref(), Some(parent.id.as_str()), "a start of the id names the task");
    assert_eq!(read(&e, &tree_of(&e, &child.id), "src/a.txt").as_deref(), Some("A1\na2\na3\na4\na5\n"), "the child stands on the parent's work");
    put(&e, &child.id, "src/a.txt", "A1\na2\na3\na4\nC5\n");
    put(&e, &child.id, "src/c.txt", "c\n");
    match e.land(&child.id).unwrap() {
        LandOutcome::NotLanded(n) => {
            assert_eq!((n.reason.as_str(), n.waits_for.clone()), ("stacked", vec![parent.id.clone()]));
            assert!(n.note.unwrap().contains("--stack"));
        }
        LandOutcome::Landed(_) => panic!("landed before its parent"),
    }
    // The parent changes: the child's refresh replays it on that, its own change kept on top.
    put(&e, &parent.id, "src/a.txt", "A1\na2\nP3\na4\na5\n");
    let c = e.catch_up(&child.id).unwrap();
    assert!(c.moved && c.conflicts.is_empty() && c.on.as_deref() == Some(parent.id.as_str()), "{c:?}");
    assert_eq!(read(&e, &tree_of(&e, &child.id), "src/a.txt").as_deref(), Some("A1\na2\nP3\na4\nC5\n"));
    match e.land_stack(&child.id).unwrap() {
        LandOutcome::Landed(l) => {
            assert_eq!(l.stack.iter().map(|s| (s.task.as_str(), s.position)).collect::<Vec<_>>(), vec![(parent.id.as_str(), 1)]);
            assert_eq!(l.position, 2);
        }
        LandOutcome::NotLanded(n) => panic!("{} {:?}", n.reason, n.note),
    }
    let line = e.tree_at(&r, 2).unwrap();
    assert_eq!((read(&e, &line, "src/a.txt").as_deref(), read(&e, &line, "src/c.txt").as_deref()), (Some("A1\na2\nP3\na4\nC5\n"), Some("c\n")));
    assert_eq!(read(&e, &e.tree_at(&r, 1).unwrap(), "src/c.txt"), None, "the parent lands its own change only");
}

#[test]
fn an_abandoned_parent_leaves_its_child_open_without_its_changes() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let parent = e.open_task(&r, req("helpers", &[])).unwrap();
    put(&e, &parent.id, "src/p.txt", "p\n");
    let child = e.open_stacked(&r, req("feature", &[]), &parent.id).unwrap();
    put(&e, &child.id, "src/c.txt", "c\n");
    assert!(e.check(&child.id).unwrap().verdict == Verdict::Green);
    e.abandon(&parent.id).unwrap();
    let st = e.status(&child.id).unwrap();
    assert_eq!(st.task.state, TaskState::Open);
    assert!(st.stack.as_deref().is_some_and(|w| w.contains("was abandoned")), "{:?}", st.stack);
    assert_eq!(events(&e, &r, &child.id, "task.parent_abandoned"), 1);
    // Its land goes back to the head alone, the head the parent stood on too, and proves it again.
    let LandOutcome::Landed(l) = e.land(&child.id).unwrap() else { panic!("not landed") };
    let line = e.tree_at(&r, l.position).unwrap();
    assert_eq!((read(&e, &line, "src/c.txt").as_deref(), read(&e, &line, "src/p.txt")), (Some("c\n"), None));
    assert!(l.replayed);
}

#[test]
fn a_reverted_parent_tells_its_child() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Advisory);
    let parent = e.open_task(&r, req("helpers", &[])).unwrap();
    put(&e, &parent.id, "src/p.txt", "p\n");
    let child = e.open_stacked(&r, req("feature", &[]), &parent.id).unwrap();
    assert!(matches!(e.land(&parent.id).unwrap(), LandOutcome::Landed(l) if l.position == 1));
    assert_eq!(events(&e, &r, &child.id, "task.parent_landed"), 1);
    assert_eq!(e.status(&child.id).unwrap().stack, None, "a parent on the line says nothing");
    let revert = e.open_task(&r, req("Revert position 1: helpers", &[])).unwrap();
    e.write(&revert.id, PutWrite { path: "src/p.txt".into(), kind: WriteKind::Delete, blob: None, mode: 0o100644 }).unwrap();
    assert!(matches!(e.land(&revert.id).unwrap(), LandOutcome::Landed(l) if l.position == 2));
    assert_eq!(events(&e, &r, &child.id, "task.parent_reverted"), 1);
    let w = e.status(&child.id).unwrap().stack.unwrap();
    assert!(w.contains("landed at position 1 and was reverted at position 2"), "{w}");
}

#[test]
fn a_stack_shares_its_leases() {
    let (e, _d) = engine();
    let r = repo(&e, LeasePolicy::Strict);
    let parent = e.open_task(&r, req("parser", &["src/a.txt"])).unwrap();
    let child = e.open_stacked(&r, req("uses the parser", &[]), &parent.id).unwrap();
    assert_eq!(put(&e, &child.id, "src/a.txt", "child\n").lease, "held", "the parent's file is the stack's");
    assert_eq!(put(&e, &parent.id, "src/a.txt", "parent\n").lease, "held", "and stays the parent's");
    let other = e.open_task(&r, req("elsewhere", &[])).unwrap();
    assert_eq!(put(&e, &other.id, "src/a.txt", "x\n").lease, "refused", "a task outside the stack is refused");
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
