//! The full suite after a landing: once per batch of landings, every shard of avp.toml's whole
//! suite on the newest position, a red shard run again once (flaky when green then), and a red twice
//! bisected and reverted by a task AVP lands itself. Its rerun (`avp full`), and the suites
//! `[[check.schedule]]` starts every day.

use crate::check::{failed_files, failing_tests, is_test_file, read_config, shard_changed, shard_prints, CheckConfig, Part, Reuse, Schedule, Shard};
use crate::db::ShardResult;
use crate::engine::{halve, Engine, LandOutcome};
use crate::model::*;
use crate::server::{blocking, ok, rid, R, S};
use anyhow::{anyhow, Result};
use axum::extract::{Path as P, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

pub(crate) fn routes() -> Router<Engine> {
    Router::new().route("/repos/{repo}/full", post(rerun_full))
}

/// Full suites share one laid-out dir per repo: one at a time.
static LINE_LOCK: Mutex<()> = Mutex::new(());

/// The positions whose suite waits for the line's lock, by repo: what a suite run again by hand on
/// an older position gives its turn to.
static WAITING: Mutex<BTreeMap<String, BTreeSet<i64>>> = Mutex::new(BTreeMap::new());

/// The retry of a red shard on the test files it names, `{files}` filled in. None when it names no
/// test file: what failed is not a test, and the whole shard runs again.
fn retry_command(retry: &str, named: &[String]) -> Option<String> {
    let tests: Vec<&str> = named.iter().map(String::as_str).filter(|p| is_test_file(p)).collect();
    (!tests.is_empty()).then(|| retry.replace("{files}", &tests.join(" ")))
}



/// How a suite takes one shard that says what it reads (`inputs`).
#[derive(Default)]
struct Plan {
    /// check::shard_prints of the shard on this tree. None: it has no inputs (or a pattern of them
    /// matches no file), and it runs as a shard always did.
    prints: Option<(String, String)>,
    /// The green run that stands for this tree: same shard, same inputs, whole within its hours.
    stands: Option<ShardResult>,
    /// Not run: `stands` is its verdict.
    reused: bool,
    /// The run `{changed}` is counted from, and the inputs that changed since.
    since: Option<(ShardResult, Vec<String>)>,
    /// Said first in the shard's output: why it did not reuse what its avp.toml asks.
    note: Option<String>,
}

/// The first words of a reused shard's output.
const REUSED: &str = "reused: ";

fn ago(ms: i64) -> String {
    match ms / 60_000 {
        m if m < 60 => format!("{m} min"),
        m => format!("{} h {:02}", m / 60, m % 60),
    }
}

impl Engine {
    /// What the suite of `position` does with `shard`. `real`: a suite a person or a schedule
    /// started runs every shard, whole, and what stood before is only compared with it.
    fn plan(&self, repo: &str, position: i64, cfg: &CheckConfig, shard: &Shard, tree: &Tree, real: bool) -> Plan {
        if !shard.reusable() {
            return Plan::default();
        }
        let (what, reads) = match shard_prints(cfg, shard, tree) {
            Ok(p) => p,
            Err(pattern) => return Plan { note: Some(format!("avp: the input {pattern:?} of this shard matches no file of the tree: it runs on every position until its inputs are right\n")), ..Default::default() },
        };
        let since = now_ms() - shard.reuse_hours as i64 * 3_600_000;
        let db = self.db();
        let stands = db.shard_result(repo, &shard.name, &what, &reads, since).ok().flatten();
        let on = shard.reuse == Reuse::On && !real;
        let mut plan = Plan { reused: on && stands.is_some(), stands, prints: Some((what, reads)), ..Default::default() };
        if on && !plan.reused && shard.run.contains("{changed}") {
            let what = plan.prints.as_ref().map(|p| p.0.as_str());
            if let Some(base) = db.shard_base(repo, &shard.name, what, position, since).ok().flatten() {
                drop(db);
                let changed = self.objects.get_tree(&base.tree).ok().and_then(|before| shard_changed(shard, tree, &before));
                plan.since = changed.map(|c| (base, c));
            }
        }
        plan
    }

    /// A shard's run is over: green at once, it stands for its inputs; red, nothing stands for them
    /// any more, and a green that did (`audit`, or a suite run again by hand) was wrong: the event
    /// names the files that changed since outside its inputs, one of which it reads.
    fn ran(&self, repo: &str, position: i64, tree_hash: &str, tree: &Tree, shard: &Shard, plan: &Plan, steps: &[Step], flaked: bool, proof: &str) -> Result<()> {
        let Some((what, reads)) = &plan.prints else { return Ok(()) };
        if plan.reused {
            return Ok(());
        }
        // Read before the database is taken: the store may ask it which objects a repo holds.
        let before = plan.stands.as_ref().map(|stood| self.objects.get_tree(&stood.tree)).transpose()?;
        let db = self.db();
        if steps.iter().any(|s| s.exit != 0) {
            db.drop_shard_result(repo, &shard.name, what, reads)?;
            if let (Some(stood), Some(before)) = (&plan.stands, before) {
                let outside: Vec<&String> = tree.iter().filter(|(p, e)| before.get(*p) != Some(*e)).map(|(p, _)| p).chain(before.keys().filter(|p| !tree.contains_key(*p))).take(30).collect();
                let task = db.line_at(repo, position)?.task;
                self.emit_told(&db, repo, "line.reuse_missed", task.as_deref(), serde_json::json!({ "position": position, "shard": shard.name, "proof": proof, "green_at": stood.position, "green_proof": stood.proof, "changed_outside_inputs": outside }))?;
            }
            return Ok(());
        }
        if flaked {
            return Ok(());
        }
        let now = now_ms();
        let seconds = steps.iter().map(|s| s.seconds).sum();
        db.put_shard_result(repo, &shard.name, what, reads, &ShardResult { position, tree: tree_hash.to_string(), proof: proof.to_string(), ran_at: now, whole_at: plan.since.as_ref().map_or(now, |(base, _)| base.whole_at), seconds })
    }
}

/// When each scheduled suite last started, by repo and schedule (its `at` and shards), in ms.
static SCHEDULED: Mutex<BTreeMap<(String, String), i64>> = Mutex::new(BTreeMap::new());

/// Where AVP's own revert went: landed, or proven green and waiting for a person's approval, with
/// the guarded paths it changes (none when the repo's every landing waits for one).
enum Reverting {
    Landed(Landed),
    Waits(Vec<String>),
}


/// What follows a landing (the seam's `after_landing`): AVP's own revert that waited for a person
/// is recorded once it lands, and the full suite runs on the new position.
pub(crate) fn after_landing(e: &Engine, task: &Task, position: i64, tree_hash: &str) {
    if task.actor.person == avp_actor().person && task.actor.agent == avp_actor().agent {
        if let Err(err) = e.revert_approved(&task.repo, &task.id, position) {
            eprintln!("revert landed at {position}: {err:#}");
        }
    }
    e.full_suite_after(&task.repo, position, tree_hash.to_string(), true, None);
}

impl Engine {
    /// The whole suite, after the fact, on a copy of the engine. Red reopens nothing by itself:
    /// it records and tells; the position stays. (Bisection is trivial on a line: the previous
    /// green position bounds it.) `scheduled`: a suite `[[check.schedule]]` started, of the shards it
    /// names.
    fn full_suite_after(&self, repo: &str, position: i64, tree_hash: String, give_way: bool, scheduled: Option<Schedule>) {
        let engine = self.clone();
        let repo = repo.to_string();
        std::thread::spawn(move || {
            let run = || -> Result<()> {
                WAITING.lock().unwrap_or_else(|e| e.into_inner()).entry(repo.clone()).or_default().insert(position);
                let _one_at_a_time = LINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let newer_waits = {
                    let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
                    let mine = waiting.entry(repo.clone()).or_default();
                    mine.remove(&position);
                    mine.range(position + 1..).next_back().copied()
                };
                let tree = engine.objects.get_tree(&tree_hash)?;
                let cfg = read_config(&tree, &engine.objects)?.check;
                let mut shards = cfg.shards();
                if shards.is_empty() {
                    return Ok(());
                }
                // A scheduled suite runs the shards it names (each job of a shard's matrix), every one
                // when it names none. Some of the shards green say nothing of the whole: then only a
                // red counts for the position.
                let every = shards.len();
                if let Some(s) = scheduled.as_ref().filter(|s| !s.shards.is_empty()) {
                    shards.retain(|shard| s.shards.iter().any(|n| shard.name == *n || shard.name.starts_with(&format!("{n} ("))));
                    anyhow::ensure!(!shards.is_empty(), "the suite scheduled at {} names no shard of avp.toml ({})", s.at, s.shards.join(", "));
                }
                let whole = shards.len() == every;
                // A batch: landings come faster than a suite runs. The newest position holds every
                // earlier one, so a suite that waited while the line moved gives its turn to it.
                let head = engine.db().head(&repo)?.map(|h| h.position).unwrap_or(position);
                if give_way && head > position {
                    engine.db().emit(&repo, "line.full_covered", None, None, serde_json::json!({ "position": position, "by": head }))?;
                    return Ok(());
                }
                // Run again by hand on an older position while a newer one waits for its suite: the
                // newer one goes first, it holds this one. Held, the line went 20 min on one verdict
                // (Murmell, 05/11, 20:30-20:50 UTC) while landings piled up behind it.
                if let (false, None, Some(by)) = (give_way, scheduled.as_ref(), newer_waits) {
                    engine.db().emit(&repo, "line.full_covered", None, None, serde_json::json!({ "position": position, "by": by, "rerun": true }))?;
                    return Ok(());
                }
                // Every shard at once: one job each, so as many workers as shards run the suite. A
                // shard comes back with its steps and, when it flaked, the tests its red run failed.
                // A shard that says what it reads does not run again on inputs it is green on, and
                // one that takes `{changed}` runs on the inputs that changed since its last green.
                let plans: Vec<Plan> = shards.iter().map(|s| engine.plan(&repo, position, &cfg, s, &tree, !give_way)).collect();
                let (runs, flakes): (Vec<Vec<Step>>, Vec<Option<Vec<String>>>) = std::thread::scope(|sc| {
                    let runs: Vec<_> = shards
                        .iter()
                        .zip(&plans)
                        .enumerate()
                        .map(|(i, (shard, plan))| {
                            let (engine, repo, tree, tree_hash) = (&engine, &repo, &tree, &tree_hash);
                            let (name, mut part) = (&shard.name, Part::of_shard(shard));
                            let run = match &plan.since {
                                Some((base, changed)) => {
                                    part.env.push(("AVP_CHANGED_SINCE".into(), base.position.to_string()));
                                    shard.run.replace("{changed}", &changed.join(" "))
                                }
                                None => shard.whole(),
                            };
                            sc.spawn(move || {
                                let run = run.as_str();
                                if let (true, Some(stood)) = (plan.reused, &plan.stands) {
                                    let output = format!("{REUSED}its inputs are those of position {}, where it ran green {} ago in {:.0} s ({}): not run again\n", stood.position, ago(now_ms() - stood.ran_at), stood.seconds, stood.proof);
                                    return (vec![Step { name: name.clone(), command: run.to_string(), exit: 0, seconds: 0.0, output }], None);
                                }
                                let attempt = |run: &str| match engine.prove(repo, tree, tree_hash, &format!("line-{i}"), &[], Some(run), Some(&part), false) {
                                    Ok(mut st) => {
                                        for s in st.iter_mut() {
                                            s.name = name.clone();
                                        }
                                        st
                                    }
                                    Err(e) => vec![Step { name: name.clone(), command: run.to_string(), exit: 1, seconds: 0.0, output: format!("{e:#}") }],
                                };
                                let mut first = attempt(run);
                                if let (Some(note), Some(s)) = (&plan.note, first.first_mut()) {
                                    s.output.insert_str(0, note);
                                }
                                if first.iter().all(|s| s.exit == 0) {
                                    return (first, None);
                                }
                                // Red once may be the machine (a browser that did not start, a timing
                                // test under load: on Murmell's line, 24/10, positions went red on tests
                                // their changes did not reach, and were blamed): the shard runs again,
                                // once, on the same tree. Green the second time is flaky and says so,
                                // with the first run's end and the tests it failed; red twice is red. A
                                // task's proof never runs again: it gates a landing, and a red there is
                                // the agent's to read.
                                // With the shard's `retry`, only the test files the red names run again:
                                // a flaky test costs its own seconds, not the shard's minutes (Murmell's
                                // line, 05/11: a red verdict came in 7 to 10 min instead of 5). A red that
                                // names none is the machine's: the whole shard again.
                                let first_out = first.iter().filter(|s| s.exit != 0).map(|s| s.output.as_str()).collect::<Vec<_>>().join("\n");
                                let again = shard.retry.as_deref().and_then(|r| retry_command(r, &failed_files(&first_out, tree)));
                                let mut second = attempt(again.as_deref().unwrap_or(run));
                                if second.iter().any(|s| s.exit != 0) {
                                    return (second, None);
                                }
                                let red: Vec<&str> = first.iter().filter(|s| s.exit != 0).map(|s| s.output.as_str()).collect();
                                let tail = red.iter().map(|o| o.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")).collect::<Vec<_>>().join("\n");
                                for s in second.iter_mut() {
                                    s.name = format!("{} (flaky: red, then green on its retry)", s.name);
                                    s.output = format!("first run, red:\n{tail}\n--- retry ---\n{}", s.output);
                                }
                                (second, Some(failing_tests(&red.join("\n"))))
                            })
                        })
                        .collect();
                    runs.into_iter().map(|h| h.join().unwrap_or_default()).unzip()
                });
                let proof_id = new_id("proof");
                for (((shard, plan), steps), flaked) in shards.iter().zip(&plans).zip(&runs).zip(&flakes) {
                    if let Err(e) = engine.ran(&repo, position, &tree_hash, &tree, shard, plan, steps, flaked.is_some(), &proof_id) {
                        eprintln!("full suite at {position}, shard {}: its run is not kept: {e:#}", shard.name);
                    }
                }
                // What the verdict's event says of the shards that did not run whole.
                let names = |keep: &dyn Fn(&Plan) -> bool| -> Vec<&String> { shards.iter().zip(&plans).filter(|(_, p)| keep(p)).map(|(s, _)| &s.name).collect() };
                let reused = names(&|p| p.reused);
                let reusable = names(&|p| !p.reused && p.stands.is_some());
                let changed_only = names(&|p| p.since.is_some());
                let saved_s: f64 = plans.iter().filter(|p| p.reused).filter_map(|p| p.stands.as_ref()).map(|r| r.seconds).sum();
                let steps: Vec<Step> = runs.into_iter().flatten().collect();
                let flaky: Vec<(&String, Vec<String>)> = shards.iter().map(|s| &s.name).zip(flakes).filter_map(|(name, tests)| tests.map(|t| (name, t))).collect();
                let red = steps.iter().any(|s| s.exit != 0);
                let verdict = if red { Verdict::Red } else if flaky.is_empty() { Verdict::Green } else { Verdict::Flaky };
                let output = steps.iter().map(|s| s.output.as_str()).collect::<Vec<_>>().join("\n");
                let blamed = if red { failed_files(&output, &tree).into_iter().map(|f| (f, None)).collect() } else { vec![] };
                let proof = Proof { id: proof_id, task: None, base_position: position, candidate_tree: tree_hash.clone(), files: vec![], steps, verdict, blamed, created_at: now_ms(), timing: None };
                let db = engine.db();
                db.insert_proof(&repo, &proof)?;
                if whole || red {
                    db.set_full(&repo, position, proof.verdict, &proof.id)?;
                }
                let task = db.line_at(&repo, position)?.task;
                // Each shard that flaked says so, whatever the others did: a flaky test is worth
                // knowing on a red position too. A flaky position has no other verdict event.
                for (shard, tests) in &flaky {
                    db.emit(&repo, "line.flaky", task.as_deref(), None, serde_json::json!({ "position": position, "proof": proof.id, "shard": shard, "tests": tests }))?;
                }
                if verdict != Verdict::Flaky {
                    let mut data = serde_json::json!({ "position": position, "proof": proof.id });
                    if let Some(s) = &scheduled {
                        data["scheduled"] = s.at.clone().into();
                    }
                    for (key, shards) in [("reused", &reused), ("reusable", &reusable), ("changed_only", &changed_only)] {
                        if !shards.is_empty() {
                            data[key] = serde_json::json!(shards);
                        }
                    }
                    if saved_s > 0.0 {
                        data["reused_seconds"] = saved_s.round().into();
                    }
                    engine.emit_told(&db, &repo, if red { "line.red" } else { "line.green" }, task.as_deref(), data)?;
                }
                drop(db);
                // Environments that follow the full suite take the newest green position, or flaky.
                if !red {
                    engine.follow_full(&repo)?;
                } else if give_way {
                    // Red twice after a landing: the line does not stay red.
                    engine.repair(&repo, position, &shards, &proof);
                }
                Ok(())
            };
            if let Err(e) = run() {
                eprintln!("full suite at {position}: {e:#}");
            }
        });
    }

    /// A full suite red twice after a landing. With agents landing every few minutes, a line left red
    /// until a person acts is a line the next ones build on: the position that broke it goes back
    /// off the line, by a task AVP opens, proves and lands itself (`line.reverted`). What AVP cannot
    /// settle that way stays red for a person, and `line.revert_failed` says why. It runs in the
    /// suite's thread, holding the line's lock: the suites of what lands meanwhile wait, then the
    /// newest of them runs, on the revert or after it.
    fn repair(&self, repo: &str, red_at: i64, shards: &[Shard], proof: &Proof) {
        let Some(red) = proof.steps.iter().find(|s| s.exit != 0) else { return };
        let Some(shard) = shards.iter().find(|s| s.name == red.name) else { return };
        if let Err(e) = self.revert_red(repo, red_at, shard, red, &proof.id) {
            let task = self.db().line_at(repo, red_at).ok().and_then(|l| l.task);
            let _ = self.db().emit(repo, "line.revert_failed", task.as_deref(), None, serde_json::json!({ "position": red_at, "shard": shard.name, "proof": proof.id, "reason": format!("{e:#}") }));
        }
    }

    /// Finds the position that turned shard `shard` red at `red_at`, and reverts it. The batch is
    /// what landed since the newest position whose suite ran (or that AVP's last revert landed): one
    /// position, it is that one; several, the shard runs on the positions in between, halving, down
    /// to the first red one. A batch whose base is not known green runs the shard there first: red
    /// there too, the red is older than the batch and nothing in it is to blame.
    fn revert_red(&self, repo: &str, red_at: i64, red_shard: &Shard, red: &Step, proof: &str) -> Result<()> {
        let (shard, red_proof) = (red_shard.name.as_str(), proof);
        let (head, below) = {
            let db = self.db();
            (db.head(repo)?.ok_or_else(|| anyhow!("no line"))?, db.line(repo, red_at - 1, 1000)?)
        };
        if !read_config(&self.objects.get_tree(&head.tree)?, &self.objects)?.line.auto_revert {
            return Ok(());
        }
        anyhow::ensure!(!self.by_avp(repo, red_at)?, "position {red_at} is AVP's own revert, and red: a person decides");
        let reverts: BTreeSet<i64> = below.iter().filter_map(|e| e.reverted.as_ref().map(|r| r.by)).collect();
        let base = below.iter().find(|e| e.full.is_some() || reverts.contains(&e.position)).or(below.last()).ok_or_else(|| anyhow!("nothing below position {red_at} to compare it with"))?;
        let mut probes = Vec::new();
        // A base whose green on this shard is another position's (reused): the positions since
        // that one never ran it, and the red may be any of them. The bisection starts where the
        // shard is last known green by a run; unknown, the base is probed like any other.
        let stood = self.db().suite_step(repo, base.position, shard)?.is_some_and(|s| s.output.starts_with(REUSED));
        let known = if stood { self.db().shard_base(repo, shard, None, base.position, 0)? } else { None };
        let known = known.map(|r| self.db().line_at(repo, r.position)).transpose()?;
        let base = known.as_ref().unwrap_or(base);
        if (stood && known.is_none()) || !matches!(base.full, Some(Verdict::Green | Verdict::Flaky)) {
            let r = self.shard_red(repo, base.position, red_shard)?;
            probes.push(serde_json::json!({ "position": base.position, "red": r.is_some() }));
            anyhow::ensure!(r.is_none(), "shard {shard} is red on position {} too, before the positions it would blame: a person decides", base.position);
        }
        let mut failed = red.clone();
        let culprit = halve(base.position, red_at, |mid| {
            let r = self.shard_red(repo, mid, red_shard)?;
            probes.push(serde_json::json!({ "position": mid, "red": r.is_some() }));
            Ok(r.map(|step| failed = step).is_some())
        })?;
        anyhow::ensure!(!self.by_avp(repo, culprit)?, "position {culprit}, AVP's own revert, turned shard {shard} red: a person decides");
        let entry = self.db().line_at(repo, culprit)?;
        let mut tests = failing_tests(&failed.output);
        if tests.is_empty() {
            // A runner that names no failing test: its last words.
            tests = failed.output.lines().rev().map(str::trim).filter(|l| !l.is_empty()).take(3).map(String::from).collect();
            tests.reverse();
        }
        tests.truncate(3);
        // A culprit below the red position read red on that shard: its verdict says so.
        let proof = if culprit == red_at {
            proof.to_string()
        } else {
            let tree = self.objects.get_tree(&entry.tree)?;
            let p = Proof { id: new_id("proof"), task: None, base_position: culprit, candidate_tree: entry.tree.clone(), files: vec![], steps: vec![failed.clone()], verdict: Verdict::Red, blamed: failed_files(&failed.output, &tree).into_iter().map(|f| (f, None)).collect(), created_at: now_ms(), timing: None };
            let db = self.db();
            db.insert_proof(repo, &p)?;
            db.set_full(repo, culprit, Verdict::Red, &p.id)?;
            p.id
        };
        let note: String = entry.note.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
        let goal = format!("Revert position {culprit}: {note} (full suite red twice, shard {shard}: {})", tests.join(" | ").chars().take(300).collect::<String>());
        let (revert, landed, files) = self.revert_as_avp(repo, culprit, goal)?;
        let culprit_task = entry.task.as_deref().map(|t| self.db().task(t)).transpose()?;
        let tail = failed.output.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        let said = serde_json::json!({
            "position": culprit, "revert_task": revert.id, "red": red_at, "shard": shard, "tests": tests, "output": tail, "proof": proof,
            "goal": entry.note, "person": culprit_task.as_ref().map(|t| &t.actor.person), "agent": culprit_task.as_ref().map(|t| &t.actor.agent), "files": files, "probes": probes,
        });
        match landed {
            Reverting::Landed(l) => self.record_revert(repo, said, l.position),
            // Proven green, it waits for a person, open: approved, it lands as any task does, and
            // its landing records it (`revert_approved`) from what this event keeps.
            Reverting::Waits(guarded) => {
                let changes = if guarded.is_empty() { String::new() } else { format!(", as it changes {}", guarded.join(", ")) };
                let reason = format!("the revert of position {culprit} is proven green and waits for a person's approval{changes}: `avp diff {0}` shows it, `avp approve {0}` lands it", revert.id);
                let task = self.db().line_at(repo, red_at)?.task;
                self.db().emit(repo, "line.revert_failed", task.as_deref(), None, serde_json::json!({ "position": red_at, "shard": shard, "proof": red_proof, "reason": reason, "revert": said }))?;
                // Approved and landed before this event was there for its landing to find.
                match self.db().task(&revert.id)?.landed_position {
                    Some(by) => self.record_revert(repo, said, by),
                    None => Ok(()),
                }
            }
        }
    }

    /// AVP's revert landed at `by`: the position it took back says by what and why, and
    /// `line.reverted` hands it back to its person and agent, once. `said`: what the event says, `by`
    /// aside.
    fn record_revert(&self, repo: &str, mut said: serde_json::Value, by: i64) -> Result<()> {
        said["by"] = by.into();
        let position = said["position"].as_i64().ok_or_else(|| anyhow!("a revert names no position"))?;
        let text = |k: &str| said[k].as_str().unwrap_or_default().to_string();
        let r = Reverted { by, task: text("revert_task"), shard: text("shard"), tests: serde_json::from_value(said["tests"].clone()).unwrap_or_default(), proof: text("proof") };
        let db = self.db();
        let entry = db.line_at(repo, position)?;
        if entry.reverted.is_some() {
            return Ok(());
        }
        let task = entry.task;
        db.set_reverted(repo, position, &r)?;
        self.emit_told(&db, repo, "line.reverted", task.as_deref(), said)
    }

    /// AVP's revert of a guarded path, landed once a person approved it: recorded as one that lands
    /// at once, from the `line.revert_failed` that said it waited. Nothing when none did.
    fn revert_approved(&self, repo: &str, task_id: &str, by: i64) -> Result<()> {
        let waited = self.db().events_of(repo, &["line.revert_failed"], 0)?.into_iter().rev().find(|e| e.data["revert"]["revert_task"] == task_id);
        match waited {
            Some(e) => self.record_revert(repo, e.data["revert"].clone(), by),
            None => Ok(()),
        }
    }

    /// Whether AVP's own revert landed this position.
    fn by_avp(&self, repo: &str, position: i64) -> Result<bool> {
        let db = self.db();
        let Some(t) = db.line_at(repo, position)?.task else { return Ok(false) };
        let a = db.task(&t)?.actor;
        Ok(a.person == avp_actor().person && a.agent == avp_actor().agent)
    }

    /// A shard on position `position`, run again once when red as the full suite runs it: its red
    /// step when red twice, None when green. A shard that cannot run stops the bisection: a position
    /// is never blamed for the machine. The shard of that name in the position's own avp.toml, as
    /// its suite ran it: a position that changed the command would otherwise blame every one before
    /// it. One without it is not red on it.
    fn shard_red(&self, repo: &str, position: i64, shard: &Shard) -> Result<Option<Step>> {
        let entry = self.db().line_at(repo, position)?;
        let tree = self.objects.get_tree(&entry.tree)?;
        let Some(shard) = read_config(&tree, &self.objects)?.check.shards().into_iter().find(|s| s.name == shard.name) else { return Ok(None) };
        let mut red = None;
        for _ in 0..2 {
            let steps = self.prove(repo, &tree, &entry.tree, &format!("bisect-{position}"), &[], Some(&shard.whole()), Some(&Part::of_shard(&shard)), false)?;
            match steps.into_iter().find(|s| s.exit != 0) {
                Some(s) => red = Some(Step { name: shard.name.clone(), ..s }),
                None => return Ok(None),
            }
        }
        Ok(red)
    }

    /// Takes position `position` back on the head, as `avp revert` does, in a task AVP opens itself:
    /// proven and landed like any other. One proven green that waits for a person (it changes a
    /// guarded path: avp.toml, always) stays open for them; one that does not land is abandoned, and
    /// the error says why. Returns the task, where it went, and the files it took back.
    fn revert_as_avp(&self, repo: &str, position: i64, goal: String) -> Result<(Task, Reverting, Vec<String>)> {
        let task = self.open_task(repo, OpenTask { goal, actor: avp_actor(), scope: vec![], transcript: None })?;
        let landed = self.write_revert(&task, position).and_then(|files| match self.land(&task.id)? {
            LandOutcome::Landed(l) => Ok((Reverting::Landed(l), files)),
            LandOutcome::NotLanded(n) if n.reason == "approval" => Ok((Reverting::Waits(n.guarded), files)),
            LandOutcome::NotLanded(n) => {
                let red = n.proof.as_ref().and_then(|p| p.steps.iter().find(|s| s.exit != 0)).map(|s| format!(", {} ends with:\n{}", s.name, crate::check::red_part(&s.output).lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"))).unwrap_or_default();
                Err(match n.reason.as_str() {
                    "conflict" => anyhow!("the revert of position {position} conflicts with what landed meanwhile, in {}: a person settles it (`avp revert {position}`)", n.conflicts.join(", ")),
                    "empty" | "already" => anyhow!("the head no longer holds what position {position} changed: nothing to take back"),
                    r => anyhow!("the revert of position {position} does not land: its proof is {r}{red}"),
                })
            }
        });
        match landed {
            Ok((l, files)) => Ok((task, l, files)),
            Err(e) => {
                let _ = self.abandon(&task.id);
                Err(e)
            }
        }
    }

    /// The writes that take `position` back on the task's base. A file nobody changed since comes
    /// back as it was; one changed since gets the inverse change merged in; lines changed again
    /// since stop it, for a person to settle. AVP takes no lease: its revert is the line's own, and
    /// neither waits for an agent's lease nor takes one away.
    fn write_revert(&self, task: &Task, position: i64) -> Result<Vec<String>> {
        anyhow::ensure!(position > 0, "position 0 is where the line starts: there is nothing to revert");
        let (before, after) = (self.tree_at(&task.repo, position - 1)?, self.tree_at(&task.repo, position)?);
        let head = self.objects.get_tree(&task.base_tree)?;
        let paths: BTreeSet<&String> = before.keys().chain(after.keys()).filter(|p| before.get(*p) != after.get(*p)).collect();
        let text = |e: &Entry| -> Result<Option<String>> { Ok(String::from_utf8(self.objects.get(&e.blob)?).ok()) };
        let (mut writes, mut conflicts) = (Vec::new(), Vec::new());
        for path in paths {
            let (was, became, now) = (before.get(path), after.get(path), head.get(path));
            if now == became {
                writes.push(match was {
                    Some(e) => PutWrite { path: path.clone(), kind: WriteKind::Put, blob: Some(e.blob.clone()), mode: e.mode },
                    None => PutWrite { path: path.clone(), kind: WriteKind::Delete, blob: None, mode: default_mode() },
                });
                continue;
            }
            let merged = match (was, became, now) {
                (Some(w), Some(b), Some(n)) => match (text(w)?, text(b)?, text(n)?) {
                    (Some(w), Some(b), Some(t)) => diffy::merge(&b, &t, &w).ok(),
                    _ => None,
                },
                // Added by the position, or deleted by it, and changed again since: a person decides.
                _ => None,
            };
            match (merged, now) {
                (Some(m), Some(n)) => writes.push(PutWrite { path: path.clone(), kind: WriteKind::Put, blob: Some(self.objects.put(m.as_bytes())?), mode: n.mode }),
                _ => conflicts.push(path.clone()),
            }
        }
        anyhow::ensure!(conflicts.is_empty(), "the lines position {position} changed were changed again since, in {}: a person settles them (`avp revert {position}`)", conflicts.join(", "));
        let db = self.db();
        for w in &writes {
            let write = db.append_write(&task.id, &w.path, w.kind, w.blob.as_deref(), w.mode)?;
            db.emit(&task.repo, "write", Some(&task.id), Some(&w.path), serde_json::json!({ "kind": w.kind, "seq": write.seq, "reverts": position }))?;
        }
        Ok(writes.into_iter().map(|w| w.path).collect())
    }

    /// Runs the full suite of a position again (none: the head), whatever its last verdict: after a
    /// red that looked like the machine, or to prove an older position. Starts it and returns.
    pub fn rerun_full(&self, repo: &str, position: Option<i64>) -> Result<i64> {
        let entry = {
            let db = self.db();
            let position = match position {
                Some(p) => p,
                None => db.head(repo)?.ok_or_else(|| anyhow!("no line"))?.position,
            };
            db.line_at(repo, position)?
        };
        self.full_suite_after(repo, entry.position, entry.tree.clone(), false, None);
        Ok(entry.position)
    }

    /// Starts again, once the server is up, the full suites a restart cut short: a suite lives in a
    /// thread of the server, so an update between two proofs lost the one running and nothing
    /// started it again. The line then waited with landings piling up and no verdict to promote
    /// (Murmell, 07/11, 21:16-21:40 UTC: six positions, a fix waited for by the users among them).
    /// A head landed in the last day without a verdict is one such suite; the head holds every
    /// position before it, so one suite on it covers the batch.
    pub fn resume_full_suites(&self) -> Vec<(String, i64)> {
        const DAY: i64 = 86_400_000;
        let mut resumed = Vec::new();
        let Ok(repos) = self.db().repos() else { return resumed };
        for repo in repos {
            let Ok(Some(head)) = self.db().head(&repo.id) else { continue };
            if head.full.is_some() || now_ms() - head.landed_at > DAY {
                continue;
            }
            if !matches!(super::orgs::may_prove(self, &repo.id), Ok(Ok(()))) {
                continue;
            }
            eprintln!("{}: full suite of position {} started again after a restart", repo.id, head.position);
            self.full_suite_after(&repo.id, head.position, head.tree.clone(), true, None);
            resumed.push((repo.id.clone(), head.position));
        }
        resumed
    }

    /// Starts the scheduled suites (`[[check.schedule]]`) as their time comes: a look every 20
    /// seconds, for as long as the server runs.
    pub fn run_schedules(&self) {
        let e = self.clone();
        std::thread::spawn(move || loop {
            if let Err(err) = e.scheduled(now_ms()) {
                eprintln!("scheduled suites: {err:#}");
            }
            std::thread::sleep(std::time::Duration::from_secs(20));
        });
    }

    /// Starts, on the head of each repo, the suites its avp.toml schedules whose time has come at
    /// `now` (ms): once a day, within the hour after its `at` (UTC), so that a server down at that
    /// minute still runs it once it is back. A `line.scheduled` event says it started; its red is told
    /// like any red. Returns the repos and positions it started on.
    pub fn scheduled(&self, now: i64) -> Result<Vec<(String, i64)>> {
        const DAY: i64 = 86_400_000;
        let mut started = Vec::new();
        // Read first: the loop takes the database again.
        let repos = self.db().repos()?;
        for repo in repos {
            let Some(head) = self.db().head(&repo.id)? else { continue };
            // An avp.toml that does not read: every proof of the repo says why.
            let Ok(cfg) = read_config(&self.objects.get_tree(&head.tree)?, &self.objects) else { continue };
            // On AVP Cloud a space past its plan's minutes starts no proof, scheduled ones neither.
            if !cfg.check.schedule.is_empty() && !matches!(super::orgs::may_prove(self, &repo.id), Ok(Ok(()))) {
                continue;
            }
            for s in cfg.check.schedule {
                let today = now - now.rem_euclid(DAY) + s.minute * 60_000;
                let due = if today > now { today - DAY } else { today };
                if now - due >= 3_600_000 {
                    continue;
                }
                let key = (repo.id.clone(), format!("{} {}", s.at, s.shards.join(",")));
                let mut ran = SCHEDULED.lock().unwrap_or_else(|e| e.into_inner());
                // Read back once per server: one that started it today before a restart did run it.
                let last = match ran.get(&key) {
                    Some(at) => *at,
                    None => self.db().events_since(&repo.id, 0, i64::MAX)?.into_iter().rev().find(|e| e.kind == "line.scheduled" && e.data["at"] == s.at.as_str() && e.data["shards"] == serde_json::json!(s.shards)).map_or(i64::MIN, |e| e.at),
                };
                ran.insert(key, if last >= due { last } else { now });
                drop(ran);
                if last >= due {
                    continue;
                }
                self.db().emit(&repo.id, "line.scheduled", head.task.as_deref(), None, serde_json::json!({ "position": head.position, "at": s.at, "shards": s.shards }))?;
                self.full_suite_after(&repo.id, head.position, head.tree.clone(), false, Some(s));
                started.push((repo.id.clone(), head.position));
            }
        }
        Ok(started)
    }
}

#[derive(Deserialize, Default)]
struct RerunFull {
    #[serde(default)]
    position: Option<i64>,
}

async fn rerun_full(State(e): S, P(repo): P<String>, body: Option<Json<RerunFull>>) -> R {
    let req = body.map(|b| b.0).unwrap_or_default();
    ok(blocking(move || { let r = rid(&e, &repo)?; e.rerun_full(&r, req.position).map(|p| serde_json::json!({ "position": p, "started": true })) }).await?)
}

#[cfg(test)]
mod tests {
    use super::retry_command;

    #[test]
    fn retry_runs_the_red_test_files_only() {
        let named = ["apps/daemon/test/childenv.test.ts", "apps/daemon/src/env.ts", "web/e2e/login.spec.ts"].map(String::from);
        assert_eq!(retry_command("pnpm vitest run {files}", &named).as_deref(), Some("pnpm vitest run apps/daemon/test/childenv.test.ts web/e2e/login.spec.ts"));
        // Only a source file named: the machine, not a test, the whole shard again.
        assert_eq!(retry_command("pnpm vitest run {files}", &["apps/daemon/src/env.ts".to_string()]), None);
    }
}
