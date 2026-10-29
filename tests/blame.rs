//! A file's history and blame read from the line: who changed it, at which position, line by line.

use avp::blame::{self, WALK};
use avp::engine::{Engine, LandOutcome};
use avp::model::*;

fn engine() -> (Engine, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    (Engine::open(&dir.path).unwrap(), dir)
}

fn import(e: &Engine, repo: &str, files: &[(&str, &str)], note: &str) {
    let entries = files.iter().map(|(p, c)| ImportEntry { path: p.to_string(), blob: e.objects.put(c.as_bytes()).unwrap(), mode: 0o100644 }).collect::<Vec<_>>();
    e.import(repo, &entries, note).unwrap();
}

/// Lands one task by `person` that writes `content` to `path` (None deletes it).
fn land(e: &Engine, repo: &str, person: &str, goal: &str, path: &str, content: Option<&str>) -> i64 {
    let actor = Actor { person: person.into(), agent: "claude-code".into(), session: "s".into() };
    let t = e.open_task(repo, OpenTask { goal: goal.into(), actor, scope: vec![], transcript: None }).unwrap();
    let (kind, blob) = match content {
        Some(c) => (WriteKind::Put, Some(e.objects.put(c.as_bytes()).unwrap())),
        None => (WriteKind::Delete, None),
    };
    e.write(&t.id, PutWrite { path: path.into(), kind, blob, mode: 0o100644 }).unwrap();
    match e.land(&t.id).unwrap() {
        LandOutcome::Landed(l) => l.position,
        LandOutcome::NotLanded(n) => panic!("{}", n.reason),
    }
}

fn line(b: &blame::Blame) -> Vec<(&str, i64, Option<&str>, bool)> {
    b.lines.iter().map(|l| (l.text.as_str(), l.position, l.person.as_deref(), l.older)).collect()
}

#[test]
fn log_and_blame_name_positions_and_people() {
    let (e, _d) = engine();
    let r = e.create_repo("hist", LeasePolicy::Advisory).unwrap().id;
    import(&e, &r, &[("src/a.txt", "one\ntwo\nthree\n"), ("src/b.txt", "b\n")], "imported");
    assert_eq!(land(&e, &r, "ada", "say TWO", "src/a.txt", Some("one\nTWO\nthree\n")), 1);
    assert_eq!(land(&e, &r, "bob", "b changes", "src/b.txt", Some("bb\n")), 2);
    assert_eq!(land(&e, &r, "cy", "count to four", "src/a.txt", Some("one\nTWO\nthree\nfour\n")), 3);
    assert_eq!(land(&e, &r, "ada", "no more b", "src/b.txt", None), 4);

    let log = blame::log(&e, &r, "src/a.txt", None, 30).unwrap();
    let got: Vec<_> = log.changes.iter().map(|c| (c.position, c.change.as_str(), c.person.as_deref(), c.goal.as_str())).collect();
    assert_eq!(got, vec![(3, "modified", Some("cy"), "count to four"), (1, "modified", Some("ada"), "say TWO"), (0, "added", None, "imported")]);
    assert_eq!(log.more, None, "the walk reached the line's start");
    assert_eq!(log.changes[0].agent.as_deref(), Some("claude-code"));

    let gone = blame::log(&e, &r, "src/b.txt", None, 30).unwrap();
    assert_eq!(gone.changes.iter().map(|c| (c.position, c.change.as_str())).collect::<Vec<_>>(), vec![(4, "deleted"), (2, "modified"), (0, "added")], "a deleted file keeps its history");

    let page = blame::log(&e, &r, "src/a.txt", None, 2).unwrap();
    assert_eq!(page.changes.iter().map(|c| c.position).collect::<Vec<_>>(), vec![3, 1]);
    assert_eq!(page.more, Some(0), "the next page starts where this one stopped");
    let rest = blame::log(&e, &r, "src/a.txt", page.more, 2).unwrap();
    assert_eq!((rest.changes.iter().map(|c| c.position).collect::<Vec<_>>(), rest.more), (vec![0], None));

    let b = blame::blame(&e, &r, "src/a.txt", None).unwrap();
    assert_eq!(b.position, 4);
    assert_eq!(line(&b), vec![("one", 0, None, false), ("TWO", 1, Some("ada"), false), ("three", 0, None, false), ("four", 3, Some("cy"), false)]);
    assert_eq!(b.lines.iter().map(|l| l.line).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(b.changes.iter().map(|c| (c.position, c.change.as_str())).collect::<Vec<_>>(), vec![(3, "modified"), (1, "modified"), (0, "added")]);
    let text = blame::blame_text(&b);
    assert!(text.contains("     1  ada       2  TWO"), "{text}");
    assert!(text.contains("     0  imported  1  one"), "{text}");

    let earlier = blame::blame(&e, &r, "src/a.txt", Some(1)).unwrap();
    assert_eq!(line(&earlier), vec![("one", 0, None, false), ("TWO", 1, Some("ada"), false), ("three", 0, None, false)], "as it was at position 1");

    let added = land(&e, &r, "bob", "a new file", "src/c.txt", Some("c1\nc2"));
    assert_eq!(line(&blame::blame(&e, &r, "src/c.txt", None).unwrap()), vec![("c1", added, Some("bob"), false), ("c2", added, Some("bob"), false)], "every line of a new file is its task's");

    let missing = blame::blame(&e, &r, "src/b.txt", None).unwrap_err().to_string();
    assert!(missing.starts_with("no src/b.txt at position"), "{missing}");
    assert!(blame::blame(&e, &r, "src/a.txt", Some(99)).unwrap_err().to_string().starts_with("no position 99"));
    land(&e, &r, "ada", "a picture", "img.bin", Some("\u{0}\u{1}"));
    assert!(blame::blame(&e, &r, "img.bin", None).unwrap_err().to_string().contains("binary"));
}

#[test]
fn context_names_six_paths_three_changes_each_and_who_holds_them() {
    let (e, _d) = engine();
    let r = e.create_repo("memory", LeasePolicy::Advisory).unwrap().id;
    let mut files = vec![("src/main.rs", "0"), ("docs/guide.md", "g")];
    let tests: Vec<String> = (1..=8).map(|i| format!("tests/t{i}.sh")).collect();
    files.extend(tests.iter().map(|t| (t.as_str(), "t")));
    import(&e, &r, &files, "imported");
    for i in 1..=4 {
        land(&e, &r, "ada", &format!("main {i}"), "src/main.rs", Some(&i.to_string()));
    }
    for (path, times) in [("tests/t1.sh", 3), ("tests/t2.sh", 2), ("tests/t3.sh", 1)] {
        for i in 0..times {
            land(&e, &r, "bob", &format!("{path} {i}"), path, Some(&format!("{i}")));
        }
    }
    land(&e, &r, "bob", "t8 goes", "tests/t8.sh", None);
    let actor = Actor { person: "cy".into(), agent: "codex".into(), session: "s".into() };
    let holder = e.open_task(&r, OpenTask { goal: "rewrite the guide".into(), actor, scope: vec!["docs/".into()], transcript: None }).unwrap();

    let scope: Vec<String> = ["src/main.rs", "tests/", "docs/guide.md", "nothing/here.rs", "src/main.rs"].map(String::from).to_vec();
    let c = blame::context(&e, &r, &scope, None).unwrap();
    let paths: Vec<&str> = c.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(paths, ["src/main.rs", "tests/t1.sh", "docs/guide.md", "tests/t2.sh", "tests/t3.sh", "tests/t4.sh"], "the asked paths in turn, a directory by its most changed files, a deleted one left out, 6 at most");
    assert_eq!(c[0].changes.iter().map(|ch| (ch.position, ch.goal.as_str())).collect::<Vec<_>>(), [(4, "main 4"), (3, "main 3"), (2, "main 2")], "the last 3, newest first");
    assert!(c.iter().all(|p| p.changes.len() <= blame::CONTEXT_CHANGES));
    assert_eq!(c[2].held_by.as_ref().map(|h| (h.task.as_str(), h.agent.as_str(), h.goal.as_str())), Some((holder.id.as_str(), "codex", "rewrite the guide")), "a lease on the directory above holds it");
    assert_eq!(c.iter().filter(|p| p.held_by.is_some()).count(), 1);

    let now = c[0].changes[0].landed_at;
    let text = blame::context_text(&c, now);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() <= blame::CONTEXT_LINES, "{text}");
    assert_eq!(lines[0], "The line's last changes to these paths, newest first (position, age, agent, goal):");
    assert_eq!(lines[1], "src/main.rs");
    assert_eq!(lines[2], "     4   1m  claude-code  main 4", "{text}");
    assert!(lines.contains(&format!("docs/guide.md  held by open task {} (codex): rewrite the guide", holder.id).as_str()), "{text}");
    assert!(lines.contains(&"     0   1m  imported     imported"), "{text}");

    assert!(blame::context(&e, &r, &scope, Some(&holder.id)).unwrap().iter().all(|p| p.held_by.is_none()), "the asking task's own leases are not named");
    assert_eq!(blame::context(&e, &r, &["nothing/".into(), ".".into()], None).unwrap(), vec![], "nothing without history");
    assert_eq!(blame::context_text(&[], now), "");
    let next = land(&e, &r, "ada", "main 5", "src/main.rs", Some("5"));
    assert_eq!(blame::context(&e, &r, &scope, None).unwrap()[0].changes[0].position, next, "the index catches up with what landed since");
}

#[test]
fn blame_walks_back_a_bounded_number_of_positions() {
    let (e, _d) = engine();
    let r = e.create_repo("long", LeasePolicy::Advisory).unwrap().id;
    let head = WALK + 100;
    for p in 0..=head {
        let a = if p < head - 10 { "old\nx\n" } else { "old\ny\n" };
        import(&e, &r, &[("a.txt", a), ("tick.txt", &p.to_string())], &format!("import {p}"));
    }
    let b = blame::blame(&e, &r, "a.txt", None).unwrap();
    assert_eq!(line(&b), vec![("old", head - WALK, None, true), ("y", head - 10, None, false)], "the first line is older than the walk went");
    assert!(blame::blame_text(&b).contains(&format!("--at {}", head - WALK)));
    assert_eq!(line(&blame::blame(&e, &r, "a.txt", Some(head - WALK)).unwrap())[0], ("old", 0, None, false), "read on from there");

    let log = blame::log(&e, &r, "a.txt", None, 30).unwrap();
    assert_eq!((log.changes.iter().map(|c| c.position).collect::<Vec<_>>(), log.more), (vec![head - 10], Some(head - WALK)), "one page reads WALK positions");
    let rest = blame::log(&e, &r, "a.txt", log.more, 30).unwrap();
    assert_eq!((rest.changes.iter().map(|c| (c.position, c.change.as_str())).collect::<Vec<_>>(), rest.more), (vec![(0, "added")], None));
    assert_eq!(rest.changes[0].goal, "import 0");

    // The context's index of an old line is built WALK positions at a time, newest first.
    let positions = || blame::context(&e, &r, &["a.txt".into()], None).unwrap()[0].changes.iter().map(|c| c.position).collect::<Vec<_>>();
    assert_eq!(positions(), vec![head - 10]);
    assert_eq!(positions(), vec![head - 10, 0], "the next one reached the line's start");
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
