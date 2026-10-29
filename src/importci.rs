//! `avp import-ci`: a team's CI read as avp.toml, so that a move off GitHub Actions or GitLab CI
//! takes an afternoon, not a week of writing shards by hand. The jobs a push or a pull request runs
//! become the full suite, one shard each, with their services and their matrix; their fast commands
//! (install, typecheck, lint, tests) become the proof; their deploys the environments. What avp.toml
//! has no word for is listed, one line each, with what to do about it.

use anyhow::{bail, Context, Result};
use regex::{Captures, Regex};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlLoader};

/// What `avp import-ci` proposes.
#[derive(Debug, Serialize)]
pub struct Imported {
    /// The CI files it read, from the repo's root.
    pub sources: Vec<String>,
    /// The proposed avp.toml.
    pub toml: String,
    /// What avp.toml has no place for, with what to do.
    pub not_translated: Vec<Note>,
    /// The secrets its steps and shards name: AVP keeps them, a person sets each once
    /// (`avp secret set <name>`), and the CI's values never leave it.
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    /// The file, and the job when it is one: `ci.yml, job test`.
    pub at: String,
    pub what: String,
    pub todo: String,
}

/// The CI files of the repo at `top`: its GitHub workflows, then its .gitlab-ci.yml.
pub fn ci_files(top: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(top.join(".github/workflows")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "yml" || x == "yaml")).collect();
    files.sort();
    if top.join(".gitlab-ci.yml").is_file() {
        files.push(top.join(".gitlab-ci.yml"));
    }
    files
}

/// The folder whose CI is read: `dir`, or the first one above it that has a CI.
pub fn root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| !ci_files(d).is_empty()).map(Path::to_path_buf)
}

/// Reads the CI of the repo at `top` and proposes its avp.toml.
pub fn import(top: &Path) -> Result<Imported> {
    let files = ci_files(top);
    if files.is_empty() {
        bail!("no CI in {}: avp import-ci reads .github/workflows/*.yml and .gitlab-ci.yml", top.display());
    }
    let mut ci = Ci::default();
    let mut sources = Vec::new();
    for path in &files {
        sources.push(path.strip_prefix(top).unwrap_or(path).to_string_lossy().to_string());
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let doc = std::fs::read_to_string(path).map_err(anyhow::Error::from).and_then(|t| YamlLoader::load_from_str(&t).map_err(|e| anyhow::anyhow!("{e}")));
        let doc = match doc {
            Ok(docs) => merged(docs.into_iter().next().unwrap_or(Yaml::Null)),
            Err(e) => {
                ci.note(&name, "could not be read", format!("{e:#}: fix it and run `avp import-ci` again"));
                continue;
            }
        };
        if name == ".gitlab-ci.yml" {
            gitlab(&name, &doc, &mut ci);
        } else {
            github(&name, &doc, &mut ci);
        }
    }
    let toml = render(top, &sources, &mut ci);
    let cfg: crate::check::AvpConfig = toml::from_str(&toml).context("the proposed avp.toml does not parse: a bug of avp import-ci")?;
    cfg.check.validate().context("the proposed avp.toml does not hold together: a bug of avp import-ci")?;
    let mut secrets: Vec<String> = Vec::new();
    for s in cfg.check.steps.iter().map(|s| &s.secrets).chain(cfg.check.full_shards.iter().map(|s| &s.secrets)).flatten() {
        if !secrets.contains(s) {
            secrets.push(s.clone());
        }
    }
    Ok(Imported { sources, toml, not_translated: ci.notes, secrets })
}

/// Writes the proposal beside the CI: avp.toml, or avp.imported.toml when the repo has one already.
/// Never over a file that is there.
pub fn write(top: &Path, imported: &Imported) -> Result<PathBuf> {
    use std::io::Write;
    let existing = ["avp.toml", "trame.toml"].iter().any(|n| top.join(n).exists());
    let path = top.join(if existing { "avp.imported.toml" } else { "avp.toml" });
    let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!("{} is already there: compare it with `avp import-ci`, remove it, then run `avp import-ci --write` again", path.display()),
        Err(e) => return Err(e.into()),
    };
    f.write_all(imported.toml.as_bytes())?;
    Ok(path)
}

/// The list of what was not translated, then the secrets to set, as TOML comments: printed after
/// the proposal, the whole stays a file `avp.toml` can be.
pub fn report(imported: &Imported) -> String {
    let mut s = String::new();
    if imported.not_translated.is_empty() {
        s.push_str("# Everything the CI says has its place in this avp.toml.\n");
    } else {
        s.push_str(&format!("# Not translated ({}), one line each with what to do:\n", imported.not_translated.len()));
        for n in &imported.not_translated {
            s.push_str(&format!("#   {}: {}: {}\n", n.at, n.what, n.todo));
        }
    }
    if !imported.secrets.is_empty() {
        // Names only: the CI's values stay in the CI, and a value on a command line stays in a history.
        s.push_str(&format!("# Secrets it names ({}): a person sets each once, the value on stdin (typed unseen, or `< file`):\n", imported.secrets.len()));
        for n in &imported.secrets {
            s.push_str(&format!("#   avp secret set {n}\n"));
        }
    }
    s
}

// ---- what a CI says, read ----

#[derive(Default)]
struct Ci {
    checks: Vec<Job>,
    deploys: Vec<EnvDeploy>,
    notes: Vec<Note>,
    /// Runtimes the CI set up on its runners: what, through what, which versions.
    tools: Vec<(String, String, Vec<String>)>,
    /// A cache of Cargo's `target`: kept between proofs instead.
    keep_target: bool,
}

impl Ci {
    fn note(&mut self, at: &str, what: impl Into<String>, todo: impl Into<String>) {
        let n = Note { at: at.into(), what: what.into(), todo: todo.into() };
        if !self.notes.contains(&n) {
            self.notes.push(n);
        }
    }

    fn tool(&mut self, what: &str, via: &str, versions: Vec<String>) {
        let i = match self.tools.iter().position(|t| t.0 == what && t.1 == via) {
            Some(i) => i,
            None => {
                self.tools.push((what.into(), via.into(), vec![]));
                self.tools.len() - 1
            }
        };
        for v in versions {
            if !v.is_empty() && !self.tools[i].2.contains(&v) {
                self.tools[i].2.push(v);
            }
        }
    }
}

/// A job of either CI that runs on a push or a pull request: a shard of the full suite.
struct Job {
    at: String,
    /// The file's name without its extension, to tell two jobs of one name apart.
    stem: String,
    id: String,
    scripts: Vec<Script>,
    /// The variables of the file and of the job, in order.
    env: Vec<(String, String)>,
    services: Vec<Service>,
    /// The matrix's combinations, keys in order; empty without one.
    matrix: Vec<Vec<(String, String)>>,
    /// GitLab hands its matrix to the job as variables; GitHub writes `${{ matrix.<key> }}`.
    matrix_env: bool,
    /// What its commands read that the CI keeps (GitHub's `secrets.` and `vars.`, GitLab's CI/CD
    /// variables): secrets AVP keeps, by name.
    secrets: Vec<String>,
    /// The times of day (UTC, "HH:MM") the CI ran it on a clock: `[[check.schedule]]`.
    clock: Vec<String>,
    /// It ran on a clock and nothing else: as a shard, it runs after each batch of landings too.
    clock_only: bool,
}

struct Script {
    text: String,
    dir: Option<String>,
    env: Vec<(String, String)>,
    /// `continue-on-error`, `allow_failure`: red does not make the job red.
    tolerate: bool,
    /// A GitHub step runs in a shell of its own: a `cd` in it stays in it.
    own_shell: bool,
}

struct Service {
    name: String,
    image: String,
    /// The port the job reached it on (GitHub's host side), and the container's.
    ports: Vec<(u16, u16)>,
    env: Vec<(String, String)>,
    ready: Option<String>,
    /// The host GitLab reaches it by (its alias): 127.0.0.1 on AVP.
    host: Option<String>,
}

struct EnvDeploy {
    name: String,
    at: String,
    how: How,
    /// The CI deployed on every push: AVP promotes each position whose full suite is green.
    follow: bool,
}

enum How {
    /// A GitHub workflow AVP's bridge dispatches, and its inputs.
    Workflow { file: String, inputs: Vec<(String, String)> },
    /// Commands AVP runs itself, on its server.
    Run(String),
}

/// `<<: *anchor` folded in (GitLab templates lean on it): the anchored keys, then the map's own
/// over them.
fn merged(y: Yaml) -> Yaml {
    match y {
        Yaml::Hash(h) => {
            let mut out = Hash::new();
            let mut own = Vec::new();
            for (k, v) in h {
                if k.as_str() == Some("<<") {
                    let sources = match v {
                        Yaml::Array(a) => a,
                        v => vec![v],
                    };
                    for s in sources {
                        if let Yaml::Hash(m) = merged(s) {
                            for (k, v) in m {
                                out.entry(k).or_insert(v);
                            }
                        }
                    }
                } else {
                    own.push((k, merged(v)));
                }
            }
            for (k, v) in own {
                out.insert(k, v);
            }
            Yaml::Hash(out)
        }
        Yaml::Array(a) => Yaml::Array(a.into_iter().map(merged).collect()),
        y => y,
    }
}

/// A scalar as a shell reads it: `20`, `3.10` as written (not 3.1), `true`.
fn text(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A string or a list of them: many keys take either.
fn texts(y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Array(a) => a.iter().filter_map(text).collect(),
        y => text(y).into_iter().collect(),
    }
}

/// A map's entries by their key's text, in order.
fn entries(y: &Yaml) -> Vec<(String, &Yaml)> {
    y.as_hash().map(|h| h.iter().filter_map(|(k, v)| Some((text(k)?, v))).collect()).unwrap_or_default()
}

/// `KEY: value` pairs, in order (GitLab's `KEY: { value: ... }` too).
fn pairs(y: &Yaml) -> Vec<(String, String)> {
    entries(y).into_iter().filter_map(|(k, v)| Some((k, text(v).or_else(|| text(&v["value"]))?))).collect()
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect(pattern)
}

static EXPR: LazyLock<Regex> = LazyLock::new(|| re(r"\$\{\{\s*(.*?)\s*\}\}"));

fn says_deploy(s: &str) -> bool {
    s.to_ascii_lowercase().contains("deploy")
}

/// An environment's name, from what the CI calls it or, failing that, from the job and file names.
fn env_name(candidates: &[&str]) -> String {
    for c in candidates.iter().map(|c| c.to_ascii_lowercase()) {
        let words: Vec<&str> = c.split(|ch: char| !ch.is_ascii_alphanumeric()).collect();
        let found = if c.contains("prod") {
            "production"
        } else if c.contains("stag") {
            "staging"
        } else if c.contains("preview") {
            "preview"
        } else if words.iter().any(|w| *w == "dev" || *w == "development") {
            "development"
        } else if let Some(w) = words.iter().find(|w| ["qa", "uat", "demo", "sandbox", "canary"].contains(w)) {
            w
        } else {
            continue;
        };
        return found.into();
    }
    "production".into()
}

/// A name as a TOML key and AVP read it: letters, digits, `-` and `_`.
fn key(name: &str) -> String {
    let k: String = name.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if k.is_empty() { "default".into() } else { k }
}

/// What a matrix key is in a shard's environment: what avp.toml's `matrix` sets.
fn matrix_var(k: &str) -> String {
    format!("MATRIX_{}", k.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect::<String>())
}

/// GitHub's `${{ ... }}` as a shell reads them: a matrix value (the first combination's when one
/// is given, else `$MATRIX_<KEY>`), `env.`, `secrets.` and `vars.` as variables, the workspace as
/// `$PWD`. Any other stays, and is listed as not translated.
fn expand(text: &str, combo: Option<&[(String, String)]>) -> String {
    EXPR.replace_all(text, |c: &Captures| {
        let e = c[1].trim();
        if let Some(k) = e.strip_prefix("matrix.") {
            return match combo {
                Some(combo) => combo.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default(),
                None => format!("${{{}}}", matrix_var(k)),
            };
        }
        for p in ["env.", "secrets.", "vars."] {
            if let Some(k) = e.strip_prefix(p).filter(|k| k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
                return format!("${{{k}}}");
            }
        }
        match e {
            "github.workspace" => "$PWD".into(),
            "runner.temp" => "$TMPDIR".into(),
            _ => c[0].to_string(),
        }
    })
    .into_owned()
}

/// The expressions of a job's commands that have no value outside GitHub; its secrets and
/// variables, which AVP keeps as secrets, returned by name (`expand` reads them as `${NAME}`).
fn expr_notes(text: &str, at: &str, ci: &mut Ci) -> Vec<String> {
    let mut secrets = Vec::new();
    for c in EXPR.captures_iter(text) {
        let e = c[1].trim();
        if let Some((p, k)) = ["secrets.", "vars."].iter().find_map(|p| Some((*p, e.strip_prefix(p)?))) {
            match crate::check::refused_secret(k) {
                Some(why) => ci.note(at, format!("`{p}{k}`"), format!("{why}: keep what needs it in GitHub")),
                None if !secrets.iter().any(|s| s == k) => secrets.push(k.to_string()),
                None => {}
            }
        } else if !(e.starts_with("matrix.") || e.starts_with("env.") || e == "github.workspace" || e == "runner.temp") {
            ci.note(at, format!("`${{{{ {e} }}}}`"), "has no value outside GitHub: replace it in the shard's run");
        }
    }
    secrets
}

/// Whether a shell text reads the variable `name`: `$NAME` or `${NAME}`.
fn reads(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(i, _)| (text[..i].ends_with('$') || text[..i].ends_with("${")) && !text[i + name.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
}

/// A cron's time of day as `[[check.schedule]]` reads it ("HH:MM", UTC: GitHub's crons are), and
/// what of its rhythm a schedule cannot say, since it runs once a day, every day: whether it ran
/// several times a day, and on some days only. None: not a cron of five fields.
fn daily(cron: &str) -> Option<(String, bool, bool)> {
    let [minute, hour, dom, month, dow] = cron.split_whitespace().collect::<Vec<_>>()[..] else { return None };
    // Its first time: `*/15`, `0,30` and `9-17` start at 0, 0 and 9.
    let first = |field: &str, max: u32| -> Option<u32> {
        let v = field.split(',').next()?.split('/').next()?.split('-').next()?;
        let n = if v == "*" { 0 } else { v.parse().ok()? };
        (n <= max).then_some(n)
    };
    let (m, h) = (first(minute, 59)?, first(hour, 23)?);
    let several = minute.parse::<u32>().is_err() || hour.parse::<u32>().is_err();
    let some_days = [dom, month, dow].iter().any(|f| *f != "*" && *f != "?");
    Some((format!("{h:02}:{m:02}"), several, some_days))
}

static PUBLISH: LazyLock<Regex> = LazyLock::new(|| re(r"\b((npm|pnpm|yarn|poetry|uv) publish|yarn npm publish|cargo publish|twine upload|docker push|gh release|gem push|mvn deploy|gradlew? publish)\b"));

/// The workflows of `.github/workflows`, one file.
fn github(file: &str, doc: &Yaml, ci: &mut Ci) {
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file).to_string();
    let wf_name = doc["name"].as_str().unwrap_or(&stem).to_string();
    // YAML 1.2: `on` is a key like any other, not 1.1's boolean.
    let on_raw = &doc["on"];
    let on: Vec<(String, &Yaml)> = match on_raw {
        Yaml::Hash(_) => entries(on_raw),
        other => texts(other).into_iter().map(|t| (t, &Yaml::Null)).collect(),
    };
    let trigger = |k: &str| on.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    let push = trigger("push");
    let on_push = push.is_some_and(|p| p["tags"].is_badvalue() || !p["branches"].is_badvalue() || !p["branches-ignore"].is_badvalue());
    let on_tags = push.is_some_and(|p| !p["tags"].is_badvalue());
    let on_pr = ["pull_request", "pull_request_target", "merge_group"].iter().any(|k| trigger(k).is_some());
    let dispatch = trigger("workflow_dispatch");
    let jobs = entries(&doc["jobs"]);
    let deployish = |id: &str, job: &Yaml| !job["environment"].is_badvalue() || says_deploy(id) || job["name"].as_str().is_some_and(says_deploy);
    let deploy_wf = !on_pr && (says_deploy(&stem) || says_deploy(&wf_name) || jobs.iter().any(|(id, j)| deployish(id, j)));
    // Its crons: the times of day its checks run on a clock too, or only.
    let mut clock: Vec<String> = Vec::new();
    for cron in trigger("schedule").and_then(Yaml::as_vec).into_iter().flatten().filter_map(|s| s["cron"].as_str()) {
        let what = format!("runs on a schedule (`{cron}`)");
        match daily(cron) {
            _ if deploy_wf => ci.note(file, format!("deploys on a schedule (`{cron}`)"), "AVP promotes on no clock: let cron run `avp promote <env>` at that time"),
            None => ci.note(file, what, "import-ci reads a cron of five fields only: add a `[[check.schedule]]` at its time of day (UTC)"),
            Some((at, several, some_days)) => {
                if several || some_days {
                    let rhythm = [(several, "several times a day"), (some_days, "on some days only")].iter().filter(|r| r.0).map(|r| r.1).collect::<Vec<_>>().join(" and ");
                    let more = if several { "; add one per time of day it should run at" } else { "" };
                    ci.note(file, format!("{what}, {rhythm}"), format!("it becomes one run a day, every day, at {at} UTC: a `[[check.schedule]]` has no other rhythm{more}"));
                }
                if !clock.contains(&at) {
                    clock.push(at);
                }
            }
        }
    }
    if !on_pr && !on_push && !deploy_wf && clock.is_empty() {
        let others: Vec<&str> = on.iter().map(|(k, _)| k.as_str()).filter(|k| !["push", "schedule", "workflow_dispatch"].contains(k)).collect();
        if on_tags {
            ci.note(file, "runs on tags", "AVP makes no tag: tag the commit the mirror wrote for a position (`avp show <position>` names it) and keep this workflow in GitHub");
        } else if others == ["workflow_call"] {
            ci.note(file, "a reusable workflow (workflow_call)", "import-ci reads it where it is called: copy its jobs into the workflow that calls it");
        } else if !others.is_empty() {
            ci.note(file, format!("runs on {}", others.join(", ")), "nothing in AVP starts it: keep it in GitHub");
        } else if dispatch.is_some() && trigger("schedule").is_none() {
            ci.note(file, "started by hand (workflow_dispatch)", "keep it in GitHub, or make it a deploy (`[envs.<name>]`) if it is one");
        }
        return;
    }
    let wf_env = pairs(&doc["env"]);
    for (_, v) in &wf_env {
        expr_notes(v, file, ci);
    }
    let wf_dir = doc["defaults"]["run"]["working-directory"].as_str();
    let wf_shell = doc["defaults"]["run"]["shell"].as_str();
    let env_jobs = jobs.iter().any(|(_, j)| !j["environment"].is_badvalue());
    let mut deployed = false;
    for (id, job) in &jobs {
        let at = format!("{file}, job {id}");
        if let Some(uses) = job["uses"].as_str() {
            if !deploy_wf {
                ci.note(&at, format!("calls the workflow `{uses}`"), "import-ci does not follow it: copy its jobs here and run `avp import-ci` again");
            }
            continue;
        }
        if deploy_wf || deployish(id, job) {
            // In a deploy workflow, the jobs without an environment build what the deploy ships: one
            // dispatch carries them all.
            let named = job["environment"].as_str().or(job["environment"]["name"].as_str()).filter(|n| !n.contains("${{")).map(key);
            let name = match named {
                Some(n) => n,
                None if deploy_wf && (env_jobs || deployed) => continue,
                None => env_name(&[id, job["name"].as_str().unwrap_or(""), &wf_name, &stem]),
            };
            deployed = true;
            let inputs = dispatch_inputs(dispatch, file, ci);
            ci.deploys.push(EnvDeploy { name, at: at.clone(), how: How::Workflow { file: file.into(), inputs }, follow: on_push });
            if on_pr {
                ci.note(&at, format!("deploys from {file}, beside its checks"), "dispatching the workflow would run them all: move this job to a workflow of its own (on workflow_dispatch) and point `github_workflow` at it");
            }
            continue;
        }
        let steps = job["steps"].as_vec().cloned().unwrap_or_default();
        if steps.iter().filter_map(|s| s["run"].as_str()).any(|r| PUBLISH.is_match(r)) {
            ci.note(&at, "publishes (a package, an image, a release)", "a full suite must not publish: keep this job in GitHub, started from a tag on a mirrored commit");
            continue;
        }
        let mut job = gh_job(file, &stem, id, job, &steps, &wf_env, wf_dir, wf_shell, ci);
        job.clock = clock.clone();
        job.clock_only = !on_pr && !on_push;
        ci.checks.push(job);
    }
    if deploy_wf && deployed && dispatch.is_none() {
        ci.note(file, "has no workflow_dispatch", "add `workflow_dispatch:` under `on:`: AVP's bridge starts a deploy that way");
    }
}

/// The inputs AVP's bridge gives a deploy workflow: the promoted commit where it takes one.
fn dispatch_inputs(dispatch: Option<&Yaml>, file: &str, ci: &mut Ci) -> Vec<(String, String)> {
    let Some(dispatch) = dispatch else { return vec![] };
    let mut out = Vec::new();
    for (k, spec) in entries(&dispatch["inputs"]) {
        let lk = k.to_ascii_lowercase();
        if ["sha", "ref", "commit", "commit_sha", "git_sha", "revision", "rev", "git_ref"].contains(&lk.as_str()) {
            out.push((k, "{git}".into()));
        } else if lk == "position" {
            out.push((k, "{position}".into()));
        } else if spec["required"].as_bool() == Some(true) && spec["default"].is_badvalue() {
            ci.note(file, format!("input `{k}` is required"), format!("give it in avp.toml: `inputs = {{ {k} = \"...\" }}` in the deploy"));
        }
    }
    if !out.iter().any(|(_, v)| v == "{git}") {
        ci.note(file, "takes no commit", "it deploys the head of the branch it is dispatched on: give it an input `sha` it checks out, for the deploy to be the promoted position exactly");
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn gh_job(file: &str, stem: &str, id: &str, job: &Yaml, steps: &[Yaml], wf_env: &[(String, String)], wf_dir: Option<&str>, wf_shell: Option<&str>, ci: &mut Ci) -> Job {
    let at = format!("{file}, job {id}");
    let matrix = gh_matrix(&job["strategy"]["matrix"], &at, ci);
    let every: Vec<Vec<(String, String)>> = if matrix.is_empty() { vec![vec![]] } else { matrix.clone() };
    let mut runners: Vec<String> = Vec::new();
    for combo in &every {
        let on = &job["runs-on"];
        for r in texts(on).into_iter().chain(texts(&on["labels"])) {
            let r = expand(&r, Some(combo));
            if (r.contains("macos") || r.contains("windows")) && !runners.contains(&r) {
                runners.push(r);
            }
        }
    }
    if !runners.is_empty() {
        ci.note(&at, format!("runs on {}", runners.join(", ")), "AVP proves on the machines the server and its workers run on: start a worker on that system, or keep this job in GitHub");
    }
    let container = job["container"].as_str().or(job["container"]["image"].as_str());
    if let Some(image) = container {
        ci.note(&at, format!("runs in the container `{image}`"), "AVP runs a job's commands on the machine that proves: install there what the image holds");
    }
    if let Some(cond) = text(&job["if"]) {
        ci.note(&at, format!("runs only `if: {cond}` on GitHub"), "the shard runs it after every batch of landings: drop it from avp.toml if it should not");
    }
    let mut env = wf_env.to_vec();
    env.extend(pairs(&job["env"]));
    let mut secrets: Vec<String> = Vec::new();
    for (_, v) in &env {
        secrets.extend(expr_notes(v, &at, ci));
    }
    let mut services = Vec::new();
    for (name, s) in entries(&job["services"]) {
        let Some(image) = s["image"].as_str().or(s.as_str()).map(|i| expand(i, every.first().map(Vec::as_slice))) else { continue };
        let ports = texts(&s["ports"]).iter().filter_map(|p| {
            let mut sides = p.trim_end_matches("/tcp").rsplitn(2, ':');
            let container: u16 = sides.next()?.parse().ok()?;
            let host = sides.next().and_then(|h| h.parse().ok()).unwrap_or(container);
            Some((host, container))
        }).collect();
        let ready = s["options"].as_str().and_then(|o| HEALTH.captures(o)).map(|c| c[1].trim_matches(|q| q == '"' || q == '\'').to_string());
        services.push(Service { name: key(&name), image, ports, env: pairs(&s["env"]), ready, host: None });
    }
    let dir = job["defaults"]["run"]["working-directory"].as_str().or(wf_dir);
    let job_shell = job["defaults"]["run"]["shell"].as_str().or(wf_shell);
    let mut scripts = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let label = step["name"].as_str().map(|n| format!("step \"{n}\"")).unwrap_or_else(|| format!("step {}", i + 1));
        if let Some(uses) = step["uses"].as_str() {
            gh_uses(uses, &step["with"], &every, &at, ci);
            continue;
        }
        let Some(run) = text(&step["run"]) else { continue };
        if let Some(shell) = step["shell"].as_str().or(job_shell).filter(|s| !s.starts_with("bash") && !s.starts_with("sh")) {
            ci.note(&at, format!("{label} runs in {shell}"), "AVP runs a step with sh: write it as a shell command, or a script the shard calls");
            continue;
        }
        if let Some(cond) = text(&step["if"]) {
            if ["failure()", "always()", "cancelled()"].iter().any(|f| cond.contains(f)) {
                ci.note(&at, format!("{label} runs `if: {cond}`"), "left out: a shard stops at its first red command");
                continue;
            }
            ci.note(&at, format!("{label} runs only `if: {cond}` on GitHub"), "the shard runs it always: drop it, or test the condition in the shell");
        }
        secrets.extend(expr_notes(&run, &at, ci));
        let step_env = pairs(&step["env"]);
        for (_, v) in &step_env {
            secrets.extend(expr_notes(v, &at, ci));
        }
        let dir = step["working-directory"].as_str().or(dir).map(String::from);
        scripts.push(Script { text: run, dir, env: step_env, tolerate: step["continue-on-error"].as_bool() == Some(true), own_shell: true });
    }
    let mut once: Vec<String> = Vec::new();
    for s in secrets {
        if !once.contains(&s) {
            once.push(s);
        }
    }
    Job { at, stem: stem.into(), id: id.into(), scripts, env, services, matrix, matrix_env: false, secrets: once, clock: vec![], clock_only: false }
}

static HEALTH: LazyLock<Regex> = LazyLock::new(|| re(r#"--health-cmd[= ]+("[^"]*"|'[^']*'|\S+)"#));

/// GitHub's `strategy.matrix`, as its combinations: every value of every key, less `exclude`, and
/// `include` added to the combinations it extends or as combinations of its own.
fn gh_matrix(m: &Yaml, at: &str, ci: &mut Ci) -> Vec<Vec<(String, String)>> {
    if let Some(s) = m.as_str() {
        ci.note(at, format!("its matrix comes from `{s}`"), "list its values in avp.toml: `matrix = { <key> = [...] }` on the shard");
        return vec![];
    }
    let flat = |v: &Yaml| text(v).unwrap_or_else(|| format!("{v:?}"));
    let mut combos: Vec<Vec<(String, String)>> = vec![vec![]];
    let mut base: Vec<String> = Vec::new();
    for (k, v) in entries(m) {
        if k == "include" || k == "exclude" {
            continue;
        }
        let values: Vec<String> = match v {
            Yaml::Array(a) => a.iter().map(flat).collect(),
            Yaml::String(s) if s.contains("${{") => {
                ci.note(at, format!("matrix `{k}` comes from `{s}`"), "list its values in avp.toml: `matrix = { <key> = [...] }` on the shard");
                continue;
            }
            v => vec![flat(v)],
        };
        let kk = &k;
        combos = combos.into_iter().flat_map(|c| values.iter().map(move |v| [c.clone(), vec![(kk.clone(), v.clone())]].concat())).collect();
        base.push(k);
    }
    if base.is_empty() {
        combos.clear();
    }
    let entry = |e: &Yaml| -> Vec<(String, String)> { entries(e).into_iter().map(|(k, v)| (k, flat(v))).collect() };
    for ex in m["exclude"].as_vec().into_iter().flatten().map(entry) {
        combos.retain(|c| !ex.iter().all(|(k, v)| c.iter().any(|(ck, cv)| ck == k && cv == v)));
    }
    // The keys each combination holds of its own: the matrix's, or all of an include's that made one.
    let mut own: Vec<Vec<String>> = combos.iter().map(|_| base.clone()).collect();
    for inc in m["include"].as_vec().into_iter().flatten().map(entry) {
        let mut added = false;
        for (c, own) in combos.iter_mut().zip(&own) {
            // Added where it overwrites no value of the combination's own; values added before may change.
            if inc.iter().filter(|(k, _)| own.contains(k)).all(|(k, v)| c.iter().any(|(ck, cv)| ck == k && cv == v)) {
                for (k, v) in &inc {
                    match c.iter_mut().find(|(ck, _)| ck == k) {
                        Some(slot) => slot.1 = v.clone(),
                        None => c.push((k.clone(), v.clone())),
                    }
                }
                added = true;
            }
        }
        if !added {
            own.push(inc.iter().map(|(k, _)| k.clone()).collect());
            combos.push(inc);
        }
    }
    combos
}

/// A step's `uses:`: what AVP has its own way for (the checkout, a runtime, a cache), or a line of
/// what is not translated.
fn gh_uses(uses: &str, with: &Yaml, every: &[Vec<(String, String)>], at: &str, ci: &mut Ci) {
    let (action, reference) = uses.split_once('@').unwrap_or((uses, ""));
    let versions = |keys: &[&str]| -> Vec<String> {
        let mut out = Vec::new();
        for k in keys {
            for combo in every {
                if let Some(v) = text(&with[*k]) {
                    let v = expand(&v, Some(combo));
                    let v = if k.ends_with("-file") { format!("from {v}") } else { v };
                    if !out.contains(&v) {
                        out.push(v);
                    }
                }
            }
        }
        out
    };
    match action.to_ascii_lowercase().as_str() {
        "actions/checkout" => {}
        "actions/setup-node" => ci.tool("Node", action, versions(&["node-version", "node-version-file"])),
        "actions/setup-python" => ci.tool("Python", action, versions(&["python-version", "python-version-file"])),
        "actions/setup-go" => ci.tool("Go", action, versions(&["go-version", "go-version-file"])),
        "actions/setup-java" => {
            let distribution = text(&with["distribution"]).map(|d| format!(" ({d})")).unwrap_or_default();
            ci.tool("Java", action, versions(&["java-version"]).into_iter().map(|v| format!("{v}{distribution}")).collect());
        }
        "pnpm/action-setup" => ci.tool("pnpm", action, versions(&["version"])),
        "oven-sh/setup-bun" => ci.tool("Bun", action, versions(&["bun-version"])),
        "astral-sh/setup-uv" => ci.tool("uv", action, versions(&["version"])),
        "dtolnay/rust-toolchain" => {
            let v = versions(&["toolchain"]);
            ci.tool("Rust", action, if v.is_empty() { vec![reference.to_string()] } else { v });
        }
        "actions/cache" | "actions/cache/restore" | "actions/cache/save" => {
            // AVP's setup is kept between proofs, and so are the folders `keep` names: Cargo's target.
            if text(&with["path"]).is_some_and(|p| p.lines().any(|l| l.trim() == "target" || l.trim().ends_with("/target"))) {
                ci.keep_target = true;
            }
        }
        "swatinem/rust-cache" => ci.keep_target = true,
        "actions/upload-artifact" => ci.note(at, format!("`{uses}`"), "AVP keeps each step's output with the proof, not files: drop it"),
        "actions/download-artifact" => ci.note(at, format!("`{uses}`"), "a shard gets no file from another job: build what it needs in the shard itself"),
        a if a.starts_with("./") => ci.note(at, format!("`{uses}`, an action of the repo"), "AVP runs commands, not actions: put the commands of its action.yml in the shard's run"),
        _ => ci.note(at, format!("`{uses}`"), "AVP runs commands, not actions: put the command it stands for in the shard's run, or drop it"),
    }
}

/// When a GitLab schedule runs, for want of its time: GitLab keeps it in the project, not in the
/// file, and offers this one first ("every day at 4:00").
const GITLAB_AT: &str = "04:00";

const GITLAB_KEYWORDS: &[&str] = &["image", "services", "stages", "types", "before_script", "after_script", "variables", "cache", "default", "include", "workflow"];

/// `.gitlab-ci.yml`.
fn gitlab(file: &str, doc: &Yaml, ci: &mut Ci) {
    let include = &doc["include"];
    let includes: Vec<String> = match include {
        Yaml::Array(a) => a.iter().filter_map(|i| text(i).or_else(|| include_name(i))).collect(),
        Yaml::Hash(_) => include_name(include).into_iter().collect(),
        other => texts(other),
    };
    for inc in includes {
        ci.note(file, format!("includes `{inc}`"), "import-ci reads this file only: paste in the jobs it brings, or translate them by hand");
    }
    let default = &doc["default"];
    let pick = |k: &str| if default[k].is_badvalue() { doc[k].clone() } else { default[k].clone() };
    let globals = pairs(&doc["variables"]);
    for (id, raw) in entries(doc) {
        if GITLAB_KEYWORDS.contains(&id.as_str()) || id.starts_with('.') || !raw.is_hash() {
            continue;
        }
        let job = extended(doc, raw, 0);
        let at = format!("{file}, job {id}");
        let mut vars = globals.clone();
        for (k, v) in pairs(&job["variables"]) {
            vars.retain(|(g, _)| *g != k);
            vars.push((k, v));
        }
        let rules: Vec<&Yaml> = job["rules"].as_vec().map(|r| r.iter().collect()).unwrap_or_default();
        let runnable: Vec<&&Yaml> = rules.iter().filter(|r| r["when"].as_str() != Some("never")).collect();
        if !rules.is_empty() && runnable.is_empty() {
            continue;
        }
        let conds: Vec<String> = runnable.iter().map(|r| r["if"].as_str().unwrap_or("").to_string()).collect();
        let only: Vec<String> = if job["only"].is_hash() { texts(&job["only"]["refs"]) } else { texts(&job["only"]) };
        let schedule = (!conds.is_empty() && conds.iter().all(|c| c.contains("schedule"))) || (!only.is_empty() && only.iter().all(|o| o == "schedules"));
        let tags = (!conds.is_empty() && conds.iter().all(|c| c.contains("CI_COMMIT_TAG") && !c.contains("== null") && !c.contains("!$CI_COMMIT_TAG"))) || (!only.is_empty() && only.iter().all(|o| o == "tags"));
        let manual = job["when"].as_str() == Some("manual") || (!runnable.is_empty() && runnable.iter().all(|r| r["when"].as_str() == Some("manual")));
        if !job["trigger"].is_badvalue() {
            ci.note(&at, "starts another pipeline (trigger)", "AVP runs no downstream pipeline: run `avp import-ci` in that project");
            continue;
        }
        if job["environment"]["action"].as_str() == Some("stop") {
            continue;
        }
        let before = if job["before_script"].is_badvalue() { pick("before_script") } else { job["before_script"].clone() };
        let lines: Vec<String> = script_lines(doc, &before).into_iter().chain(script_lines(doc, &job["script"])).collect();
        let raw_env = job["environment"].as_str().or(job["environment"]["name"].as_str());
        if raw_env.is_some() || job["stage"].as_str() == Some("deploy") || says_deploy(&id) {
            if let Some(raw) = raw_env.filter(|e| e.contains('$')) {
                ci.note(&at, format!("deploys to `{raw}`, an environment per branch"), "AVP has one line and no branch: keep these in GitLab, or give one a `[envs.<name>]` of its own");
                continue;
            }
            if schedule {
                ci.note(&at, "deploys on a schedule", "AVP promotes on no clock: let cron run `avp promote <env>` at that time");
                continue;
            }
            let name = raw_env.map(key).unwrap_or_else(|| env_name(&[&id, job["stage"].as_str().unwrap_or("")]));
            if tags {
                ci.note(&at, "deploys on tags", "AVP makes no tag: it stays a promotion by hand (`avp promote <env>`)");
            }
            if let Some(image) = image_of(&job, &pick("image")) {
                ci.note(&at, format!("deploys from the image `{image}`"), "AVP runs a `run` deploy on its server: install there what the image holds");
            }
            let script = lines.join("\n").replace("${CI_COMMIT_SHA}", "{git}").replace("$CI_COMMIT_SHA", "{git}").replace("${CI_ENVIRONMENT_NAME}", &name).replace("$CI_ENVIRONMENT_NAME", &name).replace("${CI_PROJECT_DIR}", "$PWD").replace("$CI_PROJECT_DIR", "$PWD");
            var_notes(&script, &vars, &at, true, ci);
            // The job's own variables, and those of the file it reads: not the test database's.
            let own: Vec<String> = pairs(&job["variables"]).into_iter().map(|(k, _)| k).collect();
            let used: Vec<(String, String)> = vars.iter().filter(|(k, _)| own.contains(k) || script.contains(&format!("${k}")) || script.contains(&format!("${{{k}}}"))).cloned().collect();
            let run = run_text(&exports(&used).into_iter().chain(lines_of(&script)).collect::<Vec<_>>());
            ci.deploys.push(EnvDeploy { name, at, how: How::Run(run), follow: !manual && !tags });
            continue;
        }
        if schedule {
            ci.note(file, "runs jobs on a schedule whose time GitLab keeps (CI/CD > Schedules), not the file", format!("`[[check.schedule]]` runs them at {GITLAB_AT} UTC, GitLab's every-day preset: put the schedule's own time there, in UTC"));
        }
        if tags {
            ci.note(&at, "runs on tags", "AVP makes no tag: tag the commit the mirror wrote (`avp show <position>` names it) and keep this job in GitLab");
            continue;
        }
        if manual {
            ci.note(&at, "started by hand (when: manual)", "keep it in GitLab, or run its commands yourself when needed");
            continue;
        }
        if lines.iter().any(|l| PUBLISH.is_match(l)) {
            ci.note(&at, "publishes (a package, an image, a release)", "a full suite must not publish: keep this job in GitLab, started from a tag on a mirrored commit");
            continue;
        }
        if let Some(image) = image_of(&job, &pick("image")) {
            let (name, tag) = match image.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
                Some((n, t)) => (n.to_string(), t.to_string()),
                None => (image.clone(), "latest".into()),
            };
            ci.tool(&name, "image", vec![tag]);
        }
        let matrix = match &job["parallel"] {
            Yaml::Integer(n) => {
                ci.note(&at, format!("runs as {n} parallel jobs"), "the shard runs it whole: CI_NODE_INDEX and CI_NODE_TOTAL are not set");
                vec![]
            }
            p => gl_matrix(&p["matrix"]),
        };
        if !job["after_script"].is_badvalue() || !pick("after_script").is_badvalue() {
            ci.note(&at, "has an after_script", "AVP runs nothing after a shard whatever its result: drop it, or end the shard's run with it");
        }
        let services_raw = if job["services"].is_badvalue() { pick("services") } else { job["services"].clone() };
        let services = gl_services(&services_raw, &mut vars);
        let mut known: Vec<String> = vars.iter().map(|(k, _)| k.clone()).collect();
        known.extend(matrix.iter().flatten().map(|(k, _)| k.clone()));
        let script = lines.join("\n").replace("${CI_PROJECT_DIR}", "$PWD").replace("$CI_PROJECT_DIR", "$PWD");
        let secrets = var_notes(&script, &known.iter().map(|k| (k.clone(), String::new())).collect::<Vec<_>>(), &at, false, ci);
        let tolerate = job["allow_failure"].as_bool() == Some(true);
        let clock = if schedule { vec![GITLAB_AT.to_string()] } else { vec![] };
        ci.checks.push(Job { at, stem: "gitlab".into(), id, scripts: vec![Script { text: script, dir: None, env: vec![], tolerate, own_shell: false }], env: vars, services, matrix, matrix_env: true, secrets, clock, clock_only: schedule });
    }
}

/// A job's `image:` (a name, or `{ name: ... }`), else the file's.
fn image_of(job: &Yaml, default: &Yaml) -> Option<String> {
    let y = if job["image"].is_badvalue() { default } else { &job["image"] };
    y.as_str().or(y["name"].as_str()).map(String::from)
}

fn include_name(i: &Yaml) -> Option<String> {
    ["local", "file", "remote", "template", "component", "project"].iter().find_map(|k| text(&i[*k]))
}

/// A GitLab job with what it `extends` folded in, deep: the templates first, the job over them.
fn extended(root: &Yaml, job: &Yaml, depth: usize) -> Yaml {
    let mut base = Yaml::Hash(Hash::new());
    if depth < 10 {
        for parent in texts(&job["extends"]) {
            let p = &root[parent.as_str()];
            if p.is_hash() {
                deep_merge(&mut base, &extended(root, p, depth + 1));
            }
        }
    }
    deep_merge(&mut base, job);
    if let Yaml::Hash(h) = &mut base {
        h.remove(&Yaml::String("extends".into()));
    }
    base
}

fn deep_merge(into: &mut Yaml, from: &Yaml) {
    match (into, from) {
        (Yaml::Hash(a), Yaml::Hash(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    Some(slot) => deep_merge(slot, v),
                    None => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

/// A `script:` as its commands, `!reference [.template, script]` followed (it reads as a list).
fn script_lines(root: &Yaml, y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Array(a) => a
            .iter()
            .flat_map(|item| match item {
                Yaml::Array(path) if path.first().and_then(Yaml::as_str).is_some_and(|p| p.starts_with('.')) => {
                    let mut at = root;
                    for p in path {
                        at = &at[p.as_str().unwrap_or("")];
                    }
                    script_lines(root, at)
                }
                other => script_lines(root, other),
            })
            .collect(),
        y => text(y).into_iter().collect(),
    }
}

/// GitLab's `parallel: matrix:`, as its combinations: every entry, each the product of its keys.
fn gl_matrix(m: &Yaml) -> Vec<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in m.as_vec().into_iter().flatten() {
        let mut combos: Vec<Vec<(String, String)>> = vec![vec![]];
        for (k, v) in entries(entry) {
            let (values, kk) = (texts(v), &k);
            combos = combos.into_iter().flat_map(|c| values.iter().map(move |v| [c.clone(), vec![(kk.clone(), v.clone())]].concat())).collect();
        }
        out.extend(combos.into_iter().filter(|c| !c.is_empty()));
    }
    out
}

/// The port a service of this image listens on, for the common ones.
fn default_port(image: &str) -> Option<u16> {
    let name = image.rsplit('/').next().unwrap_or(image).split(':').next().unwrap_or("");
    Some(match name {
        n if n.starts_with("postgres") || n.starts_with("postgis") || n == "timescaledb" => 5432,
        "mysql" | "mariadb" => 3306,
        "redis" | "valkey" | "keydb" | "redis-stack" => 6379,
        "mongo" | "mongodb" => 27017,
        n if n.starts_with("rabbitmq") => 5672,
        "elasticsearch" | "opensearch" => 9200,
        "memcached" => 11211,
        "minio" => 9000,
        "localstack" => 4566,
        "mailhog" | "mailpit" => 1025,
        _ => return None,
    })
}

/// The variables a GitLab service reads at its start (POSTGRES_PASSWORD...), which the job's
/// variables give it there.
fn service_prefix(image: &str) -> Option<&'static str> {
    let name = image.rsplit('/').next().unwrap_or(image).split(':').next().unwrap_or("");
    Some(match name {
        n if n.starts_with("postgres") || n.starts_with("postgis") => "POSTGRES_",
        "mysql" => "MYSQL_",
        "mariadb" => "MARIADB_",
        "mongo" | "mongodb" => "MONGO_",
        n if n.starts_with("rabbitmq") => "RABBITMQ_",
        _ => return None,
    })
}

/// GitLab's `services:`, each reached by its alias there; the variables a service reads go to it.
fn gl_services(y: &Yaml, vars: &mut Vec<(String, String)>) -> Vec<Service> {
    let mut out = Vec::new();
    for s in y.as_vec().into_iter().flatten() {
        let Some(image) = s.as_str().or(s["name"].as_str()).map(String::from) else { continue };
        let base = image.rsplit('/').next().unwrap_or(&image).split(':').next().unwrap_or(&image).to_string();
        let alias = s["alias"].as_str().map(String::from).unwrap_or_else(|| base.clone());
        let mut env = pairs(&s["variables"]);
        if let Some(prefix) = service_prefix(&image) {
            env.extend(vars.iter().filter(|(k, _)| k.starts_with(prefix)).cloned());
            vars.retain(|(k, _)| !k.starts_with(prefix));
        }
        let ports = default_port(&image).map(|p| vec![(p, p)]).unwrap_or_default();
        out.push(Service { name: key(&alias), image, ports, env, ready: None, host: Some(alias) });
    }
    out
}

static VAR: LazyLock<Regex> = LazyLock::new(|| re(r"\$\{?([A-Z_][A-Z0-9_]*)\}?"));
static ASSIGNED: LazyLock<Regex> = LazyLock::new(|| re(r"(?:^|[\s;(])(?:export\s+)?([A-Z_][A-Z0-9_]*)=|\bfor\s+([A-Z_][A-Z0-9_]*)\s+in\b|\bread\s+(?:-\w+\s+)*([A-Z_][A-Z0-9_]*)"));
const SHELL_VARS: &[&str] = &["HOME", "PATH", "PWD", "OLDPWD", "USER", "SHELL", "TMPDIR", "LANG", "TERM", "CI", "HOSTNAME", "RANDOM", "UID", "IFS"];

/// The variables a GitLab script reads that the file does not set: CI/CD variables (often
/// secrets), or GitLab's own. A check's CI/CD variables are returned, AVP keeps them as secrets; a
/// deploy's are the server's.
fn var_notes(script: &str, known: &[(String, String)], at: &str, deploy: bool, ci: &mut Ci) -> Vec<String> {
    let assigned: Vec<String> = ASSIGNED.captures_iter(script).filter_map(|c| c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().to_string())).collect();
    let mut seen = Vec::new();
    let mut secrets = Vec::new();
    for c in VAR.captures_iter(script) {
        let v = c[1].to_string();
        if seen.contains(&v) || known.iter().any(|(k, _)| *k == v) || assigned.contains(&v) || SHELL_VARS.contains(&v.as_str()) || v.starts_with("MATRIX_") {
            continue;
        }
        seen.push(v.clone());
        if v.starts_with("CI_") || v.starts_with("GITLAB_") {
            ci.note(at, format!("`${v}`"), "set by GitLab only: give it a value in the run, or drop it");
        } else if deploy {
            ci.note(at, format!("`${v}`, no variable of the file"), format!("a CI/CD variable? name {v} in AVP_DEPLOY_ENV where the server runs"));
        } else if let Some(why) = crate::check::refused_secret(&v) {
            ci.note(at, format!("`${v}`, no variable of the file"), format!("{why}: keep what needs it in GitLab"));
        } else {
            secrets.push(v);
        }
    }
    secrets
}

// ---- what avp.toml says ----

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// The dependencies: AVP's setup, once per lockfile.
    Install,
    /// The machine's own setup (apt-get, a global npm install, rustup): not a proof's business.
    Tool,
    Typecheck,
    Lint,
    Test,
    Other,
}

static SLOW: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)playwright|cypress|\be2e\b|puppeteer|detox|lighthouse|\bbench(mark)?s?\b|storybook|\bdocker\b"));
static TOOL: LazyLock<Regex> = LazyLock::new(|| re(r"^(sudo )?(apt(-get)?|yum|dnf|apk|brew|choco|snap) |^(npm|pnpm|yarn) (i|install|add) (-g|--global)\b|^npm (i|install) .*(-g|--global)\b|^(corepack|rustup|nvm|fnm|pipx|asdf|mise) "));
static INSTALL: LazyLock<Regex> = LazyLock::new(|| re(r"^((npm|pnpm) (ci|install|i)|bun install|(python3? -m )?pip3? install|poetry install|uv sync|uv pip install|pipenv (install|sync)|go mod download|composer install)\b|^yarn( install)?( +-[-\w=.]+)*$|^bundle( install)?( +-[-\w=./]+)*$"));
static TYPECHECK: LazyLock<Regex> = LazyLock::new(|| re(r"\b(tsc|vue-tsc|svelte-check|mypy|pyright|basedpyright|typecheck|type-check|check-types)\b|cargo check|go vet"));
static LINT: LazyLock<Regex> = LazyLock::new(|| re(r"\b(eslint|biome|lint|ruff|flake8|pylint|clippy|golangci-lint|gofmt|rubocop|stylelint|prettier|black|isort|shellcheck|ktlint)\b|cargo fmt|format:check"));
static TEST: LazyLock<Regex> = LazyLock::new(|| re(r"\b(test|vitest|jest|mocha|ava|pytest|tox|nox|rspec|phpunit|nextest|coverage)\b|\bverify\b|gradlew? .*\bcheck\b"));

/// A command without the `VAR=value` it starts with.
fn bare(part: &str) -> &str {
    static ASSIGN: LazyLock<Regex> = LazyLock::new(|| re(r#"^[A-Za-z_][A-Za-z0-9_]*=("[^"]*"|'[^']*'|\S*)\s+"#));
    let mut c = part.trim();
    while let Some(m) = ASSIGN.find(c) {
        c = &c[m.end()..];
    }
    c
}

fn kind(part: &str) -> Kind {
    let c = bare(part);
    let first = c.split_whitespace().next().unwrap_or("");
    if ["", "test", "[", "[[", "echo", "printf", "cd", "export", "if", "then", "else", "fi", "for", "do", "done"].contains(&first) {
        Kind::Other
    } else if TOOL.is_match(c) {
        Kind::Tool
    } else if SLOW.is_match(c) {
        Kind::Other
    } else if INSTALL.is_match(c) {
        Kind::Install
    } else if TYPECHECK.is_match(c) {
        Kind::Typecheck
    } else if LINT.is_match(c) {
        Kind::Lint
    } else if TEST.is_match(c) {
        Kind::Test
    } else {
        Kind::Other
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Eco {
    Node,
    Python,
    Other,
}

fn eco(c: &str) -> Eco {
    match c.split_whitespace().next().unwrap_or("") {
        "npm" | "pnpm" | "yarn" | "bun" => Eco::Node,
        "pip" | "pip3" | "python" | "python3" | "poetry" | "uv" | "pipenv" => Eco::Python,
        _ => Eco::Other,
    }
}

/// How a candidate runs Python: from the virtualenv the setup made, or through poetry or uv.
#[derive(Clone, Copy, PartialEq)]
enum Py {
    Venv,
    Poetry,
    Uv,
}

/// The setup a proof starts from, as `avp up` would make it, from the installs the CI runs.
#[derive(Default)]
struct Plan {
    setup: Vec<String>,
    keys: Vec<String>,
    links: Vec<String>,
    node: bool,
    py: Option<Py>,
    /// What a candidate runs before its first Python step: the project itself, from the candidate's
    /// sources (the setup holds only the dependency files).
    root: Option<String>,
    keep: Vec<String>,
}

/// A shell word list, quotes taken off: enough for pip's arguments.
fn words(c: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for ch in c.chars() {
        match (quote, ch) {
            (Some(q), ch) if ch == q => quote = None,
            (Some(_), ch) => cur.push(ch),
            (None, '"' | '\'') => {
                quote = Some(ch);
                any = true;
            }
            (None, ch) if ch.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (None, ch) => cur.push(ch),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

/// A word as sh reads it back.
fn shq(w: &str) -> String {
    if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:@%+=,".contains(c)) {
        w.to_string()
    } else if !w.contains(['"', '\\', '`']) && w.contains('$') {
        format!("\"{w}\"")
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// The commands of a script, one per line: `\` continuations joined, comments and blank lines
/// left out, a heredoc kept whole with the line that opens it.
fn lines_of(text: &str) -> Vec<String> {
    static HEREDOC: LazyLock<Regex> = LazyLock::new(|| re(r#"<<-?\s*['"]?(\w+)['"]?"#));
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut heredoc: Option<String> = None;
    for raw in text.lines() {
        if let Some(end) = &heredoc {
            cur.push('\n');
            cur.push_str(raw);
            if raw.trim() == end {
                heredoc = None;
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let l = raw.trim_end();
        if cur.is_empty() && (l.trim().is_empty() || l.trim_start().starts_with('#')) {
            continue;
        }
        if let Some(head) = l.strip_suffix('\\') {
            cur.push_str(head.trim_end());
            cur.push(' ');
            continue;
        }
        cur.push_str(if cur.is_empty() { l } else { l.trim_start() });
        if let Some(c) = HEREDOC.captures(l) {
            heredoc = Some(c[1].to_string());
            continue;
        }
        out.push(std::mem::take(&mut cur));
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn split_and(line: &str) -> Vec<&str> {
    line.split("&&").map(str::trim).filter(|p| !p.is_empty()).collect()
}

/// A script's lines, each cut at its `&&` into commands with the folder each runs in (None: the
/// repo's root): a `cd` moves the ones after it, in its line and the lines below. A heredoc has no
/// command of its own: it stays as it is.
fn walk(text: &str, start: Option<String>) -> Vec<(String, Vec<(Option<String>, String)>)> {
    let mut cwd = start;
    let mut out = Vec::new();
    for line in lines_of(text) {
        if line.contains('\n') {
            out.push((line, vec![]));
            continue;
        }
        let mut parts = Vec::new();
        for part in split_and(&line) {
            if let Some(d) = part.strip_prefix("cd ").map(str::trim) {
                cwd = moved(cwd.as_deref(), d);
            }
            parts.push((cwd.clone(), part.to_string()));
        }
        out.push((line, parts));
    }
    out
}

/// The folder after `cd d` from `cwd`: None back at the root, as written when it is not relative.
fn moved(cwd: Option<&str>, d: &str) -> Option<String> {
    if d.starts_with(['/', '$', '~', '-']) || d.is_empty() {
        return Some(d.to_string());
    }
    let mut at: Vec<&str> = match cwd {
        Some(c) if c.starts_with(['/', '$', '~', '-']) => return Some(format!("{c}/{d}")),
        Some(c) => c.split('/').collect(),
        None => vec![],
    };
    for c in d.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                at.pop();
            }
            c => at.push(c),
        }
    }
    (!at.is_empty()).then(|| at.join("/"))
}

/// How many folders below the root `cwd` is; None when it is not under it as far as one can tell.
fn depth(cwd: &str) -> Option<usize> {
    (!cwd.starts_with(['/', '$', '~', '-']) && !cwd.split('/').any(|c| c == "..")).then(|| cwd.split('/').filter(|c| !c.is_empty() && *c != ".").count())
}

/// The plan for the CI's checks: the setup from their first installs.
fn plan(top: &Path, ci: &Ci) -> Plan {
    let has = |p: &str| top.join(p).exists();
    let mut installs: Vec<String> = Vec::new();
    let mut cargo = has("Cargo.toml") || ci.keep_target;
    for job in &ci.checks {
        let first = job.matrix.first().map(Vec::as_slice).unwrap_or(&[]);
        for s in &job.scripts {
            for (_, parts) in walk(&expand(&s.text, Some(first)), s.dir.clone()) {
                for (cwd, part) in parts {
                    let c = bare(&part).to_string();
                    cargo |= c.starts_with("cargo ");
                    // An install in a folder of its own is that folder's: the setup runs at the root.
                    if cwd.is_none() && kind(&c) == Kind::Install && !installs.contains(&c) {
                        installs.push(c);
                    }
                }
            }
        }
    }
    let mut p = Plan::default();
    if let Some(node) = installs.iter().find(|c| eco(c) == Eco::Node) {
        let pm = node.split_whitespace().next().unwrap_or("npm");
        let lock = match pm {
            "pnpm" => "pnpm-lock.yaml",
            "yarn" => "yarn.lock",
            "bun" if has("bun.lockb") => "bun.lockb",
            "bun" => "bun.lock",
            _ => "package-lock.json",
        };
        p.setup.push(node.clone());
        p.keys.push("package.json".into());
        if has(lock) || node.starts_with("npm ci") || pm != "npm" {
            p.keys.push(lock.into());
        }
        if pm == "pnpm" && has("pnpm-workspace.yaml") {
            p.keys.push("pnpm-workspace.yaml".into());
        }
        p.links.push("node_modules".into());
        p.node = true;
    }
    let py: Vec<&String> = installs.iter().filter(|c| eco(c) == Eco::Python && !c.starts_with("pipenv")).collect();
    if py.iter().any(|c| c.starts_with("poetry ")) {
        p.setup.push("POETRY_VIRTUALENVS_IN_PROJECT=true poetry install --no-interaction --all-extras --no-root".into());
        p.keys.extend(["pyproject.toml".to_string(), "poetry.lock".to_string()]);
        p.root = Some("poetry install --no-interaction --only-root -q".into());
        p.py = Some(Py::Poetry);
    } else if py.iter().any(|c| c.starts_with("uv sync")) {
        p.setup.push("uv sync --frozen --all-extras --no-install-project".into());
        p.keys.extend(["pyproject.toml".to_string(), "uv.lock".to_string()]);
        p.py = Some(Py::Uv);
    } else if !py.is_empty() {
        // One virtualenv, made once per set of requirements and kept between proofs; the project
        // itself installs in each candidate, as `avp up` does.
        let (mut reqs, mut pkgs, mut proj) = (Vec::new(), Vec::new(), Vec::new());
        for c in &py {
            let w = words(c);
            let Some(at) = w.iter().position(|x| x == "install") else { continue };
            let mut it = w[at + 1..].iter();
            while let Some(x) = it.next() {
                match x.as_str() {
                    "-r" | "--requirement" => reqs.extend(it.next().map(|f| ("-r", f.clone()))),
                    "-c" | "--constraint" => reqs.extend(it.next().map(|f| ("-c", f.clone()))),
                    "-e" | "--editable" => proj.extend(it.next().map(|t| format!("-e {}", shq(t)))),
                    x if x.starts_with("-r") && x.len() > 2 => reqs.push(("-r", x[2..].to_string())),
                    x if x.starts_with('-') => {}
                    x if x == "." || x.starts_with(".[") || x.starts_with("./") => proj.push(shq(x)),
                    "pip" | "setuptools" | "wheel" => {}
                    x => pkgs.push(shq(x)),
                }
            }
        }
        reqs.dedup();
        pkgs.dedup();
        let mut setup = String::from("python3 -m venv .venv");
        if !reqs.is_empty() || !pkgs.is_empty() {
            setup.push_str(" && .venv/bin/pip install -q");
            for (flag, f) in &reqs {
                setup.push_str(&format!(" {flag} {}", shq(f)));
            }
            for pkg in &pkgs {
                setup.push_str(&format!(" {pkg}"));
            }
        }
        p.setup.push(setup);
        if reqs.is_empty() {
            p.keys.extend(["pyproject.toml", "setup.py", "setup.cfg"].into_iter().filter(|k| has(k)).map(String::from));
        } else {
            p.keys.extend(reqs.iter().map(|(_, f)| f.clone()));
        }
        if !proj.is_empty() {
            proj.dedup();
            p.root = Some(format!(".venv/bin/pip install -q {}", proj.join(" ")));
        }
        p.py = Some(Py::Venv);
    }
    if p.py.is_some() {
        p.links.push(".venv".into());
    }
    if cargo {
        // Cargo knows what in `target` is stale: kept, a proof compiles what changed.
        p.keep.push("target".into());
    }
    p
}

const PY_TOOLS: &[&str] = &["pytest", "mypy", "ruff", "flake8", "black", "isort", "pylint", "pyright", "coverage", "tox", "nox", "bandit"];

/// What a command of the CI becomes on AVP: None when the setup does it already (the
/// dependencies) or the machine should have it (a global tool); the project's own install where the
/// CI installed it; a Python tool from the virtualenv.
fn rewrite(part: &str, plan: &Plan, cwd: Option<&str>) -> Option<String> {
    let c = bare(part);
    match kind(part) {
        Kind::Tool => return None,
        Kind::Install if cwd.is_none() && eco(c) == Eco::Node && plan.node => return None,
        Kind::Install if cwd.is_none() && eco(c) == Eco::Python && plan.py.is_some() => {
            let proj = c.starts_with("poetry install") || words(c).iter().any(|w| w == "-e" || w == "--editable" || w == "." || w.starts_with(".[") || w.starts_with("./"));
            return if proj { plan.root.clone() } else { None };
        }
        _ => {}
    }
    let p = part.trim();
    let Some(py) = plan.py else { return Some(p.to_string()) };
    let assign = &p[..p.len() - c.len()];
    let first = c.split_whitespace().next().unwrap_or("");
    let python = first == "python" || first == "python3" || first.starts_with("python3.");
    if !python && !PY_TOOLS.contains(&first) {
        return Some(p.to_string());
    }
    // The virtualenv is at the root: from a folder below it, that many `../` up.
    let up = match cwd.map(depth) {
        None => String::new(),
        Some(Some(n)) => "../".repeat(n),
        Some(None) => return Some(p.to_string()),
    };
    Some(match py {
        Py::Venv if python => format!("{assign}{up}.venv/bin/python{}", &c[first.len()..]),
        Py::Venv => format!("{assign}{up}.venv/bin/python -m {c}"),
        Py::Poetry => format!("{assign}poetry run {c}"),
        Py::Uv => format!("{assign}uv run --frozen {c}"),
    })
}

/// A command of a check job, as the proof would run it.
struct Cmd {
    kind: Kind,
    /// As the CI wrote it.
    part: String,
    /// As AVP runs it: the folder it ran in first.
    run: String,
    env: Vec<(String, String)>,
    /// GitLab's matrix values, which the CI's own command may read and a related run does not.
    matrix: Vec<(String, String)>,
    /// The job's secrets it reads, itself or through the variables it gets.
    secrets: Vec<String>,
}

/// The typecheck, lint and test commands of a job, its matrix's first combination filled in.
fn commands(job: &Job, plan: &Plan) -> Vec<Cmd> {
    let first: &[(String, String)] = job.matrix.first().map(Vec::as_slice).unwrap_or(&[]);
    let env = job.env.clone();
    let matrix: Vec<(String, String)> = if job.matrix_env { first.to_vec() } else { vec![] };
    let mut out = Vec::new();
    // A red the CI tolerates is no proof's business.
    for s in job.scripts.iter().filter(|s| !s.tolerate) {
        let env: Vec<(String, String)> = env.iter().chain(&s.env).map(|(k, v)| (k.clone(), expand(v, Some(first)))).collect();
        for (_, parts) in walk(&expand(&s.text, Some(first)), s.dir.clone()) {
            for (cwd, part) in parts {
                let kind = kind(&part);
                if !matches!(kind, Kind::Typecheck | Kind::Lint | Kind::Test) {
                    continue;
                }
                let Some(run) = rewrite(&part, plan, cwd.as_deref()) else { continue };
                let run = match &cwd {
                    Some(d) => format!("cd {} && {run}", shq(d)),
                    None => run,
                };
                let secrets = job.secrets.iter().filter(|n| reads(&part, n) || env.iter().any(|(_, v)| reads(v, n))).cloned().collect();
                out.push(Cmd { kind, part, run, env: env.clone(), matrix: matrix.clone(), secrets });
            }
        }
    }
    out
}

/// `export A=1 B=2`, the variables a step needs that AVP does not set itself.
fn exports(env: &[(String, String)]) -> Option<String> {
    let set: Vec<String> = env.iter().filter(|(k, v)| k != "CI" && *v != format!("${{{k}}}") && *v != format!("${k}") && !v.contains("${{")).map(|(k, v)| format!("{k}={}", shq(v))).collect();
    (!set.is_empty()).then(|| format!("export {}", set.join(" ")))
}

/// Lines as one `run`: one line as it is, a few simple ones joined by `&&`, else a script that
/// stops at its first red command, as a CI job does.
fn run_text(lines: &[String]) -> String {
    let simple = |l: &String| {
        let first = l.split_whitespace().next().unwrap_or("");
        !l.contains('\n') && !l.contains('#') && !l.ends_with([';', '&', '|', '\\', '{', '(']) && !["if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "{", "}", "(", ")", "function"].contains(&first) && !l.trim_end().ends_with(" do") && !l.trim_end().ends_with(" then")
    };
    match lines {
        [one] => one.clone(),
        many if many.len() <= 4 && many.iter().all(simple) => many.join(" && "),
        many => format!("set -e\n{}\n", many.join("\n")),
    }
}

/// The test runner of a Node project, for the tests related to a task's files.
fn js_runner(top: &Path, ci: &Ci, plan: &Plan) -> Option<&'static str> {
    if !plan.node {
        return None;
    }
    let pkg: serde_json::Value = std::fs::read_to_string(top.join("package.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let dep = |n: &str| ["dependencies", "devDependencies"].iter().any(|k| pkg[*k].get(n).is_some());
    let said = |n: &str| ci.checks.iter().flat_map(|j| &j.scripts).any(|s| s.text.contains(n));
    if dep("vitest") || said("vitest") {
        Some("node_modules/.bin/vitest related --run --passWithNoTests {files}")
    } else if dep("jest") || said("jest") {
        Some("node_modules/.bin/jest --findRelatedTests --passWithNoTests {files}")
    } else {
        None
    }
}

fn js_test(part: &str) -> bool {
    let first = bare(part).split_whitespace().next().unwrap_or("");
    ["npm", "pnpm", "yarn", "bun", "npx", "pnpx", "turbo", "nx", "lerna", "vitest", "jest"].contains(&first) || first.starts_with("node_modules/.bin/")
}

/// The proof's steps: the typechecks, lints and tests of the check jobs, each once, the tests a
/// runner can relate to a task's files run that way. Each with the secrets of the commands it
/// stands for.
fn proof(top: &Path, ci: &Ci, plan: &Plan) -> Vec<(String, String, bool, Vec<String>)> {
    let related = js_runner(top, ci, plan);
    let cmds: Vec<Cmd> = ci.checks.iter().flat_map(|j| commands(j, plan)).collect();
    // (kind, run, the CI's own command, related, secrets)
    let mut picked: Vec<(Kind, String, String, bool, Vec<String>)> = Vec::new();
    let add = |secrets: &mut Vec<String>, more: &[String]| {
        for n in more {
            if !secrets.contains(n) {
                secrets.push(n.clone());
            }
        }
    };
    for kind in [Kind::Typecheck, Kind::Lint, Kind::Test] {
        for c in cmds.iter().filter(|c| c.kind == kind) {
            let with_env = |run: &str, env: &[(String, String)]| match exports(env) {
                Some(e) if kind == Kind::Test => format!("{e} && {run}"),
                _ => run.to_string(),
            };
            if let (Kind::Test, Some(related)) = (kind, related) {
                if js_test(&c.part) {
                    match picked.iter_mut().find(|p| p.3) {
                        Some(p) => add(&mut p.4, &c.secrets),
                        None => picked.push((kind, with_env(related, &c.env), c.part.clone(), true, c.secrets.clone())),
                    }
                    continue;
                }
            }
            let run = with_env(&c.run, &[c.env.clone(), c.matrix.clone()].concat());
            match picked.iter_mut().find(|p| p.1 == run) {
                Some(p) => add(&mut p.4, &c.secrets),
                None => picked.push((kind, run, c.part.clone(), false, c.secrets.clone())),
            }
        }
    }
    let label = |k: Kind| match k {
        Kind::Typecheck => "typecheck",
        Kind::Lint => "lint",
        _ => "tests",
    };
    let mut steps: Vec<(String, String, bool, Vec<String>)> = Vec::new();
    let mut rooted = plan.root.is_none();
    for (kind, run, part, related, secrets) in &picked {
        let alone = picked.iter().filter(|p| p.0 == *kind).count() == 1;
        let mut name = if *related { "tests related to the task's files".to_string() } else if alone { label(*kind).to_string() } else { part.chars().take(60).collect() };
        if steps.iter().any(|s| s.0 == name) {
            name = format!("{name} ({})", steps.len() + 1);
        }
        let python = run.contains(".venv/bin/") || run.contains("poetry run ") || run.contains("uv run ");
        let run = match &plan.root {
            Some(root) if python && !rooted => {
                rooted = true;
                format!("{root} && {run}")
            }
            _ => run.clone(),
        };
        steps.push((name, run, *related, secrets.clone()));
    }
    steps
}

/// The services of every check job, one per name; one of the same name on another image is
/// renamed, in its job too.
fn services(ci: &mut Ci) -> Vec<Service> {
    let mut all: Vec<Service> = Vec::new();
    let mut renames: Vec<(usize, String, String)> = Vec::new();
    for (j, job) in ci.checks.iter().enumerate() {
        for s in &job.services {
            match all.iter_mut().find(|a| a.name == s.name) {
                Some(a) if a.image == s.image => {
                    for e in &s.env {
                        if !a.env.contains(e) {
                            a.env.push(e.clone());
                        }
                    }
                }
                Some(_) => {
                    let name = format!("{}-{}", s.name, all.len() + 1);
                    renames.push((j, s.name.clone(), name.clone()));
                    all.push(Service { name, image: s.image.clone(), ports: s.ports.clone(), env: s.env.clone(), ready: s.ready.clone(), host: s.host.clone() });
                }
                None => all.push(Service { name: s.name.clone(), image: s.image.clone(), ports: s.ports.clone(), env: s.env.clone(), ready: s.ready.clone(), host: s.host.clone() }),
            }
        }
    }
    for (j, from, to) in renames {
        if let Some(s) = ci.checks[j].services.iter_mut().find(|s| s.name == from) {
            s.name = to;
        }
    }
    all
}

/// The job's variables that reach one of its services (`localhost:5432`, GitLab's `postgres:5432`)
/// move to that service's env, on the port AVP picks for it (`{port}`): each proof gets its own.
fn wire(job: &mut Job, all: &mut [Service]) {
    let mut keep = Vec::new();
    for (k, v) in std::mem::take(&mut job.env) {
        let mut moved = false;
        for s in &job.services {
            let Some(target) = all.iter_mut().find(|a| a.name == s.name) else { continue };
            let mut value = v.clone();
            for (i, (host_port, container)) in s.ports.iter().enumerate() {
                let port = if i == 0 { "{port}".to_string() } else { format!("{{port:{container}}}") };
                let mut hosts = vec!["localhost".to_string(), "127.0.0.1".to_string()];
                hosts.extend(s.host.clone());
                for h in &hosts {
                    let p = if s.host.as_deref() == Some(h.as_str()) { *container } else { *host_port };
                    value = value.replace(&format!("{h}:{p}"), &format!("127.0.0.1:{port}"));
                }
                if let Some(h) = &s.host {
                    for sep in ["@", "//"] {
                        value = value.replace(&format!("{sep}{h}/"), &format!("{sep}127.0.0.1:{port}/"));
                    }
                }
                if i == 0 && k.contains("PORT") && (v == host_port.to_string() || v == container.to_string()) {
                    value = port.clone();
                }
            }
            if s.host.as_deref() == Some(v.as_str()) {
                value = "127.0.0.1".into();
            }
            if value != v {
                if !target.env.iter().any(|(ek, _)| *ek == k) {
                    target.env.push((k.clone(), value));
                }
                moved = true;
                break;
            }
        }
        if !moved {
            keep.push((k, v));
        }
    }
    job.env = keep;
}

/// A shard's `matrix`: the keys its commands read, and whether their combinations are every value
/// of every key (else one shard per combination, each with its own values).
fn shard_matrix(job: &Job, ci: &mut Ci) -> Vec<Vec<(String, Vec<String>)>> {
    if job.matrix.is_empty() {
        return vec![vec![]];
    }
    let read: String = job.scripts.iter().map(|s| s.text.clone()).chain(job.env.iter().map(|(_, v)| v.clone())).chain(job.scripts.iter().flat_map(|s| s.env.iter().map(|(_, v)| v.clone()))).collect::<Vec<_>>().join("\n");
    let mut keys: Vec<String> = Vec::new();
    for (k, _) in job.matrix.iter().flatten() {
        if keys.contains(k) {
            continue;
        }
        let used = job.matrix_env || EXPR.captures_iter(&read).any(|c| c[1].trim().strip_prefix("matrix.").is_some_and(|m| m == k));
        let values: Vec<String> = job.matrix.iter().filter_map(|c| c.iter().find(|(ck, _)| ck == k).map(|(_, v)| v.clone())).fold(Vec::new(), |mut a, v| {
            if !a.contains(&v) {
                a.push(v);
            }
            a
        });
        if used {
            keys.push(k.clone());
        } else if values.len() > 1 {
            ci.note(&job.at, format!("matrix `{k}` ({}) only sets up the runner", values.join(", ")), format!("AVP runs the shard once, on the machine that proves: to run it per value, add `{k}` to its `matrix` and read ${} in its run", matrix_var(k)));
        }
    }
    if keys.is_empty() {
        return vec![vec![]];
    }
    let mut combos: Vec<Vec<(String, String)>> = Vec::new();
    for c in &job.matrix {
        let p: Vec<(String, String)> = keys.iter().filter_map(|k| c.iter().find(|(ck, _)| ck == k).cloned()).collect();
        if !combos.contains(&p) {
            combos.push(p);
        }
    }
    let values: Vec<(String, Vec<String>)> = keys.iter().map(|k| (k.clone(), combos.iter().filter_map(|c| c.iter().find(|(ck, _)| ck == k).map(|(_, v)| v.clone())).fold(Vec::new(), |mut a, v| {
        if !a.contains(&v) {
            a.push(v);
        }
        a
    }))).collect();
    let product: usize = values.iter().map(|(_, v)| v.len()).product();
    if product == combos.len() && combos.iter().all(|c| c.len() == keys.len()) {
        vec![values]
    } else {
        combos.into_iter().map(|c| c.into_iter().map(|(k, v)| (k, vec![v])).collect()).collect()
    }
}

/// A shard's run: the job's variables, then its commands as AVP runs them (the installs the setup
/// does left out), each GitHub step that moves or sets variables in a subshell of its own.
fn shard_run(job: &Job, plan: &Plan) -> Vec<String> {
    let mut env: Vec<(String, String)> = job.env.iter().map(|(k, v)| (k.clone(), expand(v, None))).collect();
    let mut lines: Vec<String> = Vec::new();
    if job.matrix_env {
        let mut keys: Vec<&String> = job.matrix.iter().flatten().map(|(k, _)| k).collect();
        keys.dedup();
        for k in keys {
            if !env.iter().any(|(e, _)| e == k) {
                env.push((k.clone(), format!("${}", matrix_var(k))));
            }
        }
    }
    lines.extend(exports(&env));
    let scripts: Vec<(&Script, Vec<String>)> = job
        .scripts
        .iter()
        .map(|s| {
            let mut out = Vec::new();
            for (line, parts) in walk(&expand(&s.text, None), s.dir.clone()) {
                if parts.is_empty() {
                    out.push(line);
                    continue;
                }
                let kept: Vec<String> = parts.iter().filter_map(|(cwd, p)| rewrite(p, plan, cwd.as_deref())).collect();
                if kept.iter().eq(parts.iter().map(|(_, p)| p)) {
                    out.push(line.clone());
                } else if !kept.is_empty() {
                    out.push(kept.join(" && "));
                } else if line.starts_with([' ', '\t']) {
                    // Inside an `if` or a loop, a body must not end up empty.
                    out.push(":".into());
                }
            }
            (s, out)
        })
        .filter(|(_, out)| !out.is_empty())
        .collect();
    let n = scripts.len();
    for (i, (s, body)) in scripts.into_iter().enumerate() {
        let mut pre: Vec<String> = Vec::new();
        pre.extend(exports(&s.env.iter().map(|(k, v)| (k.clone(), expand(v, None))).collect::<Vec<_>>()));
        if let Some(d) = &s.dir {
            pre.push(format!("cd {}", shq(d)));
        }
        let moves = !pre.is_empty() || body.iter().any(|l| l.starts_with("cd ") || l.contains("&& cd ") || l.starts_with("export "));
        let all: Vec<String> = pre.into_iter().chain(body).collect();
        if s.tolerate || (s.own_shell && moves && i + 1 < n) {
            // A subshell keeps its `cd` and its variables to itself; `set -e` holds in it too.
            let inner = run_text(&all);
            let inner = if inner.contains('\n') { format!("(\n{}\n)", all.join("\n")) } else { format!("({inner})") };
            lines.push(if s.tolerate { format!("{inner} || true") } else { inner });
        } else {
            lines.extend(all);
        }
    }
    lines
}

/// A TOML string: a literal block for a script, readable as the CI had it.
fn q(s: &str) -> String {
    if s.contains('\n') && !s.contains("'''") && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return format!("'''\n{}\n'''", s.trim_end_matches('\n'));
    }
    toml::Value::String(s.to_string()).to_string()
}

fn list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|i| q(i)).collect::<Vec<_>>().join(", "))
}

fn table(pairs: &[(String, String)]) -> String {
    format!("{{ {} }}", pairs.iter().map(|(k, v)| format!("{} = {}", toml_key(k), q(v))).collect::<Vec<_>>().join(", "))
}

fn toml_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') { k.to_string() } else { q(k) }
}

fn render(top: &Path, sources: &[String], ci: &mut Ci) -> String {
    let mut all = services(ci);
    for job in ci.checks.iter_mut() {
        wire(job, &mut all);
    }
    let tools: Vec<(String, String, Vec<String>)> = ci.tools.clone();
    let plan = plan(top, ci);
    // Machine setup the shards leave out: said once, where it was.
    let mut tool_notes = Vec::new();
    for job in &ci.checks {
        for s in &job.scripts {
            for line in lines_of(&expand(&s.text, None)) {
                for part in split_and(&line) {
                    if kind(part) == Kind::Tool {
                        tool_notes.push((job.at.clone(), part.to_string()));
                    }
                }
            }
        }
    }
    for (at, part) in tool_notes {
        ci.note(&at, format!("`{part}`"), "sets up the machine, left out of the shard: run it once where proofs run (the server, the workers)");
    }
    let steps = proof(top, ci, &plan);

    let mut t = format!("# avp.toml proposed by `avp import-ci` from {}: edit freely.\n", sources.join(", "));
    t.push_str("# [check] is what a task proves before it lands (the CI's install, typecheck, lint and tests);\n# `full_shards` the whole suite after each batch of landings, one per CI job; `envs` its deploys.\n");
    if !tools.is_empty() {
        t.push_str("# The CI set these up on its runners; the machines that prove (the server, the workers) need them:\n");
        for (what, via, versions) in &tools {
            if via == "image" {
                t.push_str(&format!("#   {what}:{} (image)\n", versions.join(", ")));
            } else if versions.is_empty() {
                t.push_str(&format!("#   {what} ({via})\n"));
            } else {
                t.push_str(&format!("#   {what} {} ({via})\n", versions.join(", ")));
            }
        }
    }
    if !all.is_empty() {
        t.push_str("# Its services run in containers: docker or podman on those machines.\n");
    }
    t.push_str("[check]\n");
    if !plan.setup.is_empty() {
        t.push_str(&format!("setup = {}\nsetup_keys = {}\nsetup_links = {}\n", q(&plan.setup.join(" && ")), list(&plan.keys), list(&plan.links)));
    }
    if !plan.keep.is_empty() {
        t.push_str(&format!("keep = {}\n", list(&plan.keep)));
    }
    if steps.is_empty() && !ci.checks.is_empty() {
        t.push_str("# No typecheck, lint nor test command in the CI's jobs: add the steps a task should prove.\n");
    }
    if ci.checks.is_empty() {
        t.push_str("# No job of the CI runs on a push or a pull request: `avp up` guesses the checks from the project.\n");
    }
    for (name, run, needs, secrets) in &steps {
        t.push_str(&format!("\n[[check.steps]]\nname = {}\nrun = {}\n", q(name), q(run)));
        if *needs {
            t.push_str("needs_files = true\n");
        }
        if !secrets.is_empty() {
            t.push_str(&format!("secrets = {}\n", list(secrets)));
        }
    }
    for s in &all {
        t.push_str(&format!("\n[[check.services]]\nname = {}\nimage = {}\n", q(&s.name), q(&s.image)));
        if !s.ports.is_empty() {
            t.push_str(&format!("ports = [{}]\n", s.ports.iter().map(|(_, c)| c.to_string()).collect::<Vec<_>>().join(", ")));
        }
        if let Some(r) = &s.ready {
            t.push_str(&format!("ready = {}\n", q(r)));
        }
        if !s.env.is_empty() {
            t.push_str(&format!("env = {}\n", table(&s.env)));
        }
    }
    let mut names: Vec<String> = Vec::new();
    // Each time of day the CI ran jobs on a clock, with the shards they became.
    let mut clock: Vec<(String, Vec<String>)> = Vec::new();
    let jobs = std::mem::take(&mut ci.checks);
    for job in &jobs {
        let run = shard_run(job, &plan);
        if run.iter().all(|l| l.starts_with("export ")) {
            ci.note(&job.at, "has no command left once its actions are set aside", "nothing of it runs in the full suite: add its commands to a shard if it needs some");
            continue;
        }
        let run = run_text(&run);
        // Two files may both have a job `test`: the second takes its file's name.
        let name = if names.contains(&job.id) { format!("{}-{}", job.stem, job.id) } else { job.id.clone() };
        names.push(name.clone());
        for matrix in shard_matrix(job, ci) {
            t.push_str(&format!("\n[[check.full_shards]]\nname = {}\nrun = {}\n", q(&name), q(&run)));
            let mine: Vec<String> = job.services.iter().map(|s| s.name.clone()).collect();
            if !all.is_empty() && mine.len() != all.len() {
                t.push_str(&format!("services = {}\n", list(&mine)));
            }
            if !matrix.is_empty() {
                t.push_str(&format!("matrix = {{ {} }}\n", matrix.iter().map(|(k, v)| format!("{} = {}", toml_key(k), list(v))).collect::<Vec<_>>().join(", ")));
            }
            if !job.secrets.is_empty() {
                t.push_str(&format!("secrets = {}\n", list(&job.secrets)));
            }
        }
        for at in &job.clock {
            match clock.iter_mut().find(|(a, _)| a == at) {
                Some((_, shards)) => shards.push(name.clone()),
                None => clock.push((at.clone(), vec![name.clone()])),
            }
        }
        if job.clock_only {
            ci.note(&job.at, "runs only on a schedule", format!("as a shard, it runs after each batch of landings too, not only at {} UTC: if that is too often, leave it out and keep it in the CI", job.clock.join(", ")));
        }
    }
    ci.checks = jobs;
    clock.sort();
    for (at, shards) in &clock {
        t.push_str(&format!("\n[[check.schedule]]\nat = {}\nshards = {}\n", q(at), list(shards)));
    }
    let mut envs: Vec<&EnvDeploy> = Vec::new();
    let mut doubles = Vec::new();
    for d in &ci.deploys {
        match envs.iter().find(|e| e.name == d.name) {
            Some(first) => doubles.push((d.at.clone(), d.name.clone(), first.at.clone())),
            None => envs.push(d),
        }
    }
    // Production last: it comes after staging, in the file as on the line.
    envs.sort_by_key(|e| e.name == "production");
    let staging = envs.iter().any(|e| e.name == "staging");
    for e in &envs {
        t.push_str(&format!("\n[envs.{}]\n", e.name));
        if e.follow {
            t.push_str("follow = \"full\"\n");
        } else if e.name == "production" && staging {
            t.push_str("after = \"staging\"\n");
        }
        match &e.how {
            How::Workflow { file, inputs } if inputs.is_empty() => t.push_str(&format!("deploy = {{ github_workflow = {} }}\n", q(file))),
            How::Workflow { file, inputs } => t.push_str(&format!("deploy = {{ github_workflow = {}, inputs = {} }}\n", q(file), table(inputs))),
            How::Run(run) if !run.contains('\n') => t.push_str(&format!("deploy = {{ run = {} }}\n", q(run))),
            How::Run(run) => t.push_str(&format!("\n[envs.{}.deploy]\nrun = {}\n", e.name, q(run))),
        }
    }
    for (at, name, first) in doubles {
        ci.note(&at, format!("also deploys {name}"), format!("avp.toml has one deploy per environment ({first}'s is): call this one from it, or give it an environment of its own"));
    }
    t
}
