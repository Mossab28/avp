//! A file's history read from the line, so a person stops going to GitHub for it: `avp log --path`
//! (the positions that changed the file, newest first) and `avp blame` (each line of the file with
//! the position that last changed it). The server works them out from the line's trees and blobs,
//! for the CLI and AVP Live alike. The same history, short, is handed to an agent when it opens a
//! task (`context`): read from an index of what each position changed, not from the trees.

use crate::engine::Engine;
use crate::model::*;
use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The positions one request reads back at most: a blame stops there, a log answers a page. Each
/// one costs a tree read, and a line grows by the hundred every day.
pub const WALK: i64 = 500;

/// What the line's memory names at most in one answer: paths (a directory counts by its most
/// changed files), the last changes of each, lines in all. Agents never asked for this history,
/// they read and grepped to rebuild it, and AVP cost as many tokens per change as git: handed up
/// front, it has to stay shorter than what it saves.
pub const CONTEXT_PATHS: usize = 6;
pub const CONTEXT_CHANGES: usize = 3;
pub const CONTEXT_LINES: usize = 40;

/// A position that changed a file: when, why, who.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Change {
    pub position: i64,
    pub landed_at: i64,
    /// The task's goal, or the note of an import.
    pub goal: String,
    pub task: Option<String>,
    /// None for an import: no task, so no person.
    pub person: Option<String>,
    pub agent: Option<String>,
    /// "added", "modified" or "deleted".
    pub change: String,
}

/// The positions that changed a file, newest first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileLog {
    pub path: String,
    pub changes: Vec<Change>,
    /// Where to read on for older changes (`from`), when this page stopped before the line's start.
    pub more: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlameLine {
    /// 1-based.
    pub line: usize,
    pub text: String,
    /// The position that last changed the line.
    pub position: i64,
    pub person: Option<String>,
    /// The walk stopped `WALK` positions back with the line unchanged: it is as old as `position`
    /// or older, and nobody is named.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub older: bool,
}

/// A file at a position, line by line, with the positions its lines come from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Blame {
    pub path: String,
    /// The position whose file this is: the head unless another was asked.
    pub position: i64,
    pub lines: Vec<BlameLine>,
    /// The positions the lines name, newest first.
    pub changes: Vec<Change>,
}

/// What the line remembers of one path: its last changes, and the open task that holds it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PathContext {
    pub path: String,
    /// Newest first, `CONTEXT_CHANGES` at most.
    pub changes: Vec<Change>,
    /// Another open task with a lease on the path, or on a directory above it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<Holder>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Holder {
    pub task: String,
    pub goal: String,
    pub agent: String,
}

/// What opening a task answers: the task, and the line's memory of its scope, read before the task
/// took its leases, so that the tasks holding them until then are named.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opened {
    #[serde(flatten)]
    pub task: Task,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<PathContext>,
}

/// The positions from `from` (None: the head) back, newest first: `WALK` steps back and the one
/// before the last, which the last step compares with. Read in one go, so the walk that follows
/// reads objects without holding the database.
fn walk(e: &Engine, repo: &str, from: Option<i64>) -> Result<Vec<LineEntry>> {
    let db = e.db.lock().unwrap_or_else(|p| p.into_inner());
    let head = db.head(repo)?.ok_or_else(|| anyhow!("no line in repo {repo}: import first"))?.position;
    let from = from.unwrap_or(head);
    let entries = db.line(repo, from, WALK + 1)?;
    anyhow::ensure!(entries.first().is_some_and(|f| f.position == from), "no position {from}: the head is {head}");
    Ok(entries)
}

/// The file as it was at the position before the walk's `at`th: Some(None) where it was not, or
/// before the line's start; None past what this walk read.
fn before(e: &Engine, entries: &[LineEntry], at: usize, path: &str) -> Result<Option<Option<Entry>>> {
    Ok(match entries.get(at + 1) {
        Some(b) => Some(e.objects.get_tree(&b.tree)?.get(path).cloned()),
        None if entries[at].position == 0 => Some(None),
        None => None,
    })
}

fn kind(before: &Option<Entry>, after: &Option<Entry>) -> &'static str {
    match (before, after) {
        (None, _) => "added",
        (_, None) => "deleted",
        _ => "modified",
    }
}

/// Who and why for each position, from its task: one look at the database for all of them.
fn changes(e: &Engine, entries: &[LineEntry], found: impl IntoIterator<Item = (i64, &'static str)>) -> Result<Vec<Change>> {
    let db = e.db.lock().unwrap_or_else(|p| p.into_inner());
    found
        .into_iter()
        .map(|(position, change)| {
            let entry = entries.iter().find(|e| e.position == position).ok_or_else(|| anyhow!("no position {position}"))?;
            let task = entry.task.as_deref().map(|t| db.task(t)).transpose()?;
            let actor = task.map(|t| t.actor);
            Ok(Change {
                position,
                landed_at: entry.landed_at,
                goal: entry.note.clone(),
                task: entry.task.clone(),
                person: actor.as_ref().map(|a| a.person.clone()).filter(|p| !p.is_empty()),
                agent: actor.map(|a| a.agent).filter(|a| !a.is_empty()),
                change: change.to_string(),
            })
        })
        .collect()
}

/// The positions that changed `path` from `from` (None: the head) back, newest first, `limit` of
/// them at most and read over `WALK` positions at most. A file deleted since still has its history.
pub fn log(e: &Engine, repo: &str, path: &str, from: Option<i64>, limit: usize) -> Result<FileLog> {
    let entries = walk(e, repo, from)?;
    let mut current = e.objects.get_tree(&entries[0].tree)?.get(path).cloned();
    let (mut found, mut more) = (Vec::new(), None);
    for i in 0..entries.len() {
        if found.len() >= limit.max(1) {
            more = Some(entries[i].position);
            break;
        }
        let Some(was) = before(e, &entries, i, path)? else {
            more = Some(entries[i].position);
            break;
        };
        if was != current {
            found.push((entries[i].position, kind(&was, &current)));
        }
        current = was;
    }
    Ok(FileLog { path: path.to_string(), changes: changes(e, &entries, found)?, more })
}

/// For each line of `new`, its index in `old`, or None where `new` wrote it.
pub fn origins(old: &str, new: &str) -> Vec<Option<usize>> {
    let lines = new.split_inclusive('\n').count();
    let mut map = Vec::with_capacity(lines);
    // A range of length 0 names the line before it; any other range starts at its first line.
    let zero = |r: diffy::HunkRange| if r.len() == 0 { r.start() } else { r.start() - 1 };
    let (mut o, mut n) = (0, 0);
    for h in diffy::create_patch(old, new).hunks() {
        // Between hunks, lines are the same on both sides.
        while n < zero(h.new_range()) {
            map.push(Some(o));
            o += 1;
            n += 1;
        }
        o = zero(h.old_range());
        for l in h.lines() {
            match l {
                diffy::Line::Context(_) => {
                    map.push(Some(o));
                    o += 1;
                    n += 1;
                }
                diffy::Line::Delete(_) => o += 1,
                diffy::Line::Insert(_) => {
                    map.push(None);
                    n += 1;
                }
            }
        }
    }
    while n < lines {
        map.push(Some(o));
        o += 1;
        n += 1;
    }
    map
}

/// Each line of `path` at position `at` (None: the head), with the position that last changed it:
/// the line is followed back through every change, `WALK` positions back at most.
pub fn blame(e: &Engine, repo: &str, path: &str, at: Option<i64>) -> Result<Blame> {
    let entries = walk(e, repo, at)?;
    let position = entries[0].position;
    let entry = e.objects.get_tree(&entries[0].tree)?.get(path).cloned().ok_or_else(|| anyhow!("no {path} at position {position}"))?;
    let bytes = e.objects.get(&entry.blob)?;
    anyhow::ensure!(!bytes.contains(&0), "{path} is binary at position {position}: it has no lines");
    let shown = String::from_utf8(bytes).map_err(|_| anyhow!("{path} is binary at position {position}: it has no lines"))?;
    let (mut blob, mut text) = (entry.blob, shown.clone());
    // Each line not yet given a position, with where it sits in the version the walk is at.
    let mut open: Vec<(usize, usize)> = (0..shown.lines().count()).map(|l| (l, l)).collect();
    let mut owner: Vec<Option<i64>> = vec![None; open.len()];
    let mut found: BTreeMap<i64, &'static str> = BTreeMap::new();
    let mut reached = position;
    for i in 0..entries.len() {
        reached = entries[i].position;
        if open.is_empty() {
            break;
        }
        let Some(was) = before(e, &entries, i, path)? else { break };
        let Some(was) = was else {
            for (l, _) in open.drain(..) {
                owner[l] = Some(reached);
            }
            found.insert(reached, "added");
            break;
        };
        if was.blob == blob {
            continue;
        }
        let old = String::from_utf8_lossy(&e.objects.get(&was.blob)?).to_string();
        let map = origins(&old, &text);
        open.retain_mut(|(l, at)| match map[*at] {
            Some(o) => {
                *at = o;
                true
            }
            None => {
                owner[*l] = Some(reached);
                found.insert(reached, "modified");
                false
            }
        });
        (blob, text) = (was.blob, old);
    }
    let changes = changes(e, &entries, found.into_iter().rev())?;
    let person = |p: i64| changes.iter().find(|c| c.position == p).and_then(|c| c.person.clone());
    let lines = shown
        .lines()
        .zip(owner)
        .enumerate()
        .map(|(i, (t, o))| BlameLine { line: i + 1, text: t.to_string(), position: o.unwrap_or(reached), person: o.and_then(person), older: o.is_none() })
        .collect();
    Ok(Blame { path: path.to_string(), position, lines, changes })
}

/// Which paths each position changed, and the span of the line that is indexed so far. What a
/// position holds never changes: each one costs its tree read once, ever.
const INDEX: &str = "
CREATE TABLE IF NOT EXISTS path_changes (
  repo TEXT NOT NULL, path TEXT NOT NULL, position INTEGER NOT NULL, change TEXT NOT NULL,
  PRIMARY KEY (repo, path, position)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS path_changes_span (repo TEXT PRIMARY KEY, low INTEGER NOT NULL, high INTEGER NOT NULL);
";

/// One catch-up at a time: two opens side by side would read the same trees.
static INDEXING: Mutex<()> = Mutex::new(());

/// What `after` changed against `before`, path by path.
fn changed(before: &Tree, after: &Tree) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = after
        .iter()
        .filter_map(|(p, a)| match before.get(p) {
            None => Some((p.clone(), "added")),
            Some(b) if b != a => Some((p.clone(), "modified")),
            Some(_) => None,
        })
        .collect();
    out.extend(before.keys().filter(|p| !after.contains_key(*p)).map(|p| (p.clone(), "deleted")));
    out
}

/// Brings the index up to the head: every position landed since the last catch-up, and `WALK` more
/// of the older history, until the line's start is reached. Once the line is indexed, an open reads
/// only the trees of what landed since the open before it.
fn index(e: &Engine, repo: &str) -> Result<()> {
    let _one = INDEXING.lock().unwrap_or_else(|p| p.into_inner());
    let (span, head) = {
        let db = e.db.lock().unwrap_or_else(|p| p.into_inner());
        db.conn.execute_batch(INDEX)?;
        let span: Option<(i64, i64)> = db.conn.query_row("SELECT low, high FROM path_changes_span WHERE repo = ?1", params![repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        (span, db.head(repo)?.map(|h| h.position))
    };
    let Some(head) = head else { return Ok(()) };
    let (low, high) = span.unwrap_or((head + 1, head));
    let new_low = (low - WALK).max(0);
    if high == head && new_low == low {
        return Ok(());
    }
    let mut rows = Vec::new();
    for (from, to) in [(high + 1, head), (new_low, low - 1)].into_iter().filter(|(f, t)| f <= t) {
        // Newest first, down to the position before `from`, which the oldest of the range is read against.
        let entries = e.db.lock().unwrap_or_else(|p| p.into_inner()).line(repo, to, to - from + 2)?;
        let mut newer: Option<(i64, Tree)> = None;
        for entry in &entries {
            let tree = e.objects.get_tree(&entry.tree)?;
            if let Some((position, after)) = newer.take() {
                rows.extend(changed(&tree, &after).into_iter().map(|(path, change)| (path, position, change)));
            }
            newer = (entry.position >= from).then_some((entry.position, tree));
        }
        // The line's start: everything it holds was added there.
        if let Some((0, after)) = newer {
            rows.extend(changed(&Tree::new(), &after).into_iter().map(|(path, change)| (path, 0, change)));
        }
    }
    let mut db = e.db.lock().unwrap_or_else(|p| p.into_inner());
    let tx = db.conn.transaction()?;
    {
        let mut put = tx.prepare("INSERT OR IGNORE INTO path_changes (repo, path, position, change) VALUES (?1, ?2, ?3, ?4)")?;
        for (path, position, change) in &rows {
            put.execute(params![repo, path, position, change])?;
        }
    }
    tx.execute("INSERT INTO path_changes_span (repo, low, high) VALUES (?1, ?2, ?3) ON CONFLICT (repo) DO UPDATE SET low = MIN(low, excluded.low), high = MAX(high, excluded.high)", params![repo, new_low, head])?;
    tx.commit()?;
    Ok(())
}

/// What the line remembers of `paths`, for an agent before it reads them: for each (a directory by
/// its most changed files, `CONTEXT_PATHS` paths in all, the asked ones in turn), its last
/// `CONTEXT_CHANGES` changes, and the open task other than `except` that holds it. Paths without
/// history are left out. Once the index has caught up with the line, two indexed queries: the
/// history, then the leases of the other open tasks.
pub fn context(e: &Engine, repo: &str, paths: &[String], except: Option<&str>) -> Result<Vec<PathContext>> {
    let mut asked: Vec<&str> = Vec::new();
    for p in paths.iter().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty() && *p != ".") {
        if !asked.contains(&p) {
            asked.push(p);
        }
    }
    // Two searches a path in one compound select, which SQLite caps at 500; the first ones are
    // what gets named anyway.
    asked.truncate(64);
    if asked.is_empty() {
        return Ok(vec![]);
    }
    index(e, repo)?;
    let db = e.db.lock().unwrap_or_else(|p| p.into_inner());
    // Each asked path as a file, and as a directory: one index search each. CROSS JOIN keeps the
    // line read by position, for the few changes kept, instead of whole.
    let searches: Vec<String> = (2..asked.len() + 2)
        .map(|n| format!("SELECT path, position, change FROM path_changes WHERE repo = ?1 AND path = ?{n} UNION SELECT path, position, change FROM path_changes WHERE repo = ?1 AND path >= ?{n} || '/' AND path < ?{n} || '0'"))
        .collect();
    let sql = format!(
        "SELECT c.path, c.times, c.position, c.change, l.landed_at, l.note, l.task, t.actor FROM (SELECT * FROM (SELECT path, position, change, ROW_NUMBER() OVER (PARTITION BY path ORDER BY position DESC) AS k, COUNT(*) OVER (PARTITION BY path) AS times FROM ({})) WHERE k <= {CONTEXT_CHANGES}) c CROSS JOIN line l ON l.repo = ?1 AND l.position = c.position LEFT JOIN tasks t ON t.id = l.task ORDER BY c.path, c.position DESC",
        searches.join(" UNION ")
    );
    let mut st = db.conn.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(std::iter::once(repo).chain(asked.iter().copied())), |r| {
        let actor: Option<Actor> = r.get::<_, Option<String>>(7)?.and_then(|a| serde_json::from_str(&a).ok());
        let change = Change {
            position: r.get(2)?,
            landed_at: r.get(4)?,
            goal: r.get(5)?,
            task: r.get(6)?,
            person: actor.as_ref().map(|a| a.person.clone()).filter(|p| !p.is_empty()),
            agent: actor.map(|a| a.agent).filter(|a| !a.is_empty()),
            change: r.get(3)?,
        };
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, change))
    })?;
    // Path: how many times it changed, its last changes.
    let mut found: BTreeMap<String, (i64, Vec<Change>)> = BTreeMap::new();
    for row in rows {
        let (path, times, change) = row?;
        found.entry(path).or_insert_with(|| (times, vec![])).1.push(change);
    }
    // What each asked path gives, best first: itself, or the most changed files still under it.
    let offers: Vec<Vec<&String>> = asked
        .iter()
        .map(|a| match found.get_key_value(*a) {
            Some((p, _)) => vec![p],
            None => {
                let mut under: Vec<(&String, &(i64, Vec<Change>))> = found.iter().filter(|(p, (_, c))| p.starts_with(&format!("{a}/")) && c[0].change != "deleted").collect();
                under.sort_by_key(|(_, (times, c))| std::cmp::Reverse((*times, c[0].position)));
                under.into_iter().map(|(p, _)| p).collect()
            }
        })
        .collect();
    let mut picked: Vec<&String> = Vec::new();
    for round in 0..CONTEXT_PATHS {
        for p in offers.iter().filter_map(|o| o.get(round)) {
            if picked.len() < CONTEXT_PATHS && !picked.contains(p) {
                picked.push(p);
            }
        }
    }
    // The leases of the other open tasks, scope and paths written alike; they are few.
    let mut st = db.conn.prepare("SELECT s.path, t.id, t.goal, t.actor FROM leases s JOIN tasks t ON t.id = s.task WHERE s.repo = ?1 AND t.state = 'open' AND t.id IS NOT ?2 ORDER BY s.taken_at")?;
    let leases: Vec<(String, Holder)> = st
        .query_map(params![repo, except], |r| {
            let actor: Actor = serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default();
            Ok((r.get(0)?, Holder { task: r.get(1)?, goal: r.get(2)?, agent: if actor.agent.is_empty() { actor.person } else { actor.agent } }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let held = |p: &str| leases.iter().find(|(l, _)| l.trim_end_matches('/') == p || p.starts_with(&format!("{}/", l.trim_end_matches('/')))).map(|(_, h)| h.clone());
    Ok(picked.into_iter().map(|p| PathContext { path: p.clone(), changes: found[p].1.clone(), held_by: held(p) }).collect())
}

/// `2026-10-26 14:02` (UTC) from milliseconds since the epoch.
pub fn utc(ms: i64) -> String {
    let (days, secs) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000) / 1000);
    // Days to a civil date (Howard Hinnant's algorithm), to spare a date crate.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

fn who(c: &Change) -> String {
    match (&c.task, &c.person) {
        (None, _) => "imported".into(),
        (Some(_), Some(p)) => p.clone(),
        (Some(_), None) => "-".into(),
    }
}

/// `avp log --path` as a person reads it.
pub fn log_text(log: &FileLog) -> String {
    if log.changes.is_empty() && log.more.is_none() {
        return format!("no position of the line changed {}", log.path);
    }
    let width = log.changes.iter().map(|c| who(c).chars().count()).max().unwrap_or(0);
    let mut out: Vec<String> = log.changes.iter().map(|c| format!("{:>5}  {} UTC  {:<width$}  {:<8}  {}", c.position, utc(c.landed_at), who(c), c.change, c.goal.trim())).collect();
    if let Some(p) = log.more {
        out.push(format!("  older changes: avp log --path {} --from {p}", log.path));
    }
    out.join("\n")
}

/// `avp blame` as a person reads it: position, person, line number, text. A line the walk did not
/// see written is marked `^`: as old as its position, or older.
pub fn blame_text(b: &Blame) -> String {
    let name = |l: &BlameLine| if l.older { "?".to_string() } else { b.changes.iter().find(|c| c.position == l.position).map(who).unwrap_or_default() };
    let width = b.lines.iter().map(|l| name(l).chars().count()).max().unwrap_or(0);
    let digits = b.lines.len().to_string().len();
    let mut out: Vec<String> = b.lines.iter().map(|l| format!("{:>6}  {:<width$}  {:>digits$}  {}", format!("{}{}", if l.older { "^" } else { "" }, l.position), name(l), l.line, l.text)).collect();
    if let Some(old) = b.lines.iter().find(|l| l.older) {
        out.push(format!("^ unchanged over the last {WALK} positions: as old as position {p} or older (avp blame {} --at {p} reads on)", b.path, p = old.position));
    }
    if out.is_empty() {
        return format!("{} is empty at position {}", b.path, b.position);
    }
    out.join("\n")
}

/// `12m`, `5h`, `3d`: how long ago, short.
fn age(ms: i64) -> String {
    let minutes = (ms / 60_000).max(1);
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 48 * 60 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / 1440)
    }
}

/// At most `n` characters of `s`, on one line.
fn clip(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}...", one.chars().take(n).collect::<String>()) }
}

/// The line's memory as an agent reads it, `now` being the time of reading: each path, with the
/// open task holding it, then its last changes, one line each (position, age, agent, goal).
/// Nothing when the paths have no history.
pub fn context_text(paths: &[PathContext], now: i64) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let agent = |c: &Change| c.agent.clone().or_else(|| c.person.clone()).unwrap_or_else(|| if c.task.is_some() { "-".into() } else { "imported".into() });
    let width = paths.iter().flat_map(|p| &p.changes).map(|c| agent(c).chars().count()).max().unwrap_or(0);
    let mut out = vec!["The line's last changes to these paths, newest first (position, age, agent, goal):".to_string()];
    for p in paths {
        out.push(match &p.held_by {
            Some(h) if h.goal.trim().is_empty() => format!("{}  held by open task {} ({})", p.path, h.task, h.agent),
            Some(h) => format!("{}  held by open task {} ({}): {}", p.path, h.task, h.agent, clip(&h.goal, 60)),
            None => p.path.clone(),
        });
        for c in &p.changes {
            out.push(format!("  {:>4}  {:>3}  {:<width$}  {}", c.position, age(now - c.landed_at), agent(c), clip(&c.goal, 72)));
        }
    }
    out.truncate(CONTEXT_LINES);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_follow_lines_through_a_diff() {
        assert_eq!(origins("a\nb\nc\n", "a\nb\nc\n"), vec![Some(0), Some(1), Some(2)]);
        assert_eq!(origins("a\nb\nc\n", "z\na\nb\nc\n"), vec![None, Some(0), Some(1), Some(2)], "a line written first");
        assert_eq!(origins("a\nb\nc\n", "a\nc\nd\n"), vec![Some(0), Some(2), None], "one deleted, one appended");
        assert_eq!(origins("a\nb\nc\n", "a\nB\nc\n"), vec![Some(0), None, Some(2)], "one rewritten");
        assert_eq!(origins("", "a\nb"), vec![None, None], "from nothing, without a final newline");
        assert_eq!(origins("a\nb\n", ""), Vec::<Option<usize>>::new());
    }

    #[test]
    fn origins_past_the_context_of_a_hunk() {
        // Far apart changes make two hunks, with lines between and after that no hunk shows.
        let old: String = (0..30).map(|i| format!("{i}\n")).collect();
        let new = old.replacen("2\n", "two\n", 1).replacen("20\n", "twenty\nmore\n", 1);
        let map = origins(&old, &new);
        assert_eq!(map.len(), 31);
        assert_eq!(map[2], None);
        assert_eq!((map[19], map[20], map[21], map[22]), (Some(19), None, None, Some(21)));
        assert_eq!(map[30], Some(29));
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00");
        assert_eq!(utc(1_700_000_000_000), "2023-11-14 22:13");
        assert_eq!(utc(951_825_600_000), "2000-02-29 12:00", "a leap day");
        assert_eq!(utc(1_798_761_599_000), "2026-12-31 23:59");
    }

    #[test]
    fn blame_text_marks_what_the_walk_did_not_reach() {
        let c = Change { position: 7, landed_at: 0, goal: "say hi".into(), task: Some("task_1".into()), person: Some("ada".into()), agent: None, change: "modified".into() };
        let b = Blame {
            path: "a.txt".into(),
            position: 9,
            lines: vec![
                BlameLine { line: 1, text: "hi".into(), position: 7, person: Some("ada".into()), older: false },
                BlameLine { line: 2, text: "old".into(), position: 2, person: None, older: true },
            ],
            changes: vec![c],
        };
        let text = blame_text(&b);
        assert!(text.contains("     7  ada  1  hi"), "{text}");
        assert!(text.contains("    ^2  ?    2  old"), "{text}");
        assert!(text.contains("avp blame a.txt --at 2"), "{text}");
    }
}
