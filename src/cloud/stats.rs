//! The numbers of `avp stats` as the server computes them, read back from the line, the tasks, the
//! proofs and the events: a landing's times end at the first event that covers its position (a full
//! suite or a deploy of a later position holds it too: the line is linear), whenever it came, while
//! the counts keep to the window. The before a person saved beside them. What a client reads and
//! prints of them is the core's (src/stats.rs).

use crate::engine::Engine;
use crate::model::*;
use crate::server::{blocking, ok, rid, R, S};
use crate::stats::{rfc3339, tenth, EnvStats, MachineMinutes, Spread, Stats};
use anyhow::Result;
use axum::extract::{Path as P, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn routes() -> Router<Engine> {
    Router::new()
        .route("/repos/{repo}/stats", get(stats_route))
        .route("/repos/{repo}/before", get(before).post(save_before))
}

/// The events the numbers come from.
pub const KINDS: [&str; 8] = ["task.replayed", "line.green", "line.red", "line.flaky", "env.promoted", "env.moved", "env.failed", "env.interrupted"];

/// What `compute` reads: the window's landings with their tasks and its proofs (steps without
/// their output, see `Db::proof_times`), and the events of `KINDS` from the window's start on.
#[derive(Default)]
pub struct Records {
    pub landings: Vec<(LineEntry, Task)>,
    pub proofs: Vec<Proof>,
    pub events: Vec<Event>,
}

/// The repo's numbers over [since, until), read from the server's database.
pub fn stats(e: &Engine, repo: &str, since: i64, until: i64) -> Result<Stats> {
    anyhow::ensure!(since < until, "since ({}) must come before until ({})", rfc3339(since), rfc3339(until));
    let db = e.db.lock().unwrap_or_else(|p| p.into_inner());
    let name = db.repo(repo)?.name;
    let mut landings = Vec::new();
    for l in db.landed_between(repo, since, until)? {
        let task = db.task(l.task.as_deref().unwrap_or_default())?;
        landings.push((l, task));
    }
    let records = Records { landings, proofs: db.proof_times(repo, since, until)?, events: db.events_of(repo, &KINDS, since)? };
    let before = db.before(repo)?;
    drop(db);
    Ok(Stats { before, ..compute(&name, since, until, &records) })
}

pub fn compute(repo: &str, since: i64, until: i64, r: &Records) -> Stats {
    let within = |at: i64| at >= since && at < until;
    let secs = |ms: i64| ms.max(0) as f64 / 1000.0;
    let mut events: Vec<&Event> = r.events.iter().collect();
    events.sort_by_key(|e| (e.at, e.id));
    let kind = |k: &'static str| move |e: &&&Event| e.kind == k;
    let in_window = |e: &&&Event| within(e.at);
    let verdicts: Vec<&Event> = events.iter().filter(|e| matches!(e.kind.as_str(), "line.green" | "line.red" | "line.flaky")).copied().collect();
    let env_of = |e: &Event| e.data["env"].as_str().map(String::from);
    let names: BTreeSet<String> = events.iter().filter(|e| e.kind.starts_with("env.")).filter_map(|e| env_of(e)).collect();
    // The first event at or after `at` whose position holds `position`.
    let first = |list: &[&Event], position: i64, at: i64| list.iter().find(|e| e.at >= at && e.data["position"].as_i64().is_some_and(|p| p >= position)).map(|e| e.at);

    let mut s = Stats { repo: repo.to_string(), since: rfc3339(since), until: rfc3339(until), landings: r.landings.len(), ..Default::default() };
    let (mut to_land, mut to_full) = (vec![], vec![]);
    let mut served: BTreeMap<&str, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for name in &names {
        let moves: Vec<&Event> = events.iter().filter(|e| e.kind == "env.moved" && env_of(e).as_ref() == Some(name)).copied().collect();
        let (after_landing, after_opening) = served.entry(name).or_default();
        for (l, t) in &r.landings {
            if let Some(at) = first(&moves, l.position, l.landed_at) {
                after_landing.push(secs(at - l.landed_at));
                after_opening.push(secs(at - t.created_at));
            }
        }
        let count = |k: &str| events.iter().filter(|e| e.kind == k && within(e.at) && env_of(e).as_ref() == Some(name)).count();
        s.envs.insert(name.clone(), EnvStats { promotions: count("env.promoted"), moves: count("env.moved"), failed: count("env.failed"), interrupted: count("env.interrupted"), ..Default::default() });
    }
    for (name, (after_landing, after_opening)) in served {
        let env = s.envs.get_mut(name).expect("every name has its entry");
        env.landed_to_served = Spread::of(after_landing);
        env.opened_to_served = Spread::of(after_opening);
    }
    let who = |w: &str| if w.trim().is_empty() { "unknown".to_string() } else { w.to_string() };
    for (l, t) in &r.landings {
        to_land.push(secs(l.landed_at - t.created_at));
        if let Some(at) = first(&verdicts, l.position, l.landed_at) {
            to_full.push(secs(at - l.landed_at));
        }
        *s.by_agent.entry(who(&t.actor.agent)).or_default() += 1;
        *s.by_person.entry(who(&t.actor.person)).or_default() += 1;
        if l.note.starts_with("Revert ") {
            s.reverts += 1;
        }
    }
    s.opened_to_landed = Spread::of(to_land);
    s.landed_to_full = Spread::of(to_full);

    // A proof with no task is a full suite's, or a deploy's: the deploy's event names it.
    let deploys: BTreeSet<&str> = events.iter().filter(|e| e.kind.starts_with("env.")).filter_map(|e| e.data["proof"].as_str()).collect();
    let (mut durations, mut proof_s, mut full_s) = (vec![], 0.0, 0.0);
    for p in &r.proofs {
        if p.task.is_none() {
            if !deploys.contains(p.id.as_str()) {
                full_s += machine_seconds(p, true);
            }
            continue;
        }
        s.proofs += 1;
        s.proofs_red += usize::from(p.verdict == Verdict::Red);
        s.proofs_blocked += usize::from(p.verdict == Verdict::Blocked);
        proof_s += machine_seconds(p, false);
        // A proof where nothing ran (unsettled conflicts) checked nothing.
        let took = wall_seconds(p);
        if took > 0.0 {
            durations.push(took);
        }
    }
    s.proof_duration = Spread::of(durations);
    s.machine_minutes = MachineMinutes { proofs: tenth(proof_s / 60.0), full_suites: tenth(full_s / 60.0), total: tenth((proof_s + full_s) / 60.0) };

    let replayed: Vec<&Event> = events.iter().filter(kind("task.replayed")).filter(in_window).copied().collect();
    s.replays = replayed.len();
    s.tasks_replayed = replayed.iter().filter_map(|e| e.task.as_deref()).collect::<BTreeSet<_>>().len();
    s.conflicts = replayed.iter().filter(|e| e.data["conflicts"].as_array().is_some_and(|c| !c.is_empty())).count();
    s.full_suites = verdicts.iter().filter(in_window).filter_map(|e| e.data["proof"].as_str()).collect::<BTreeSet<_>>().len();
    s.full_red = events.iter().filter(kind("line.red")).filter(in_window).count();
    s.flaky = events.iter().filter(kind("line.flaky")).filter(in_window).count();
    s
}

/// What a split step's output says of its shares: `== share 2/3, 41s, exit 0`, one line each.
fn shares(output: &str) -> Option<f64> {
    let head = output.lines().next()?;
    if !head.split_once(' ').is_some_and(|(n, rest)| n.parse::<usize>().is_ok() && rest.starts_with("jobs side by side")) {
        return None;
    }
    Some(output.lines().filter_map(|l| l.strip_prefix("== share ")?.split(", ").nth(1)?.strip_suffix('s')?.parse::<f64>().ok()).sum())
}

/// The name of the full suite's shard a step ran in: a flaky one says so after it.
fn shard(step: &str) -> &str {
    step.split(" (flaky:").next().unwrap_or(step)
}

/// What a proof cost the machines, in seconds: every step, a split step's shares added up (its own
/// time is its slowest share's). A full suite's shard that went red ran twice, red twice or red then
/// green (flaky), and only the second run's time is kept: it is counted twice.
fn machine_seconds(p: &Proof, full: bool) -> f64 {
    let twice: BTreeSet<&str> = if full { p.steps.iter().filter(|s| s.exit != 0 || s.name.contains(" (flaky:")).map(|s| shard(&s.name)).collect() } else { BTreeSet::new() };
    p.steps.iter().map(|s| shares(&s.output).unwrap_or(s.seconds) * if twice.contains(shard(&s.name)) { 2.0 } else { 1.0 }).sum()
}

/// How long a proof took: its steps one after the other, beside its split steps' shares.
fn wall_seconds(p: &Proof) -> f64 {
    let (split, plain): (Vec<&Step>, Vec<&Step>) = p.steps.iter().partition(|s| shares(&s.output).is_some());
    let plain: f64 = plain.iter().map(|s| s.seconds).sum();
    split.iter().map(|s| s.seconds).fold(plain, f64::max)
}

#[derive(Deserialize)]
struct StatsQuery {
    /// RFC3339, a date, or how long ago (`7d`, `48h`): see stats::parse_when.
    since: Option<String>,
    until: Option<String>,
}

/// The line's numbers over a window (default: the 7 days before now): what `avp stats`, `avp_stats`
/// and AVP Live read.
async fn stats_route(State(e): S, P(repo): P<String>, Query(q): Query<StatsQuery>) -> R {
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        let (since, until) = crate::stats::window(q.since.as_deref(), q.until.as_deref(), now_ms())?;
        stats(&e, &r, since, until)
    })
    .await?)
}

/// The before AVP a person saved (`avp stats --github --before --save`): GitHub's numbers only, read
/// where the token is; the server never sees the token. Each save replaces the one before it.
async fn before(State(e): S, P(repo): P<String>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; e.db.lock().unwrap().before(&r) }).await?)
}

async fn save_before(State(e): S, P(repo): P<String>, Json(b): Json<crate::stats::Before>) -> R {
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        let (since, until) = (crate::stats::parse_time(&b.since)?, crate::stats::parse_time(&b.until)?);
        anyhow::ensure!(since < until, "the before's window ends before it starts");
        e.db.lock().unwrap().set_before(&r, &b)?;
        Ok(b)
    })
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: i64 = 1_793_000_000_000;
    const MIN: i64 = 60_000;

    fn landing(position: i64, opened: i64, landed: i64, agent: &str, goal: &str) -> (LineEntry, Task) {
        let line = LineEntry { position, task: Some(format!("task_{position}")), tree: String::new(), landed_at: landed, proof: None, full: None, note: goal.into(), git: None, reverted: None };
        let task = Task { id: format!("task_{position}"), repo: "r".into(), goal: goal.into(), actor: Actor { person: "ada".into(), agent: agent.into(), session: String::new() }, scope: vec![], base_position: position - 1, base_tree: String::new(), state: TaskState::Landed, created_at: opened, landed_position: Some(position), transcript: None, similar: vec![], on: None };
        (line, task)
    }

    fn event(id: i64, at: i64, kind: &str, task: Option<&str>, data: serde_json::Value) -> Event {
        Event { id, at, kind: kind.into(), task: task.map(String::from), path: None, data }
    }

    fn step(name: &str, exit: i32, seconds: f64, output: &str) -> Step {
        Step { name: name.into(), command: String::new(), exit, seconds, output: output.into() }
    }

    fn proof(id: &str, task: Option<&str>, verdict: Verdict, steps: Vec<Step>) -> Proof {
        Proof { id: id.into(), task: task.map(String::from), base_position: 0, candidate_tree: String::new(), files: vec![], steps, verdict, blamed: vec![], created_at: T0 }
    }

    #[test]
    fn a_window_of_the_line() {
        let r = Records {
            landings: vec![
                landing(1, T0, T0 + 2 * MIN, "claude-code", "say hello"),
                landing(2, T0 + MIN, T0 + 5 * MIN, "codex", "Revert position 1: say hello"),
            ],
            proofs: vec![
                proof("p1", Some("task_1"), Verdict::Green, vec![step("build", 0, 30.0, ""), step("tests", 0, 30.0, "")]),
                // A split step: three shares side by side, 20 s at most, 45 s of machines.
                proof("p2", Some("task_2"), Verdict::Red, vec![step("build", 0, 10.0, ""), step("tests", 1, 20.0, "3 jobs side by side (3 shares of the related tests)\n\n== share 1/3, 20s, exit 1\nx\n== share 2/3, 15s, exit 0\n\n== share 3/3, 10s, exit 0\n")]),
                proof("p3", Some("task_2"), Verdict::Red, vec![step("conflicts", 1, 0.0, "unsettled conflicts in:\na")]),
                // A full suite whose `web` shard flaked: its first run counts too.
                proof("f1", None, Verdict::Flaky, vec![step("api", 0, 60.0, ""), step("web (flaky: red, then green on its retry)", 0, 30.0, "")]),
                // A deploy's output kept as a proof: not CI machine time.
                proof("d1", None, Verdict::Green, vec![step("deploy staging", 0, 600.0, "")]),
            ],
            events: vec![
                event(1, T0 + 3 * MIN, "task.replayed", Some("task_2"), json!({ "conflicts": ["a"] })),
                event(2, T0 + 4 * MIN, "task.replayed", Some("task_2"), json!({ "conflicts": [] })),
                // The suite of position 2 is the first verdict on position 1 as well.
                event(3, T0 + 9 * MIN, "line.flaky", None, json!({ "position": 2, "proof": "f1", "shard": "web" })),
                event(4, T0 + 10 * MIN, "env.promoted", None, json!({ "env": "staging", "position": 2 })),
                event(5, T0 + 12 * MIN, "env.moved", None, json!({ "env": "staging", "position": 2, "proof": "d1" })),
                // After the window: still the time position 2 reached production, not a count.
                event(6, T0 + 90 * MIN, "env.moved", None, json!({ "env": "production", "position": 2 })),
            ],
        };
        let s = compute("toy", T0, T0 + 60 * MIN, &r);
        assert_eq!((s.since.as_str(), s.landings, s.reverts), (rfc3339(T0).as_str(), 2, 1));
        assert_eq!(s.opened_to_landed, Spread { count: 2, median_s: Some(180.0), p90_s: Some(228.0) });
        assert_eq!((s.landed_to_full.count, s.landed_to_full.median_s), (2, Some(330.0)));
        let staging = &s.envs["staging"];
        assert_eq!((staging.landed_to_served.median_s, staging.opened_to_served.p90_s, staging.promotions, staging.moves), (Some(510.0), Some(714.0), 1, 1));
        assert_eq!((s.envs["production"].landed_to_served.count, s.envs["production"].moves), (2, 0));
        assert_eq!((s.proofs, s.proofs_red, s.proofs_blocked), (3, 2, 0));
        assert_eq!(s.proof_duration, Spread { count: 2, median_s: Some(40.0), p90_s: Some(56.0) }, "the conflicts' proof ran nothing; the split shares ran side by side");
        assert_eq!(s.machine_minutes, MachineMinutes { proofs: 1.9, full_suites: 2.0, total: 3.9 });
        assert_eq!((s.replays, s.tasks_replayed, s.conflicts), (2, 1, 1));
        assert_eq!((s.full_suites, s.full_red, s.flaky), (1, 0, 1));
        assert_eq!((s.by_agent["claude-code"], s.by_agent["codex"], s.by_person["ada"]), (1, 1, 2));
    }
}
