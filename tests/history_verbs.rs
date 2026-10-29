//! History verbs Git users expect, on the engine: a position's change picked into a task three
//! ways, tags naming positions, and the halving `avp bisect` and the auto-revert share.

use avp::engine::{halve, Engine, LandOutcome};
use avp::model::*;

struct Dir(std::path::PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn engine() -> (Engine, Dir) {
    avp::check::set_check_slots(64);
    let dir = Dir(std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new())));
    std::fs::create_dir_all(&dir.0).unwrap();
    (Engine::open(&dir.0).unwrap(), dir)
}

fn repo(e: &Engine) -> String {
    let r = e.create_repo("hv", LeasePolicy::Advisory).unwrap();
    let entries: Vec<ImportEntry> = [("src/a.txt", "a1\na2\na3\na4\n"), ("src/b.txt", "b\n")].iter().map(|(p, c)| ImportEntry { path: p.to_string(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 }).collect();
    e.import(&r.id, &entries, "imported").unwrap();
    r.id
}

fn open(e: &Engine, repo: &str, goal: &str) -> Task {
    e.open_task(repo, OpenTask { goal: goal.into(), actor: Actor::default(), scope: vec![], transcript: None }).unwrap()
}

/// A file written, or deleted (None).
fn write(e: &Engine, task: &str, path: &str, content: Option<&str>) {
    let (kind, blob) = match content {
        Some(c) => (WriteKind::Put, Some(e.objects.put(c.as_bytes()).unwrap())),
        None => (WriteKind::Delete, None),
    };
    e.write(task, PutWrite { path: path.into(), kind, blob, mode: 0o100644 }).unwrap();
}

fn land(e: &Engine, repo: &str, goal: &str, files: &[(&str, Option<&str>)]) -> i64 {
    let t = open(e, repo, goal);
    for (p, c) in files {
        write(e, &t.id, p, *c);
    }
    match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{goal}: {}", n.reason),
    }
}

#[test]
fn a_reverted_change_is_picked_back_into_a_task_three_ways() {
    let (e, _d) = engine();
    let r = repo(&e);
    let p = land(&e, &r, "A4, B and c", &[("src/a.txt", Some("a1\na2\na3\nA4\n")), ("src/b.txt", Some("B\n")), ("src/c.txt", Some("c\n"))]);
    land(&e, &r, "take it back", &[("src/a.txt", Some("a1\na2\na3\na4\n")), ("src/b.txt", Some("b\n")), ("src/c.txt", None)]);
    let t = open(&e, &r, "A4, B and c again, fixed");
    write(&e, &t.id, "src/a.txt", Some("A1\na2\na3\na4\n"));
    write(&e, &t.id, "src/b.txt", Some("bee\n"));
    let picked = e.pick(&t.id, p).unwrap();
    assert_eq!((picked.files, picked.merged, picked.conflicts), (vec!["src/c.txt".to_string()], vec!["src/a.txt".to_string()], vec!["src/b.txt".to_string()]));
    let st = e.status(&t.id).unwrap();
    let tree = e.candidate(&st.task, &st.writes).unwrap();
    let text = |path: &str| String::from_utf8(e.objects.get(&tree[path].blob).unwrap()).unwrap();
    assert_eq!(text("src/a.txt"), "A1\na2\na3\nA4\n", "the task's line and the position's, merged");
    assert_eq!(text("src/c.txt"), "c\n", "back as the position added it");
    assert!(text("src/b.txt").contains("<<<<<<<") && text("src/b.txt").contains("bee") && text("src/b.txt").contains('B'), "{}", text("src/b.txt"));
    assert_eq!(e.check(&t.id).unwrap().verdict, Verdict::Red, "markers are settled before anything is proven");
}

#[test]
fn a_tag_names_a_position_moves_when_asked_and_leaves_once_git_has_let_it_go() {
    let (e, _d) = engine();
    let r = repo(&e);
    let one = land(&e, &r, "one", &[("src/b.txt", Some("b1\n"))]);
    let two = land(&e, &r, "two", &[("src/b.txt", Some("b2\n"))]);
    assert_eq!(e.tag(&r, "v1.0", Some(one), false, "ada").unwrap().position, one);
    let head = e.tag(&r, "release/2.0", None, false, "ada").unwrap();
    assert_eq!((head.position, head.note.as_str()), (two, "two"), "no position: the head");
    let refused = e.tag(&r, "v1.0", Some(two), false, "ada").unwrap_err().to_string();
    assert!(refused.contains("`avp tag -f v1.0 2`"), "{refused}");
    assert_eq!(e.tag(&r, "v1.0", Some(two), true, "ada").unwrap().position, two);
    for bad in ["12", "-x", "a..b", "v1.lock", "a b", "v1/", ""] {
        assert!(e.tag(&r, bad, None, false, "ada").is_err(), "{bad:?} taken as a tag");
    }
    assert!(e.tag(&r, "v9", Some(9), false, "ada").is_err(), "a position the line does not hold");
    // A tag the mirror wrote in git stays, deleted, until the mirror has deleted it there too.
    e.tag_git(&r, "v1.0", Some("abc1234")).unwrap();
    e.untag(&r, "v1.0", "ada").unwrap();
    e.untag(&r, "release/2.0", "ada").unwrap();
    let names = |deleted: bool| e.tags(&r, deleted).unwrap().into_iter().map(|t| t.name).collect::<Vec<_>>();
    assert!(names(false).is_empty());
    assert_eq!(names(true), ["v1.0"], "release/2.0 never reached git: gone at once");
    e.tag_git(&r, "v1.0", None).unwrap();
    assert!(names(true).is_empty());
}

#[test]
fn halving_names_the_first_bad_position_in_log_steps() {
    let mut asked = vec![];
    let first = halve(3, 100, |p| {
        asked.push(p);
        Ok(p >= 42)
    })
    .unwrap();
    assert_eq!(first, 42);
    assert!(asked.len() <= 7, "{asked:?}");
}
