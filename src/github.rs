//! AVP's bridge to GitHub, inside `avp serve` when the server has AVP_GITHUB_BRIDGE=1: what
//! deploy/k8s/scripts/watch.mjs did beside our own server, for every server. Every 30 s, a
//! promotion whose deploy is a `github_workflow` is carried out (the workflow dispatched on the
//! branch, its run found, its pending deployment approved in the name of who promoted, followed to
//! its end), the full suite's verdict of each position becomes the commit status `avp/full` on its
//! commit, and what each environment's `serves` URL answers moves it. GH_TOKEN (or GITHUB_TOKEN)
//! stays in the server: it goes in a header, never in an error, an event or a step's environment.

use crate::check::AvpConfig;
use crate::engine::Engine;
use crate::model::*;
use anyhow::{anyhow, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often the bridge reads GitHub and the environments.
pub const EVERY: Duration = Duration::from_secs(30);

/// A workflow dispatched whose run does not show within this long never started: the deploy failed.
const NO_RUN_MS: i64 = 10 * 60_000;

/// Commit statuses written in one round at most: GitHub limits the writes of a token per minute
/// (80), and a bridge turned on for the first time has the last 50 positions to write.
const STATUSES_PER_ROUND: usize = 10;

/// Starts the bridge on a thread of its own when the server has AVP_GITHUB_BRIDGE=1: a server that
/// still runs watch.mjs (ours, until we switch) does not carry its promotions twice.
pub fn start(engine: Engine) {
    if std::env::var("AVP_GITHUB_BRIDGE").ok().as_deref() != Some("1") {
        return;
    }
    std::thread::spawn(move || {
        // Made here, off the server's async runtime, where a blocking client cannot be made.
        let github = match GitHub::from_env() {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("github bridge: {e:#}. Until then, only the `serves` URLs are read");
                None
            }
        };
        eprintln!("github bridge on, every {} s", EVERY.as_secs());
        let mut bridge = Bridge::new(engine, github);
        loop {
            bridge.round(now_ms());
            std::thread::sleep(EVERY);
        }
    });
}

/// GitHub's REST API with the server's token, within its rate limits.
pub struct GitHub {
    api: String,
    token: String,
    http: reqwest::blocking::Client,
    /// No call before then: GitHub said the token's rate limit is spent until that time.
    paused: std::cell::Cell<Option<Instant>>,
}

impl GitHub {
    pub fn new(api: &str, token: &str) -> Self {
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { api: api.trim_end_matches('/').to_string(), token: token.to_string(), http, paused: Default::default() }
    }

    /// GH_TOKEN (or GITHUB_TOKEN) of the server's environment; AVP_GITHUB_API for another API than
    /// github.com's (GitHub Enterprise Server's `https://<host>/api/v3`).
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")).ok().filter(|t| !t.is_empty());
        let token = token.ok_or_else(|| anyhow!("set GH_TOKEN on the server: a token that runs the deploy workflows, approves their environments and writes commit statuses"))?;
        let api = std::env::var("AVP_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
        Ok(Self::new(&api, &token))
    }

    /// One call: its JSON answer (null when it has none). Once GitHub says the rate limit is spent
    /// (`x-ratelimit-remaining: 0` until `x-ratelimit-reset`, a `retry-after`, a minute for a
    /// secondary limit that says neither), nothing is asked of it until then.
    pub fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        if let Some(until) = self.paused.get() {
            let left = until.saturating_duration_since(Instant::now());
            anyhow::ensure!(left.is_zero(), "GitHub's rate limit is spent: nothing is asked of it for {} s", left.as_secs() + 1);
            self.paused.set(None);
        }
        let mut req = self.http.request(method.clone(), format!("{}{path}", self.api)).bearer_auth(&self.token).header("accept", "application/vnd.github+json").header("user-agent", "avp");
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().map_err(|e| anyhow!("GitHub {method} {path}: {}", e.without_url()))?;
        let status = r.status();
        let header = |n: &str| r.headers().get(n).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
        let (remaining, reset, retry) = (header("x-ratelimit-remaining"), header("x-ratelimit-reset"), header("retry-after"));
        let v: Value = serde_json::from_str(&r.text().unwrap_or_default()).unwrap_or(Value::Null);
        let message = v["message"].as_str().unwrap_or("");
        let limited = matches!(status.as_u16(), 403 | 429) && (retry.is_some() || remaining == Some(0) || message.contains("rate limit"));
        let wait = match (retry, remaining, reset) {
            (Some(s), _, _) if limited => Some(s),
            (_, Some(0), Some(at)) => Some(at.saturating_sub(now_ms() as u64 / 1000)),
            _ if limited => Some(60),
            _ => None,
        };
        if let Some(s) = wait {
            self.paused.set(Some(Instant::now() + Duration::from_secs(s.max(1))));
        }
        anyhow::ensure!(status.is_success(), "GitHub {method} {path}: {status} {message}{}", wait.filter(|_| limited).map(|s| format!(": nothing more is asked of it for {s} s")).unwrap_or_default());
        Ok(v)
    }
}

/// The bridge: what it carries, what it wrote.
pub struct Bridge {
    engine: Engine,
    github: Option<GitHub>,
    /// For `serves` URLs: the environment's own server, without the token.
    http: reqwest::blocking::Client,
    /// When the bridge started (ms): a promotion from before may have its run already (a restart,
    /// watch.mjs before the switch), one made since is dispatched.
    started: i64,
    /// The deploy carried for each environment, by repo and environment.
    deploys: HashMap<(String, String), Carried>,
    /// The `avp/full` verdict written on each position's commit, by repo, kept in `file`: a
    /// restart writes none of them again.
    sent: BTreeMap<String, BTreeMap<i64, String>>,
    file: PathBuf,
    /// The last error of each part, said once rather than every round.
    said: HashMap<String, String>,
}

/// A promotion the bridge carries out.
struct Carried {
    promotion: Promotion,
    /// When the bridge dispatched the workflow (ms), and the runs it had before.
    dispatched: Option<i64>,
    before: BTreeSet<u64>,
    /// The run: its id and page.
    run: Option<(u64, String)>,
    /// Over, and still asked for: the step it ended on (failed, or waiting for `serves`) and when.
    over: Option<(String, i64)>,
}

impl Bridge {
    pub fn new(engine: Engine, github: Option<GitHub>) -> Self {
        let file = engine.data.join("github-bridge.json");
        let sent = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| serde_json::from_value(v["statuses"].clone()).ok()).unwrap_or_default();
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        Self { engine, github, http, started: now_ms(), deploys: HashMap::new(), sent, file, said: HashMap::new() }
    }

    /// One round over every repo of the server.
    pub fn round(&mut self, now: i64) {
        let repos = self.engine.db.lock().unwrap_or_else(|e| e.into_inner()).repos();
        let repos = match repos {
            Ok(r) => r,
            Err(e) => return self.say("repos", Err(e)),
        };
        for repo in repos {
            let cfg = match self.engine.config(&repo.id) {
                Ok(c) => c,
                Err(e) => {
                    self.say(&repo.name, Err(e));
                    continue;
                }
            };
            let r = self.observe(&repo.id, &cfg);
            self.say(&format!("{} serves", repo.name), r);
            let r = self.carry_all(&repo.id, &cfg, now);
            self.say(&format!("{} deploys", repo.name), r);
            if let (Some(slug), true) = (&cfg.github.repo, self.github.is_some()) {
                let r = self.statuses(&repo.id, slug);
                self.say(&format!("{} avp/full", repo.name), r);
            }
        }
    }

    fn say(&mut self, what: &str, r: Result<()>) {
        match r {
            Ok(()) => {
                self.said.remove(what);
            }
            Err(e) => {
                let e = format!("{e:#}");
                if self.said.get(what) != Some(&e) {
                    eprintln!("github bridge, {what}: {e}");
                    self.said.insert(what.to_string(), e);
                }
            }
        }
    }

    /// What each environment's `serves` URL answers moves it, when four readings agree (during a
    /// rollout, old and new pods answer in turn) and a position of the line has that commit.
    fn observe(&self, repo: &str, cfg: &AvpConfig) -> Result<()> {
        let mut errors = vec![];
        for (name, env) in &cfg.envs {
            let Some(url) = &env.serves else { continue };
            let read = || -> Result<Option<String>> {
                let mut shas = BTreeSet::new();
                for _ in 0..4 {
                    shas.insert(served(&self.http, url)?);
                }
                Ok(if shas.len() == 1 { shas.pop_first() } else { None })
            };
            match read() {
                Ok(Some(sha)) => {
                    // A commit from before the line, or one the mirror has not written yet: nothing moves.
                    let known = self.engine.db.lock().unwrap_or_else(|e| e.into_inner()).position_of_git(repo, &sha)?.is_some();
                    if known {
                        self.engine.observe_env(repo, name, &sha)?;
                    }
                }
                Ok(None) => {}
                Err(e) => errors.push(format!("{name}, {url}: {e:#}")),
            }
        }
        anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }

    /// Each promotion still on its way whose deploy is a GitHub workflow, one step further.
    fn carry_all(&mut self, repo: &str, cfg: &AvpConfig, now: i64) -> Result<()> {
        if !cfg.envs.values().any(|e| e.deploy.as_ref().and_then(|d| d.github()).is_some()) {
            return Ok(());
        }
        let mut errors = vec![];
        for e in self.engine.envs_view(repo)? {
            let key = (repo.to_string(), e.name.clone());
            let (Some(w), Some(d)) = (e.wanted, e.deploy) else {
                self.deploys.remove(&key);
                continue;
            };
            if e.position == w.position {
                self.deploys.remove(&key);
                continue;
            }
            let progress = |step: String, since: i64, url: Option<String>| {
                let _ = self.engine.deploy_progress(repo, &e.name, DeployProgress { position: w.position, step, since, url, at: 0 });
            };
            let (Some(slug), Some(github)) = (&cfg.github.repo, &self.github) else {
                let missing = if cfg.github.repo.is_none() { "`[github] repo = \"<owner>/<name>\"` in avp.toml" } else { "GH_TOKEN on the server" };
                progress(format!("waits for {missing}: the bridge dispatches {} then", d.github_workflow), w.at, None);
                continue;
            };
            if !self.deploys.get(&key).is_some_and(|c| (c.promotion.position, c.promotion.at) == (w.position, w.at)) {
                let fresh = Carried::recorded(&self.engine, repo, &e.name, w.clone())?;
                self.deploys.insert(key.clone(), fresh);
            }
            let c = self.deploys.get_mut(&key).expect("just put");
            let serves = cfg.envs.get(&e.name).and_then(|x| x.serves.as_deref());
            let to = Target { engine: &self.engine, github, repo, slug, branch: cfg.github.branch.as_deref().unwrap_or("main"), env: &e.name, deploy: &d, serves };
            if let Err(err) = to.carry(c, self.started, now) {
                errors.push(format!("{}: {err:#}", e.name));
            }
        }
        anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }

    /// The full suite's verdict of each of the last 50 positions that has a commit, written as the
    /// commit status `avp/full` (teams gate workflows and branch rules on it), once per verdict.
    fn statuses(&mut self, repo: &str, slug: &str) -> Result<()> {
        let github = self.github.as_ref().ok_or_else(|| anyhow!("no GH_TOKEN"))?;
        valid_slug(slug)?;
        let line = self.engine.db.lock().unwrap_or_else(|e| e.into_inner()).line(repo, i64::MAX, 50)?;
        let sent = self.sent.entry(repo.to_string()).or_default();
        let (mut wrote, mut out) = (0, Ok(()));
        for e in line.iter().rev() {
            let (Some(git), Some(full)) = (&e.git, &e.full) else { continue };
            let verdict = match full {
                Verdict::Green => "green",
                Verdict::Flaky => "flaky",
                Verdict::Red => "red",
                Verdict::Blocked => continue,
            };
            if sent.get(&e.position).is_some_and(|v| v == verdict) || !git.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            if wrote == STATUSES_PER_ROUND {
                break;
            }
            let state = if verdict == "red" { "failure" } else { "success" };
            let body = json!({ "state": state, "context": "avp/full", "description": format!("AVP full suite, position {}: {verdict}", e.position) });
            if let Err(err) = github.call(Method::POST, &format!("/repos/{slug}/statuses/{git}"), Some(body)) {
                out = Err(err);
                break;
            }
            sent.insert(e.position, verdict.to_string());
            wrote += 1;
        }
        if wrote > 0 {
            // The newest 200 positions are enough: the bridge never reads further back than 50.
            while sent.len() > 200 {
                sent.pop_first();
            }
            let tmp = self.file.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&json!({ "statuses": self.sent }))?)?;
            std::fs::rename(&tmp, &self.file)?;
        }
        out
    }
}

impl Carried {
    /// A promotion as the events left it: its run once found (a server that restarted follows it
    /// again rather than dispatching twice), its failure once told.
    fn recorded(engine: &Engine, repo: &str, env: &str, promotion: Promotion) -> Result<Self> {
        let mut c = Carried { promotion, dispatched: None, before: BTreeSet::new(), run: None, over: None };
        let events = engine.db.lock().unwrap_or_else(|e| e.into_inner()).events_of(repo, &["deploy.dispatched", "env.failed"], c.promotion.at)?;
        for e in events.iter().filter(|e| e.data["env"] == env && e.data["position"] == c.promotion.position) {
            match e.kind.as_str() {
                "deploy.dispatched" => {
                    c.dispatched = Some(e.at);
                    c.run = e.data["run"].as_u64().map(|id| (id, e.data["url"].as_str().unwrap_or("").to_string()));
                }
                _ => c.over = Some((format!("failed: {}", e.data["reason"].as_str().unwrap_or("?")), e.at)),
            }
        }
        Ok(c)
    }
}

/// Where a promotion goes: the environment, its deploy, the repo on GitHub.
struct Target<'a> {
    engine: &'a Engine,
    github: &'a GitHub,
    repo: &'a str,
    slug: &'a str,
    branch: &'a str,
    env: &'a str,
    deploy: &'a crate::model::DeployConfig,
    serves: Option<&'a str>,
}

impl Target<'_> {
    fn progress(&self, c: &Carried, step: String, since: i64) {
        let url = c.run.as_ref().map(|(_, u)| u.clone()).filter(|u| !u.is_empty());
        let _ = self.engine.deploy_progress(self.repo, self.env, DeployProgress { position: c.promotion.position, step, since, url, at: 0 });
    }

    /// One step further: dispatched, its run found, approved, followed, over.
    fn carry(&self, c: &mut Carried, started: i64, now: i64) -> Result<()> {
        if let Some((step, since)) = &c.over {
            self.progress(c, step.clone(), *since);
            return Ok(());
        }
        valid_slug(self.slug)?;
        let workflow = &self.deploy.github_workflow;
        anyhow::ensure!(!workflow.is_empty() && workflow.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch)), "github_workflow is a file of .github/workflows (deploy.yml), not {workflow:?}");
        let p = c.promotion.clone();
        if c.run.is_none() {
            // Read now: the mirror may have written the position's commit since it was promoted.
            let git = self.engine.db.lock().unwrap_or_else(|e| e.into_inner()).line_at(self.repo, p.position)?.git;
            let Some(git) = git else {
                self.progress(c, format!("waits for position {}'s git commit, which the mirror writes", p.position), p.at);
                return Ok(());
            };
            match c.dispatched {
                None => self.dispatch(c, &git, started, now)?,
                Some(at) => self.find_run(c, at)?,
            }
            if let Some((step, since)) = &c.over {
                self.progress(c, step.clone(), *since);
                return Ok(());
            }
            match (&c.run, c.dispatched) {
                (Some((id, url)), _) => self.engine.github_dispatched(self.repo, self.env, &p, workflow, *id, url)?,
                (None, Some(at)) if now - at > NO_RUN_MS => {
                    let reason = format!("{workflow} was dispatched on {}, and no run of it started within {} minutes", self.branch, NO_RUN_MS / 60_000);
                    return self.failed(c, reason, now);
                }
                (None, _) => {
                    self.progress(c, format!("{workflow} dispatched, its run not started yet"), c.dispatched.unwrap_or(now));
                    return Ok(());
                }
            }
        }
        let (id, _) = c.run.clone().expect("found above");
        let run = self.github.call(Method::GET, &format!("/repos/{}/actions/runs/{id}", self.slug), None)?;
        if let Some(url) = run["html_url"].as_str() {
            c.run = Some((id, url.to_string()));
        }
        let url = c.run.as_ref().map(|(_, u)| u.clone()).unwrap_or_default();
        match run["status"].as_str().unwrap_or("") {
            "completed" => match run["conclusion"].as_str() {
                Some("success") => match self.serves {
                    Some(serves) => {
                        // The environment moves once its URL answers the commit (`observe`).
                        let git = self.engine.db.lock().unwrap_or_else(|e| e.into_inner()).line_at(self.repo, p.position)?.git.unwrap_or_default();
                        c.over = Some((format!("deployed, waits for {serves} to answer {}", &git[..git.len().min(12)]), now));
                        self.progress(c, c.over.clone().unwrap().0, now);
                    }
                    None => {
                        self.engine.github_deployed(self.repo, self.env, &p, &url)?;
                    }
                },
                conclusion => {
                    let job = self.failing_job(id)?;
                    let how = match conclusion.unwrap_or("") {
                        "failure" => "failed".to_string(),
                        "cancelled" => "was cancelled".to_string(),
                        "timed_out" => "timed out".to_string(),
                        other => format!("ended {other}"),
                    };
                    let reason = match job {
                        Some(j) => format!("{workflow}: job \"{j}\" {how}"),
                        None => format!("{workflow}: its run {how}"),
                    };
                    self.failed(c, reason, now)?;
                }
            },
            "waiting" => {
                self.approve(id, &p)?;
                self.progress(c, "waiting for an approval".into(), time(&run["updated_at"]).unwrap_or(now));
            }
            "queued" | "requested" => self.progress(c, "waiting for a runner".into(), time(&run["created_at"]).unwrap_or(now)),
            "pending" => self.progress(c, "waiting for the run before it (concurrency)".into(), time(&run["created_at"]).unwrap_or(now)),
            _ => {
                let (step, since) = self.current_step(id)?;
                self.progress(c, step, since.or(time(&run["run_started_at"])).unwrap_or(now));
            }
        }
        Ok(())
    }

    fn runs(&self) -> Result<Vec<Value>> {
        let path = format!("/repos/{}/actions/workflows/{}/runs?event=workflow_dispatch&branch={}&per_page=20", self.slug, self.deploy.github_workflow, query(self.branch));
        Ok(self.github.call(Method::GET, &path, None)?["workflow_runs"].as_array().cloned().unwrap_or_default())
    }

    /// The runs other promotions' deploys took, as their `deploy.dispatched` say.
    fn claimed(&self, now: i64) -> Result<BTreeSet<u64>> {
        let events = self.engine.db.lock().unwrap_or_else(|e| e.into_inner()).events_of(self.repo, &["deploy.dispatched"], now - 24 * 3600 * 1000)?;
        Ok(events.iter().filter_map(|e| e.data["run"].as_u64()).collect())
    }

    /// Dispatches the workflow on the branch, its inputs filled in. A promotion from before the
    /// bridge started whose run is there already (a server that restarted, watch.mjs before the
    /// switch) takes that run instead: dispatched twice, it would deploy twice.
    fn dispatch(&self, c: &mut Carried, git: &str, started: i64, now: i64) -> Result<()> {
        let p = c.promotion.clone();
        let runs = self.runs()?;
        let claimed = self.claimed(now)?;
        if p.at < started {
            let since = |r: &Value| time(&r["created_at"]).is_some_and(|t| t >= p.at);
            if let Some(r) = runs.iter().filter(|r| since(r) && r["id"].as_u64().is_some_and(|id| !claimed.contains(&id))).max_by_key(|r| r["id"].as_u64()) {
                c.run = Some((r["id"].as_u64().unwrap_or(0), r["html_url"].as_str().unwrap_or("").to_string()));
                c.dispatched = Some(now);
                return Ok(());
            }
            // Without a run, and older than a deploy is given (AVP_FOLLOW_STUCK_S, as `follow_full`
            // reads it): one that failed under another bridge, days ago perhaps. Dispatched now, it
            // would deploy what nobody asks for any more.
            let stuck_ms = std::env::var("AVP_FOLLOW_STUCK_S").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(1800) * 1000;
            if now - p.at > stuck_ms {
                c.over = Some((format!("promoted {} min before the bridge started, and no run of {} since: promote again", (started - p.at) / 60_000, self.deploy.github_workflow), now));
                return Ok(());
            }
        }
        c.before = runs.iter().filter_map(|r| r["id"].as_u64()).collect();
        let fill = |v: &str| v.replace("{git}", git).replace("{position}", &p.position.to_string());
        let inputs: BTreeMap<&String, String> = self.deploy.inputs.iter().map(|(k, v)| (k, fill(v))).collect();
        let path = format!("/repos/{}/actions/workflows/{}/dispatches", self.slug, self.deploy.github_workflow);
        // `return_run_details` makes GitHub answer the run it starts; a GitHub that does not know it
        // yet (an older Enterprise Server) refuses it, and the run is found among the workflow's.
        let answer = match self.github.call(Method::POST, &path, Some(json!({ "ref": self.branch, "inputs": inputs, "return_run_details": true }))) {
            Err(e) if format!("{e:#}").contains(": 422 ") => self.github.call(Method::POST, &path, Some(json!({ "ref": self.branch, "inputs": inputs })))?,
            answer => answer?,
        };
        c.dispatched = Some(now);
        eprintln!("github bridge: {} dispatched on {} for position {} ({}) to {}, promoted by {}", self.deploy.github_workflow, self.branch, p.position, &git[..git.len().min(12)], self.env, p.by);
        if let Some(id) = answer["workflow_run_id"].as_u64() {
            c.run = Some((id, answer["html_url"].as_str().unwrap_or("").to_string()));
        }
        Ok(())
    }

    /// The run a dispatch without its run in the answer started: the oldest of the workflow's runs
    /// that was not there before it and no other deploy took (two promotions dispatched together
    /// take theirs in turn).
    fn find_run(&self, c: &mut Carried, dispatched: i64) -> Result<()> {
        let claimed = self.claimed(dispatched)?;
        let runs = self.runs()?;
        let new = |r: &&Value| r["id"].as_u64().is_some_and(|id| !c.before.contains(&id) && !claimed.contains(&id)) && time(&r["created_at"]).is_some_and(|t| t >= dispatched - 60_000);
        if let Some(r) = runs.iter().filter(new).min_by_key(|r| r["id"].as_u64()) {
            c.run = Some((r["id"].as_u64().unwrap_or(0), r["html_url"].as_str().unwrap_or("").to_string()));
        }
        Ok(())
    }

    /// GitHub's required review of the environment the deploy names, given in the name of the
    /// person who promoted: their promotion is the approval.
    fn approve(&self, id: u64, p: &Promotion) -> Result<()> {
        let Some(environment) = &self.deploy.approve_environment else { return Ok(()) };
        let path = format!("/repos/{}/actions/runs/{id}/pending_deployments", self.slug);
        let pending = self.github.call(Method::GET, &path, None)?;
        let ids: Vec<i64> = pending.as_array().into_iter().flatten().filter(|d| d["environment"]["name"] == environment.as_str()).filter_map(|d| d["environment"]["id"].as_i64()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        let comment = format!("AVP: position {} promoted to {} by {}", p.position, self.env, p.by);
        self.github.call(Method::POST, &path, Some(json!({ "environment_ids": ids, "state": "approved", "comment": comment })))?;
        eprintln!("github bridge: {environment} approved on run {id} for {} (position {})", p.by, p.position);
        Ok(())
    }

    fn jobs(&self, id: u64) -> Result<Vec<Value>> {
        Ok(self.github.call(Method::GET, &format!("/repos/{}/actions/runs/{id}/jobs?per_page=100", self.slug), None)?["jobs"].as_array().cloned().unwrap_or_default())
    }

    /// What a running deploy does now: the step running in its job (`deploy: helm upgrade`), or
    /// the job that waits for a runner, and since when.
    fn current_step(&self, id: u64) -> Result<(String, Option<i64>)> {
        let jobs = self.jobs(id)?;
        let name = |j: &Value| j["name"].as_str().unwrap_or("?").to_string();
        if let Some(job) = jobs.iter().find(|j| j["status"] == "in_progress") {
            let step = job["steps"].as_array().into_iter().flatten().find(|s| s["status"] == "in_progress");
            return Ok(match step {
                Some(s) => (format!("{}: {}", name(job), s["name"].as_str().unwrap_or("?")), time(&s["started_at"])),
                None => (name(job), time(&job["started_at"])),
            });
        }
        if let Some(job) = jobs.iter().find(|j| j["status"] == "queued" || j["status"] == "waiting") {
            return Ok((format!("waiting for a runner ({})", name(job)), time(&job["created_at"])));
        }
        Ok(("running".into(), None))
    }

    /// The job a run that did not succeed ended on: the first that failed, else the first that
    /// neither succeeded nor was skipped.
    fn failing_job(&self, id: u64) -> Result<Option<String>> {
        let jobs = self.jobs(id)?;
        let bad = jobs.iter().find(|j| j["conclusion"] == "failure").or_else(|| jobs.iter().find(|j| !matches!(j["conclusion"].as_str(), Some("success" | "skipped" | "neutral"))));
        Ok(bad.and_then(|j| j["name"].as_str()).map(String::from))
    }

    fn failed(&self, c: &mut Carried, reason: String, now: i64) -> Result<()> {
        let url = c.run.as_ref().map(|(_, u)| u.clone()).unwrap_or_default();
        self.engine.github_failed(self.repo, self.env, &c.promotion, &reason, &url)?;
        c.over = Some((format!("failed: {reason}"), now));
        self.progress(c, format!("failed: {reason}"), now);
        Ok(())
    }
}

/// The commit an environment's `serves` URL answers: `{"sha": "<commit>"}`, or the commit alone.
fn served(http: &reqwest::blocking::Client, url: &str) -> Result<String> {
    let r = http.get(url).header("cache-control", "no-store").send().map_err(|e| anyhow!("{}", e.without_url()))?;
    anyhow::ensure!(r.status().is_success(), "it answers {}", r.status());
    let text = r.text()?;
    let sha = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["sha"].as_str().map(String::from)).unwrap_or_else(|| text.trim().to_string());
    anyhow::ensure!((7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit()), "it answers no git commit: {{\"sha\": \"<commit>\"}} or the commit alone");
    Ok(sha.to_ascii_lowercase())
}

fn valid_slug(slug: &str) -> Result<()> {
    let part = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    anyhow::ensure!(slug.split('/').count() == 2 && slug.split('/').all(part), "[github] repo is owner/name, not {slug:?}");
    Ok(())
}

/// A value of a query string, its reserved characters escaped (a branch may hold `&` or `#`).
fn query(v: &str) -> String {
    v.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn time(v: &Value) -> Option<i64> {
    v.as_str().and_then(|s| crate::stats::parse_time(s).ok())
}
