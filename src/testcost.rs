//! `avp tests`: what each test file costs and what it has caught, read from the proofs' output.
//!
//! Agents write many tests and a proof runs them on every change. Which ones take the machines'
//! time, which ones go red only to pass again (noise an agent learns to rerun), and which slow ones
//! have never failed: the server has seen every run, so it says, instead of an audit by hand.
//!
//! Read from the lines test runners print for a file: vitest's `✓ path (12 tests) 340ms` (or `✗`,
//! `×`), jest's `PASS path (1.2 s)` / `FAIL path`. A runner that prints neither is not counted.

use crate::check::{AutoSkip, AUTO_SKIP_MAX_AFTER};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

/// One test file over a window.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TestFile {
    pub path: String,
    pub runs: usize,
    pub failed: usize,
    /// Failures in a proof that still ended green or flaky: the file went red, then passed.
    pub flaky: usize,
    pub median_s: f64,
    /// Every run added up: the machine time it took.
    pub total_s: f64,
}

/// `GET /repos/{repo}/tests`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TestReport {
    pub repo: String,
    pub since: String,
    pub until: String,
    pub proofs_read: usize,
    pub files_seen: usize,
    /// The machine time of every file run in the window.
    pub total_s: f64,
    /// The files that took the most machine time.
    pub costliest: Vec<TestFile>,
    /// The files that went red and passed again: noise before they are signal.
    pub flaky: Vec<TestFile>,
    /// Files run at least `NEVER_FAILED_RUNS` times without one failure, costliest first: the first
    /// to read when a suite must get shorter.
    pub never_failed: Vec<TestFile>,
    /// `[check] auto_skip` at the head of the line; None: off.
    pub auto_skip: Option<AutoSkip>,
    /// Whether a step of the head's avp.toml takes `{skip_files}`: without one, nothing is left out.
    pub skip_files_taken: bool,
    /// The files the rule leaves out of the proofs now, the longest out first: the full suite still
    /// runs them.
    pub left_out: Vec<LeftOut>,
}

/// A test file out of the proofs, since the run that made its streak long enough.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LeftOut {
    pub path: String,
    /// RFC3339.
    pub since: String,
    /// Green runs in a row, the last one red (or the first one read).
    pub green_runs: usize,
    pub median_s: f64,
}

pub const NEVER_FAILED_RUNS: usize = 20;
const SHOWN: usize = 15;

/// The files a runner's output reports: (path, seconds, passed).
pub fn files_in(output: &str) -> Vec<(String, f64, bool)> {
    let mut out = Vec::new();
    for raw in output.lines() {
        let line = strip_ansi(raw);
        let line = line.trim_start();
        if let Some((mark, rest)) = line.split_once(' ') {
            let passed = match mark {
                "✓" => Some(true),
                "✗" | "×" | "❯" => Some(false),
                "PASS" => Some(true),
                "FAIL" => Some(false),
                _ => None,
            };
            let Some(passed) = passed else { continue };
            let Some((path, after)) = rest.split_once(' ').map(|(p, a)| (p, a)).or(Some((rest, ""))) else { continue };
            if !is_test_file(path) {
                continue;
            }
            out.push((path.to_string(), seconds_in(after).unwrap_or(0.0), passed));
        }
    }
    out
}

fn is_test_file(p: &str) -> bool {
    let name = p.rsplit('/').next().unwrap_or(p);
    [".test.", ".spec.", "_test."].iter().any(|m| name.contains(m)) || (name.starts_with("test_") && name.ends_with(".py"))
}

/// The last duration a line states: `340ms`, `1.2 s`, `2.31s`.
fn seconds_in(s: &str) -> Option<f64> {
    let words: Vec<&str> = s.split(|c: char| c.is_whitespace() || c == '(' || c == ')').filter(|w| !w.is_empty()).collect();
    for (i, w) in words.iter().enumerate().rev() {
        if let Some(n) = w.strip_suffix("ms").and_then(|n| n.parse::<f64>().ok()) {
            return Some(n / 1000.0);
        }
        if let Some(n) = w.strip_suffix('s').and_then(|n| n.parse::<f64>().ok()) {
            return Some(n);
        }
        if *w == "s" || *w == "ms" {
            if let Some(n) = i.checked_sub(1).and_then(|j| words[j].parse::<f64>().ok()) {
                return Some(if *w == "ms" { n / 1000.0 } else { n });
            }
        }
    }
    None
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Gathers proofs one at a time (their output weighs megabytes): `add` each, then `report`.
#[derive(Default)]
pub struct Tally {
    proofs: usize,
    files: BTreeMap<String, (Vec<f64>, usize, usize)>,
}

impl Tally {
    /// One proof's steps' output, and whether the proof ended green (or flaky).
    pub fn add<'a>(&mut self, outputs: impl Iterator<Item = &'a str>, passed: bool) {
        self.proofs += 1;
        for out in outputs {
            for (path, s, ok) in files_in(out) {
                let e = self.files.entry(path).or_default();
                e.0.push(s);
                if !ok {
                    e.1 += 1;
                    if passed {
                        e.2 += 1;
                    }
                }
            }
        }
    }

    pub fn report(self, repo: &str, since: String, until: String) -> TestReport {
        let mut all: Vec<TestFile> = self
            .files
            .into_iter()
            .map(|(path, (mut times, failed, flaky))| {
                times.sort_by(f64::total_cmp);
                let total: f64 = times.iter().sum();
                TestFile { path, runs: times.len(), failed, flaky, median_s: tenth(times[times.len() / 2]), total_s: tenth(total) }
            })
            .collect();
        all.sort_by(|a, b| b.total_s.total_cmp(&a.total_s));
        let total_s = tenth(all.iter().map(|f| f.total_s).sum());
        let pick = |keep: &dyn Fn(&TestFile) -> bool| all.iter().filter(|f| keep(f)).take(SHOWN).cloned().collect::<Vec<_>>();
        let mut flaky = pick(&|f| f.flaky > 0);
        flaky.sort_by(|a, b| b.flaky.cmp(&a.flaky));
        TestReport {
            repo: repo.into(),
            since,
            until,
            proofs_read: self.proofs,
            files_seen: all.len(),
            total_s,
            costliest: pick(&|_| true),
            flaky,
            never_failed: pick(&|f| f.failed == 0 && f.runs >= NEVER_FAILED_RUNS),
            ..Default::default()
        }
    }
}

/// What `[check] auto_skip` reads: each test file's runs, proofs and full suites alike, oldest
/// first. Kept by the server and fed each proof as it is made: it reads no output twice.
#[derive(Default)]
pub struct Ledger {
    files: BTreeMap<String, Runs>,
}

#[derive(Default)]
struct Runs {
    /// The last runs' seconds.
    seconds: VecDeque<f32>,
    /// Green runs since the last red one.
    streak: usize,
    /// When the first of them ran: the rule's `after`-th one says since when the file is out.
    streak_at: Vec<i64>,
}

impl Ledger {
    /// One proof's steps' output, made at `at` (milliseconds).
    pub fn add<'a>(&mut self, at: i64, outputs: impl Iterator<Item = &'a str>) {
        for out in outputs {
            for (path, s, ok) in files_in(out) {
                let r = self.files.entry(path).or_default();
                r.seconds.push_back(s as f32);
                if r.seconds.len() > AUTO_SKIP_MAX_AFTER {
                    r.seconds.pop_front();
                }
                if ok {
                    r.streak += 1;
                    if r.streak_at.len() < AUTO_SKIP_MAX_AFTER {
                        r.streak_at.push(at);
                    }
                } else {
                    r.streak = 0;
                    r.streak_at.clear();
                }
            }
        }
    }

    /// The files the rule leaves out: green in each of their last `after` runs, their median run
    /// over those `min_seconds` or more. The longest out first.
    pub fn left_out(&self, rule: &AutoSkip) -> Vec<LeftOut> {
        let mut out: Vec<(i64, LeftOut)> = Vec::new();
        for (path, r) in &self.files {
            let Some(&since) = r.streak_at.get(rule.after.wrapping_sub(1)).filter(|_| r.streak >= rule.after) else { continue };
            let mut last: Vec<f64> = r.seconds.iter().rev().take(rule.after).map(|s| f64::from(*s)).collect();
            last.sort_by(f64::total_cmp);
            let median = last[last.len() / 2];
            if median >= rule.min_seconds {
                out.push((since, LeftOut { path: path.clone(), since: crate::stats::rfc3339(since), green_runs: r.streak, median_s: tenth(median) }));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.path.cmp(&b.1.path)));
        out.into_iter().map(|(_, l)| l).collect()
    }
}

fn tenth(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// `avp tests`: three short lists a person reads in a minute.
pub fn text(r: &TestReport) -> String {
    let mut out = format!(
        "{}: {} test files in {} proofs, {:.0} machine minutes of tests, {} to {}\n",
        r.repo,
        r.files_seen,
        r.proofs_read,
        r.total_s / 60.0,
        r.since,
        r.until
    );
    let list = |title: &str, files: &[TestFile], out: &mut String| {
        if files.is_empty() {
            return;
        }
        out.push_str(&format!("\n{title}\n"));
        for f in files {
            out.push_str(&format!("  {:>7.1} min  {:>4} runs  {:>6.1}s median  {:>3} red  {:>3} flaky  {}\n", f.total_s / 60.0, f.runs, f.median_s, f.failed, f.flaky, f.path));
        }
    };
    list("Costliest", &r.costliest, &mut out);
    list("Red, then passed again (flaky)", &r.flaky, &mut out);
    list(&format!("Never failed in {NEVER_FAILED_RUNS}+ runs, costliest first"), &r.never_failed, &mut out);
    match &r.auto_skip {
        None => out.push_str("\nLeft out of the proofs: none, `[check] auto_skip = false`\n"),
        Some(rule) if !r.left_out.is_empty() => {
            let note = if r.skip_files_taken { "" } else { "; no step takes {skip_files} yet, so the proofs still run them" };
            out.push_str(&format!("\nLeft out of the proofs (green in their last {} runs, {}s or more each; the full suite still runs them{note})\n", rule.after, rule.min_seconds));
            for f in &r.left_out {
                out.push_str(&format!("  since {}  {:>5} green  {:>6.1}s median  {}\n", crate::blame::utc(crate::stats::parse_time(&f.since).unwrap_or(0)), f.green_runs, f.median_s, f.path));
            }
        }
        Some(_) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_vitest_and_jest_lines() {
        let out = "\u{1b}[32m✓\u{1b}[39m test/bench.test.ts \u{1b}[2m(12 tests)\u{1b}[22m\u{1b}[33m 44312\u{1b}[2mms\u{1b}[22m\u{1b}[39m\n × src/a.test.ts (3 tests | 1 failed) 1.20s\nPASS src/b.spec.js (2.5 s)\nFAIL src/c.test.js\n✓ a single test inside a file 3ms\n";
        let got = files_in(out);
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[0], ("test/bench.test.ts".into(), 44.312, true));
        assert_eq!(got[1], ("src/a.test.ts".into(), 1.2, false));
        assert_eq!(got[2], ("src/b.spec.js".into(), 2.5, true));
        assert_eq!(got[3].2, false);
    }

    #[test]
    fn sorts_out_noise_and_dead_weight() {
        let mut t = Tally::default();
        for i in 0..25 {
            t.add(["✓ test/slow.test.ts (1 test) 30s\n✓ test/quick.test.ts (1 test) 10ms"].into_iter(), true);
            let flake = if i == 3 { "× test/timing.test.ts (1 test) 2s" } else { "✓ test/timing.test.ts (1 test) 2s" };
            t.add([flake].into_iter(), true);
        }
        let r = t.report("r", "a".into(), "b".into());
        assert_eq!(r.costliest[0].path, "test/slow.test.ts");
        assert_eq!(r.flaky.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["test/timing.test.ts"]);
        assert_eq!(r.never_failed[0].path, "test/slow.test.ts", "the slow file that never failed reads first");
        assert!(!r.never_failed.iter().any(|f| f.path == "test/timing.test.ts"));
    }

    #[test]
    fn a_slow_file_green_long_enough_leaves_the_proofs() {
        let rule = AutoSkip { after: 3, min_seconds: 5.0 };
        let mut l = Ledger::default();
        for at in 1..=2 {
            l.add(at, ["✓ test/slow.test.ts 30s\n✓ test/quick.test.ts 10ms"].into_iter());
        }
        assert!(l.left_out(&rule).is_empty(), "two green runs are not three");
        l.add(3, ["✓ test/slow.test.ts 30s\n✓ test/quick.test.ts 10ms"].into_iter());
        let out = l.left_out(&rule);
        assert_eq!(out.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["test/slow.test.ts"], "the quick file costs too little to leave");
        assert_eq!((out[0].since.as_str(), out[0].green_runs, out[0].median_s), (crate::stats::rfc3339(3).as_str(), 3, 30.0));
    }

    #[test]
    fn red_once_in_a_full_suite_a_file_is_back_in() {
        let rule = AutoSkip { after: 2, min_seconds: 1.0 };
        let mut l = Ledger::default();
        for at in 1..=5 {
            l.add(at, ["✓ test/slow.test.ts 9s"].into_iter());
        }
        assert_eq!(l.left_out(&rule).len(), 1);
        l.add(6, ["× test/slow.test.ts 9s"].into_iter());
        assert!(l.left_out(&rule).is_empty(), "red in the full suite: back in the proofs");
        l.add(7, ["✓ test/slow.test.ts 9s"].into_iter());
        assert!(l.left_out(&rule).is_empty());
        l.add(8, ["✓ test/slow.test.ts 9s"].into_iter());
        assert_eq!(l.left_out(&rule)[0].since, crate::stats::rfc3339(8), "out again once green `after` times since");
    }
}
