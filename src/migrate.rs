//! `avp migrate`: a team moves to AVP with one command, the first two steps of docs/MIGRATE.md. It
//! reads the checkout (its CI, its agents, its server), puts AVP beside `main` as `avp up` does (the
//! server, the line, the mirror on `avp/line`, the agents), writes avp.toml from the CI, and proves
//! it on this machine before anyone relies on it: the proof steps and the whole suite once, on the
//! head of the line, each green or red with what to change. Then the plan of what is left. Run
//! again after a fix, it starts from where it is. It never pushes to main and never changes a
//! setting on GitHub: those steps stay a person's.

use crate::check::{self, AvpConfig, Part, Workspace};
use crate::client::Client;
use crate::model::*;
use crate::objects::hash_bytes;
use crate::{importci, offline, up};
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What `avp migrate` did and found, kept for the next run and for `avp_migrate_report`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub checkout: String,
    pub repo: String,
    pub repo_id: String,
    /// The server, as `avp` shows it: never its password.
    pub server: String,
    pub at: i64,
    /// The day of the first run: the window before AVP ends there.
    pub started: String,
    /// What it found or did, one line each: the server, the repo, the CI, the mirror.
    pub setup: Vec<Said>,
    pub checks: Checks,
    pub proof: Proved,
    /// The coding agents of this machine, one line each, as `avp agents` says them.
    pub agents: Vec<String>,
    /// What is left, numbered like docs/MIGRATE.md.
    pub plan: Vec<Todo>,
    /// What it did not do, on purpose.
    pub not_done: Vec<String>,
    pub next: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Said {
    pub what: String,
    pub said: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Checks {
    /// Where the avp.toml proven comes from: `checkout` (the one there, kept), `line` (the line's,
    /// kept), `ci` (written from the CI), `guess` (written from the project's files), `none`.
    pub source: String,
    /// The file in the checkout, when it is the one proven.
    pub file: Option<String>,
    /// blake3 of what was proven.
    pub hash: Option<String>,
    /// What the CI says that the avp.toml kept does not, or says otherwise.
    pub differs: Vec<String>,
    /// What the CI has that avp.toml has no word for, one line each with what to do.
    pub not_translated: Vec<String>,
    /// `landed`, `approval` (green, waits for a person's word on `task`), `red` (not landed: red on
    /// this machine), `refused` (the server's proof, or a person, said no), `invalid`, `none`.
    pub state: String,
    pub position: Option<i64>,
    pub task: Option<String>,
    pub said: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Proved {
    /// The head of the line it ran on, and the tree: that head with the avp.toml proven.
    pub position: i64,
    pub tree: String,
    /// `green`, `red`, or `none` (avp.toml runs nothing).
    pub verdict: String,
    pub at: i64,
    /// The last run's verdict on the same tree, green: not run again.
    pub reused: bool,
    pub runs: Vec<Run>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Run {
    /// `proof` (a step every task runs) or `full` (a shard of the whole suite).
    pub part: String,
    pub name: String,
    pub command: String,
    /// `green`, `red` or `skipped`.
    pub verdict: String,
    pub seconds: f64,
    /// Where it went red: the first lines of the failure.
    pub lines: Vec<String>,
    /// What to change, and the command that does it when there is one.
    pub todo: Option<String>,
    pub command_to_run: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Todo {
    /// Its step in docs/MIGRATE.md: `2`, `3.1`.
    pub step: String,
    pub what: String,
    pub command: String,
}

pub struct Migrate {
    pub name: Option<String>,
    /// The person's word for the checks: the avp.toml proven green here lands without waiting.
    pub yes: bool,
}

/// The goal of the task that lands avp.toml: an earlier run's is found by it.
const GOAL: &str = "AVP: the checks a task proves before it lands (avp.toml from the CI, by avp migrate)";

/// Where the report of a checkout is kept: outside it, in the AVP home.
fn report_path(top: &Path) -> PathBuf {
    let key: String = top.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    up::home().join("migrate").join(format!("{key}.json"))
}

/// The last report for the checkout `dir` is in, or else for the repo `key` (its name or id):
/// what `avp_migrate_report` answers.
pub fn last_report(dir: &Path, key: Option<&str>) -> Option<Report> {
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Report>(&t).ok());
    if let Some(r) = up::git_top(dir).and_then(|top| read(&report_path(&top))) {
        return Some(r);
    }
    let key = key?;
    let all = std::fs::read_dir(up::home().join("migrate")).ok()?;
    all.flatten().filter_map(|e| read(&e.path())).filter(|r| r.repo == key || r.repo_id == key).max_by_key(|r| r.at)
}

fn said(what: &str, said: impl Into<String>) -> Said {
    Said { what: what.into(), said: said.into() }
}

fn git(top: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(top).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// `owner/name` of the GitHub repo origin points at, when it is one.
fn github_of(top: &Path) -> Option<String> {
    let url = git(top, &["remote", "get-url", "origin"])?;
    let re = Regex::new(r"github\.com[:/]([^/\s]+)/([^/\s]+?)(?:\.git)?/?$").unwrap();
    let c = re.captures(url.trim())?;
    Some(format!("{}/{}", &c[1], &c[2]))
}

pub fn migrate(client: &Client, dir: &Path, o: Migrate) -> Result<Report> {
    let top = up::git_top(dir).context("not inside a git checkout: run `avp migrate` in your repository's checkout of main")?;
    let name = o.name.or_else(|| up::repo_name(&top)).context("no name for this repo: pass --name")?;
    let last = last_report(&top, None).filter(|l| l.repo == name);
    let today = crate::stats::rfc3339(now_ms())[..10].to_string();
    let mut r = Report { checkout: top.display().to_string(), repo: name.clone(), server: client.shown(), at: now_ms(), started: last.as_ref().map(|l| l.started.clone()).filter(|s| !s.is_empty()).unwrap_or(today), ..Default::default() };

    // 1. The checkout's CI, read as avp.toml. One that does not read is a line of the report.
    let ci = importci::ci_files(&top);
    let imported = (!ci.is_empty()).then(|| importci::import(&top));
    let ci_line = match &imported {
        None => "none found (.github/workflows/*.yml, .gitlab-ci.yml): the checks are guessed from the project's files".to_string(),
        Some(Ok(i)) => format!("{} read, {} thing(s) avp.toml has no word for", i.sources.join(", "), i.not_translated.len()),
        Some(Err(e)) => format!("could not be read ({e:#}): the checks are guessed from the project's files"),
    };
    if let Some(Ok(i)) = &imported {
        r.checks.not_translated = i.not_translated.iter().map(|n| format!("{}: {}: {}", n.at, n.what, n.todo)).collect();
    }

    // 2. AVP beside main: the server, the line, the mirror on its own branch, the agents.
    r.setup.push(said("server", up::serve_here(client)?));
    let (repo, repo_line) = up::repo_here(client, &top, &name, 50)?;
    r.repo_id = repo.id.clone();
    r.setup.push(said("repo", repo_line));
    r.setup.push(said("ci", ci_line));
    let head: LineEntry = client.get(&format!("/repos/{}/head", repo.id))?;
    let tree: Tree = client.get(&format!("/repos/{}/tree/{}", repo.id, head.position))?;
    let on_line = match tree.get("avp.toml") {
        Some(e) => Some(String::from_utf8_lossy(&client.get_object(&repo.id, &e.blob)?).to_string()),
        None => None,
    };
    let text = choose(&top, &mut r.checks, on_line.as_deref(), imported.as_ref().and_then(|i| i.as_ref().ok()), last.as_ref())?;
    r.setup.push(said("mirror", up::mirror_here(client, &top, &name, "avp/line", true, false)?));
    let seen = up::wire_here(client, &top, &name)?;
    r.agents = up::listed(&seen);

    // The avp.toml proven, read, and compared with what the CI says.
    let cfg = match text.as_deref().map(|t| toml::from_str::<AvpConfig>(t).map_err(anyhow::Error::from).and_then(|c| c.check.validate().map(|_| c))) {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => {
            r.checks.state = "invalid".into();
            r.checks.said = format!("avp.toml does not hold together ({e:#}): fix it, then run `avp migrate` again");
            None
        }
        None => None,
    };
    if let (Some(c), Some(Ok(i)), false) = (&cfg, &imported, r.checks.source == "ci") {
        if let Ok(from_ci) = toml::from_str::<AvpConfig>(&i.toml) {
            r.checks.differs = differs(c, &from_ci);
        }
    }

    // 3. Proven on this machine, on the head with this avp.toml: the steps and the whole suite.
    if let (Some(cfg), Some(text)) = (&cfg, &text) {
        r.proof = prove(client, &repo, &head, &tree, text, cfg, last.as_ref())?;
        r.checks.hash = Some(hash_bytes(text.as_bytes()));
        land(client, &repo, &mut r, text, on_line.as_deref(), last.as_ref(), o.yes)?;
    } else if r.checks.state.is_empty() {
        r.checks.state = "none".into();
        r.checks.said = "no avp.toml: nothing in the CI nor the project's files says what a proof runs. Write one (docs/GUIDE.md), then run `avp migrate` again".into();
    }

    // 4. What is left.
    r.plan = plan(&r, cfg.as_ref(), &seen, &top);
    r.not_done = vec![
        "pushed nothing to main: the mirror writes avp/line".into(),
        "changed no setting on GitHub: main is not protected yet (3.1)".into(),
        "set no secret: a value is a person's to give (`avp secret set`)".into(),
        "changed no workflow: the CI runs as before, on avp/line too where it runs on every push (`branches: [main]` under `push:` keeps it off)".into(),
        "brought in no pull request (3.3)".into(),
    ];
    r.next = match r.checks.state.as_str() {
        "landed" => "AVP runs beside main: start an agent in this folder and ask for a change. When the team is ready, step 3 of the plan.".into(),
        "approval" => format!("avp.toml is green here and waits for a person's word: `avp migrate --yes`, or `avp approve {}`.", r.checks.task.as_deref().unwrap_or("<task>")),
        _ => "Change what the plan says, then run `avp migrate` again: it starts from where it is.".into(),
    };
    let p = report_path(&top);
    std::fs::create_dir_all(p.parent().context("no folder")?)?;
    std::fs::write(&p, serde_json::to_string_pretty(&r)?)?;
    Ok(r)
}

/// The avp.toml to prove, and where it comes from. The one in the checkout is the person's and
/// wins, unless it is only the commit's (or the one an earlier run landed) while the line holds a
/// newer one. Without one, it is written there, never over a file: the line's, the CI's, or the
/// checks guessed from the project's files.
fn choose(top: &Path, c: &mut Checks, on_line: Option<&str>, imported: Option<&importci::Imported>, last: Option<&Report>) -> Result<Option<String>> {
    let path = top.join("avp.toml");
    let here = std::fs::read_to_string(&path).ok();
    let committed = git(top, &["show", "HEAD:avp.toml"]);
    let landed_before = last.filter(|l| l.checks.state == "landed").and_then(|l| l.checks.hash.clone());
    if let (Some(h), Some(l)) = (&here, on_line) {
        let stale = committed.as_deref() == Some(h.as_str()) || landed_before.as_deref() == Some(hash_bytes(h.as_bytes()).as_str());
        if stale && h != l {
            c.source = "line".into();
            c.said = "avp.toml here is the commit's, or the one avp migrate landed: the line's, newer, is kept".into();
            return Ok(Some(l.to_string()));
        }
    }
    if let Some(h) = here {
        c.source = "checkout".into();
        c.file = Some(path.display().to_string());
        return Ok(Some(h));
    }
    let (source, text) = match (on_line, imported, up::guess_checks(top)) {
        (Some(l), _, _) => ("line", l.to_string()),
        (None, Some(i), _) => ("ci", i.toml.clone()),
        (None, None, Some((t, _))) => ("guess", t),
        (None, None, None) => {
            c.source = "none".into();
            return Ok(None);
        }
    };
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).with_context(|| format!("writing {}", path.display()))?;
    std::io::Write::write_all(&mut f, text.as_bytes())?;
    c.source = source.into();
    c.file = Some(path.display().to_string());
    Ok(Some(text))
}

/// What the CI's avp.toml says that `kept` does not, or says otherwise: by step, shard, service,
/// secret and environment, by name.
fn differs(kept: &AvpConfig, ci: &AvpConfig) -> Vec<String> {
    let mut out = Vec::new();
    let (k, c) = (&kept.check, &ci.check);
    if c.setup.is_some() && k.setup != c.setup {
        out.push(format!("the CI installs with `{}`, avp.toml {}", c.setup.as_deref().unwrap_or_default(), k.setup.as_deref().map(|s| format!("with `{s}`")).unwrap_or_else(|| "has no setup".into())));
    }
    for s in &c.steps {
        match k.steps.iter().find(|x| x.name == s.name) {
            None => out.push(format!("the CI's proof step `{}` (`{}`) is not in avp.toml", s.name, s.run)),
            Some(x) if x.run != s.run => out.push(format!("the step `{}` runs `{}` in avp.toml, `{}` in the CI", s.name, x.run, s.run)),
            _ => {}
        }
    }
    let shards = |cfg: &check::CheckConfig| -> Vec<(String, String)> {
        if cfg.full_shards.is_empty() { cfg.full.iter().map(|f| ("full".to_string(), f.clone())).collect() } else { cfg.full_shards.iter().map(|s| (s.name.clone(), s.run.clone())).collect() }
    };
    let ks = shards(k);
    for (name, run) in shards(c) {
        match ks.iter().find(|(n, _)| *n == name) {
            None => out.push(format!("the CI's job `{name}` is not a shard of the whole suite in avp.toml")),
            Some((_, x)) if *x != run => out.push(format!("the shard `{name}` runs otherwise in avp.toml than in the CI")),
            _ => {}
        }
    }
    for s in &c.services {
        if !k.services.iter().any(|x| x.name == s.name) {
            out.push(format!("the CI's service `{}` is not in avp.toml's [[check.services]]", s.name));
        }
    }
    let secrets = |cfg: &check::CheckConfig| -> BTreeSet<String> {
        let mut all = check::secrets_named(cfg, None, None);
        for s in cfg.shards() {
            all.extend(check::secrets_named(cfg, Some(&s.run), Some(&Part::of_shard(&s))));
        }
        all
    };
    let ksec = secrets(k);
    for n in secrets(c).difference(&ksec) {
        out.push(format!("the CI's tests read the secret {n}, which no step of avp.toml names"));
    }
    for e in ci.envs.keys() {
        if !kept.envs.contains_key(e) {
            out.push(format!("the CI deploys to {e}, which avp.toml has no [envs.{e}] for"));
        }
    }
    out
}

/// The files the newest position that changed code changed, among the last ten: what a task like
/// the ones the line sees touches, for the steps that run on `{files}`.
fn recent_files(client: &Client, repo: &Repo, head: &LineEntry, tree: &Tree) -> Vec<String> {
    let mut newer = tree.clone();
    for p in (head.position.saturating_sub(10).max(0)..head.position).rev() {
        let Ok(older) = client.get::<Tree>(&format!("/repos/{}/tree/{p}", repo.id)) else { break };
        let mut files: Vec<String> = newer.iter().filter(|(path, e)| older.get(*path) != Some(*e)).map(|(path, _)| path.clone()).collect();
        files.extend(older.keys().filter(|path| !newer.contains_key(*path)).cloned());
        if files.iter().any(|f| check::is_code(f)) {
            return files;
        }
        newer = older;
    }
    vec![]
}

/// The proof steps and the whole suite of `cfg`, once, on the head with `text` as its avp.toml, on
/// this machine as the server runs them: in a folder of the replica's, with the setup and the
/// folders avp.toml keeps, and the services around them. The secrets come from the server's own
/// data when it runs here, else from a variable of the same name; never from anywhere else.
fn prove(client: &Client, repo: &Repo, head: &LineEntry, tree: &Tree, text: &str, cfg: &AvpConfig, last: Option<&Report>) -> Result<Proved> {
    let replica = offline::Replica::open(&client.base, &repo.id)?;
    let objects = &replica.objects;
    let mut cand = tree.clone();
    cand.insert("avp.toml".into(), Entry { blob: objects.put(text.as_bytes())?, mode: 0o100644 });
    let hash = objects.put_tree(&cand)?;
    if let Some(l) = last.filter(|l| l.proof.tree == hash && l.proof.verdict == "green") {
        return Ok(Proved { reused: true, ..l.proof.clone() });
    }
    let missing: Vec<String> = cand.values().map(|e| e.blob.clone()).filter(|b| !objects.has(b)).collect::<BTreeSet<_>>().into_iter().collect();
    for chunk in missing.chunks(200) {
        for bytes in client.get_objects(&repo.id, chunk)? {
            objects.put(&bytes)?;
        }
    }
    let shards = cfg.check.shards();
    let mut named = check::secrets_named(&cfg.check, None, None);
    for s in &shards {
        named.extend(check::secrets_named(&cfg.check, Some(&s.run), Some(&Part::of_shard(s))));
    }
    let mut values = BTreeMap::new();
    if !up::is_team(client) {
        let data = std::env::var("AVP_DATA").map(PathBuf::from).unwrap_or_else(|_| up::home().join("data"));
        values = check::SecretStore::new(&data).values(&repo.id, &named).unwrap_or_default();
    }
    for n in &named {
        if let (false, None, Ok(v)) = (values.contains_key(n), check::refused_secret(n), std::env::var(n)) {
            values.insert(n.clone(), v);
        }
    }
    let on_server: BTreeSet<String> = client.get::<Vec<check::SecretInfo>>(&format!("/repos/{}/secrets", repo.id)).unwrap_or_default().into_iter().map(|s| s.name).collect();
    let files = recent_files(client, repo, head, tree);
    let ws = Workspace::new(&replica.root, "local").with_secrets(values);
    // One proof at a time on this machine: they share the replica's work folder.
    let one = std::fs::File::create(replica.root.join("work.lock"))?;
    one.lock()?;
    let mut runs = Vec::new();
    eprintln!("proving avp.toml on this machine, on position {}: its proof steps...", head.position);
    let mut setup_red = false;
    match ws.check(objects, &cand, &hash, "migrate", &files, None, None) {
        Ok((steps, _)) => {
            for s in steps.iter().filter(|s| s.name.starts_with("service ")) {
                runs.push(run_of("proof", &s.name, s, &on_server));
            }
            let red = steps.iter().any(|s| s.exit != 0);
            for c in &cfg.check.steps {
                match steps.iter().find(|s| s.name == c.name) {
                    Some(s) => runs.push(run_of("proof", &c.name, s, &on_server)),
                    None => {
                        let why = if red { "not run: a step before it is red" } else { "not run: it runs on the tests related to a task's files, and none of the last positions changed code" };
                        runs.push(Run { part: "proof".into(), name: c.name.clone(), command: c.run.clone(), verdict: "skipped".into(), todo: Some(why.into()), ..Default::default() });
                    }
                }
            }
        }
        Err(e) => {
            setup_red = true;
            let s = Step { name: "setup".into(), command: cfg.check.setup.clone().unwrap_or_default(), exit: 1, seconds: 0.0, output: format!("{e:#}") };
            runs.push(run_of("proof", "setup", &s, &on_server));
        }
    }
    for (i, shard) in shards.iter().enumerate() {
        if setup_red {
            runs.push(Run { part: "full".into(), name: shard.name.clone(), command: shard.run.clone(), verdict: "skipped".into(), todo: Some("not run: the setup is red".into()), ..Default::default() });
            continue;
        }
        eprintln!("the whole suite, {} of {}: {}...", i + 1, shards.len(), shard.name);
        match ws.check(objects, &cand, &hash, &format!("migrate-full-{i}"), &[], Some(&shard.run), Some(&Part::of_shard(shard))) {
            Ok((steps, _)) => {
                let s = steps.iter().find(|s| s.exit != 0).or(steps.last());
                if let Some(s) = s {
                    runs.push(run_of("full", &shard.name, s, &on_server));
                }
            }
            Err(e) => runs.push(run_of("full", &shard.name, &Step { name: shard.name.clone(), command: shard.run.clone(), exit: 1, seconds: 0.0, output: format!("{e:#}") }, &on_server)),
        }
    }
    drop(one);
    let verdict = if runs.iter().any(|x| x.verdict == "red") { "red" } else if runs.iter().any(|x| x.verdict == "green") { "green" } else { "none" };
    Ok(Proved { position: head.position, tree: hash, verdict: verdict.into(), at: now_ms(), reused: false, runs })
}

/// A step as the report says it: green, or red with where it went red and what to change.
fn run_of(part: &str, name: &str, s: &Step, on_server: &BTreeSet<String>) -> Run {
    let mut run = Run { part: part.into(), name: name.into(), command: s.command.clone(), verdict: if s.exit == 0 { "green" } else { "red" }.into(), seconds: s.seconds, ..Default::default() };
    if s.exit != 0 {
        run.lines = first_lines(check::red_part(&s.output));
        let (todo, cmd) = what_to_change(&s.name, &s.output, on_server);
        run.todo = Some(todo);
        run.command_to_run = cmd;
    }
    run
}

/// Where the failure starts: from the first line that says an error, at most 8 lines, else the
/// last 8 lines it printed.
fn first_lines(output: &str) -> Vec<String> {
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    let re = Regex::new(r"(?i)error|fail|not found|panic|denied|refused|cannot|no such|not set|not ready|did not").unwrap();
    let from = lines.iter().position(|l| re.is_match(l)).unwrap_or(lines.len().saturating_sub(8));
    lines[from..].iter().take(8).map(|l| l.chars().take(200).collect()).collect()
}

/// What to change for a red step, and the command that does it when there is one: a secret not
/// set, a service never ready, a tool not installed, a setup that fails.
fn what_to_change(step: &str, output: &str, on_server: &BTreeSet<String>) -> (String, Option<String>) {
    let secret = Regex::new(r"the secret (\S+) this step names is not set").unwrap();
    if let Some(c) = secret.captures(output) {
        let n = &c[1];
        if on_server.contains(n) {
            return (format!("{n} is set on the server, whose value this machine does not read: run `avp migrate` on the server's machine, or give it to this run as a variable"), Some(format!("{n}=... avp migrate")));
        }
        return (format!("the secret {n} is not set: a person gives a test value (never a production one) on stdin, then runs `avp migrate` again"), Some(format!("avp secret set {n} < {}.txt", n.to_lowercase())));
    }
    if let Some(service) = step.strip_prefix("service ") {
        let why = if output.contains("neither docker nor podman") {
            "is a container, and this machine has neither docker nor podman: install one on the machines that prove (here, the server, the workers), or make it a `run` service".to_string()
        } else if output.contains("was not ready after") {
            "never got ready: give it more time (`ready_s`), or fix its `ready` command (the lines above say what it printed)".to_string()
        } else {
            "did not start: fix its `image` or `run` in avp.toml (the lines above say why)".to_string()
        };
        return (format!("the service {service} {why}"), None);
    }
    let missing = Regex::new(r"(?m)^(?:\S*sh)(?:: line \d+|: \d+)?: ([^\s:]+): (?:command )?not found").unwrap();
    if let Some(c) = missing.captures(output) {
        let tool = &c[1];
        return (format!("`{tool}` is not installed on this machine: install it here and on the machines that prove (the server, the workers), at the version the CI set up (`avp import-ci` lists them)"), None);
    }
    if step == "setup" || output.starts_with("setup failed") {
        return ("the setup is red: fix `[check] setup` in avp.toml, or what it installs, then run `avp migrate` again".into(), None);
    }
    ("red on the head as it stands: if the CI is green on the same commit, this step needs something its runner had (a tool, a variable, a service): the lines above say what".into(), None)
}

/// Lands the avp.toml proven, as a task of its own: the server proves it again, on its machine,
/// and a change of the checks waits for a person's word. `yes` is that word. A task an earlier run
/// opened for the same avp.toml, still open, is taken again.
fn land(client: &Client, repo: &Repo, r: &mut Report, text: &str, on_line: Option<&str>, last: Option<&Report>, yes: bool) -> Result<()> {
    let c = &mut r.checks;
    if on_line == Some(text) {
        c.state = "landed".into();
        c.position = last.filter(|l| l.checks.hash == c.hash).and_then(|l| l.checks.position);
        c.said = match c.position {
            Some(p) => format!("on the line since position {p}"),
            None => "on the line already".into(),
        };
        return Ok(());
    }
    if r.proof.verdict == "red" {
        c.state = "red".into();
        c.said = "not landed: red on this machine. Change what the plan says, then run `avp migrate` again".into();
        return Ok(());
    }
    let status = |t: &str| client.get::<TaskStatus>(&format!("/repos/{}/tasks/{t}", repo.id)).ok();
    let earlier = last.filter(|l| l.checks.hash == c.hash).and_then(|l| l.checks.task.clone()).filter(|t| status(t).is_some_and(|s| s.task.state == TaskState::Open));
    if let Some(old) = last.and_then(|l| l.checks.task.clone()).filter(|t| Some(t) != earlier.as_ref()) {
        // Opened for an avp.toml changed since: it would wait for good.
        let _: Result<serde_json::Value> = client.post(&format!("/repos/{}/tasks/{old}/abandon", repo.id), &json!({}));
    }
    let person = std::env::var("AVP_PERSON").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "someone".into());
    let t = match earlier {
        Some(t) => t,
        None => {
            let actor = Actor { person: person.clone(), agent: "avp-migrate".into(), session: new_id("s") };
            let task: Task = client.post(&format!("/repos/{}/tasks", repo.id), &OpenTask { goal: GOAL.into(), actor, scope: vec!["avp.toml".into()], transcript: None })?;
            let blob = client.put_object(&repo.id, text.as_bytes().to_vec())?;
            let _: serde_json::Value = client.post(&format!("/repos/{}/tasks/{}/writes", repo.id, task.id), &PutWrite { path: "avp.toml".into(), kind: WriteKind::Put, blob: Some(blob), mode: 0o100644 })?;
            task.id
        }
    };
    c.task = Some(t.clone());
    eprintln!("landing avp.toml: the server proves it on the head ({t})...");
    match client.land(&repo.id, &t)? {
        Ok(l) => {
            c.state = "landed".into();
            c.position = Some(l.position);
            c.said = format!("landed at position {}", l.position);
        }
        Err(n) if n.reason == "approval" && yes => {
            let _: Approval = client.post(&format!("/repos/{}/tasks/{t}/approve", repo.id), &json!({ "person": person, "approved": true, "reason": "avp migrate --yes: proven on this machine" }))?;
            // An approval of a green proof lands it at once, on the server's side.
            let t0 = std::time::Instant::now();
            let mut landed = None;
            while t0.elapsed().as_secs() < 120 {
                match status(&t) {
                    Some(s) if s.task.state == TaskState::Open => std::thread::sleep(std::time::Duration::from_millis(300)),
                    Some(s) => {
                        landed = Some(s.task.landed_position);
                        break;
                    }
                    None => break,
                }
            }
            match landed.flatten() {
                Some(p) => {
                    c.state = "landed".into();
                    c.position = Some(p);
                    c.said = format!("landed at position {p}, approved by {person}");
                }
                None => {
                    c.state = "approval".into();
                    c.said = format!("approved by {person}: it lands once the server's proof is green again (`avp log`)");
                }
            }
        }
        Err(n) if n.reason == "approval" => {
            c.state = "approval".into();
            c.said = format!("green on the server too, waits for a person's word: `avp migrate --yes` (you are that person), or `avp approve {t}`");
        }
        Err(n) => {
            let red = n.proof.as_ref().and_then(|p| p.steps.iter().find(|s| s.exit != 0));
            let why = red.map(|s| format!(": {} on the server: {}", s.name, s.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim())).unwrap_or_default();
            let _: Result<serde_json::Value> = client.post(&format!("/repos/{}/tasks/{t}/abandon", repo.id), &json!({}));
            c.task = None;
            c.state = "refused".into();
            c.said = format!("not landed ({}){why}. Green here, not there: the server's machine lacks what this one has (a tool, a secret)", n.reason);
        }
    }
    Ok(())
}

/// What is left, numbered like docs/MIGRATE.md: what steps 1 and 2 still need, then 3, 4 and 5.
fn plan(r: &Report, cfg: Option<&AvpConfig>, seen: &[up::Seen], top: &Path) -> Vec<Todo> {
    let todo = |step: &str, what: String, command: String| Todo { step: step.into(), what, command };
    let mut out = Vec::new();
    for s in seen.iter().filter(|s| s.state == "found") {
        out.push(todo("1", format!("{}: found, not wired", s.name), s.todo.join("; ")));
    }
    match r.checks.state.as_str() {
        "invalid" | "none" | "refused" => out.push(todo("2", r.checks.said.clone(), "avp migrate".into())),
        "approval" => out.push(todo("2", "avp.toml is green here and on the server, and waits for a person's word".into(), format!("avp migrate --yes   # or: avp approve {}", r.checks.task.as_deref().unwrap_or("<task>")))),
        _ => {}
    }
    let mut told = BTreeSet::new();
    // A service red in the proof is red in each shard of the suite too: said once.
    for x in r.proof.runs.iter().filter(|x| x.verdict == "red" && told.insert(x.todo.clone())) {
        let part = if x.part == "full" { "the whole suite" } else { "the proof" };
        out.push(todo("2", format!("{} ({part}): {}", x.name, x.todo.as_deref().unwrap_or("red")), x.command_to_run.clone().unwrap_or_else(|| "avp migrate   # once it is changed".into())));
    }
    if !r.checks.differs.is_empty() {
        out.push(todo("2", format!("avp.toml is kept, and the CI says {} thing(s) it does not (`differs` in --json)", r.checks.differs.len()), "avp import-ci --write   # avp.imported.toml beside it: carry what you keep, then avp migrate".into()));
    }
    if !r.checks.not_translated.is_empty() {
        out.push(todo("2", format!("the CI has {} thing(s) avp.toml has no word for, each with what to do", r.checks.not_translated.len()), "avp import-ci".into()));
    }
    let gh = github_of(top);
    let slug = gh.clone().unwrap_or_else(|| "<owner>/<name>".into());
    out.push(todo("3.1", "protect main on GitHub, so that only the mirror writes it: no force push, no deletion, no update but the mirror's deploy key; nobody merges a pull request from then on".into(), format!("gh api -X POST repos/{slug}/rulesets --input - <<< '{{\"name\":\"main, written by AVP\",\"target\":\"branch\",\"enforcement\":\"active\",\"conditions\":{{\"ref_name\":{{\"include\":[\"refs/heads/main\"],\"exclude\":[]}}}},\"rules\":[{{\"type\":\"deletion\"}},{{\"type\":\"non_fast_forward\"}},{{\"type\":\"update\"}}],\"bypass_actors\":[{{\"actor_type\":\"DeployKey\",\"bypass_mode\":\"always\"}}]}}'")));
    let imported = git(top, &["rev-parse", "--short", "HEAD"]).map(|s| s.trim().to_string()).unwrap_or_else(|| "HEAD".into());
    out.push(todo("3.2", format!("main becomes the mirror: main must not have moved since the import ({imported}); stop the mirror of avp/line and start the one of main on the server's machine"), format!("avp down && avp mirror --repo {} --branch main --remote origin --follow 15", r.repo)));
    out.push(todo("3.3", "bring in the open pull requests that should not wait, each proven and landed, then closed with its position".into(), format!("GH_TOKEN=... avp import-pr <number> --repo {} --land", r.repo)));
    let envs: Vec<&String> = cfg.map(|c| c.envs.keys().collect()).unwrap_or_default();
    if envs.is_empty() {
        out.push(todo("4", "staging and production by promotion: add [envs.staging] and [envs.production] to avp.toml in a task (docs/MIGRATE.md, step 4)".into(), format!("avp open --repo {} --goal \"staging and production by promotion\" --scope avp.toml", r.repo)));
    } else {
        let names = envs.iter().map(|e| e.as_str()).collect::<Vec<_>>().join(", ");
        out.push(todo("4", format!("avp.toml promotes to {names}: take `push:` out of the deploy workflows' triggers, then promote"), "avp envs && avp promote <env>".into()));
    }
    // Three ordinary days, ending the day the move started.
    let before = crate::stats::parse_when(&r.started, now_ms()).map(|day| crate::stats::rfc3339(day - 3 * 86_400_000)[..10].to_string()).unwrap_or_else(|_| "<from>".into());
    out.push(todo("5", "the before and the after: the days before the move, read from GitHub, beside the days after step 4, read from AVP".into(), format!("GH_TOKEN=... avp stats --github {slug} --before {before}..{} --since <the day after step 4>", r.started)));
    out
}

/// The report as `avp migrate` prints it, and as `avp_migrate_report` answers.
pub fn text(r: &Report) -> String {
    let mut s = format!("avp migrate: {} from {}\n", r.repo, r.checkout);
    for x in &r.setup {
        s.push_str(&format!("  {:8} {}\n", x.what, x.said));
    }
    let what = match r.checks.source.as_str() {
        "checkout" => "the one in this checkout, kept",
        "line" => "the line's, kept",
        "ci" => "written from the CI",
        "guess" => "written from the project's files (no CI read)",
        _ => "none",
    };
    s.push_str(&format!("  checks   avp.toml {what}{}\n", r.checks.file.as_ref().map(|f| format!(": {f}")).unwrap_or_default()));
    for d in &r.checks.differs {
        s.push_str(&format!("           differs: {d}\n"));
    }
    for (i, a) in r.agents.iter().enumerate() {
        s.push_str(&format!("  {}{a}\n", if i == 0 { "agents   " } else { "         " }));
    }
    if !r.proof.runs.is_empty() {
        let when = if r.proof.reused { format!(", as the last run found it ({} UTC)", crate::blame::utc(r.proof.at)) } else { String::new() };
        s.push_str(&format!("Proven on this machine, on position {} with this avp.toml{when}: {}\n", r.proof.position, r.proof.verdict));
        let w = r.proof.runs.iter().map(|x| x.name.len()).max().unwrap_or(0);
        for x in &r.proof.runs {
            let secs = if x.verdict == "skipped" { String::new() } else { format!("  {:.1}s", x.seconds) };
            s.push_str(&format!("  {:7} {:5} {:w$}{secs}\n", x.verdict, x.part, x.name));
            for l in &x.lines {
                s.push_str(&format!("            | {l}\n"));
            }
            if let (Some(t), true) = (&x.todo, x.verdict != "green") {
                s.push_str(&format!("            {t}\n"));
            }
        }
    }
    s.push_str(&format!("avp.toml on the line: {}\n", r.checks.said));
    if !r.plan.is_empty() {
        s.push_str("What is left (docs/MIGRATE.md):\n");
        for t in &r.plan {
            s.push_str(&format!("  {:4} {}\n       {}\n", t.step, t.what, t.command));
        }
    }
    s.push_str(&format!("Not done, on purpose: {}.\n", r.not_done.join("; ")));
    s.push_str(&r.next);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_red_step_says_what_to_change() {
        let none = BTreeSet::new();
        let (t, c) = what_to_change("tests", "the secret STRIPE_KEY this step names is not set for this repo: a person sets it", &none);
        assert!(t.contains("STRIPE_KEY is not set") && c.as_deref() == Some("avp secret set STRIPE_KEY < stripe_key.txt"), "{t}");
        let (t, c) = what_to_change("tests", "the secret STRIPE_KEY this step names is not set for this repo", &BTreeSet::from(["STRIPE_KEY".to_string()]));
        assert!(t.contains("set on the server") && c.is_some_and(|c| c.starts_with("STRIPE_KEY=")), "{t}");
        let (t, _) = what_to_change("service db", "the service db is a container, and this machine has neither docker nor podman: the steps did not run", &none);
        assert!(t.contains("docker nor podman") && t.contains("`run` service"), "{t}");
        let (t, _) = what_to_change("service db", "the service db was not ready after 2s (ready = `false`): the steps did not run", &none);
        assert!(t.contains("ready_s"), "{t}");
        for out in ["sh: pnpm: command not found", "sh: 1: pnpm: not found", "sh: line 1: pnpm: command not found"] {
            assert!(what_to_change("install", out, &none).0.starts_with("`pnpm` is not installed"), "{out}");
        }
        assert!(what_to_change("setup", "setup failed (1):\nnpm ERR!", &none).0.starts_with("the setup is red"));
    }

    #[test]
    fn the_first_failing_lines_start_where_it_fails() {
        let out = "compiling a\ncompiling b\n\nerror[E0425]: cannot find value `x`\n --> src/lib.rs:3:5\nmore\n";
        assert_eq!(first_lines(out), ["error[E0425]: cannot find value `x`", " --> src/lib.rs:3:5", "more"]);
        let quiet: String = (0..20).map(|i| format!("line {i}\n")).collect();
        assert_eq!(first_lines(&quiet).first().map(String::as_str), Some("line 12"));
    }

    #[test]
    fn what_the_ci_says_that_avp_toml_does_not() {
        let kept: AvpConfig = toml::from_str("[check]\n[[check.steps]]\nname = \"tests\"\nrun = \"npm test\"\n").unwrap();
        let ci: AvpConfig = toml::from_str("[check]\nsecrets = [\"STRIPE_KEY\"]\n[[check.steps]]\nname = \"tests\"\nrun = \"npm run test:unit\"\n[[check.steps]]\nname = \"lint\"\nrun = \"npm run lint\"\n[[check.services]]\nname = \"db\"\nrun = \"sleep 9\"\n[[check.full_shards]]\nname = \"e2e\"\nrun = \"npm run e2e\"\n[envs.staging]\nfollow = \"full\"\ndeploy = { run = \"./deploy.sh\" }\n").unwrap();
        let d = differs(&kept, &ci).join("\n");
        for said in ["`tests` runs `npm test` in avp.toml, `npm run test:unit` in the CI", "`lint`", "job `e2e`", "service `db`", "secret STRIPE_KEY", "[envs.staging]"] {
            assert!(d.contains(said), "{said} in:\n{d}");
        }
        assert!(differs(&ci, &ci).is_empty());
    }
}
