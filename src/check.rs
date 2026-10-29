//! Proofs: lay out a candidate tree, run the project's checks on it, say what failed and whose
//! file it is. The project describes its checks in `avp.toml` at its root
//! (`trame.toml`, its former name, is still read).

use crate::model::*;
use crate::objects::Objects;
use crate::view::Stamp;
use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One setup at a time per server: two candidates must never run `pnpm install` into the same cache.
static SETUP_LOCK: Mutex<()> = Mutex::new(());

/// How many checks may run at once (AVP_CHECK_SLOTS, default 4): the machine budget a bench
/// compares against a CI's runner count.
fn check_slots() -> usize {
    match SLOTS.load(Ordering::Relaxed) {
        0 => std::env::var("AVP_CHECK_SLOTS").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(4),
        n => n,
    }
}
/// What `set_check_slots` said, 0 until it does.
static SLOTS: AtomicUsize = AtomicUsize::new(0);

/// How many checks this process may run at once, whatever AVP_CHECK_SLOTS says. AVP's own tests
/// start their engines side by side in one process, all on the same slots: a test that waits for
/// a slot another test holds would measure that test, and how many there are must not depend on
/// the environment `cargo test` runs in (on 26/10 it was AVP_CHECK_SLOTS=1, inside a proof).
pub fn set_check_slots(n: usize) {
    // Under the lock a check waits with: none sees the old count and then misses the wake-up.
    let _taken = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    SLOTS.store(n, Ordering::Relaxed);
    SLOT_FREED.notify_all();
}
/// The slots taken, by number: a check lays its candidate out in its slot's folder.
static RUNNING: Mutex<BTreeSet<usize>> = Mutex::new(BTreeSet::new());

/// The exit of a step stopped before its end: the tree it proved is no longer the task's.
pub const CANCELLED: i32 = -2;

/// Checks in flight: whose (a task's id), the tree each one proves and the flag that stops it.
/// A task may have several at once, the parts of one proof.
static IN_FLIGHT: Mutex<Vec<(String, String, Arc<AtomicBool>)>> = Mutex::new(Vec::new());

/// Stops the checks running under `name` that prove another tree than `tree_hash`: their verdict
/// would be about files the task no longer has.
pub fn cancel_other(name: &str, tree_hash: &str) {
    for (n, h, flag) in IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if n == name && h != tree_hash {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Stops the checks of `name` on `tree_hash`: one part of the proof is red, the others are moot.
pub fn cancel_tree(name: &str, tree_hash: &str) {
    for (n, h, flag) in IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if n == name && h == tree_hash {
            flag.store(true, Ordering::Relaxed);
        }
    }
}
static SLOT_FREED: std::sync::Condvar = std::sync::Condvar::new();
/// Checks waiting for a slot (or for the drain to lift), not running yet.
static TAKING: AtomicUsize = AtomicUsize::new(0);

/// The drain: while it lasts, this server starts no new work (no slot taken here, no job handed to
/// a worker, no deploy), and what runs goes to its end, so that a new version takes its place
/// without cutting anything (server-update.sh). What comes meanwhile waits, it is not refused. It
/// lifts when asked, at its bound, or with the process.
struct Drain {
    until: Option<Instant>,
    /// What waits here for it to lift, by name (a task's id, `line-N`, a deploy).
    held: Vec<String>,
    /// Jobs in the workers' queue no worker has claimed yet, by id and name: while the server
    /// drains, none is claimed.
    queued: Vec<(String, String)>,
}
impl Drain {
    fn left(&self) -> Option<Duration> {
        self.until.and_then(|u| u.checked_duration_since(Instant::now())).filter(|d| !d.is_zero())
    }
}
static DRAIN: Mutex<Drain> = Mutex::new(Drain { until: None, held: Vec::new(), queued: Vec::new() });
static DRAIN_LIFTED: Condvar = Condvar::new();

fn drain_state() -> std::sync::MutexGuard<'static, Drain> {
    DRAIN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts the drain, or sets its bound again: it lifts by itself `bound` from now.
pub fn drain(bound: Duration) {
    // Under the slots' lock: a check that saw no drain has its slot by the time this returns.
    let _taken = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    drain_state().until = Some(Instant::now() + bound);
}

/// Lifts the drain: what it held starts.
pub fn undrain() {
    let _taken = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    drain_state().until = None;
    DRAIN_LIFTED.notify_all();
    SLOT_FREED.notify_all();
}

/// How long the drain still lasts at most; None when the server does not drain.
pub fn draining() -> Option<Duration> {
    drain_state().left()
}

/// Waits while the server drains, `name` held meanwhile.
pub fn wait_drain(name: &str) {
    let mut d = drain_state();
    if d.left().is_none() {
        return;
    }
    d.held.push(name.to_string());
    while let Some(left) = d.left() {
        d = DRAIN_LIFTED.wait_timeout(d, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    if let Some(i) = d.held.iter().position(|n| n == name) {
        d.held.remove(i);
    }
}

/// Waits up to `within` for the drain to lift: false when it still holds then.
pub fn lifted_within(within: Duration) -> bool {
    let deadline = Instant::now() + within;
    let mut d = drain_state();
    while let Some(left) = d.left() {
        let Some(until_deadline) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { return false };
        d = DRAIN_LIFTED.wait_timeout(d, left.min(until_deadline)).unwrap_or_else(|e| e.into_inner()).0;
    }
    true
}

/// Whether the drain holds a proof of `name`: one waiting here, or a job no worker may claim.
/// What a land that waits tells its agent.
pub fn held(name: &str) -> bool {
    let d = drain_state();
    d.left().is_some() && (d.held.iter().any(|n| n == name) || d.queued.iter().any(|(_, n)| n == name))
}

/// A job in the workers' queue, listed until a worker claims it or this is dropped.
pub struct Queued(String);
impl Drop for Queued {
    fn drop(&mut self) {
        claimed(&self.0);
    }
}
pub fn queued(id: &str, name: &str) -> Queued {
    drain_state().queued.push((id.to_string(), name.to_string()));
    Queued(id.to_string())
}
pub fn claimed(id: &str) {
    drain_state().queued.retain(|(i, _)| i != id);
}

/// The checks this process has in hand: (waiting for a slot or for the drain, running in a slot).
pub fn local() -> (usize, usize) {
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).len();
    (TAKING.load(Ordering::Relaxed), running)
}

struct Slot(usize);
impl Slot {
    /// A slot for the check `name`, once one is free and the server does not drain.
    fn take(name: &str) -> Slot {
        TAKING.fetch_add(1, Ordering::Relaxed);
        let n = loop {
            wait_drain(name);
            let mut taken = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
            while taken.len() >= check_slots() {
                taken = SLOT_FREED.wait(taken).unwrap_or_else(|e| e.into_inner());
            }
            // A drain begun while it waited for a slot: the slot stays free.
            if draining().is_some() {
                continue;
            }
            let n = (0..).find(|n| !taken.contains(n)).unwrap();
            taken.insert(n);
            break n;
        };
        TAKING.fetch_sub(1, Ordering::Relaxed);
        Slot(n)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let mut taken = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        taken.remove(&self.0);
        SLOT_FREED.notify_one();
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct CheckConfig {
    /// Run once per dependency fingerprint in the repo's cache dir, e.g. `pnpm install --frozen-lockfile`.
    pub setup: Option<String>,
    /// Files whose hash keys the setup, e.g. ["package.json", "pnpm-lock.yaml"].
    #[serde(default)]
    pub setup_keys: Vec<String>,
    /// Directories the setup produces, linked into every candidate, e.g. ["node_modules"].
    #[serde(default)]
    pub setup_links: Vec<String>,
    /// Folders a step builds that stay from one proof to the next, like a CI cache, because the tool
    /// that builds them knows what in them is stale: ["target"] for Cargo. Whatever else a step
    /// leaves goes before the next proof.
    #[serde(default)]
    pub keep: Vec<String>,
    /// Steps run on every candidate, in order. `{files}` expands to the task's code files,
    /// `{all_files}` to every file the task leaves in the tree.
    #[serde(default)]
    pub steps: Vec<ConfiguredStep>,
    /// The whole suite, run after a landing, never before.
    pub full: Option<String>,
    /// The whole suite cut in shards, each its own job (its own worker, when there are workers),
    /// all side by side. Takes the place of `full` when set.
    #[serde(default)]
    pub full_shards: Vec<FullShard>,
    /// What the tests talk to (a database, a cache), started before the steps of each proof and
    /// of each shard of the whole suite, on the machine that runs them, and stopped after.
    #[serde(default)]
    pub services: Vec<Service>,
    /// Secrets the server keeps for the repo (`avp secret set`) that every step and the whole suite
    /// get, beside those a step or a shard names itself.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// The whole suite on the head at set times of the day, whatever landed (`[[check.schedule]]`).
    #[serde(default)]
    pub schedule: Vec<Schedule>,
}

impl CheckConfig {
    /// The jobs of the whole suite: each shard once per combination of its matrix's values, or
    /// `full` alone. Empty: there is no suite.
    pub fn shards(&self) -> Vec<Shard> {
        if self.full_shards.is_empty() {
            return self.full.iter().map(|run| Shard { name: "full".into(), run: run.clone(), env: vec![], services: None, secrets: vec![], artifacts: vec![] }).collect();
        }
        let mut out = Vec::new();
        for s in &self.full_shards {
            // Every combination, the keys in their order: [[]] then one value of each key more.
            let mut combos: Vec<Vec<(&str, &str)>> = vec![vec![]];
            for (key, values) in &s.matrix {
                combos = combos.iter().flat_map(|c| values.iter().map(move |v| [c.as_slice(), &[(key.as_str(), v.as_str())]].concat())).collect();
            }
            for combo in combos {
                let name = if combo.is_empty() { s.name.clone() } else { format!("{} ({})", s.name, combo.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ")) };
                let env = combo.iter().map(|(k, v)| (matrix_var(k), v.to_string())).collect();
                out.push(Shard { name, run: s.run.clone(), env, services: s.services.clone(), secrets: s.secrets.clone(), artifacts: s.artifacts.clone() });
            }
        }
        out
    }

    /// What no field alone can say is wrong: two services or two shards of one name, a shard that
    /// needs a service there is none of.
    pub fn validate(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for s in &self.services {
            anyhow::ensure!(seen.insert(s.name.as_str()), "two services are named {:?}", s.name);
        }
        for s in &self.full_shards {
            for needed in s.services.iter().flatten() {
                anyhow::ensure!(seen.contains(needed.as_str()), "the shard {:?} needs the service {needed:?}, and no [[check.services]] is named so", s.name);
            }
        }
        let mut names = BTreeSet::new();
        for s in self.shards() {
            anyhow::ensure!(names.insert(s.name.clone()), "two shards of the whole suite are named {:?}", s.name);
        }
        for (name, paths) in self.steps.iter().map(|s| (&s.name, &s.artifacts)).chain(self.full_shards.iter().map(|s| (&s.name, &s.artifacts))) {
            for p in paths {
                anyhow::ensure!(safe_path(p.trim_end_matches('/')), "{name}: artifacts = {p:?} is not a path inside the step's folder (relative, without `..`)");
            }
        }
        Ok(())
    }
}

/// `node` -> `MATRIX_NODE`, `node-version` -> `MATRIX_NODE_VERSION`: what a shell reads.
fn matrix_var(key: &str) -> String {
    format!("MATRIX_{}", key.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect::<String>())
}

/// `[[check.full_shards]]`: one part of the whole suite.
#[derive(Clone, Debug, Deserialize)]
pub struct FullShard {
    pub name: String,
    pub run: String,
    /// The services it needs, by name. None said: every one.
    #[serde(default)]
    pub services: Option<Vec<String>>,
    /// Run once per value, `matrix = { node = ["20", "22"] }`: the shards `web (node 20)` and
    /// `web (node 22)`, each with `MATRIX_NODE` set. Several keys: every combination.
    #[serde(default, deserialize_with = "matrix")]
    pub matrix: BTreeMap<String, Vec<String>>,
    /// Secrets this shard gets beside `[check] secrets`: no other shard sees them.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// What it makes that is kept with its proof, as a step's `artifacts`.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

fn matrix<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<BTreeMap<String, Vec<String>>, D::Error> {
    let raw = BTreeMap::<String, Vec<toml::Value>>::deserialize(d)?;
    let mut out = BTreeMap::new();
    for (key, values) in raw {
        if values.is_empty() {
            return Err(serde::de::Error::custom(format!("matrix.{key} has no value: the shard would never run")));
        }
        let values = values.iter().map(|v| scalar(v).ok_or_else(|| serde::de::Error::custom(format!("matrix.{key} holds {v}: a value is a string, a number or a boolean")))).collect::<std::result::Result<_, _>>()?;
        out.insert(key, values);
    }
    Ok(out)
}

/// A TOML value as a shell would read it: `20`, `"20"` and `20.0` are what they say.
fn scalar(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) => Some(v.to_string()),
        _ => None,
    }
}

/// One job of the whole suite as it runs: a shard with one value of each key of its matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct Shard {
    pub name: String,
    pub run: String,
    /// `MATRIX_<KEY>` and its value.
    pub env: Vec<(String, String)>,
    pub services: Option<Vec<String>>,
    pub secrets: Vec<String>,
    pub artifacts: Vec<String>,
}

/// `[[check.services]]`: a database, a cache, anything the tests reach on a port. Each proof gets its
/// own, on its own ports and with its own data, so that two proofs at once never share one.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "ServiceFields")]
pub struct Service {
    pub name: String,
    pub kind: ServiceKind,
    /// None: up once its first port answers (an image without ports, at once).
    pub ready: Option<Ready>,
    /// How long it has to be ready, in seconds.
    pub ready_s: u64,
    /// Given to the service and to every step, `{port}` (and `{port:<its port>}` for an image's
    /// other ports, `{data}` for a command's folder) filled in: `DATABASE_URL`.
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ServiceKind {
    /// A container, run by docker or podman: each of its ports is mapped to a free port of the
    /// machine, on 127.0.0.1.
    Image { image: String, ports: Vec<u16> },
    /// A command, for a machine without containers, run in the candidate's folder until the steps
    /// are over: `{port}` is a free port, `{data}` an empty folder of its own.
    Run(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ready {
    /// Up once it exits 0: inside the container for an image (where `pg_isready` is), in the
    /// candidate's folder for a command.
    Command(String),
    /// Up once this port of the container answers.
    Port(u16),
}

/// Every field a service may hold, of either kind: one of the two, said plainly, as for a deploy.
#[derive(Deserialize)]
struct ServiceFields {
    name: String,
    image: Option<String>,
    run: Option<String>,
    #[serde(default)]
    ports: Vec<toml::Value>,
    ready: Option<toml::Value>,
    ready_s: Option<u64>,
    #[serde(default)]
    env: toml::Table,
}

impl TryFrom<ServiceFields> for Service {
    type Error = String;
    fn try_from(f: ServiceFields) -> std::result::Result<Self, String> {
        let name = f.name;
        // It names a container and a folder: nothing a shell or a path would read otherwise.
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!("a service's name is letters, digits, `-` and `_`: {name:?}"));
        }
        // "5432", "5432:5432" (GitHub's host:container, the host side AVP picks itself) or 5432.
        let port = |v: &toml::Value| -> Option<u16> {
            match v {
                toml::Value::Integer(n) => u16::try_from(*n).ok(),
                toml::Value::String(s) => s.rsplit(':').next()?.trim_end_matches("/tcp").parse().ok(),
                _ => None,
            }
            .filter(|p| *p > 0)
        };
        let ports = f.ports.iter().map(|v| port(v).ok_or_else(|| format!("service {name}: {v} is not a port"))).collect::<std::result::Result<Vec<u16>, _>>()?;
        let kind = match (f.image, f.run) {
            (Some(image), None) => ServiceKind::Image { image, ports },
            (None, Some(run)) if ports.is_empty() => ServiceKind::Run(run),
            (None, Some(_)) => return Err(format!("service {name}: `ports` is for an image; a `run` service listens on {{port}}, a free port AVP picks")),
            (Some(_), Some(_)) => return Err(format!("service {name} is `image` (a container) or `run` (a command), not both")),
            (None, None) => return Err(format!("service {name} needs `image` (a container, run by docker or podman) or `run` (a command)")),
        };
        let ready = match (&f.ready, &kind) {
            (None, _) => None,
            (Some(toml::Value::String(cmd)), _) => Some(Ready::Command(cmd.clone())),
            (Some(v @ toml::Value::Integer(_)), ServiceKind::Image { ports, .. }) => match port(v) {
                Some(p) if ports.contains(&p) => Some(Ready::Port(p)),
                _ => return Err(format!("service {name}: ready = {v} is none of its ports")),
            },
            (Some(toml::Value::Integer(_)), ServiceKind::Run(_)) => return Err(format!("service {name} listens on {{port}}, a free port AVP picks: leave `ready` out to wait for it, or give a command")),
            (Some(v), _) => return Err(format!("service {name}: ready = {v} is neither a command nor a port")),
        };
        let env = f.env.iter().map(|(k, v)| scalar(v).map(|v| (k.clone(), v)).ok_or_else(|| format!("service {name}: env.{k} is a string, a number or a boolean"))).collect::<std::result::Result<_, _>>()?;
        Ok(Service { name, kind, ready, ready_s: f.ready_s.unwrap_or(60), env })
    }
}

/// `[[check.schedule]]`: each day at `at` (UTC), the full suite on the head, or the shards it names.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "ScheduleFields")]
pub struct Schedule {
    /// "HH:MM", UTC.
    pub at: String,
    /// Minutes after midnight UTC, read from `at`.
    pub minute: i64,
    /// The `full_shards` it runs (`full` names `full`); none: all of them.
    pub shards: Vec<String>,
}

#[derive(Deserialize)]
struct ScheduleFields {
    at: String,
    #[serde(default)]
    shards: Vec<String>,
}

impl TryFrom<ScheduleFields> for Schedule {
    type Error = String;
    fn try_from(f: ScheduleFields) -> std::result::Result<Self, String> {
        let hm = f.at.split_once(':').and_then(|(h, m)| Some((h.parse::<i64>().ok()?, m.parse::<i64>().ok()?))).filter(|(h, m)| (0..24).contains(h) && (0..60).contains(m) && f.at.len() == 5);
        let Some((h, m)) = hm else { return Err(format!("a schedule's at = {:?} is a time of the day in UTC, \"HH:MM\" (\"02:00\")", f.at)) };
        Ok(Schedule { at: f.at, minute: h * 60 + m, shards: f.shards })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ConfiguredStep {
    pub name: String,
    pub run: String,
    /// Skip the step when the task touched no file of the kind it names ({files}: code).
    #[serde(default)]
    pub needs_files: bool,
    /// About this many related test files per job: a task whose tests reach further runs the step
    /// as several jobs side by side, each with `{shard}` as `i/n` (vitest and jest take
    /// `--shard={shard}`). AVP counts the tests through the imports to choose n; the test runner
    /// still decides which tests are related, so a proof runs the same tests, sooner.
    #[serde(default)]
    pub split: Option<usize>,
    /// Secrets this step gets beside `[check] secrets`: no other step sees them.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Paths in the step's folder (a folder: what is under it) packed after it, green or red, and
    /// kept with the proof for 14 days: a coverage report, Playwright's traces, screenshots.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

/// What to show of a red step: a split step lists its shares failures first, so the first share,
/// not the tail of the whole, which would be a green share's.
pub fn red_part(output: &str) -> &str {
    output.split("\n== share ").nth(1).unwrap_or(output)
}

/// One part of a task's proof, run as its own job beside the others: the steps no step splits,
/// or one share of a step that does. Or one shard of the whole suite: its matrix's values and the
/// services it needs.
#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
pub struct Part {
    /// Steps another part runs.
    #[serde(default)]
    pub skip: Vec<String>,
    /// The one step this part runs, its `{shard}` being `shard`.
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default)]
    pub shard: Option<String>,
    /// Given to the steps beside what they see: a shard's `MATRIX_<KEY>`.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The services started, by name. None: every one.
    #[serde(default)]
    pub services: Option<Vec<String>>,
    /// The secrets a shard names beside `[check] secrets`: their names, the values never.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// A task that changes avp.toml: the secrets the line's checks give, the most its steps get. A
    /// step that names another runs without it, as it would under the line's avp.toml.
    #[serde(default)]
    pub line_secrets: Option<Vec<String>>,
    /// What a shard keeps of what it makes (its `artifacts`). A worker from before ignores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

impl Part {
    /// The part that runs a shard of the whole suite.
    pub fn of_shard(s: &Shard) -> Part {
        Part { env: s.env.clone(), services: s.services.clone(), secrets: s.secrets.clone(), artifacts: s.artifacts.clone(), ..Default::default() }
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AvpConfig {
    #[serde(default)]
    pub check: CheckConfig,
    /// The repo's environments: who promotes to each, and the deploy that carries it out.
    #[serde(default)]
    pub envs: std::collections::BTreeMap<String, EnvConfig>,
    #[serde(default)]
    pub line: LineConfig,
    /// Where AVP tells people what needs them.
    #[serde(default)]
    pub notify: NotifyConfig,
    /// The repo on GitHub that AVP's bridge dispatches deploys on and writes `avp/full` to.
    #[serde(default)]
    pub github: GitHubConfig,
    /// What `avp serve` takes in from Git while a team moves over.
    #[serde(default)]
    pub git: GitConfig,
}

/// `[github]` in avp.toml, read by the bridge inside `avp serve` (AVP_GITHUB_BRIDGE=1).
#[derive(Clone, Debug, Deserialize, Default)]
pub struct GitHubConfig {
    /// `owner/name`. None: the bridge leaves this repo's GitHub alone.
    #[serde(default)]
    pub repo: Option<String>,
    /// The branch deploy workflows are dispatched on: the one the mirror writes. `main` by default.
    #[serde(default)]
    pub branch: Option<String>,
}

/// `[git]` in avp.toml: the commits that still reach a branch by Git, and the pull requests that
/// keep coming, brought into the line by the server itself.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct GitConfig {
    /// The Git repo the server fetches from: a URL (on GitHub, the one whose pull requests it reads).
    #[serde(default)]
    pub remote: Option<String>,
    /// A branch whose new commits each become a task, replayed on the head, proven and landed.
    #[serde(default)]
    pub follow: Option<String>,
    /// Open pull requests become tasks: `true`, or `"land"` to land those whose proof is green.
    #[serde(default)]
    pub pull_requests: PullRequests,
    /// Seconds between two looks (default 60).
    #[serde(default)]
    pub every: Option<u64>,
}

/// `pull_requests` in `[git]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(try_from = "PullRequestsField")]
pub enum PullRequests {
    #[default]
    Off,
    /// Each one a task, left open for a person or an agent to land.
    Tasks,
    /// Each one a task, landed when its proof is green.
    Land,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PullRequestsField {
    On(bool),
    Mode(String),
}

impl TryFrom<PullRequestsField> for PullRequests {
    type Error = String;
    fn try_from(f: PullRequestsField) -> std::result::Result<Self, String> {
        match f {
            PullRequestsField::On(false) => Ok(PullRequests::Off),
            PullRequestsField::On(true) => Ok(PullRequests::Tasks),
            PullRequestsField::Mode(m) if m == "land" => Ok(PullRequests::Land),
            PullRequestsField::Mode(m) => Err(format!("pull_requests is true, false or \"land\", not {m:?}")),
        }
    }
}

/// `[notify]` in avp.toml.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct NotifyConfig {
    /// The URLs AVP posts a short JSON to (Slack's `text` among its fields) when the full suite is
    /// red, a position is reverted, a task waits for a person's approval, a deploy is done or failed.
    /// `$AVP_NOTIFY_<NAME>` stands for a variable of the server's environment that holds the URL:
    /// a URL that carries a token stays out of the tree, its mirror and AVP Live.
    #[serde(default)]
    pub webhooks: Vec<String>,
}

/// `[line]` in avp.toml: what AVP does to the line by itself.
#[derive(Clone, Debug, Deserialize)]
pub struct LineConfig {
    /// A full suite red twice after a landing: the position that broke it is reverted, by a task
    /// AVP opens, proves and lands itself. On unless set to false.
    #[serde(default = "yes")]
    pub auto_revert: bool,
}

impl Default for LineConfig {
    fn default() -> Self {
        Self { auto_revert: true }
    }
}

fn yes() -> bool {
    true
}

/// `[envs.<name>]` in avp.toml.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct EnvConfig {
    /// `full`: AVP promotes here, by itself, each position whose full suite is green.
    #[serde(default)]
    pub follow: Option<String>,
    /// Only what that environment serves can be promoted here (production after staging).
    #[serde(default)]
    pub after: Option<String>,
    /// The people who may promote here and roll back; their promotion is the approval. Empty:
    /// anyone, or on a server with accounts, the repo's admins. `promoters` says the same.
    #[serde(default, alias = "promoters")]
    pub approvers: Vec<String>,
    /// How a promotion is carried out. None: the environment moves at once.
    #[serde(default)]
    pub deploy: Option<Deploy>,
    /// A URL that answers the git commit the environment serves (`{"sha": "..."}`, or the sha
    /// alone): read by the bridge, it moves the environment, whoever deployed. With a GitHub deploy,
    /// the environment moves once it answers the promoted commit, not once the run is green.
    #[serde(default)]
    pub serves: Option<String>,
}

/// `[envs.<name>.deploy]`: a command AVP runs itself, or a GitHub workflow its bridge dispatches.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "DeployFields")]
pub enum Deploy {
    /// `run = "<command>"`: run on the server, in the promoted position laid out, as a proof's
    /// steps are, with `{position}` and `{git}` filled in.
    Run(String),
    /// `github_workflow = "<file>"`, its `inputs` and `approve_environment`.
    GitHub(crate::model::DeployConfig),
}

impl Deploy {
    /// What AVP's bridge carries out: the GitHub kind. AVP runs the other itself.
    pub fn github(&self) -> Option<&crate::model::DeployConfig> {
        match self {
            Deploy::GitHub(d) => Some(d),
            Deploy::Run(_) => None,
        }
    }
}

/// Every field a deploy may hold, of either kind (DeployConfig's for GitHub): one of the two, said
/// plainly when it is neither or both, where an untagged enum would only say "no variant matched".
#[derive(Deserialize)]
struct DeployFields {
    run: Option<String>,
    github_workflow: Option<String>,
    #[serde(default)]
    inputs: BTreeMap<String, String>,
    approve_environment: Option<String>,
}

impl TryFrom<DeployFields> for Deploy {
    type Error = String;
    fn try_from(f: DeployFields) -> std::result::Result<Self, String> {
        match (f.run, f.github_workflow) {
            (Some(run), None) => Ok(Deploy::Run(run)),
            (None, Some(github_workflow)) => Ok(Deploy::GitHub(crate::model::DeployConfig { github_workflow, inputs: f.inputs, approve_environment: f.approve_environment })),
            (Some(_), Some(_)) => Err("a deploy is `run` (a command AVP runs) or `github_workflow` (a workflow AVP's bridge dispatches), not both".into()),
            (None, None) => Err("a deploy needs `run` (a command AVP runs) or `github_workflow` (a workflow AVP's bridge dispatches)".into()),
        }
    }
}

pub fn read_config(tree: &Tree, objects: &Objects) -> Result<AvpConfig> {
    // trame.toml: the file's name before AVP was AVP, still read when avp.toml is absent.
    let Some((name, entry)) = ["avp.toml", "trame.toml"].into_iter().find_map(|n| tree.get(n).map(|e| (n, e))) else { return Ok(AvpConfig::default()) };
    let text = String::from_utf8(objects.get(&entry.blob)?)?;
    let cfg: AvpConfig = toml::from_str(&text).context(name)?;
    cfg.check.validate().context(name)?;
    Ok(cfg)
}

/// A file as it was laid: its blob, and what it looked like on disk right after.
#[derive(serde::Serialize, Deserialize, PartialEq)]
struct Laid {
    blob: Hash,
    mode: u32,
    stamp: Stamp,
}

fn stamp(path: &Path) -> Option<Stamp> {
    fs::symlink_metadata(path).ok().filter(|m| m.is_file()).map(|m| crate::view::stamp_of(&m))
}

/// Lays `tree` out in `dir` as a fresh checkout would hold it, reusing what is already there: a
/// file is written again only if its blob differs or a step changed it since, and whatever is not
/// in the tree goes (an earlier proof's outputs, a file the tree no longer has). The setup's links,
/// named in `keep`, stay, and so do AVP's own `.avp-*` files at the top.
pub fn materialize(objects: &Objects, tree: &Tree, dir: &Path, keep: &[String]) -> Result<()> {
    if let Some(bad) = tree.keys().find(|p| !safe_path(p)) {
        anyhow::bail!("a path outside the candidate: {bad:?}");
    }
    fs::create_dir_all(dir)?;
    let marker = dir.join(".avp-laid");
    let previous: BTreeMap<String, Laid> = fs::read(&marker).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut dirs = std::collections::HashSet::new();
    for p in tree.keys() {
        let mut s = p.as_str();
        while let Some((parent, _)) = s.rsplit_once('/') {
            if !dirs.insert(parent) {
                break;
            }
            s = parent;
        }
    }
    sweep(dir, "", tree, &dirs, keep)?;
    let mut laid = BTreeMap::new();
    for (path, entry) in tree {
        let dest = dir.join(path);
        let unchanged = previous.get(path).is_some_and(|l| l.blob == entry.blob && l.mode == entry.mode && stamp(&dest) == Some(l.stamp));
        if !unchanged {
            objects.copy_to(&entry.blob, &dest, entry.mode)?;
            // A copy keeps its object's date on macOS: older than the build an earlier candidate
            // left in `keep` (`target/`), so cargo would take the file for the one it built and the
            // proof would run what that candidate built (the full suite of position 61 ran 60's avp).
            crate::view::date_now(&dest)?;
        }
        let stamp = stamp(&dest).with_context(|| format!("{path} was not laid out"))?;
        laid.insert(path.clone(), Laid { blob: entry.blob.clone(), mode: entry.mode, stamp });
    }
    fs::write(&marker, serde_json::to_vec(&laid)?)?;
    Ok(())
}

/// Removes from `dir`/`rel` what is neither in `tree` nor a folder on the way to it, but `keep`.
fn sweep(dir: &Path, rel: &str, tree: &Tree, dirs: &std::collections::HashSet<&str>, keep: &[String]) -> Result<()> {
    // A folder of the tree a step left read-only (a test stopped before it gave the mode back) is
    // writable again, as on a fresh checkout: else nothing in it could be removed or laid again, and
    // the next step to write there would fail in this slot only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(dir.join(rel))?.permissions().mode() & 0o700 != 0o700 {
            fs::set_permissions(dir.join(rel), fs::Permissions::from_mode(0o755))?;
        }
    }
    for entry in fs::read_dir(dir.join(rel))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if (rel.is_empty() && name.starts_with(".avp-")) || keep.iter().any(|k| *k == name || *k == path) {
            continue;
        }
        let is_dir = entry.file_type()?.is_dir();
        if is_dir && dirs.contains(path.as_str()) {
            sweep(dir, &path, tree, dirs, keep)?;
        } else if is_dir {
            remove_all(&entry.path())?;
        } else if !tree.contains_key(&path) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// `remove_dir_all`, even when a step left folders it cannot be removed from (mode 0555).
fn remove_all(path: &Path) -> Result<()> {
    if fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut stack = vec![path.to_path_buf()];
        while let Some(d) = stack.pop() {
            let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o755));
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
    }
    fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
}

fn fingerprint(tree: &Tree, keys: &[String]) -> String {
    let mut h = blake3::Hasher::new();
    for k in keys {
        h.update(k.as_bytes());
        if let Some(e) = tree.get(k) {
            h.update(e.blob.as_bytes());
        }
    }
    h.finalize().to_hex().to_string()
}

/// What a build needs of the environment AVP runs in, beside `LC_*`. The npm and corepack ones are
/// those the server and worker images set (deploy/).
const STEP_ENV: [&str; 13] = ["PATH", "HOME", "LANG", "TERM", "TMPDIR", "USER", "SHELL", "CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR", "XDG_CACHE_HOME", "COREPACK_HOME", "npm_config_store_dir"];

/// AVP's own secret (TRAME_TOKEN is its former name): no command AVP runs gets it, a deploy neither.
fn avp_secret(name: &str) -> bool {
    matches!(name.to_ascii_uppercase().as_str(), "AVP_TOKEN" | "TRAME_TOKEN")
}

/// Never given to a step, even when AVP_PROOF_ENV names it: AVP's own secret, a cloud's credentials,
/// the forge's and the models' tokens.
fn secret(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    avp_secret(&name) || matches!(name.as_str(), "GH_TOKEN" | "GITHUB_TOKEN") || ["AWS_", "CLAUDE_", "ANTHROPIC_", "OPENAI_"].iter().any(|p| name.starts_with(p))
}

/// The names of a comma separated list (AVP_PROOF_ENV, AVP_DEPLOY_ENV).
fn names_in(list: &str) -> Vec<&str> {
    list.split(',').map(str::trim).filter(|n| !n.is_empty()).collect()
}

/// The variables of `vars` a step sees: `STEP_ENV`, `LC_*` and the names `listed` gives
/// (AVP_PROOF_ENV, comma separated), never a secret, nor a name `deploy` gives (AVP_DEPLOY_ENV): a
/// deploy's credential still listed in both, from before AVP_DEPLOY_ENV, is the deploy's alone. A
/// step runs what an agent wrote, so it gets no token of the machine, and none of the server's own
/// settings either: on 26/10 the line that develops AVP ran its server with AVP_CHECK_SLOTS=1, every
/// step inherited it, and AVP's unit tests, which start engines in the test process, shared one
/// proof slot and went red on timings.
fn step_env(vars: impl Iterator<Item = (OsString, OsString)>, listed: &str, deploy: &str) -> Vec<(OsString, OsString)> {
    let (listed, deploy) = (names_in(listed), names_in(deploy));
    vars.filter(|(k, _)| k.to_str().is_some_and(|k| (STEP_ENV.contains(&k) || k.starts_with("LC_") || (listed.contains(&k) && !deploy.contains(&k))) && !secret(k)))
        .collect()
}

/// What a deploy gets beside what a step sees: the variables of `vars` `listed` names
/// (AVP_DEPLOY_ENV, comma separated), a cloud's or the forge's credentials included, since listing
/// them there is how the server's owner hands a deploy what it needs. AVP's own secret, never: with
/// it, a deploy script could act on the line as anyone.
fn deploy_env(vars: impl Iterator<Item = (OsString, OsString)>, listed: &str) -> Vec<(OsString, OsString)> {
    let listed = names_in(listed);
    vars.filter(|(k, _)| k.to_str().is_some_and(|k| listed.contains(&k) && !avp_secret(k))).collect()
}

/// What an output must not show of `given`: each value, and each line of one written on several (a
/// key), when longer than 6 characters (a shorter one, a "true" or a region, would hide words that
/// say nothing), the longest first, so that a value holding another is masked whole.
fn hidden(given: &[(OsString, OsString)]) -> Vec<String> {
    let mut hidden: Vec<String> = given
        .iter()
        .map(|(_, v)| v.to_string_lossy().to_string())
        .flat_map(|v| {
            // Split as the output is told line by line: at `\n` and at `\r`.
            let lines: Vec<String> = v.split(['\n', '\r']).map(String::from).collect();
            std::iter::once(v).chain(lines)
        })
        .filter(|v| v.chars().count() > 6)
        .collect();
    hidden.sort_by_key(|v| std::cmp::Reverse(v.len()));
    hidden
}

fn mask(text: &str, hidden: &[String]) -> String {
    hidden.iter().fold(text.to_string(), |text, h| text.replace(h.as_str(), "***"))
}

/// Why `name` cannot be a secret for tests, if it cannot: not a variable's name, a name `secret`
/// refuses a step, AVP's own settings, or what AVP already gives every step.
pub fn refused_secret(name: &str) -> Option<&'static str> {
    let upper = name.to_ascii_uppercase();
    if name.is_empty() || name.len() > 128 || name.starts_with(|c: char| c.is_ascii_digit()) || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some("a secret's name is a variable's name: letters, digits and _, not starting with a digit")
    } else if secret(name) {
        Some("AVP's own token, a cloud's credentials, the forge's and the models' tokens never reach a step")
    } else if upper.starts_with("AVP_") || upper.starts_with("TRAME_") {
        Some("AVP_* and TRAME_* are AVP's own settings")
    } else if STEP_ENV.contains(&name) || upper.starts_with("LC_") || ["CI", "AVP", "TRAME", "LANG"].contains(&upper.as_str()) {
        Some("every step gets it from AVP already")
    } else {
        None
    }
}

/// A secret's name and when it was last set, in ms: all that leaves the store.
#[derive(Clone, Debug, serde::Serialize, Deserialize)]
pub struct SecretInfo {
    pub name: String,
    pub set_at: i64,
}

fn run(dir: &Path, command: &str, cancel: &AtomicBool) -> Step {
    run_with(dir, command, cancel, &[], &[], None).0
}

/// `sh -c command` in `dir` as a step runs: from a scrubbed environment, `env` beside it (a shard's
/// matrix, the services' `env`), in a process group of its own.
fn shell(dir: &Path, command: &str, env: &[(OsString, OsString)]) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(dir)
        // Not the environment AVP runs in: what an agent wrote never reads the machine's secrets.
        .env_clear()
        .envs(step_env(std::env::vars_os(), &std::env::var("AVP_PROOF_ENV").unwrap_or_default(), &std::env::var("AVP_DEPLOY_ENV").unwrap_or_default()))
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("CI", "true")
        .env("AVP", "1")
        .env("TRAME", "1")
        // The same locale as a CI runner: a test that formats money must not depend on the host.
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .stdin(Stdio::null());
    // Its own process group, so that stopping it stops what it started (test runners fork).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd
}

/// A deploy's command, run as a proof's steps are, with the variables AVP_DEPLOY_ENV lists beside (a
/// registry's token, a cluster's config), telling `watch` each line of the output once it is
/// written: how a deploy AVP runs says where it is while it runs. Their values are masked in what
/// `watch` is told and in the output kept, which events, `avp proof` and AVP Live show.
pub fn run_deploy(dir: &Path, command: &str, watch: &mut dyn FnMut(&str)) -> Step {
    let given = deploy_env(std::env::vars_os(), &std::env::var("AVP_DEPLOY_ENV").unwrap_or_default());
    run_with(dir, command, &AtomicBool::new(false), &[], &given, Some(watch)).0
}

/// `run`, `env` and `given` beside what a step sees, the values of `given` masked in the output and
/// in its whole log, each line dated.
fn run_with(dir: &Path, command: &str, cancel: &AtomicBool, env: &[(OsString, OsString)], given: &[(OsString, OsString)], mut watch: Option<&mut dyn FnMut(&str)>) -> (Step, Log) {
    let start = Instant::now();
    let mut capture = Capture::new(start);
    let hidden = hidden(given);
    let failed = |e: std::io::Error| (Step { name: String::new(), command: command.to_string(), exit: -1, seconds: 0.0, output: e.to_string() }, Log::default());
    // stdout and stderr in one file, in the order they were written: no pipe to drain while polling.
    let log = std::env::temp_dir().join(format!("avp-step-{}.log", ulid::Ulid::new()));
    let mut open = fs::OpenOptions::new();
    open.write(true).create_new(true);
    // It holds a deploy's credentials or a step's secrets as printed, before they are masked: the
    // user's alone.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let file = match open.open(&log) {
        Ok(f) => f,
        Err(e) => return failed(e),
    };
    let mut cmd = shell(dir, command, &[env, given].concat());
    match file.try_clone() {
        Ok(f) => cmd.stdout(f).stderr(file),
        Err(e) => return failed(e),
    };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    // Read back as it grows: each line dated when it comes, as a CI's log is; and told to whoever
    // watches once it ends, at `\n` or at the `\r` of a progress bar drawn again in place, and what
    // is left once the step is over.
    let mut reader = fs::File::open(&log).ok();
    let mut partial = Vec::new();
    let mut tell = |end: bool| {
        let Some(reader) = reader.as_mut() else { return };
        let mut fresh = Vec::new();
        let _ = std::io::Read::read_to_end(reader, &mut fresh);
        capture.feed(&fresh, end, &hidden);
        let Some(watch) = watch.as_mut() else { return };
        partial.extend_from_slice(&fresh);
        let mut lines: Vec<&[u8]> = partial.split(|b| *b == b'\n' || *b == b'\r').collect();
        let rest = if end { vec![] } else { lines.pop().unwrap_or_default().to_vec() };
        for line in lines {
            watch(&mask(&String::from_utf8_lossy(line), &hidden));
        }
        partial = rest;
    };
    let exit = loop {
        tell(false);
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => {}
            Err(_) => break -1,
        }
        if cancel.load(Ordering::Relaxed) {
            // The whole group, what the step started with it, by kill(2) itself: procps 4.0.2 (Debian
            // bookworm, the server's and the workers' image) reads `kill -KILL -<group>` as every
            // process of the user, and stopping a superseded proof killed the server or the worker
            // running it (25/10, found by CI on the builder).
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            break CANCELLED;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    tell(true);
    // Masked before it is cut, which could leave half a value on either side of the cut.
    let mut text = mask(&fs::read(&log).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default(), &hidden);
    let _ = fs::remove_file(&log);
    if exit == CANCELLED {
        text.push_str("\ncancelled: the task's files changed, a newer tree is being proven");
    }
    if text.len() > 60_000 {
        let (mut a, mut b) = (20_000, text.len() - 40_000);
        while !text.is_char_boundary(a) {
            a -= 1;
        }
        while !text.is_char_boundary(b) {
            b += 1;
        }
        text = format!("{}\n…\n{}", &text[..a], &text[b..]);
    }
    let mut log = capture.end();
    if exit == CANCELLED {
        log.lines.push((start.elapsed().as_millis() as u64, "cancelled: the task's files changed, a newer tree is being proven".into()));
    }
    (Step { name: String::new(), command: command.to_string(), exit, seconds: start.elapsed().as_secs_f64(), output: text }, log)
}

/// How much of a step's output its log keeps: the first megabyte and the last three, the lines
/// between counted. A line longer than `LINE_MAX` (a minified bundle printed whole) is cut there.
const LOG_HEAD: usize = 1 << 20;
const LOG_TAIL: usize = 3 << 20;
const LINE_MAX: usize = 64 << 10;

/// A step's whole output as a CI keeps it: each line with when it came.
#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
pub struct Log {
    /// When the step started, in milliseconds since the epoch.
    pub started: i64,
    /// Each line, with the milliseconds since the start when it came, as a terminal shows it: a
    /// progress bar drawn again with `\r` as its last state.
    pub lines: Vec<(u64, String)>,
    /// The lines left out when there were more than `LOG_HEAD` and `LOG_TAIL` keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut: Option<Cut>,
}

/// What a log leaves out: `lines` lines, `bytes` bytes, between `lines[at - 1]` and `lines[at]`.
#[derive(Clone, Copy, Debug, serde::Serialize, Deserialize)]
pub struct Cut {
    pub at: usize,
    pub lines: usize,
    pub bytes: u64,
}

/// A log as it is written: the head while it holds less than `LOG_HEAD`, then the tail, whose
/// oldest lines go once it holds more than `LOG_TAIL`.
struct Capture {
    start: Instant,
    log: Log,
    pending: Vec<u8>,
    head: usize,
    tail: std::collections::VecDeque<(u64, String)>,
    tail_bytes: usize,
    cut_lines: usize,
    cut_bytes: u64,
}

impl Capture {
    fn new(start: Instant) -> Capture {
        Capture { start, log: Log { started: now_ms(), ..Default::default() }, pending: Vec::new(), head: 0, tail: Default::default(), tail_bytes: 0, cut_lines: 0, cut_bytes: 0 }
    }

    /// What the step wrote since: each line that ended, masked, and the rest once it is over.
    fn feed(&mut self, bytes: &[u8], end: bool, hidden: &[String]) {
        self.pending.extend_from_slice(bytes);
        let mut done = 0;
        while let Some(n) = self.pending[done..].iter().position(|b| *b == b'\n') {
            let line = self.pending[done..done + n].to_vec();
            self.line(&line, hidden);
            done += n + 1;
        }
        self.pending.drain(..done);
        if (end && !self.pending.is_empty()) || self.pending.len() > LINE_MAX {
            let line = std::mem::take(&mut self.pending);
            self.line(&line[..line.len().min(LINE_MAX)], hidden);
        }
    }

    fn line(&mut self, bytes: &[u8], hidden: &[String]) {
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let shown = bytes.rsplit(|b| *b == b'\r').next().unwrap_or(bytes);
        let line = mask(&String::from_utf8_lossy(shown), hidden);
        let ms = self.start.elapsed().as_millis() as u64;
        if self.head < LOG_HEAD {
            self.head += line.len() + 1;
            self.log.lines.push((ms, line));
            return;
        }
        self.tail_bytes += line.len() + 1;
        self.tail.push_back((ms, line));
        while self.tail_bytes > LOG_TAIL {
            let Some((_, old)) = self.tail.pop_front() else { break };
            self.tail_bytes -= old.len() + 1;
            self.cut_lines += 1;
            self.cut_bytes += old.len() as u64 + 1;
        }
    }

    fn end(mut self) -> Log {
        if self.cut_lines > 0 {
            self.log.cut = Some(Cut { at: self.log.lines.len(), lines: self.cut_lines, bytes: self.cut_bytes });
        }
        self.log.lines.extend(self.tail);
        self.log
    }
}

/// The line of a log where a red step first says what failed: a compiler's `error`, a runner's
/// FAIL, `×` or `✗`, a panic, a traceback. None: nothing says so, the end is what to read.
pub fn first_failure<'a>(lines: impl IntoIterator<Item = &'a str>) -> Option<usize> {
    static FAILED: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let failed = FAILED.get_or_init(|| {
        Regex::new(r"^error(\[E\d+\])?:|: error TS\d+|\berror TS\d+:|\bFAIL(ED)?\b|^\s*(×|✗|✕|●) |panicked at|^Traceback \(most recent call last\)|^(\w+)?(Error|Exception): |^npm ERR!|\bERR_PNPM_|^\s*--- FAIL:|\*\*\* \[.*\] Error \d+").unwrap()
    });
    let colors = Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap();
    lines.into_iter().position(|l| failed.is_match(&colors.replace_all(l, "")))
}

/// The lines of a log worth reading first, as indexes `[from, to)`: `n` lines from a few before
/// the first failure, or its last `n` when none is found.
pub fn around_failure(log: &Log, n: usize) -> (usize, usize) {
    let total = log.lines.len();
    match first_failure(log.lines.iter().map(|(_, l)| l.as_str())) {
        Some(i) => {
            let from = i.saturating_sub(n / 6);
            (from, (from + n).min(total))
        }
        None => (total.saturating_sub(n), total),
    }
}

/// What a proof keeps of a step beside its output, for 14 days: its whole log, and the files its
/// `artifacts` name, packed (`.tar.gz`). Under `<data>/kept/<key>/` on the machine that proves it,
/// until a worker sends them back with its result; on the server, read by `avp proof` and AVP Live.
pub struct Kept {
    dir: PathBuf,
}

/// What one step left in the store, as a worker sends it back.
#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
pub struct KeptStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<Log>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Artifacts>,
    /// The pack, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
}

/// What a step's pack holds, and what it could not.
#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
pub struct Artifacts {
    pub files: usize,
    /// The files' bytes, and the pack's.
    pub bytes: u64,
    pub packed: u64,
    /// Paths it names that the step did not make.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// Files past the proof's cap (`ARTIFACTS_MAX`), or that are not plain files (a link).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left_out: Vec<String>,
    /// Why nothing could be packed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How long a proof's logs and artifacts are kept.
const KEPT_FOR: Duration = Duration::from_secs(14 * 86_400);
/// The bytes of artifacts one proof keeps (one job of a split proof, one shard of the whole suite).
pub const ARTIFACTS_MAX: u64 = 100 << 20;

/// The name a step's log and artifacts are kept under: what the step says of itself. No field of a
/// proof names them, so they reach it by whatever way its steps go (a worker's result, a shard run
/// again, a revert's proof), and a proof from before simply has none. The seconds count to the
/// millisecond: a float read back from JSON may differ in its last bit.
pub fn kept_key(step: &Step) -> String {
    let mut h = blake3::Hasher::new();
    h.update(format!("{}\0{}\0{}\0", step.command, step.exit, (step.seconds * 1000.0).round() as i64).as_bytes());
    h.update(step.output.as_bytes());
    h.finalize().to_hex()[..32].to_string()
}

impl Kept {
    pub fn new(data: &Path) -> Kept {
        Kept { dir: data.join("kept") }
    }

    fn entry(&self, key: &str) -> Option<PathBuf> {
        (key.len() == 32 && key.bytes().all(|b| b.is_ascii_hexdigit())).then(|| self.dir.join(key))
    }

    /// Keeps a step's log and artifacts (a pack written as `artifacts.tar.gz` in `pack_from`'s
    /// place), then lets go of what is older than 14 days.
    pub fn put(&self, key: &str, step: &KeptStep, pack_from: Option<&Path>) -> Result<()> {
        let dir = self.entry(key).context("not a step's key")?;
        fs::create_dir_all(&dir)?;
        if let Some(log) = &step.log {
            fs::write(dir.join("log.json"), serde_json::to_vec(log)?)?;
            // How many lines it says, read for every proof shown without reading the log.
            fs::write(dir.join("log.lines"), (log.lines.len() + log.cut.map_or(0, |c| c.lines)).to_string())?;
        }
        if let Some(a) = &step.artifacts {
            fs::write(dir.join("artifacts.json"), serde_json::to_vec(a)?)?;
        }
        if let Some(from) = pack_from {
            fs::rename(from, dir.join("artifacts.tar.gz"))?;
        } else if let Some(b64) = &step.pack {
            use base64::Engine as _;
            fs::write(dir.join("artifacts.tar.gz"), base64::engine::general_purpose::STANDARD.decode(b64)?)?;
        }
        self.sweep();
        Ok(())
    }

    pub fn log(&self, key: &str) -> Option<Log> {
        serde_json::from_slice(&fs::read(self.entry(key)?.join("log.json")).ok()?).ok()
    }

    /// The lines the step printed, when its log is kept.
    pub fn lines(&self, key: &str) -> Option<usize> {
        fs::read_to_string(self.entry(key)?.join("log.lines")).ok()?.trim().parse().ok()
    }

    pub fn artifacts(&self, key: &str) -> Option<Artifacts> {
        serde_json::from_slice(&fs::read(self.entry(key)?.join("artifacts.json")).ok()?).ok()
    }

    /// The step's pack, when it has one.
    pub fn pack(&self, key: &str) -> Option<PathBuf> {
        Some(self.entry(key)?.join("artifacts.tar.gz")).filter(|p| p.is_file())
    }

    /// What a worker sends back of a step, taken out of its own store.
    pub fn take(&self, key: &str) -> Option<KeptStep> {
        use base64::Engine as _;
        let dir = self.entry(key)?;
        let step = KeptStep { log: self.log(key), artifacts: self.artifacts(key), pack: fs::read(dir.join("artifacts.tar.gz")).ok().map(|b| base64::engine::general_purpose::STANDARD.encode(b)) };
        let _ = fs::remove_dir_all(&dir);
        (step.log.is_some() || step.artifacts.is_some()).then_some(step)
    }

    /// A step a task's proof ran as several jobs, shown as one: its log is its shares' one after the
    /// other, its pack theirs, each in a folder of its own (`share-1-3/`).
    pub fn merge(&self, merged: &Step, shares: &[(&str, &Step)]) {
        let mut log = Log { started: i64::MAX, ..Default::default() };
        let (mut packs, mut artifacts) = (Vec::new(), Artifacts::default());
        for (shard, st) in shares {
            let key = kept_key(st);
            if let Some(l) = self.log(&key) {
                log.started = log.started.min(l.started);
                log.lines.push((0, format!("== share {shard}, exit {}", st.exit)));
                log.lines.extend(l.lines);
            }
            if let (Some(a), Some(p)) = (self.artifacts(&key), self.pack(&key)) {
                artifacts.files += a.files;
                artifacts.bytes += a.bytes;
                artifacts.missing.extend(a.missing);
                artifacts.left_out.extend(a.left_out);
                packs.push((format!("share-{}", shard.replace('/', "-")), p));
            }
        }
        if log.lines.is_empty() {
            return;
        }
        let key = kept_key(merged);
        let mut step = KeptStep { log: Some(log), ..Default::default() };
        let mut packed = None;
        if !packs.is_empty() {
            let tmp = self.dir.join(format!("merge-{}", ulid::Ulid::new()));
            let out = tmp.with_extension("tar.gz");
            let unpacked = packs.iter().all(|(sub, p)| fs::create_dir_all(tmp.join(sub)).is_ok() && Command::new("tar").arg("-xzf").arg(p).arg("-C").arg(tmp.join(sub)).status().is_ok_and(|s| s.success()));
            if unpacked && Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(&tmp).arg(".").status().is_ok_and(|s| s.success()) {
                artifacts.packed = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                packed = Some(out);
            }
            let _ = remove_all(&tmp);
            step.artifacts = Some(artifacts);
        }
        if let Err(e) = self.put(&key, &step, packed.as_deref()) {
            eprintln!("the log of a split step not kept: {e:#}");
        }
    }

    /// Lets go of what was kept more than 14 days ago, at most once an hour.
    fn sweep(&self) {
        static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
        let now = now_ms();
        if now - LAST.load(Ordering::Relaxed) < 3_600_000 {
            return;
        }
        LAST.store(now, Ordering::Relaxed);
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.modified().is_ok_and(|t| t.elapsed().is_ok_and(|age| age > KEPT_FOR)) {
                // A step's folder, or a pack a stop left halfway.
                let _ = if meta.is_dir() { remove_all(&e.path()) } else { fs::remove_file(e.path()).map_err(Into::into) };
            }
        }
    }
}

/// Packs what `paths` name in the step's folder `dir` (a folder: what is under it) into `out`, at
/// most `budget` bytes of files, what is past it listed as left out. Plain files only, found without
/// following a link: nothing outside the folder is ever packed. By the machine's `tar`.
fn pack(dir: &Path, paths: &[String], budget: &mut u64, out: &Path) -> Artifacts {
    let mut a = Artifacts::default();
    let (mut files, mut seen, mut over) = (Vec::new(), BTreeSet::new(), 0);
    let Ok(root) = dir.canonicalize() else { return Artifacts { error: Some("the step's folder is gone".into()), ..a } };
    for p in paths {
        let rel = p.trim_end_matches('/');
        match dir.join(rel).canonicalize() {
            Err(_) => a.missing.push(p.clone()),
            // A path through a link (`out/` pointing at /tmp) would leave the folder.
            Ok(c) if !safe_path(rel) || !c.starts_with(&root) => a.left_out.push(p.clone()),
            Ok(_) => {}
        }
        if a.missing.contains(p) || a.left_out.contains(p) {
            continue;
        }
        let mut todo = vec![PathBuf::from(rel)];
        while let Some(rel) = todo.pop() {
            let Ok(meta) = fs::symlink_metadata(dir.join(&rel)) else { continue };
            let name = rel.to_string_lossy().to_string();
            if meta.is_dir() {
                let mut inner: Vec<PathBuf> = fs::read_dir(dir.join(&rel)).into_iter().flatten().flatten().map(|e| rel.join(e.file_name())).collect();
                inner.sort_by(|x, y| y.cmp(x));
                todo.extend(inner);
            } else if !seen.insert(name.clone()) {
                continue;
            } else if !meta.is_file() || name.chars().any(|c| c.is_control() || c == '\\') || meta.len() > *budget {
                // A link, a socket, a name `tar -T` would read as another, or past the cap.
                if a.left_out.len() < 50 { a.left_out.push(name) } else { over += 1 }
            } else {
                *budget -= meta.len();
                a.bytes += meta.len();
                files.push(name);
            }
        }
    }
    if over > 0 {
        a.left_out.push(format!("… {over} more"));
    }
    a.files = files.len();
    if files.is_empty() {
        return a;
    }
    // `./` before each name: one starting with `-` must not read as an option.
    let list = out.with_extension("list");
    let packed = fs::write(&list, files.iter().map(|f| format!("./{f}\n")).collect::<String>()).map_err(|e| e.to_string()).and_then(|_| {
        let o = Command::new("tar").arg("-czf").arg(out).arg("-C").arg(dir).arg("-T").arg(&list).stdin(Stdio::null()).output().map_err(|e| format!("tar: {e}"))?;
        if o.status.success() { Ok(()) } else { Err(format!("tar: {}", String::from_utf8_lossy(&o.stderr).trim())) }
    });
    let _ = fs::remove_file(&list);
    match packed {
        Ok(()) => a.packed = fs::metadata(out).map(|m| m.len()).unwrap_or(0),
        Err(e) => {
            let _ = fs::remove_file(out);
            a = Artifacts { files: 0, bytes: 0, error: Some(e), ..a };
        }
    }
    a
}

/// `step=<name>` for a proof's log and artifacts routes, the name percent-encoded.
pub fn step_query(name: &str) -> String {
    let enc: String = name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    format!("step={enc}")
}

/// Ports given to the services of the checks this process runs, until they stop: two checks at once
/// never get the same one, even freed by the OS between its pick and the service's bind.
static PORTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());

/// `n` free ports of 127.0.0.1, none another check of this process holds.
fn free_ports(n: usize) -> std::io::Result<Vec<u16>> {
    let mut held = PORTS.lock().unwrap_or_else(|e| e.into_inner());
    // Every listener open until all are picked: the OS hands out n different ports.
    let mut open = Vec::new();
    let mut out = Vec::new();
    while out.len() < n {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = l.local_addr()?.port();
        if held.insert(port) {
            out.push(port);
        }
        open.push(l);
    }
    Ok(out)
}

/// Whether something answers on 127.0.0.1:`port`: it accepts the connection and does not close it
/// at once. Docker's proxy accepts on a container's port before the service inside listens, then
/// hangs up.
fn answers(port: u16) -> bool {
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
    match std::io::Read::read(&mut s, &mut [0u8; 1]) {
        Ok(n) => n > 0,
        Err(e) => matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
    }
}

/// Whether `cmd` exits 0 within `within`; past it, it is stopped, with what it started.
fn succeeds(cmd: &mut Command, within: Duration) -> bool {
    let Ok(mut child) = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return false };
    let deadline = Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                stop(&mut child, Duration::ZERO);
                return false;
            }
        }
    }
}

/// Stops a process AVP started in a group of its own, and what it started: asked first, killed
/// after `grace`.
fn stop(child: &mut std::process::Child, grace: Duration) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// docker, or podman where docker is not: whichever this machine has.
fn container_tool() -> Option<&'static str> {
    static TOOL: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *TOOL.get_or_init(|| ["docker", "podman"].into_iter().find(|t| Command::new(t).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())))
}

/// `{port}`, `{port:<inner port>}` and `{data}` of a service filled in.
fn fill(text: &str, ports: &[(u16, u16)], data: Option<&Path>) -> String {
    let mut text = text.to_string();
    for (inner, host) in ports {
        text = text.replace(&format!("{{port:{inner}}}"), &host.to_string());
    }
    if let Some((_, host)) = ports.first() {
        text = text.replace("{port}", &host.to_string());
    }
    match data {
        Some(d) => text.replace("{data}", &d.to_string_lossy()),
        None => text,
    }
}

/// The last lines of what a service printed, why it never was ready: the reason before them stays
/// in the 25 lines `avp check` shows of a red step.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

enum Running {
    /// docker or podman, and the container's name.
    Container(&'static str, String),
    Process(std::process::Child),
}

/// One service started: what it is, how it is reached.
struct Started<'a> {
    service: &'a Service,
    /// (its port, the machine's port): a container's ports, or ({port}, {port}) for a command.
    ports: Vec<(u16, u16)>,
    env: Vec<(OsString, OsString)>,
    log: PathBuf,
}

/// The services of one check, up. Dropped, whatever ended the check (red, a cancel, an error, a
/// panic), they are stopped and removed, with their data.
struct Up {
    running: Vec<Running>,
    ports: Vec<u16>,
    folder: PathBuf,
    /// What every step gets of them: each one's `env`, its ports filled in.
    env: Vec<(OsString, OsString)>,
}

impl Drop for Up {
    fn drop(&mut self) {
        for r in &mut self.running {
            match r {
                Running::Container(tool, name) => {
                    let _ = Command::new(*tool).args(["rm", "-f", "-v", name.as_str()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
                }
                Running::Process(child) => stop(child, Duration::from_secs(5)),
            }
        }
        let mut held = PORTS.lock().unwrap_or_else(|e| e.into_inner());
        for p in &self.ports {
            held.remove(p);
        }
        let _ = remove_all(&self.folder);
    }
}

impl Up {
    /// Starts `services` for the candidate laid out in `dir`, their data and logs in `folder`, all
    /// at once, and waits until each is ready. Err: the step that says which one never was, and why.
    fn start(services: &[&Service], dir: &Path, folder: &Path, env: &[(OsString, OsString)], cancel: &AtomicBool) -> std::result::Result<Up, Step> {
        let began = Instant::now();
        let failed = |s: &Service, exit: i32, output: String| Step {
            name: format!("service {}", s.name),
            command: match &s.kind {
                ServiceKind::Image { image, .. } => image.clone(),
                ServiceKind::Run(run) => run.clone(),
            },
            exit,
            seconds: began.elapsed().as_secs_f64(),
            output,
        };
        let mut up = Up { running: vec![], ports: vec![], folder: folder.to_path_buf(), env: vec![] };
        // What an earlier check in this slot left, cut before its end (the server stopped).
        let _ = remove_all(folder);
        if let Err(e) = fs::create_dir_all(folder) {
            return Err(failed(services[0], 1, format!("the services' folder {}: {e}", folder.display())));
        }
        let wanted: usize = services.iter().map(|s| if let ServiceKind::Image { ports, .. } = &s.kind { ports.len() } else { 1 }).sum();
        let mut free = match free_ports(wanted) {
            Ok(p) => p,
            Err(e) => return Err(failed(services[0], 1, format!("no free port on this machine: {e}"))),
        };
        up.ports = free.clone();
        let tool = container_tool();
        if let Some(tool) = tool.filter(|_| services.iter().any(|s| matches!(s.kind, ServiceKind::Image { .. }))) {
            // Containers of an earlier check in this slot, cut before it could remove them.
            let label = format!("label=avp.folder={}", folder.display());
            if let Ok(out) = Command::new(tool).args(["ps", "-aq", "--filter", &label]).stderr(Stdio::null()).output() {
                for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                    let _ = Command::new(tool).args(["rm", "-f", "-v", id]).stdout(Stdio::null()).stderr(Stdio::null()).status();
                }
            }
        }
        let mut started = Vec::new();
        for s in services {
            let (ports, data): (Vec<(u16, u16)>, Option<PathBuf>) = match &s.kind {
                ServiceKind::Image { ports, .. } => (ports.iter().map(|p| (*p, free.remove(0))).collect(), None),
                ServiceKind::Run(_) => {
                    let port = free.remove(0);
                    (vec![(port, port)], Some(folder.join(&s.name)))
                }
            };
            let own: Vec<(OsString, OsString)> = s.env.iter().map(|(k, v)| (k.into(), fill(v, &ports, data.as_deref()).into())).collect();
            let log = folder.join(format!("{}.log", s.name));
            match &s.kind {
                ServiceKind::Image { image, .. } => {
                    let Some(tool) = tool else { return Err(failed(s, 1, format!("the service {} is a container, and this machine has neither docker nor podman: the steps did not run", s.name))) };
                    let name = format!("avp-{}-{}", s.name, ulid::Ulid::new().to_string().to_lowercase());
                    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone(), "--label".into(), format!("avp.folder={}", folder.display())];
                    for (inner, host) in &ports {
                        args.extend(["-p".into(), format!("127.0.0.1:{host}:{inner}")]);
                    }
                    for (k, v) in &own {
                        args.extend(["-e".into(), format!("{}={}", k.to_string_lossy(), v.to_string_lossy())]);
                    }
                    args.push(image.clone());
                    // Pulls the image the first time: as long as it takes.
                    let out = Command::new(tool).args(&args).stdin(Stdio::null()).output();
                    up.running.push(Running::Container(tool, name));
                    match out {
                        Ok(o) if o.status.success() => {}
                        Ok(o) => return Err(failed(s, 1, format!("the service {} did not start ({tool} run {image}): the steps did not run\n{}", s.name, tail(&String::from_utf8_lossy(&o.stderr))))),
                        Err(e) => return Err(failed(s, 1, format!("the service {} did not start ({tool}: {e}): the steps did not run", s.name))),
                    }
                }
                ServiceKind::Run(run) => {
                    let data = data.as_ref().unwrap();
                    let out = fs::create_dir_all(data).and_then(|_| fs::File::create(&log)).and_then(|f| Ok((f.try_clone()?, f)));
                    let child = out.and_then(|(o, e)| shell(dir, &fill(run, &ports, Some(data)), &[env, own.as_slice()].concat()).stdout(o).stderr(e).spawn());
                    match child {
                        Ok(c) => up.running.push(Running::Process(c)),
                        Err(e) => return Err(failed(s, 1, format!("the service {} did not start: {e}: the steps did not run", s.name))),
                    }
                }
            }
            up.env.extend(own.iter().cloned());
            started.push(Started { service: s, ports, env: own, log });
        }
        // All of them starting side by side, each one waited for in turn, from now: the first pull
        // of an image takes what it takes.
        let launched = Instant::now();
        for (i, st) in started.iter().enumerate() {
            let s = st.service;
            let deadline = launched + Duration::from_secs(s.ready_s);
            let first = st.ports.first().map(|(_, host)| *host);
            let said = match &s.ready {
                Some(Ready::Command(c)) => format!("ready = `{c}`"),
                Some(Ready::Port(p)) => format!("ready = {p}"),
                None => "waiting for its port to answer".to_string(),
            };
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(failed(s, CANCELLED, "cancelled: the task's files changed, a newer tree is being proven".into()));
                }
                let ready = match (&s.ready, &up.running[i]) {
                    (Some(Ready::Command(c)), Running::Container(tool, name)) => succeeds(Command::new(*tool).args(["exec", name.as_str(), "sh", "-c", c.as_str()]), Duration::from_secs(10)),
                    (Some(Ready::Command(c)), Running::Process(_)) => succeeds(&mut shell(dir, &fill(c, &st.ports, Some(&up.folder.join(&s.name))), &[env, st.env.as_slice()].concat()), Duration::from_secs(10)),
                    (Some(Ready::Port(p)), _) => st.ports.iter().find(|(inner, _)| inner == p).is_some_and(|(_, host)| answers(*host)),
                    (None, _) => first.is_none_or(answers),
                };
                if ready {
                    break;
                }
                // Gone before it was ready: no use waiting.
                let gone = match &mut up.running[i] {
                    Running::Process(child) => child.try_wait().ok().flatten().map(|status| (format!("exit {}", status.code().unwrap_or(-1)), fs::read_to_string(&st.log).unwrap_or_default())),
                    Running::Container(tool, name) => {
                        let running = Command::new(*tool).args(["inspect", "-f", "{{.State.Running}}", name.as_str()]).stderr(Stdio::null()).output();
                        running.ok().filter(|o| String::from_utf8_lossy(&o.stdout).trim() != "true").map(|_| ("its container stopped".to_string(), logs(tool, name)))
                    }
                };
                if let Some((how, printed)) = gone {
                    return Err(failed(s, 1, format!("the service {} stopped before it was ready ({how}): the steps did not run\n{}", s.name, tail(&printed))));
                }
                if Instant::now() >= deadline {
                    let printed = match &up.running[i] {
                        Running::Process(_) => fs::read_to_string(&st.log).unwrap_or_default(),
                        Running::Container(tool, name) => logs(tool, name),
                    };
                    return Err(failed(s, 1, format!("the service {} was not ready after {}s ({said}): the steps did not run\n{}", s.name, s.ready_s, tail(&printed))));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        Ok(up)
    }
}

/// What a container printed, stdout and stderr.
fn logs(tool: &str, name: &str) -> String {
    Command::new(tool).args(["logs", "--tail", "30", name]).output().map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))).unwrap_or_default()
}

/// The test files of the tree that name one of the task's non-code files (a stylesheet a test reads
/// as text, a fixture, a migration). Names shorter than five characters are too common to mean it.
pub fn tests_naming(objects: &Objects, tree: &Tree, files: &[String]) -> Vec<String> {
    let names: Vec<&str> = files
        .iter()
        .filter(|f| !is_code(f) && tree.contains_key(f.as_str()))
        .filter_map(|f| Path::new(f).file_name().and_then(|n| n.to_str()))
        .filter(|n| n.len() >= 5)
        .collect();
    if names.is_empty() {
        return vec![];
    }
    let test = Regex::new(r"(^|/)(test_[^/]+\.py|[^/]+[._](test|spec)\.[cm]?[jt]sx?|[^/]+_test\.(go|py)|[^/]+[._]test\.py)$").unwrap();
    tree.iter()
        .filter(|(p, _)| test.is_match(p))
        .filter(|(_, e)| objects.get(&e.blob).map(|b| { let text = String::from_utf8_lossy(&b); names.iter().any(|n| text.contains(n)) }).unwrap_or(false))
        .map(|(p, _)| p.clone())
        .collect()
}

pub fn is_code(path: &str) -> bool {
    Regex::new(r"\.(tsx?|jsx?|mjs|cjs|mts|cts|vue|svelte|py|rs|go)$").unwrap().is_match(path)
}

/// Files a failing output names, skipping a runner's "passed" lines.
pub fn failed_files(output: &str, known: &Tree) -> Vec<String> {
    // Colors first: a runner that colors its lines ("\x1b[32m✓\x1b[39m test/a.test.ts") hid the mark
    // of a file that passed, and every file it ran was blamed (25/10 bench: the task was told to
    // wait for whoever held the passing ones).
    let colors = Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap();
    let output = colors.replace_all(output, "");
    let passed = Regex::new(r"^\s*(?:[✓✔√]|PASS\b|ok \d)").unwrap();
    let file = Regex::new(r"(?:^|[\s(])((?:\./)?[A-Za-z0-9_@.-][A-Za-z0-9_@./-]*\.(?:tsx?|jsx?|mjs|cjs|mts|cts|vue|svelte|py|rs|go))(?:[(:\s]|$)").unwrap();
    let mut out = BTreeSet::new();
    for line in output.lines() {
        if passed.is_match(line) {
            continue;
        }
        for c in file.captures_iter(line) {
            let p = c[1].trim_start_matches("./").to_string();
            if known.contains_key(&p) {
                out.insert(p);
            }
        }
    }
    out.into_iter().collect()
}

/// The lines of a red output that name a failing test (a runner's FAIL, vitest's `×`, `✗`), each
/// once, in their order: what a flaky shard failed on. Colors go, and so does a duration at the end
/// (`12ms`, `(12 ms)`), which changes from one run to the next: the same test reads the same in
/// every flake.
pub fn failing_tests(output: &str) -> Vec<String> {
    let colors = Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap();
    let duration = Regex::new(r"\s+\(?\d+(?:\.\d+)?\s?ms\)?$").unwrap();
    // A count of none (`git-bridge: 19 PASS, 0 FAIL`) names no failure: 27 such lines read as
    // flaky tests in AVP Live (28/10).
    let none = Regex::new(r"\b0\s+(?:FAIL\w*|failed|failures?)\b").unwrap();
    let mut out: Vec<String> = Vec::new();
    for line in colors.replace_all(output, "").lines() {
        let said = none.replace_all(line, "");
        if !(said.contains("FAIL") || said.contains('×') || said.contains('✗')) {
            continue;
        }
        let line: String = duration.replace(line.trim(), "").chars().take(300).collect();
        if !out.contains(&line) {
            out.push(line);
        }
        if out.len() == 50 {
            break;
        }
    }
    out
}

pub struct Workspace {
    pub work: PathBuf,
    pub cache: PathBuf,
    pub kept: Kept,
    secrets: Given,
}

/// Where the values of the secrets a proof's steps name come from.
enum Given {
    /// The server's store (AVP Cloud's, see `seam::secret_values`), in this data folder, for this repo.
    Store(PathBuf, String),
    /// What the job a worker claimed carries: a worker keeps none.
    Sent(BTreeMap<String, String>),
}

/// The steps a check runs, each with the secrets it gets, `[check] secrets` and its own (no more
/// than the line's, for a task that changes avp.toml): the command of a shard of the whole suite
/// (its part names its secrets), one part of a task's proof, or every step.
fn steps_of(cfg: &CheckConfig, steps_override: Option<&str>, part: Option<&Part>) -> Vec<ConfiguredStep> {
    let mut steps: Vec<ConfiguredStep> = match (steps_override, part) {
        (Some(cmd), p) => vec![ConfiguredStep { name: "full".into(), run: cmd.to_string(), needs_files: false, split: None, secrets: p.map(|p| p.secrets.clone()).unwrap_or_default(), artifacts: p.map(|p| p.artifacts.clone()).unwrap_or_default() }],
        (None, Some(Part { step: Some(only), .. })) => cfg.steps.iter().filter(|s| &s.name == only).cloned().collect(),
        (None, Some(p)) => cfg.steps.iter().filter(|s| !p.skip.contains(&s.name)).cloned().collect(),
        (None, None) => cfg.steps.clone(),
    };
    let line = part.and_then(|p| p.line_secrets.as_ref());
    for s in &mut steps {
        let own = std::mem::take(&mut s.secrets);
        for name in cfg.secrets.iter().chain(&own) {
            if !s.secrets.contains(name) && line.is_none_or(|l| l.contains(name)) {
                s.secrets.push(name.clone());
            }
        }
    }
    steps
}

/// The secrets the steps of a check name: what the server sends with the job a worker claims.
pub fn secrets_named(cfg: &CheckConfig, steps_override: Option<&str>, part: Option<&Part>) -> BTreeSet<String> {
    steps_of(cfg, steps_override, part).into_iter().flat_map(|s| s.secrets).collect()
}

impl Workspace {
    pub fn new(data: &Path, repo: &str) -> Self {
        Self { work: data.join("work").join(repo), cache: data.join("cache").join(repo), kept: Kept::new(data), secrets: Given::Store(data.to_path_buf(), repo.to_string()) }
    }

    /// On a worker: the secrets come with the job, never from this machine.
    pub fn with_secrets(mut self, sent: BTreeMap<String, String>) -> Self {
        self.secrets = Given::Sent(sent);
        self
    }

    /// Keeps a step's log, and what its `artifacts` name, green or red: a failure to keep them is the
    /// server's to log, never a red step.
    fn keep(&self, step: &Step, log: Log, dir: &Path, artifacts: &[String], budget: &mut u64) {
        let key = kept_key(step);
        let out = self.kept.dir.join(format!("{key}.tar.gz"));
        let packed = (!artifacts.is_empty()).then(|| fs::create_dir_all(&self.kept.dir).map(|_| pack(dir, artifacts, budget, &out)));
        let artifacts = match packed {
            Some(Ok(a)) => Some(a),
            Some(Err(e)) => Some(Artifacts { error: Some(e.to_string()), ..Default::default() }),
            None => None,
        };
        let from = artifacts.as_ref().filter(|a| a.packed > 0).map(|_| out.as_path());
        if let Err(e) = self.kept.put(&key, &KeptStep { log: Some(log), artifacts, pack: None }, from) {
            eprintln!("{}: log not kept: {e:#}", step.name);
        }
    }

    /// Runs the setup once per fingerprint and returns the dir holding its products.
    fn setup(&self, objects: &Objects, tree: &Tree, cfg: &CheckConfig) -> Result<Option<PathBuf>> {
        let Some(setup) = &cfg.setup else { return Ok(None) };
        let _one_at_a_time = SETUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fp = fingerprint(tree, &cfg.setup_keys);
        let dir = self.cache.join("setup");
        let stamp = dir.join(".avp-setup");
        if fs::read_to_string(&stamp).ok().as_deref() == Some(fp.as_str()) {
            return Ok(Some(dir));
        }
        let mut subset = Tree::new();
        for k in &cfg.setup_keys {
            if let Some(e) = tree.get(k) {
                subset.insert(k.clone(), e.clone());
            }
        }
        // Workspace manifests (pnpm) need every package.json: include them all, they are small.
        for (p, e) in tree {
            if p.ends_with("package.json") || p.ends_with("pnpm-workspace.yaml") || p.ends_with(".npmrc") {
                subset.insert(p.clone(), e.clone());
            }
        }
        materialize(objects, &subset, &dir, &cfg.setup_links)?;
        // Shared by every candidate of the repo: never stopped halfway.
        let step = run(&dir, setup, &AtomicBool::new(false));
        anyhow::ensure!(step.exit == 0, "setup failed ({}):\n{}", step.exit, step.output);
        fs::write(&stamp, fp)?;
        Ok(Some(dir))
    }

    /// Lays the candidate out and runs the steps. `name` (the task's id, or a shard of the whole
    /// suite) is what a newer tree stops; `files` are the task's paths (for `{files}`).
    pub fn check(&self, objects: &Objects, tree: &Tree, tree_hash: &str, name: &str, files: &[String], steps_override: Option<&str>, part: Option<&Part>) -> Result<(Vec<Step>, PathBuf)> {
        // Registered before waiting for a slot: a newer tree of the same task can stop it there too.
        let cancel = Arc::new(AtomicBool::new(false));
        IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).push((name.to_string(), tree_hash.to_string(), cancel.clone()));
        let out = self.check_steps(objects, tree, name, files, steps_override, part, &cancel);
        IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).retain(|(_, _, f)| !Arc::ptr_eq(f, &cancel));
        out
    }

    fn check_steps(&self, objects: &Objects, tree: &Tree, name: &str, files: &[String], steps_override: Option<&str>, part: Option<&Part>, cancel: &AtomicBool) -> Result<(Vec<Step>, PathBuf)> {
        let slot = Slot::take(name);
        let cfg = read_config(tree, objects)?.check;
        let setup_dir = self.setup(objects, tree, &cfg)?;
        // One folder per slot, reused: the disk holds as many candidates as run at once. Folders
        // of the AVP that named them after their task go.
        let dir = self.work.join(format!("slot-{}", slot.0));
        for old in fs::read_dir(&self.work).into_iter().flatten().flatten() {
            if !old.file_name().to_string_lossy().starts_with("slot-") {
                let _ = remove_all(&old.path());
            }
        }
        // The setup's links stay from one candidate to the next only while they point into the
        // same setup: a dependency it gained since must be there.
        let linked = dir.join(".avp-linked");
        let setup_fp = setup_dir.as_ref().and_then(|s| fs::read_to_string(s.join(".avp-setup")).ok()).unwrap_or_default();
        let same_setup = fs::read_to_string(&linked).ok().as_deref() == Some(setup_fp.as_str());
        let mut keep = cfg.keep.clone();
        if same_setup {
            keep.extend(cfg.setup_links.iter().cloned());
        }
        materialize(objects, tree, &dir, &keep)?;
        if let Some(setup_dir) = &setup_dir {
            for link in &cfg.setup_links {
                if link == "node_modules" {
                    // Root and every workspace package: entry by entry, so a link to a workspace
                    // package points at the candidate's own sources, not the setup skeleton's.
                    let mut pkgs: Vec<PathBuf> = vec![PathBuf::new()];
                    for (p, _) in tree.iter().filter(|(p, _)| p.ends_with("/package.json")) {
                        pkgs.push(Path::new(p).parent().unwrap().to_path_buf());
                    }
                    for pkg in pkgs {
                        link_node_modules(setup_dir, &dir, &pkg)?;
                    }
                    continue;
                }
                let src = setup_dir.join(link);
                let dst = dir.join(link);
                if src.exists() && !dst.exists() {
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&src, &dst)?;
                }
            }
        }
        fs::write(&linked, &setup_fp)?;
        // pnpm 11 reinstalls `node_modules` before `pnpm <command>` when the directory looks foreign
        // (it is a link into the setup cache). Only an `.npmrc` beside the command turns that off.
        let npmrc = dir.join(".npmrc");
        let mut text = tree.get(".npmrc").and_then(|e| objects.get(&e.blob).ok()).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
        if !text.contains("verify-deps-before-run") {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("verify-deps-before-run=false\n");
            let _ = fs::remove_file(&npmrc);
            fs::write(&npmrc, text)?;
        }
        // pnpm 11 reads its settings from pnpm-workspace.yaml, and only there.
        if tree.contains_key("pnpm-lock.yaml") {
            let ws = dir.join("pnpm-workspace.yaml");
            let mut text = tree.get("pnpm-workspace.yaml").and_then(|e| objects.get(&e.blob).ok()).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
            if !text.contains("verifyDepsBeforeRun") {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("verifyDepsBeforeRun: false\n");
                let _ = fs::remove_file(&ws);
                fs::write(&ws, text)?;
            }
        }
        let mut code: Vec<&str> = files.iter().map(String::as_str).filter(|f| is_code(f) && tree.contains_key(*f)).collect();
        // A test that reads a touched file by its name (a stylesheet, a fixture, a migration) is
        // related to it though no import says so: it goes to the steps that take {files} too.
        let named = tests_naming(objects, tree, files);
        for t in &named {
            if !code.contains(&t.as_str()) {
                code.push(t.as_str());
            }
        }
        // Every file the task leaves in the tree, code or not: a security scan must see the
        // Dockerfiles, workflows and charts a typecheck has no use for.
        let all: Vec<&str> = files.iter().map(String::as_str).filter(|f| tree.contains_key(*f)).collect();
        let mut out = Vec::new();
        let steps = steps_of(&cfg, steps_override, part);
        let named: BTreeSet<String> = steps.iter().flat_map(|s| s.secrets.iter().cloned()).collect();
        let values = match &self.secrets {
            Given::Store(data, repo) => crate::seam::secret_values(data, repo, &named)?,
            Given::Sent(sent) => sent.iter().filter(|(n, _)| named.contains(*n)).map(|(n, v)| (n.clone(), v.clone())).collect(),
        };
        let shard = part.and_then(|p| p.shard.clone()).unwrap_or_else(|| "1/1".into());
        let mut env: Vec<(OsString, OsString)> = part.iter().flat_map(|p| &p.env).map(|(k, v)| (k.into(), v.into())).collect();
        let mut todo = Vec::new();
        for s in steps {
            let wanted = if s.run.contains("{all_files}") { &all } else { &code };
            if s.needs_files && wanted.is_empty() {
                continue;
            }
            let cmd = s.run.replace("{files}", &code.join(" ")).replace("{all_files}", &all.join(" ")).replace("{shard}", &shard);
            todo.push((s.name, cmd, s.secrets, s.artifacts));
        }
        // The services, once there is a step to run: a shard of the whole suite says which it
        // needs, every other check gets them all. Stopped at the end of this function, whatever ends it.
        let services: Vec<&Service> = cfg.services.iter().filter(|s| part.and_then(|p| p.services.as_ref()).is_none_or(|names| names.contains(&s.name))).collect();
        let _up = if todo.is_empty() || services.is_empty() {
            None
        } else {
            match Up::start(&services, &dir, &self.work.join(format!("slot-{}.services", slot.0)), &env, cancel) {
                Ok(up) => {
                    env.extend(up.env.iter().cloned());
                    Some(up)
                }
                Err(step) => return Ok((vec![step], dir)),
            }
        };
        let mut budget = ARTIFACTS_MAX;
        for (name, cmd, secrets, artifacts) in todo {
            if cancel.load(Ordering::Relaxed) {
                out.push(Step { name, command: cmd, exit: CANCELLED, seconds: 0.0, output: "cancelled: the task's files changed, a newer tree is being proven".into() });
                break;
            }
            // A step without a secret it names would fail far from the cause, or skip the tests
            // that need it and pass: it says what is missing instead.
            if let Some(n) = secrets.iter().find(|n| !values.contains_key(*n)) {
                let output = match refused_secret(n) {
                    Some(why) => format!("{n} never reaches a step ({why}): take it out of avp.toml's secrets"),
                    None => format!("the secret {n} this step names is not set for this repo: a person sets it with `avp secret set {n}` (the value on stdin), `avp secret list` shows those set"),
                };
                out.push(Step { name, command: cmd, exit: 1, seconds: 0.0, output });
                break;
            }
            let given: Vec<(OsString, OsString)> = secrets.iter().map(|n| (OsString::from(n), OsString::from(&values[n]))).collect();
            let (mut step, log) = run_with(&dir, &cmd, cancel, &env, &given, None);
            step.name = name;
            if step.exit != CANCELLED {
                self.keep(&step, log, &dir, &artifacts, &mut budget);
            }
            let failed = step.exit != 0;
            out.push(step);
            if failed {
                break;
            }
        }
        Ok((out, dir))
    }
}

/// Repository-relative files `source` imports, resolved the way TypeScript and Node do for the
/// common cases. Cheap and deterministic; a package name resolves to nothing.
pub fn imports_of(file: &str, source: &str, exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    if !is_code(file) {
        return vec![];
    }
    let specs = [
        r#"\bimport\s+(?:type\s+)?(?:[\w*{}\s,$]+\s+from\s+)?["']([^"']+)["']"#,
        r#"\bexport\s+(?:type\s+)?[\w*{}\s,$]*\s+from\s+["']([^"']+)["']"#,
        r#"\brequire\(\s*["']([^"']+)["']\s*\)"#,
        r#"\bimport\(\s*["']([^"']+)["']\s*\)"#,
    ];
    let exts = [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];
    let dir = Path::new(file).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let mut out = BTreeSet::new();
    for re in specs {
        for c in Regex::new(re).unwrap().captures_iter(source) {
            let spec = &c[1];
            if !(spec.starts_with("./") || spec.starts_with("../")) {
                continue;
            }
            let base = normalize(&format!("{}/{}", dir, spec));
            let stem = Regex::new(r"\.(?:js|jsx|mjs|cjs)$").unwrap().replace(&base, "").to_string();
            let mut candidates = vec![base.clone()];
            for e in exts {
                candidates.push(format!("{stem}{e}"));
            }
            for e in exts {
                candidates.push(format!("{base}/index{e}"));
            }
            if let Some(hit) = candidates.into_iter().find(|c| exists(c)) {
                out.insert(hit);
            }
        }
    }
    out.into_iter().collect()
}

fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// How many test files reach one of `files` through their imports, at any depth, plus the tests
/// that name one of them: what a step's `split` divides. An estimate (an alias or a package name
/// resolves to nothing), used to size the jobs, never to choose the tests.
pub fn related_tests(objects: &Objects, tree: &Tree, files: &[String]) -> usize {
    let test = Regex::new(r"(^|/)(test_[^/]+\.py|[^/]+[._](test|spec)\.[cm]?[jt]sx?|[^/]+_test\.(go|py)|[^/]+[._]test\.py)$").unwrap();
    let exists = |p: &str| tree.contains_key(p);
    // Who imports whom, read once per blob: the same files come back in every proof.
    static IMPORTS: Mutex<Option<HashMap<(String, Hash), Vec<String>>>> = Mutex::new(None);
    let mut importers: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut cache = IMPORTS.lock().unwrap_or_else(|e| e.into_inner());
        let cache = cache.get_or_insert_with(HashMap::new);
        if cache.len() > 200_000 {
            cache.clear();
        }
        for (path, entry) in tree.iter().filter(|(p, _)| is_code(p)) {
            let imports = cache
                .entry((path.clone(), entry.blob.clone()))
                .or_insert_with(|| objects.get(&entry.blob).map(|b| imports_of(path, &String::from_utf8_lossy(&b), &exists)).unwrap_or_default());
            for i in imports.iter() {
                importers.entry(i.clone()).or_default().push(path.clone());
            }
        }
    }
    let mut seen: BTreeSet<String> = files.iter().filter(|f| is_code(f)).cloned().collect();
    let mut todo: Vec<String> = seen.iter().cloned().collect();
    while let Some(f) = todo.pop() {
        for i in importers.get(&f).into_iter().flatten() {
            if seen.insert(i.clone()) {
                todo.push(i.clone());
            }
        }
    }
    seen.extend(tests_naming(objects, tree, files));
    seen.iter().filter(|p| test.is_match(p)).count()
}

/// Which files among `candidates` touch `targets` by import, either way, one hop.
pub fn import_neighbors(objects: &Objects, tree: &Tree, targets: &BTreeSet<String>, candidates: &BTreeSet<String>) -> BTreeSet<String> {
    let exists = |p: &str| tree.contains_key(p);
    let mut cache: HashMap<String, Vec<String>> = HashMap::new();
    let mut imports = |p: &str| -> Vec<String> {
        if let Some(v) = cache.get(p) {
            return v.clone();
        }
        let v = tree.get(p).and_then(|e| objects.get(&e.blob).ok()).map(|b| imports_of(p, &String::from_utf8_lossy(&b), &exists)).unwrap_or_default();
        cache.insert(p.to_string(), v.clone());
        v
    };
    let target_imports: BTreeSet<String> = targets.iter().flat_map(|t| imports(t)).collect();
    let mut out = BTreeSet::new();
    for c in candidates {
        if target_imports.contains(c) || imports(c).iter().any(|i| targets.contains(i)) {
            out.insert(c.clone());
        }
    }
    out
}


/// Links `setup/<pkg>/node_modules/*` into `candidate/<pkg>/node_modules/*`. An entry that is a
/// symlink to a workspace package (a path under the setup dir but outside any `node_modules`)
/// is recreated to point at the same path under the candidate. Scoped dirs (`@x`) go one level
/// deeper. Everything else links to the setup's entry.
fn link_node_modules(setup: &Path, candidate: &Path, pkg: &Path) -> Result<()> {
    let src_root = setup.join(pkg).join("node_modules");
    if !src_root.is_dir() {
        return Ok(());
    }
    let dst_root = candidate.join(pkg).join("node_modules");
    if dst_root.exists() {
        return Ok(());
    }
    fs::create_dir_all(&dst_root)?;
    let setup_real = fs::canonicalize(setup)?;
    let mut stack = vec![(src_root.clone(), dst_root.clone())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for entry in fs::read_dir(&src_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let src = entry.path();
            let dst = dst_dir.join(&name);
            let is_scope = name.to_string_lossy().starts_with('@') && src.is_dir() && !entry.file_type()?.is_symlink();
            if is_scope {
                fs::create_dir_all(&dst)?;
                stack.push((src, dst));
                continue;
            }
            if entry.file_type()?.is_symlink() {
                if let Ok(real) = fs::canonicalize(&src) {
                    if let Ok(rel) = real.strip_prefix(&setup_real) {
                        let inside_modules = rel.components().any(|c| c.as_os_str() == "node_modules");
                        if !inside_modules {
                            #[cfg(unix)]
                            std::os::unix::fs::symlink(candidate.join(rel), &dst)?;
                            continue;
                        }
                    }
                }
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&src, &dst)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The count `set_check_slots` gives holds whatever AVP_CHECK_SLOTS says, and raising it lets a
    /// check that waits for a slot go at once. No other test of this binary takes a slot.
    #[test]
    fn the_slots_set_hold_over_the_environment() {
        set_check_slots(1);
        let first = Slot::take("first");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(Slot::take("second").0).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "one slot, and it is taken");
        set_check_slots(2);
        assert_eq!(rx.recv_timeout(Duration::from_secs(60)).unwrap(), 1, "the second slot");
        drop(first);
    }

    /// A service is a container or a command, ready on a command or a port, said plainly when it is
    /// neither, both, or ready on a port it does not have.
    #[test]
    fn a_service_is_a_container_or_a_command() {
        let parse = |toml: &str| toml::from_str::<AvpConfig>(&format!("[[check.services]]\n{toml}")).map(|c| c.check.services.into_iter().next().unwrap()).map_err(|e| e.to_string());
        let db = parse("name = \"db\"\nimage = \"postgres:16\"\nports = [\"5432:5432\", 6432]\nready = 5432\nenv = { POSTGRES_PASSWORD = \"avp\", PGPORT = 5432 }").unwrap();
        assert_eq!(db.kind, ServiceKind::Image { image: "postgres:16".into(), ports: vec![5432, 6432] });
        assert_eq!((db.ready, db.ready_s), (Some(Ready::Port(5432)), 60));
        assert_eq!(db.env, [("PGPORT".to_string(), "5432".to_string()), ("POSTGRES_PASSWORD".into(), "avp".into())]);
        let cache = parse("name = \"cache\"\nrun = \"redis-server --port {port} --dir {data}\"\nready = \"redis-cli -p {port} ping\"\nready_s = 5").unwrap();
        assert_eq!((cache.kind, cache.ready, cache.ready_s), (ServiceKind::Run("redis-server --port {port} --dir {data}".into()), Some(Ready::Command("redis-cli -p {port} ping".into())), 5));
        for (toml, said) in [
            ("name = \"db\"", "needs `image`"),
            ("name = \"db\"\nimage = \"p\"\nrun = \"r\"", "not both"),
            ("name = \"db\"\nrun = \"r\"\nports = [5432]", "`ports` is for an image"),
            ("name = \"db\"\nrun = \"r\"\nready = 5432", "leave `ready` out"),
            ("name = \"db\"\nimage = \"p\"\nports = [5432]\nready = 6379", "none of its ports"),
            ("name = \"my db\"\nimage = \"p\"", "letters, digits"),
        ] {
            let e = parse(toml).unwrap_err();
            assert!(e.contains(said), "{toml}: {e}");
        }
    }

    /// `[git]`: off unless it names a branch to follow or pull requests to take, and `pull_requests`
    /// is a yes, a no or "land", said plainly when it is something else.
    #[test]
    fn git_says_what_the_server_takes_in() {
        let git = |toml: &str| toml::from_str::<AvpConfig>(toml).map(|c| c.git).map_err(|e| e.to_string());
        let off = git("").unwrap();
        assert_eq!((off.follow, off.pull_requests), (None, PullRequests::Off));
        let on = git("[git]\nremote = \"https://github.com/acme/shop\"\nfollow = \"main\"\npull_requests = true\nevery = 30").unwrap();
        assert_eq!((on.remote.as_deref(), on.follow.as_deref(), on.pull_requests, on.every), (Some("https://github.com/acme/shop"), Some("main"), PullRequests::Tasks, Some(30)));
        assert_eq!(git("[git]\npull_requests = \"land\"").unwrap().pull_requests, PullRequests::Land);
        assert_eq!(git("[git]\npull_requests = false").unwrap().pull_requests, PullRequests::Off);
        assert!(git("[git]\npull_requests = \"merge\"").unwrap_err().contains("true, false or \"land\""));
    }

    /// A shard with a matrix runs once per combination, named after its values, which its steps
    /// read as MATRIX_<KEY>; `full` is the one shard when there are none.
    #[test]
    fn a_matrix_is_one_shard_per_combination() {
        let cfg: AvpConfig = toml::from_str("[[check.services]]\nname = \"db\"\nrun = \"r\"\n[[check.full_shards]]\nname = \"web\"\nrun = \"npm test\"\nservices = [\"db\"]\nmatrix = { node = [20, \"22\"], os-name = [\"linux\"] }\n[[check.full_shards]]\nname = \"lint\"\nrun = \"npm run lint\"\nservices = []\n").unwrap();
        cfg.check.validate().unwrap();
        let shards = cfg.check.shards();
        let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        assert_eq!(shards.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["web (node 20, os-name linux)", "web (node 22, os-name linux)", "lint"]);
        assert_eq!(shards[1].env, env(&[("MATRIX_NODE", "22"), ("MATRIX_OS_NAME", "linux")]));
        assert_eq!((shards[1].services.clone(), shards[2].services.clone(), shards[2].env.clone()), (Some(vec!["db".to_string()]), Some(vec![]), vec![]));
        let full: AvpConfig = toml::from_str("[check]\nfull = \"make test\"\n").unwrap();
        assert_eq!(full.check.shards(), [Shard { name: "full".into(), run: "make test".into(), env: vec![], services: None, secrets: vec![], artifacts: vec![] }]);
        // What a field alone cannot say.
        let invalid = |toml: &str| toml::from_str::<AvpConfig>(toml).map_err(|e| e.to_string()).and_then(|c| c.check.validate().map_err(|e| e.to_string())).unwrap_err();
        assert!(invalid("[[check.full_shards]]\nname = \"web\"\nrun = \"t\"\nmatrix = { node = [] }").contains("matrix.node has no value"));
        assert!(invalid("[[check.full_shards]]\nname = \"web\"\nrun = \"t\"\nservices = [\"db\"]").contains("needs the service \"db\""));
        assert!(invalid("[[check.services]]\nname = \"db\"\nrun = \"a\"\n[[check.services]]\nname = \"db\"\nrun = \"b\"").contains("two services"));
    }

    /// A port answers when something accepts and waits, or greets: not when nothing listens, nor when
    /// what accepts hangs up at once, as docker's proxy does before the service inside listens.
    #[test]
    fn a_port_answers_once_what_accepts_stays() {
        let serve = |greet: Option<&'static [u8]>| {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    match greet {
                        Some(g) => std::thread::spawn(move || {
                            let mut s = s;
                            let _ = std::io::Write::write_all(&mut s, g);
                            std::thread::sleep(Duration::from_secs(2));
                        }),
                        None => continue,
                    };
                }
            });
            port
        };
        assert!(answers(serve(Some(b""))), "accepts and waits for the client, as postgres does");
        assert!(answers(serve(Some(b"220 ready\r\n"))), "greets first, as mysql does");
        assert!(!answers(serve(None)), "accepts and hangs up");
        assert!(!answers(free_ports(1).unwrap()[0]), "nothing listens");
        assert_eq!(fill("{port} {port:6432} {data}", &[(5432, 40001), (6432, 40002)], Some(Path::new("/d"))), "40001 40002 /d");
    }

    /// A candidate's file is laid out dated now, not when its object was stored: older, cargo would
    /// take it for the one an earlier candidate built in the slot's `target/`.
    #[test]
    fn a_candidate_is_laid_out_dated_now() {
        let root = std::env::temp_dir().join(format!("avp-test-dated-{}", ulid::Ulid::new()));
        let objects = Objects::open(&root.join("objects")).unwrap();
        let blob = objects.put(b"fn main() {}\n").unwrap();
        fs::File::open(objects.path_of(&blob)).unwrap().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000)).unwrap();
        let tree: Tree = [("src/main.rs".to_string(), Entry { blob, mode: 0o100644 })].into_iter().collect();
        materialize(&objects, &tree, &root.join("slot"), &[]).unwrap();
        let age = fs::metadata(root.join("slot/src/main.rs")).unwrap().modified().unwrap().elapsed().unwrap_or_default();
        fs::remove_dir_all(&root).unwrap();
        assert!(age < Duration::from_secs(60), "laid out dated {age:?} ago");
    }

    #[test]
    fn failed_files_skips_colored_passes() {
        let known: Tree = ["test/a.test.ts", "test/b.test.ts", "src/b.ts"].iter().map(|p| (p.to_string(), Entry { blob: String::new(), mode: 0o100644 })).collect();
        let out = "\x1b[32m✓\x1b[39m test/a.test.ts \x1b[2m(3 tests)\x1b[22m\n \x1b[31m❯\x1b[39m test/b.test.ts \x1b[2m(2 tests | \x1b[22m\x1b[31m1 failed\x1b[39m\x1b[2m)\x1b[22m\n \x1b[31mFAIL\x1b[39m test/b.test.ts > b > sums\n \x1b[36m❯\x1b[39m src/b.ts:4:10\n";
        assert_eq!(failed_files(out, &known), vec!["src/b.ts".to_string(), "test/b.test.ts".to_string()]);
    }

    /// What vitest, cargo and a `✗` runner print for a failing test, colored or not, once each and
    /// without the duration that changes every run; what passed, and the summary, stay out.
    #[test]
    fn failing_tests_are_the_red_lines_of_an_output() {
        let vitest = " \x1b[32m✓\x1b[39m apps/web/src/a.test.ts (3 tests) 40ms\n \x1b[31m❯\x1b[39m apps/daemon/src/codexActivity.test.ts (4 tests | 1 failed) 812ms\n   \x1b[31m×\x1b[39m codexActivity > reads the log 503ms\n\n Test Files  1 failed | 1 passed (2)\n \x1b[31mFAIL\x1b[39m  apps/daemon/src/codexActivity.test.ts > codexActivity > reads the log\nAssertionError: expected 2 to be 3\n \x1b[31mFAIL\x1b[39m  apps/daemon/src/codexActivity.test.ts > codexActivity > reads the log\n";
        assert_eq!(failing_tests(vitest), ["× codexActivity > reads the log", "FAIL  apps/daemon/src/codexActivity.test.ts > codexActivity > reads the log"]);
        let cargo = "test engine::lands ... ok\ntest engine::follows ... FAILED\n\nfailures:\n    engine::follows\n\ntest result: FAILED. 1 passed; 1 failed\n";
        assert_eq!(failing_tests(cargo), ["test engine::follows ... FAILED", "test result: FAILED. 1 passed; 1 failed"]);
        assert_eq!(failing_tests("  ✓ renders\n  ✗ opens the menu (35 ms)\n"), ["✗ opens the menu"]);
        assert!(failing_tests("Error: EACCES: permission denied, mkdir '/root/.cache'\n").is_empty());
        assert_eq!(failing_tests("git-bridge: 19 PASS, 0 FAIL\nmigrate: 2 PASS, 15 FAIL\ntest result: ok. 3 passed; 0 failed\n"), ["migrate: 2 PASS, 15 FAIL"]);
    }

    #[test]
    fn a_step_sees_what_a_build_needs_and_what_is_listed_never_a_secret() {
        let server = ["PATH=/bin", "HOME=/h", "LC_CTYPE=C", "npm_config_store_dir=/s", "AVP_TOKEN=s3cret", "AVP_CHECK_SLOTS=1", "AVP_WORKERS=1", "AWS_SECRET_ACCESS_KEY=s3cret", "GH_TOKEN=s3cret", "GITHUB_TOKEN=s3cret", "ANTHROPIC_API_KEY=s3cret", "OPENAI_API_KEY=s3cret", "CLAUDE_CODE_OAUTH_TOKEN=s3cret", "TRAME_TOKEN=s3cret", "DATABASE_URL=s3cret", "NODE_OPTIONS=--x"];
        let vars = || server.iter().map(|kv| kv.split_once('=').unwrap()).map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let names = |listed: &str| step_env(vars(), listed, "").into_iter().map(|(k, _)| k.into_string().unwrap()).collect::<Vec<_>>();
        assert_eq!(names(""), ["PATH", "HOME", "LC_CTYPE", "npm_config_store_dir"]);
        // Listed, a setting reaches the step; a secret never does.
        let listed = " NODE_OPTIONS, AVP_CHECK_SLOTS,AVP_TOKEN,TRAME_TOKEN,AWS_SECRET_ACCESS_KEY,GH_TOKEN,GITHUB_TOKEN,ANTHROPIC_API_KEY,OPENAI_API_KEY,CLAUDE_CODE_OAUTH_TOKEN,";
        assert_eq!(names(listed), ["PATH", "HOME", "LC_CTYPE", "npm_config_store_dir", "AVP_CHECK_SLOTS", "NODE_OPTIONS"]);
        // What AVP_DEPLOY_ENV lists is the deploy's, even when AVP_PROOF_ENV lists it too; what a
        // build needs stays.
        let step = step_env(vars(), "NODE_OPTIONS,DATABASE_URL", " DATABASE_URL,PATH").into_iter().map(|(k, _)| k.into_string().unwrap()).collect::<Vec<_>>();
        assert_eq!(step, ["PATH", "HOME", "LC_CTYPE", "npm_config_store_dir", "NODE_OPTIONS"]);
    }

    #[test]
    fn a_deploy_gets_what_is_listed_for_it_never_avps_token() {
        let server = ["PATH=/bin", "AVP_TOKEN=s3cret", "TRAME_TOKEN=s3cret", "avp_token=s3cret", "AWS_PROFILE=deployer", "GH_TOKEN=gh-s3cret", "REGISTRY_TOKEN=reg-s3cret", "UNLISTED=private"];
        let vars = || server.iter().map(|kv| kv.split_once('=').unwrap()).map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let names = |listed: &str| deploy_env(vars(), listed).into_iter().map(|(k, _)| k.into_string().unwrap()).collect::<Vec<_>>();
        assert!(names("").is_empty());
        assert_eq!(names("AVP_TOKEN, TRAME_TOKEN,avp_token,AWS_PROFILE,GH_TOKEN,REGISTRY_TOKEN"), ["AWS_PROFILE", "GH_TOKEN", "REGISTRY_TOKEN"]);
    }

    #[test]
    fn a_deploys_values_are_masked_when_longer_than_six_characters() {
        let given = |kvs: &[(&str, &str)]| kvs.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))).collect::<Vec<_>>();
        let hidden = hidden(&given(&[("REGION", "eu-w1"), ("TOKEN", "abcdefg"), ("LONGER", "abcdefghij"), ("KEY", "-----BEGIN KEY-----\nline-one-of-it\n-----END KEY-----\n")]));
        assert_eq!(mask("eu-w1 abcdefghij abcdefg abcdef", &hidden), "eu-w1 *** *** abcdef");
        // A key, whole in the output kept, line by line in what a deploy says as it runs.
        assert_eq!(mask("key: -----BEGIN KEY-----\nline-one-of-it\n-----END KEY-----\n.", &hidden), "key: ***.");
        assert_eq!(mask("line-one-of-it", &hidden), "***");
    }

    /// A deploy's credential reaches its command, and what it prints of it is masked, as it runs and
    /// in the output kept, even when the output is too long to keep whole.
    #[test]
    fn a_deploy_sees_its_credentials_and_its_output_masks_them() {
        let dir = std::env::temp_dir().join(format!("avp-test-deploy-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        let given = vec![(OsString::from("REGISTRY_TOKEN"), OsString::from("reg-s3cret-value"))];
        let mut told = vec![];
        let mut watch = |line: &str| told.push(line.to_string());
        let step = run_with(&dir, "echo \"pushing with $REGISTRY_TOKEN\"; seq 1 20000; echo done $REGISTRY_TOKEN", &AtomicBool::new(false), &[], &given, Some(&mut watch)).0;
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(step.exit, 0, "{}", step.output);
        assert!(step.output.starts_with("pushing with ***\n") && step.output.ends_with("done ***\n"), "{}", step.output);
        assert!(!step.output.contains("reg-s3cret"));
        assert_eq!(told.first().map(String::as_str), Some("pushing with ***"));
        assert!(told.iter().any(|l| l == "done ***") && !told.iter().any(|l| l.contains("reg-s3cret")));
    }

    /// Every step gets `[check] secrets` and the ones it names itself; a shard of the whole suite,
    /// those it names.
    #[test]
    fn a_step_gets_the_checks_secrets_and_its_own() {
        let cfg: CheckConfig = toml::from_str::<AvpConfig>(
            r#"
[check]
secrets = ["SHARED_KEY"]
full = "all"
[[check.steps]]
name = "unit"
run = "u"
[[check.steps]]
name = "integration"
run = "i"
secrets = ["STRIPE_KEY", "SHARED_KEY"]
[[check.full_shards]]
name = "e2e"
run = "e2e"
secrets = ["BROWSER_TOKEN"]
"#,
        )
        .unwrap()
        .check;
        let steps: Vec<(String, Vec<String>)> = steps_of(&cfg, None, None).into_iter().map(|s| (s.name, s.secrets)).collect();
        assert_eq!(steps, [("unit".to_string(), vec!["SHARED_KEY".to_string()]), ("integration".to_string(), vec!["SHARED_KEY".to_string(), "STRIPE_KEY".to_string()])]);
        let named = |o: Option<&str>, p: Option<&Part>| secrets_named(&cfg, o, p).into_iter().collect::<Vec<_>>();
        assert_eq!(named(None, Some(&Part { skip: vec!["integration".into()], ..Default::default() })), ["SHARED_KEY"]);
        // A shard's secrets go with its part, as the engine builds it: two shards may run one command.
        let shards = cfg.shards();
        assert_eq!(named(Some("e2e"), Some(&Part::of_shard(&shards[0]))), ["BROWSER_TOKEN", "SHARED_KEY"]);
        assert_eq!(named(Some("all"), None), ["SHARED_KEY"]);
    }

    /// Whatever the process running the proof holds (here cargo's CARGO_PKG_* and the rest of the
    /// test's environment), a step sees only what `step_env` lets through and what AVP sets.
    #[test]
    fn a_step_starts_from_a_scrubbed_environment() {
        let dir = std::env::temp_dir().join(format!("avp-test-env-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        let step = run(&dir, "env", &AtomicBool::new(false));
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(step.exit, 0, "{}", step.output);
        let allowed: Vec<String> = step_env(std::env::vars_os(), &std::env::var("AVP_PROOF_ENV").unwrap_or_default(), &std::env::var("AVP_DEPLOY_ENV").unwrap_or_default()).into_iter().map(|(k, _)| k.into_string().unwrap()).collect();
        // Set by AVP for every step, and by the shell itself.
        let set = ["CI", "AVP", "TRAME", "LANG", "LC_ALL", "PWD", "OLDPWD", "SHLVL", "_"];
        let seen: Vec<&str> = step.output.lines().filter_map(|l| l.split_once('=')).map(|(k, _)| k).collect();
        assert!(seen.contains(&"CI"), "{}", step.output);
        for k in seen {
            assert!(allowed.iter().any(|a| a == k) || set.contains(&k), "{k} reached the step:\n{}", step.output);
        }
    }

    /// A step's log dates each line as it comes, shows a progress bar as its last state, masks what
    /// the step was given, and past its cap keeps the start and the end, the lines between counted.
    #[test]
    fn a_steps_log_is_dated_masked_and_cut_in_the_middle() {
        let dir = std::env::temp_dir().join(format!("avp-test-log-{}", ulid::Ulid::new()));
        fs::create_dir_all(&dir).unwrap();
        let given = [(OsString::from("KEY"), OsString::from("sk_test_51abc"))];
        let (step, log) = run_with(&dir, "echo first; printf '10%%\\r100%%\\n'; sleep 0.3; echo \"key $KEY\"", &AtomicBool::new(false), &[], &given, None);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(step.exit, 0, "{}", step.output);
        let lines: Vec<&str> = log.lines.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(lines, ["first", "100%", "key ***"]);
        assert!(log.lines[2].0 >= 250 && log.lines[0].0 < 250, "{:?}", log.lines);
        assert!(log.started > 0 && log.cut.is_none());

        let mut c = Capture::new(Instant::now());
        let line = "x".repeat(99);
        let n = (LOG_HEAD + LOG_TAIL) / 100 + 5000;
        for i in 0..n {
            c.feed(format!("{i:06}{}\n", &line[6..]).as_bytes(), false, &[]);
        }
        let log = c.end();
        let cut = log.cut.expect("cut");
        assert_eq!(log.lines.len() + cut.lines, n);
        assert!(log.lines[0].1.starts_with("000000") && log.lines.last().unwrap().1.starts_with(&format!("{:06}", n - 1)));
        assert!(log.lines[cut.at - 1].1 < log.lines[cut.at].1 && cut.bytes == cut.lines as u64 * 100);
    }

    #[test]
    fn the_first_failing_line_is_where_a_compiler_or_a_runner_says_so() {
        let first = |text: &str| first_failure(text.lines());
        assert_eq!(first("   Compiling avp\nwarning: unused\nerror[E0308]: mismatched types\nerror: could not compile"), Some(2));
        assert_eq!(first("running 3 tests\ntest a ... ok\ntest b ... FAILED\ntest result: FAILED. 2 passed; 1 failed"), Some(2));
        assert_eq!(first(" ✓ src/a.test.ts (2)\n \x1b[31m×\x1b[39m src/b.test.ts > adds\n FAIL  src/b.test.ts"), Some(1));
        assert_eq!(first("src/a.ts(3,5): error TS2322: Type 'string'"), Some(0));
        assert_eq!(first("collected 2 items\nTraceback (most recent call last):\nAssertionError: 1 != 2"), Some(1));
        assert_eq!(first("test result: ok. 3 passed; 0 failed; 0 ignored\nan error_handling test passed"), None);
    }

    /// Plain files of the step's folder only, never through a link nor past the cap; what the
    /// config names outside the folder is refused before any step runs.
    #[test]
    fn artifacts_are_packed_from_the_steps_folder_only() {
        let root = std::env::temp_dir().join(format!("avp-test-pack-{}", ulid::Ulid::new()));
        let (dir, outside) = (root.join("slot"), root.join("outside"));
        fs::create_dir_all(dir.join("coverage/lcov")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(dir.join("coverage/index.html"), "<html>").unwrap();
        fs::write(dir.join("coverage/lcov/big.info"), "x".repeat(4000)).unwrap();
        fs::write(dir.join("coverage/-rf"), "a name tar must not read as an option").unwrap();
        fs::write(outside.join("secret.txt"), "the machine's").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("coverage/linked.txt")).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("out")).unwrap();
        }
        let out = root.join("pack.tar.gz");
        let mut budget = 1000;
        let a = pack(&dir, &["coverage/".into(), "out/".into(), "report/".into()], &mut budget, &out);
        assert_eq!((a.files, a.error.as_deref()), (2, None), "{a:?}");
        assert_eq!(a.missing, ["report/"]);
        assert!(a.left_out.contains(&"out/".to_string()) && a.left_out.contains(&"coverage/lcov/big.info".to_string()) && a.left_out.contains(&"coverage/linked.txt".to_string()), "{a:?}");
        let unpacked = root.join("unpacked");
        fs::create_dir_all(&unpacked).unwrap();
        assert!(Command::new("tar").arg("-xzf").arg(&out).arg("-C").arg(&unpacked).status().unwrap().success());
        assert_eq!(fs::read_to_string(unpacked.join("coverage/index.html")).unwrap(), "<html>");
        assert!(unpacked.join("coverage/-rf").is_file() && !unpacked.join("coverage/linked.txt").exists());
        fs::remove_dir_all(&root).unwrap();

        for bad in ["../up", "/etc", "a/../../b"] {
            let toml = format!("[[check.steps]]\nname = \"t\"\nrun = \"t\"\nartifacts = [{bad:?}]");
            let err = toml::from_str::<AvpConfig>(&toml).unwrap().check.validate().unwrap_err().to_string();
            assert!(err.contains("not a path inside the step's folder"), "{bad}: {err}");
        }
    }

    /// The key a step's log is kept under reads the same once the step went through JSON, as a
    /// worker's result and the proofs table carry it.
    #[test]
    fn a_steps_key_survives_json() {
        let step = Step { name: "tests".into(), command: "cargo test".into(), exit: 101, seconds: 12.345_678_901_234_5, output: "test a ... FAILED".into() };
        let back: Step = serde_json::from_str(&serde_json::to_string(&step).unwrap()).unwrap();
        assert_eq!(kept_key(&back), kept_key(&step));
        assert_ne!(kept_key(&Step { exit: 0, ..back }), kept_key(&step));
    }
}
