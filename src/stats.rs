//! `avp stats`: a team's before and after, from what the server already records. Murmell's README
//! table (to staging 91 -> 8.5 min, CI machine time -77 %, 131 landings) was computed by hand; a
//! team trying AVP gets its own in one command (`GET /repos/{repo}/stats`, `avp stats`, `avp_stats`).
//!
//! Here, what a client reads and prints of them, GitHub's before it reads itself, and the times they
//! are said in; AVP Cloud's server computes them (src/cloud/stats.rs).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// How long something took over many of them, in seconds. No median nor p90 when there is none.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Spread {
    pub count: usize,
    pub median_s: Option<f64>,
    pub p90_s: Option<f64>,
}

impl Spread {
    pub fn of(mut v: Vec<f64>) -> Self {
        v.sort_by(f64::total_cmp);
        Spread { count: v.len(), median_s: quantile(&v, 0.5).map(tenth), p90_s: quantile(&v, 0.9).map(tenth) }
    }
}

/// The q-quantile of sorted values, between the two nearest ranks.
fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    let last = sorted.len().checked_sub(1)?;
    let at = q * last as f64;
    let (lo, hi) = (at.floor() as usize, at.ceil() as usize);
    Some(sorted[lo] + (sorted[hi] - sorted[lo]) * (at - lo as f64))
}

pub(crate) fn tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// One environment: how long a landing takes to be served there, and its deploys in the window.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EnvStats {
    /// From the landing to the first time the environment serves it (or a later position).
    pub landed_to_served: Spread,
    /// From the task's opening: the README's "to staging".
    pub opened_to_served: Spread,
    pub promotions: usize,
    /// Times the environment moved: a deploy that ended well, or one observed.
    pub moves: usize,
    pub failed: usize,
    /// Deploys AVP ran that a restart of the server cut.
    pub interrupted: usize,
}

/// What proofs and full suites cost the machines that ran them (the server's slots and the
/// workers alike), in minutes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MachineMinutes {
    pub proofs: f64,
    pub full_suites: f64,
    pub total: f64,
}

/// `GET /repos/{repo}/stats`. AVP Live charts it: fields are added, never renamed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Stats {
    pub repo: String,
    /// The window, RFC3339 (UTC): `since` included, `until` not.
    pub since: String,
    pub until: String,
    /// Tasks landed in the window (imports are not landings).
    pub landings: usize,
    pub opened_to_landed: Spread,
    /// From the landing to the first full suite verdict on it or a later position.
    pub landed_to_full: Spread,
    pub envs: BTreeMap<String, EnvStats>,
    /// Tasks' proofs made in the window, speculative ones included.
    pub proofs: usize,
    pub proofs_red: usize,
    pub proofs_blocked: usize,
    /// How long a task's proof took, from its first step to its last (split shares side by side).
    pub proof_duration: Spread,
    pub machine_minutes: MachineMinutes,
    /// Times a task was replayed because the line moved under it, the tasks it happened to, and
    /// the replays that handed conflicts back to their agent.
    pub replays: usize,
    pub tasks_replayed: usize,
    pub conflicts: usize,
    /// Full suites that ended in the window, the red ones, and the shards red then green on their
    /// retry.
    pub full_suites: usize,
    pub full_red: usize,
    pub flaky: usize,
    /// Landings that undo a position (`avp revert`).
    pub reverts: usize,
    pub by_agent: BTreeMap<String, usize>,
    pub by_person: BTreeMap<String, usize>,
    /// The before a person saved (`avp stats --github --before --save`), whatever the window: what
    /// AVP Live compares with, labeled "before AVP".
    pub before: Option<Before>,
}

// ---- times ----

/// Days since the epoch of a civil date (Howard Hinnant's algorithm), to spare a date crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// `2026-10-26T14:02:05Z` from milliseconds since the epoch, `14:02:05.250Z` when they say more: a
/// window cut to the second would leave out what happened in its last one.
pub fn rfc3339(ms: i64) -> String {
    let (days, secs, milli) = (ms.div_euclid(DAY), ms.rem_euclid(DAY) / 1000, ms.rem_euclid(1000));
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + i64::from(m <= 2);
    let frac = if milli > 0 { format!(".{milli:03}") } else { String::new() };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{frac}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// Milliseconds since the epoch of an RFC3339 time (`2026-10-25T14:00:00Z`, `+02:00`, fractions of
/// a second; no zone is UTC), or of a date alone (`2026-10-25`: its midnight, UTC).
pub fn parse_time(s: &str) -> Result<i64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})(?:[Tt ](\d{2}):(\d{2})(?::(\d{2})(?:\.(\d+))?)?)?(?:([Zz])|([+-])(\d{2}):?(\d{2}))?$").unwrap());
    let c = re.captures(s.trim()).ok_or_else(|| anyhow!("{s:?} is not a time: 2026-10-25, 2026-10-25T14:00:00Z, 48h or 7d"))?;
    let n = |i: usize| c.get(i).map(|m| m.as_str().parse::<i64>().unwrap_or(0)).unwrap_or(0);
    let (y, mo, d, h, mi, sec) = (n(1), n(2), n(3), n(4), n(5), n(6));
    anyhow::ensure!((1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24 && mi < 60 && sec <= 60, "{s:?} is not a time: a field is out of its range");
    let frac = c.get(7).map(|m| format!("{:0<3}", &m.as_str()[..m.as_str().len().min(3)]).parse::<i64>().unwrap_or(0)).unwrap_or(0);
    let offset = match c.get(9) {
        Some(sign) => (n(10) * 60 + n(11)) * 60_000 * if sign.as_str() == "-" { -1 } else { 1 },
        None => 0,
    };
    Ok(days_from_civil(y, mo, d) * DAY + (h * 3600 + mi * 60 + sec) * 1000 + frac - offset)
}

/// A time as a person gives it: `now`, how long ago (`30m`, `48h`, `7d`, `2w`), or a time
/// (`parse_time`).
pub fn parse_when(s: &str, now: i64) -> Result<i64> {
    let s = s.trim();
    if s == "now" {
        return Ok(now);
    }
    let unit = |u: char| match u {
        'm' => Some(60_000),
        'h' => Some(HOUR),
        'd' => Some(DAY),
        'w' => Some(7 * DAY),
        _ => None,
    };
    if let (Some(u), Some(n)) = (s.chars().last().and_then(unit), s.get(..s.len().saturating_sub(1)).and_then(|n| n.parse::<f64>().ok())) {
        anyhow::ensure!(n >= 0.0, "{s:?}: how long ago is not negative");
        return Ok(now - (n * u as f64) as i64);
    }
    parse_time(s)
}

/// The window `since` and `until` name (each as `parse_when`): until now and since 7 days before
/// `until` when they are not given.
pub fn window(since: Option<&str>, until: Option<&str>, now: i64) -> Result<(i64, i64)> {
    let until = until.map(|u| parse_when(u, now).map_err(|e| anyhow!("until: {e:#}"))).transpose()?.unwrap_or(now);
    let since = since.map(|s| parse_when(s, now).map_err(|e| anyhow!("since: {e:#}"))).transpose()?.unwrap_or(until - 7 * DAY);
    anyhow::ensure!(since < until, "since ({}) must come before until ({})", rfc3339(since), rfc3339(until));
    Ok((since, until))
}

/// `<from>..<to>`, each as `parse_when`: `2026-10-22..2026-10-24` is the 22nd and the 23rd.
pub fn range(s: &str, now: i64) -> Result<(i64, i64)> {
    let (from, to) = s.split_once("..").ok_or_else(|| anyhow!("{s:?}: <from>..<to>, e.g. 2026-10-22..2026-10-24"))?;
    let (from, to) = (parse_when(from, now)?, parse_when(to, now)?);
    anyhow::ensure!(from < to, "{s:?} ends before it starts");
    Ok((from, to))
}

// ---- as a person reads it ----

/// `15 s`, `8.5 min`, `91 min`, `3.2 h`: the README's words.
pub fn duration(s: Option<f64>) -> String {
    match s {
        None => "-".into(),
        Some(s) if s < 60.0 => format!("{s:.0} s"),
        Some(s) if s < 600.0 => format!("{:.1} min", s / 60.0),
        Some(s) if s < 3.0 * 3600.0 => format!("{:.0} min", s / 60.0),
        Some(s) => format!("{:.1} h", s / 3600.0),
    }
}

/// `15,310`, or `9.5` for a small number.
fn thousands(n: f64) -> String {
    if n < 100.0 && n.fract() != 0.0 {
        return format!("{n:.1}");
    }
    let digits = format!("{:.0}", n.max(0.0));
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `2026-10-25 14:00 to 2026-10-27 14:00 UTC (48 hours)`.
fn span(since: &str, until: &str) -> String {
    let (Ok(a), Ok(b)) = (parse_time(since), parse_time(until)) else { return format!("{since} to {until}") };
    let hours = (b - a) as f64 / HOUR as f64;
    let long = if hours < 72.0 { format!("{hours:.0} hours") } else { format!("{:.0} days", hours / 24.0) };
    format!("{} to {} UTC ({long})", crate::blame::utc(a), crate::blame::utc(b))
}

fn median_p90(s: &Spread) -> String {
    format!("{} (median), {} (p90)", duration(s.median_s), duration(s.p90_s))
}

/// `1 landing`, `131 landings`.
fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// `claude-code 100, codex 31`, the most first.
fn tally(m: &BTreeMap<String, usize>) -> String {
    let mut v: Vec<(&String, &usize)> = m.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    v.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ")
}

/// `avp stats`: a short table a person reads in ten seconds.
pub fn text(s: &Stats) -> String {
    let mut rows: Vec<(String, String)> = vec![("To main".into(), format!("{}, from task opened to landed", median_p90(&s.opened_to_landed)))];
    for (name, e) in &s.envs {
        if e.opened_to_served.count > 0 {
            rows.push((format!("To {name}"), format!("{}; {} (median) after landing", median_p90(&e.opened_to_served), duration(e.landed_to_served.median_s))));
        }
    }
    let reverts = if s.reverts > 0 { format!(" ({})", count(s.reverts, "revert")) } else { String::new() };
    rows.push(("Changes shipped".into(), format!("{}{reverts}", count(s.landings, "landing"))));
    if !s.by_agent.is_empty() {
        rows.push(("  by agent".into(), tally(&s.by_agent)));
        rows.push(("  by person".into(), tally(&s.by_person)));
    }
    rows.push(("Checking one change".into(), format!("{}; {}, {} red, {} blocked", median_p90(&s.proof_duration), count(s.proofs, "proof"), s.proofs_red, s.proofs_blocked)));
    rows.push(("Full suite".into(), format!("{} (median) after landing; {}, {} red, {}", duration(s.landed_to_full.median_s), count(s.full_suites, "run"), s.full_red, count(s.flaky, "flaky rerun"))));
    let m = &s.machine_minutes;
    rows.push(("CI machine time".into(), format!("{} min: {} in proofs, {} in full suites", thousands(m.total), thousands(m.proofs), thousands(m.full_suites))));
    rows.push(("The line moved".into(), format!("{} of {}, {} handed back", count(s.replays, "replay"), count(s.tasks_replayed, "task"), count(s.conflicts, "conflict"))));
    if let Some(b) = &s.before {
        rows.push(("Before AVP".into(), format!("{}, {} (median) to main, {} min of CI: {}, {}", count(b.pull_requests, "pull request"), duration(b.opened_to_merged.median_s), thousands(b.job_minutes), b.repo, span(&b.since, &b.until))));
    }
    let mut out = format!("{} on AVP, {}\n", s.repo, span(&s.since, &s.until));
    for (k, v) in rows {
        out.push_str(&format!("\n  {k:<21}{v}"));
    }
    out
}

// ---- the before: GitHub ----

/// The same numbers from GitHub, over the window before AVP: pull requests and Actions runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Before {
    /// owner/name.
    pub repo: String,
    pub since: String,
    pub until: String,
    /// Pull requests merged in the window, from their opening to their merge.
    pub pull_requests: usize,
    pub opened_to_merged: Spread,
    /// Actions runs created in the window, and the minutes their jobs ran (reruns included).
    pub runs: usize,
    pub job_minutes: f64,
    /// Runs of a workflow whose name or file says "deploy", and those cancelled.
    pub deploy_runs: usize,
    pub deploy_runs_cancelled: usize,
}

/// Reads one path of GitHub's REST API (`/repos/...`) as JSON: `github_get`, or recorded answers.
pub type Fetch<'a> = dyn Fn(&str) -> Result<serde_json::Value> + Sync + 'a;

/// GitHub's REST API with GH_TOKEN (or GITHUB_TOKEN). The token goes in a header: no error, no
/// line printed carries it.
pub fn github_get(path: &str) -> Result<serde_json::Value> {
    static HTTP: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    let token = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")).map_err(|_| anyhow!("set GH_TOKEN: a token that reads the repo's pull requests and Actions runs"))?;
    let http = HTTP.get_or_init(reqwest::blocking::Client::new);
    let r = http.get(format!("https://api.github.com{path}")).bearer_auth(token).header("accept", "application/vnd.github+json").header("user-agent", "avp").send().map_err(|e| anyhow!("GitHub {path}: {}", e.without_url()))?;
    let status = r.status();
    let v: serde_json::Value = r.json().unwrap_or(serde_json::Value::Null);
    anyhow::ensure!(status.is_success(), "GitHub {path}: {status} {}", v["message"].as_str().unwrap_or(""));
    Ok(v)
}

/// Pull requests merged in [since, until), and the Actions runs created in it with their jobs.
pub fn github_before(get: &Fetch, slug: &str, since: i64, until: i64) -> Result<Before> {
    anyhow::ensure!(slug.split('/').count() == 2 && slug.split('/').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))), "--github takes owner/name, not {slug:?}");
    anyhow::ensure!(since < until, "the before's window ends before it starts");
    let time = |v: &serde_json::Value| v.as_str().and_then(|t| parse_time(t).ok());
    let within = |t: Option<i64>| t.is_some_and(|t| t >= since && t < until);
    let mut b = Before { repo: slug.into(), since: rfc3339(since), until: rfc3339(until), ..Default::default() };

    // The closed pull requests, last updated first: one merged in the window was updated since.
    let mut merged = vec![];
    for page in 1..=50 {
        let v = get(&format!("/repos/{slug}/pulls?state=closed&sort=updated&direction=desc&per_page=100&page={page}"))?;
        let prs = v.as_array().ok_or_else(|| anyhow!("GitHub answered no list of pull requests"))?;
        for pr in prs.iter().filter(|p| within(time(&p["merged_at"]))) {
            if let (Some(opened), Some(at)) = (time(&pr["created_at"]), time(&pr["merged_at"])) {
                merged.push((at - opened).max(0) as f64 / 1000.0);
            }
        }
        if prs.len() < 100 || prs.last().and_then(|p| time(&p["updated_at"])).is_some_and(|u| u < since) {
            break;
        }
    }
    b.pull_requests = merged.len();
    b.opened_to_merged = Spread::of(merged);

    // GitHub's filter takes whole seconds: widened to them, the window is kept below.
    let created = format!("{}..{}", rfc3339(since - since.rem_euclid(1000)), rfc3339(until + (1000 - until.rem_euclid(1000)) % 1000));
    let mut runs = vec![];
    for page in 1..=10 {
        let v = get(&format!("/repos/{slug}/actions/runs?created={created}&per_page=100&page={page}"))?;
        let list = v["workflow_runs"].as_array().ok_or_else(|| anyhow!("GitHub answered no list of Actions runs"))?;
        // GitHub's range takes in its end: the window does not.
        runs.extend(list.iter().filter(|r| within(time(&r["created_at"]))).cloned());
        if list.len() < 100 {
            break;
        }
    }
    let deploy = |r: &&serde_json::Value| ["name", "path"].iter().any(|k| r[*k].as_str().is_some_and(|s| s.to_lowercase().contains("deploy")));
    b.runs = runs.len();
    b.deploy_runs = runs.iter().filter(deploy).count();
    b.deploy_runs_cancelled = runs.iter().filter(deploy).filter(|r| r["conclusion"] == "cancelled").count();

    // A run's jobs are one request each: eight at a time.
    let ids: Vec<i64> = runs.iter().filter_map(|r| r["id"].as_i64()).collect();
    let seconds = std::thread::scope(|sc| {
        let parts: Vec<_> = ids.chunks(ids.len().div_ceil(8).max(1)).map(|chunk| sc.spawn(move || chunk.iter().map(|id| job_seconds(get, slug, *id)).sum::<Result<f64>>())).collect();
        parts.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("reading the runs' jobs stopped")))).sum::<Result<f64>>()
    })?;
    b.job_minutes = tenth(seconds / 60.0);
    Ok(b)
}

/// The seconds a run's jobs ran, every attempt's.
fn job_seconds(get: &Fetch, slug: &str, run: i64) -> Result<f64> {
    let time = |v: &serde_json::Value| v.as_str().and_then(|t| parse_time(t).ok());
    let mut total = 0.0;
    for page in 1..=10 {
        let v = get(&format!("/repos/{slug}/actions/runs/{run}/jobs?filter=all&per_page=100&page={page}"))?;
        let jobs = v["jobs"].as_array().ok_or_else(|| anyhow!("GitHub answered no list of jobs for run {run}"))?;
        total += jobs.iter().filter_map(|j| Some((time(&j["completed_at"])? - time(&j["started_at"])?).max(0) as f64 / 1000.0)).sum::<f64>();
        if jobs.len() < 100 {
            break;
        }
    }
    Ok(total)
}

/// Both columns side by side, as the README's table. Counts compare per day when the two windows
/// do not last as long.
pub fn side_by_side(b: &Before, s: &Stats) -> String {
    let days = |a: &str, z: &str| match (parse_time(a), parse_time(z)) {
        (Ok(a), Ok(z)) => ((z - a) as f64 / DAY as f64).max(1e-9),
        _ => 1.0,
    };
    let (before_days, after_days) = (days(&b.since, &b.until), days(&s.since, &s.until));
    let same = (before_days - after_days).abs() < before_days.max(after_days) * 0.01;
    // How the after compares: `10x faster`, `2.3x more`, `77 % less`.
    let faster = |was: Option<f64>, is: Option<f64>| match (was, is) {
        (Some(w), Some(i)) if i > 0.0 && w >= i => format!(", {:.1}x faster", w / i),
        (Some(w), Some(i)) if w > 0.0 && i > w => format!(", {:.1}x slower", i / w),
        _ => String::new(),
    };
    let more = |was: f64, is: f64| {
        let (w, i) = (was / before_days, is / after_days);
        match (w > 0.0, i >= w) {
            (true, true) => format!(", {:.1}x more", i / w),
            (true, false) => format!(", {:.0} % fewer", (1.0 - i / w) * 100.0),
            _ => String::new(),
        }
    };
    let less = |was: f64, is: f64| {
        let (w, i) = (was / before_days, is / after_days);
        match (w > 0.0, i <= w) {
            (true, true) => format!(", {:.0} % less", (1.0 - i / w) * 100.0),
            (true, false) => format!(", {:.0} % more", (i / w - 1.0) * 100.0),
            _ => String::new(),
        }
    };
    let promoted: usize = s.envs.values().map(|e| e.promotions).sum();
    let cut: usize = s.envs.values().map(|e| e.interrupted).sum();
    let short = |t: &str| parse_time(t).map(crate::blame::utc).unwrap_or_else(|_| t.to_string());
    let mut rows: Vec<(String, String, String)> = vec![
        ("".into(), "Pull requests and CI".into(), "AVP".into()),
        ("".into(), format!("{} to {}", short(&b.since), short(&b.until)), format!("{} to {}", short(&s.since), short(&s.until))),
        ("To main".into(), format!("{} (median)", duration(b.opened_to_merged.median_s)), format!("{} (median){}", duration(s.opened_to_landed.median_s), faster(b.opened_to_merged.median_s, s.opened_to_landed.median_s))),
        ("".into(), format!("{} (p90)", duration(b.opened_to_merged.p90_s)), format!("{} (p90)", duration(s.opened_to_landed.p90_s))),
    ];
    for (name, e) in s.envs.iter().filter(|(_, e)| e.opened_to_served.count > 0) {
        rows.push((format!("To {name}"), "-".into(), format!("{} (median)", duration(e.opened_to_served.median_s))));
    }
    rows.push(("Changes shipped".into(), count(b.pull_requests, "pull request"), format!("{}{}", count(s.landings, "landing"), more(b.pull_requests as f64, s.landings as f64))));
    rows.push(("CI machine time".into(), format!("{} min", thousands(b.job_minutes)), format!("{} min{}", thousands(s.machine_minutes.total), less(b.job_minutes, s.machine_minutes.total))));
    rows.push(("Checking one change".into(), "a CI run".into(), format!("{} (median)", duration(s.proof_duration.median_s))));
    let share = |n: usize, of: usize| if of == 0 { "-".to_string() } else { format!("{:.0} % ({n} of {of})", n as f64 * 100.0 / of as f64) };
    rows.push(("Deploys cancelled".into(), share(b.deploy_runs_cancelled, b.deploy_runs), if promoted == 0 { "-".into() } else if cut == 0 { format!("none ({promoted} promotions)") } else { share(cut, promoted) }));
    let width = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0) + 3;
    let mut out = format!("{} before AVP, {} on AVP\n", b.repo, s.repo);
    for (k, was, is) in rows {
        out.push_str(&format!("\n  {k:<21}{was:<width$}{is}"));
    }
    if !same {
        out.push_str(&format!("\n\nThe windows differ ({before_days:.1} and {after_days:.1} days): more and less compare per day."));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_793_000_000_000;

    #[test]
    fn times_read_and_written() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse_time("2026-10-25").unwrap(), parse_time("2026-10-25T00:00:00Z").unwrap());
        let t = parse_time("2026-10-25T14:02:05Z").unwrap();
        assert_eq!(rfc3339(t), "2026-10-25T14:02:05Z");
        assert_eq!(parse_time("2026-10-25T16:02:05.250+02:00").unwrap(), t + 250);
        assert_eq!(parse_time(&rfc3339(t + 250)).unwrap(), t + 250, "{}", rfc3339(t + 250));
        assert_eq!(parse_time("2024-02-29T00:00:00Z").unwrap() + DAY, parse_time("2024-03-01").unwrap());
        assert!(parse_time("2026-13-01").is_err() && parse_time("yesterday").is_err());
        assert_eq!(parse_when("48h", T0).unwrap(), T0 - 2 * DAY);
        assert_eq!(parse_when("7d", T0).unwrap(), T0 - 7 * DAY);
        assert_eq!(window(None, None, T0).unwrap(), (T0 - 7 * DAY, T0));
        assert!(window(Some("1d"), Some("2d"), T0).is_err(), "since after until");
    }

    #[test]
    fn spread_median_and_p90() {
        assert_eq!(Spread::of(vec![]), Spread { count: 0, median_s: None, p90_s: None });
        let s = Spread::of((1..=10).rev().map(f64::from).collect());
        assert_eq!((s.count, s.median_s, s.p90_s), (10, Some(5.5), Some(9.1)));
    }

    #[test]
    fn the_saved_before_in_the_table() {
        let b = Before { repo: "acme/shop".into(), since: "2026-10-01T00:00:00Z".into(), until: "2026-10-08T00:00:00Z".into(), pull_requests: 12, opened_to_merged: Spread::of(vec![5460.0]), job_minutes: 900.0, ..Default::default() };
        let t = text(&Stats { repo: "shop".into(), before: Some(b), ..Default::default() });
        assert!(t.contains("Before AVP           12 pull requests, 91 min (median) to main, 900 min of CI: acme/shop, 2026-10-01"), "{t}");
        assert!(!text(&Stats::default()).contains("Before AVP"));
    }
}
