//! Waiting on other tasks of the line: what a proof may need that teammates have not landed yet.
//!
//! 25/10 delegation bench: a subagent whose tests used two domains other subagents were still
//! writing got a red proof, wrote its own polling loop on `avp refresh` and waited ten minutes, long
//! after those domains had landed. AVP knows what is open and what the line holds: it says so on a
//! red proof, and waits for the agent when asked.

use crate::client::Client;
use crate::model::*;
use anyhow::Result;
use serde::Serialize;
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
/// that comes from work not landed yet is fixed by waiting, not by editing. When an open task holds
/// a file the proof fails on, that one is named first, to wait on it and not on the others.
pub fn others_hint(client: &Client, repo: &str, mine: &str, proof: Option<&Proof>) -> Option<String> {
    let open: Vec<Task> = client.get(&format!("/repos/{repo}/tasks?state=open")).ok()?;
    let others: Vec<&Task> = open.iter().filter(|t| t.id != mine).collect();
    if others.is_empty() {
        return None;
    }
    Some(red_hint(&others, proof))
}

fn red_hint(others: &[&Task], proof: Option<&Proof>) -> String {
    let mut holding: Vec<(&Task, &str)> = Vec::new();
    for (f, h) in proof.map(|p| p.blamed.as_slice()).unwrap_or_default() {
        if let Some(t) = others.iter().find(|t| h.as_deref() == Some(t.id.as_str())) {
            if !holding.iter().any(|(o, _)| o.id == t.id) {
                holding.push((t, f));
            }
        }
    }
    if holding.is_empty() {
        let mut s = named(others);
        s.push_str("\nIf this red comes from their work not being on the line yet, do not work around it: wait for them (`avp wait --task <id>` or `--path <file>`, or the avp_wait tool), then land again. If they need this task too, the wait says so at once, and how to land together.");
        return s;
    }
    let mut s = String::new();
    for (t, f) in &holding {
        s.push_str(&format!("The proof fails on {f}, which task {} holds: {}\n", t.id, t.goal));
    }
    let ids: Vec<&str> = holding.iter().map(|(t, _)| t.id.as_str()).collect();
    s.push_str(&format!("If this red comes from its work not being on the line yet, do not work around it: wait for that one, not the others (`avp wait --task {}`, or the avp_wait tool), then land again. If it needs this task too, the wait says so at once, and how to land both together.", ids.join(" --task ")));
    s
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

/// What `avp wait` saw: for an agent (`--json`), and as it reads (`text`).
#[derive(Clone, Debug, Serialize)]
pub struct Waited {
    /// `ready`: what it waited for is on the line, land again. `cycle`: the tasks it waits on need
    /// this one in turn, none lands alone: `land` lands them together. `landed`: this task landed
    /// meanwhile, with others. `waiting`: the time ran out.
    pub state: String,
    /// The head of the line, or the position this task landed at.
    pub position: i64,
    /// The cycle, this task first: each needs the next, the last needs this one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cycle: Vec<String>,
    /// The command that lands the cycle as one position, from this task's folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub land: Option<String>,
    /// The tasks this one landed with.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub with: Vec<String>,
    pub text: String,
}

/// Blocks until none of `tasks` (ids or id prefixes) is open any more and every one of `paths`
/// is on the head of the line, or until `timeout`. From an open task (`mine`), the server hears what
/// it waits on at every look, and the wait ends at once when the tasks it waits on need this one in
/// turn (they would wait forever on each other), or when this task landed with them.
pub fn wait(client: &Client, repo: &str, mine: Option<&str>, tasks: &[String], paths: &[String], timeout: Duration) -> Result<Waited> {
    let start = Instant::now();
    let answer = |state: &str, position: i64, text: String| Waited { state: state.into(), position, cycle: vec![], land: None, with: vec![], text };
    loop {
        let open: Vec<Task> = client.get(&format!("/repos/{repo}/tasks?state=open"))?;
        // A prefix names whatever open task starts with it, never the waiting task itself.
        let still: Vec<&Task> = tasks.iter().filter_map(|w| open.iter().find(|t| Some(t.id.as_str()) != mine && (t.id == *w || t.id.starts_with(w.as_str())))).collect();
        let head: LineEntry = client.get(&format!("/repos/{repo}/head"))?;
        let tree: Tree = client.get(&format!("/repos/{repo}/tree/{}", head.position))?;
        let missing: Vec<&String> = paths.iter().filter(|p| !tree.contains_key(p.as_str())).collect();
        // A server from before it heard waits says nothing here: the wait goes on as it did.
        let seen: WaitSeen = match mine {
            Some(m) => client.post(&format!("/repos/{repo}/tasks/{m}/waits"), &serde_json::json!({ "tasks": tasks, "paths": paths })).unwrap_or_default(),
            None => WaitSeen::default(),
        };
        if let Some(at) = seen.landed {
            let text = format!("landed: this task landed at position {at}{} (`avp land --with`): nothing is left to land", together(&seen.with));
            return Ok(Waited { with: seen.with, ..answer("landed", at, text) });
        }
        if still.is_empty() && missing.is_empty() {
            return Ok(answer("ready", head.position, format!("ready after {}s: the line is at position {}; land again", start.elapsed().as_secs(), head.position)));
        }
        if seen.cycle.len() > 1 && !seen.landing {
            let (text, land) = cycle_text(&seen);
            return Ok(Waited { cycle: seen.cycle, land: Some(land), ..answer("cycle", head.position, text) });
        }
        if start.elapsed() >= timeout {
            let mut s = format!("still waiting after {}s:", timeout.as_secs());
            for t in still {
                s.push_str(&format!("\n  task {} open: {}", t.id, t.goal));
            }
            for p in missing {
                s.push_str(&format!("\n  {p} not on the line"));
            }
            return Ok(answer("waiting", head.position, s));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// `, together with task <id>, ...` for a task landed with others (`avp land --with`), or nothing.
pub fn together(with: &[String]) -> String {
    if with.is_empty() {
        return String::new();
    }
    format!(", together with {}", with.iter().map(|t| format!("task {t}")).collect::<Vec<_>>().join(", "))
}

/// What a wait says of a cycle, and the command that lands it: every other task of it, `--with`.
fn cycle_text(seen: &WaitSeen) -> (String, String) {
    let others = &seen.cycle[1..];
    let land = format!("avp land {}", others.iter().map(|t| format!("--with {t}")).collect::<Vec<_>>().join(" "));
    let (who, told) = match others {
        [one] => (format!("this task and task {one} need each other"), "Its agent is told the same: whichever lands first lands both, the other's land says where."),
        _ => (format!("this task and tasks {} need each other", others.join(", ")), "Their agents are told the same: whichever lands first lands them all, the others' land says where."),
    };
    let text = format!("cycle: {who} ({}): none lands alone, and waiting changes nothing.\nLand them together, proven as one position: `{land}` (avp_land with `with`). {told}", seen.needs.join(", "));
    (text, land)
}

#[cfg(test)]
mod tests {
    use super::{cycle_text, open_hint, red_hint};
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

    #[test]
    fn a_red_names_the_open_task_holding_what_failed() {
        let (a, b) = (task("task_a", "invoices", &["src/invoices.ts"]), task("task_b", "docs", &[]));
        let proof = Proof { id: "proof_1".into(), task: Some("task_me".into()), base_position: 1, candidate_tree: String::new(), files: vec![], steps: vec![], verdict: Verdict::Red, blamed: vec![("src/me.ts".into(), None), ("src/invoices.ts".into(), Some("task_a".into()))], created_at: 0 };
        let h = red_hint(&[&a, &b], Some(&proof));
        assert!(h.starts_with("The proof fails on src/invoices.ts, which task task_a holds: invoices\n"), "{h}");
        assert!(h.contains("`avp wait --task task_a`"), "{h}");
        assert!(!h.contains("task_b"), "{h}");
        // Nobody holds what failed: every other task, as before.
        assert!(red_hint(&[&a, &b], None).contains("\n  task_b docs"));
    }

    #[test]
    fn a_cycle_says_the_command_that_lands_it_together() {
        let seen = WaitSeen { cycle: vec!["task_a".into(), "task_b".into(), "task_c".into()], needs: vec!["task_a waits for task_b".into(), "task_b waits for task_c".into(), "task_c is red on src/a.ts, which task_a holds".into()], ..Default::default() };
        let (text, land) = cycle_text(&seen);
        assert_eq!(land, "avp land --with task_b --with task_c");
        assert!(text.starts_with("cycle: this task and tasks task_b, task_c need each other (task_a waits for task_b, "), "{text}");
        assert!(text.contains("`avp land --with task_b --with task_c` (avp_land with `with`)"), "{text}");
    }
}
