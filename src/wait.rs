//! Waiting on other tasks of the line: what a proof may need that teammates have not landed yet.
//!
//! 25/10 delegation bench: a subagent whose tests used two domains other subagents were still
//! writing got a red proof, wrote its own polling loop on `avp refresh` and waited ten minutes, long
//! after those domains had landed. AVP knows what is open and what the line holds: it says so on a
//! red proof, and waits for the agent when asked.

use crate::client::Client;
use crate::model::*;
use anyhow::Result;
use std::time::{Duration, Instant};

/// How many other open tasks an answer names, and how many paths of each: with 8 or 12 agents on
/// one repo, the whole list buries what the answer is for.
const NAMED: usize = 8;
const PATHS: usize = 4;

/// The other open tasks, one line each with its id, goal and the paths it holds, past `NAMED` how
/// many more: what an agent needs to wait for one of them.
fn named(others: &[&Task]) -> String {
    let mut s = String::from("Other tasks are open on this line:");
    for t in others.iter().take(NAMED) {
        let mut scope = t.scope.iter().take(PATHS).cloned().collect::<Vec<_>>().join(", ");
        if t.scope.len() > PATHS {
            scope.push_str(&format!(" and {} more", t.scope.len() - PATHS));
        }
        let scope = if scope.is_empty() { scope } else { format!(" (holds {scope})") };
        s.push_str(&format!("\n  {} {}{}", t.id, t.goal, scope));
    }
    if others.len() > NAMED {
        s.push_str(&format!("\n  and {} more (`avp tasks` or the avp_tasks tool)", others.len() - NAMED));
    }
    s
}

/// The other open tasks of the line, with the paths they hold: told with a red proof, since a red
/// that comes from work not landed yet is fixed by waiting, not by editing.
pub fn others_hint(client: &Client, repo: &str, mine: &str) -> Option<String> {
    let open: Vec<Task> = client.get(&format!("/repos/{repo}/tasks?state=open")).ok()?;
    let others: Vec<&Task> = open.iter().filter(|t| t.id != mine).collect();
    if others.is_empty() {
        return None;
    }
    let mut s = named(&others);
    s.push_str("\nIf this red comes from their work not being on the line yet, do not work around it: wait for them (`avp wait --task <id>` or `--path <file>`, or the avp_wait tool), then land again.");
    Some(s)
}

/// The other open tasks, told when a task opens: the work it builds on may be one of them. 25/10
/// bench, 8 and 12 agents on one repo: agents whose task needed another's work polled `avp log` or
/// `avp refresh` for 10 to 25 minutes, the open tasks listed in front of them, without a word on
/// how to wait for one.
pub fn open_hint(open: &[Task], mine: &str) -> Option<String> {
    let others: Vec<&Task> = open.iter().filter(|t| t.id != mine).collect();
    if others.is_empty() {
        return None;
    }
    let mut s = named(&others);
    s.push_str("\nIf this task needs the work of one of them, do not poll the line: `avp wait --task <id>` (or the avp_wait tool) waits until it lands and brings it into your folder. To build on it before it lands, open this task on it instead: `avp open --on <id>` (avp_open with `on`).");
    Some(s)
}

/// Blocks until none of `tasks` (ids or id prefixes) is open any more and every one of `paths`
/// is on the head of the line, or until `timeout`. Answers what it saw.
pub fn wait(client: &Client, repo: &str, mine: Option<&str>, tasks: &[String], paths: &[String], timeout: Duration) -> Result<String> {
    let start = Instant::now();
    loop {
        let open: Vec<Task> = client.get(&format!("/repos/{repo}/tasks?state=open"))?;
        // A prefix names whatever open task starts with it, never the waiting task itself.
        let still: Vec<&Task> = tasks.iter().filter_map(|w| open.iter().find(|t| Some(t.id.as_str()) != mine && (t.id == *w || t.id.starts_with(w.as_str())))).collect();
        let head: LineEntry = client.get(&format!("/repos/{repo}/head"))?;
        let tree: Tree = client.get(&format!("/repos/{repo}/tree/{}", head.position))?;
        let missing: Vec<&String> = paths.iter().filter(|p| !tree.contains_key(p.as_str())).collect();
        if still.is_empty() && missing.is_empty() {
            return Ok(format!("ready after {}s: the line is at position {}; land again", start.elapsed().as_secs(), head.position));
        }
        if start.elapsed() >= timeout {
            let mut s = format!("still waiting after {}s:", timeout.as_secs());
            for t in still {
                s.push_str(&format!("\n  task {} open: {}", t.id, t.goal));
            }
            for p in missing {
                s.push_str(&format!("\n  {p} not on the line"));
            }
            return Ok(s);
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::open_hint;
    use crate::model::*;

    fn task(id: &str, goal: &str, scope: &[&str]) -> Task {
        Task { id: id.into(), repo: "r".into(), goal: goal.into(), actor: Actor::default(), scope: scope.iter().map(|s| s.to_string()).collect(), base_position: 1, base_tree: String::new(), state: TaskState::Open, created_at: 0, landed_position: None, transcript: None, similar: vec![], on: None }
    }

    #[test]
    fn open_names_the_other_tasks_and_how_to_wait_for_them() {
        let open = vec![task("task_me", "mine", &[]), task("task_a", "payments", &["src/pay.ts", "src/pay.test.ts"]), task("task_b", "docs", &[])];
        let h = open_hint(&open, "task_me").unwrap();
        assert!(h.contains("\n  task_a payments (holds src/pay.ts, src/pay.test.ts)"), "{h}");
        assert!(h.contains("\n  task_b docs\n"), "{h}");
        assert!(!h.contains("task_me"), "{h}");
        assert!(h.contains("`avp wait --task <id>` (or the avp_wait tool)"), "{h}");
        assert_eq!(open_hint(&open[..1], "task_me"), None);
    }

    #[test]
    fn many_open_tasks_are_counted_past_eight() {
        let scope = ["a", "b", "c", "d", "e", "f"];
        let open: Vec<Task> = (0..12).map(|i| task(&format!("task_{i}"), &format!("goal {i}"), &scope)).collect();
        let h = open_hint(&open, "task_none").unwrap();
        assert_eq!(h.lines().filter(|l| l.starts_with("  task_")).count(), 8, "{h}");
        assert!(h.contains("\n  and 4 more (`avp tasks`"), "{h}");
        assert!(h.contains("(holds a, b, c, d and 2 more)"), "{h}");
        assert!(!h.contains("task_8"), "{h}");
    }
}
