//! `git push https://<server>/<space>/<repo>.git <local>:main` lands, as `avp land` does.
//!
//! The line moves only by proven landings, and a push is one: its commits go into a scratch bare
//! repo (git http-backend's receive-pack), their tree is read against the line, a task opens on the
//! position the push started from (its goal the last commit's message, its author the person who
//! pushes), takes the change, and lands through the same proof. git's sideband shows the proof as
//! it runs; the push succeeds once the change has landed, and is refused when its proof is red,
//! when it conflicts with what landed since (a task opened on an older position replays), or while
//! it waits for a person's approval. No branch exists: a push names `main` (any other ref is refused,
//! so work never reaches the line under another name), and the advertisement names the line's commit as a `.have`, never as a ref, so git
//! sends only what is new and never asks for a fast-forward the line does not need.
//!
//! A push to a repo that does not exist yet, in a space the person may create in, creates it: its
//! first-parent history becomes the first positions, as `avp import` brings a git repo in, and
//! `git clone` then gives back the very commits.

use crate::engine::{Engine, LandOutcome};
use crate::gitbridge::{cat_blobs, git, served_repo, served_root, sync_served, tree_of, SERVED, SYNCED};
use crate::model::*;
use anyhow::{anyhow, Result};
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The agent a pushed change lands as: git, on behalf of the person who pushed.
pub const PUSH_AGENT: &str = "git-push";

const ZERO: &str = "0000000000000000000000000000000000000000";
/// What the advertisement offers: git reports each ref and shows `remote:` lines as they come.
const CAPS: &str = "report-status side-band-64k ofs-delta object-format=sha1 agent=avp";

fn pkt(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    out
}

/// What `git push` reads first (`info/refs?service=git-receive-pack`): no ref, and the line's head
/// commit as one git has, so a push sends only the commits the line does not have.
pub fn advert(e: &Engine, key: &str) -> Vec<u8> {
    let mut out = pkt(b"# service=git-receive-pack\n");
    out.extend_from_slice(b"0000");
    let tip = (|| -> Result<String> {
        let r = e.db.lock().unwrap().repo(key)?.id;
        let dir = sync_served(e, &r)?;
        Ok(git(&dir, &["rev-parse", "--verify", "-q", "refs/heads/main"])?.trim().to_string())
    })()
    .ok()
    .filter(|s| s.len() == 40);
    let first = match tip {
        Some(sha) => format!("{sha} .have\0{CAPS}\n"),
        None => format!("{ZERO} capabilities^{{}}\0{CAPS}\n"),
    };
    out.extend_from_slice(&pkt(first.as_bytes()));
    out.extend_from_slice(b"0000");
    out
}

/// One ref a push updates.
#[derive(Debug, PartialEq)]
pub struct Update {
    pub old: String,
    pub new: String,
    pub name: String,
}

/// The ref updates at the start of a push's request, and the capabilities the client chose.
pub fn commands(body: &[u8]) -> Result<(Vec<Update>, Vec<String>)> {
    let (mut at, mut updates, mut caps) = (0, vec![], vec![]);
    loop {
        let len = body.get(at..at + 4).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| usize::from_str_radix(h, 16).ok()).ok_or_else(|| anyhow!("not a git push"))?;
        if len == 0 {
            break;
        }
        anyhow::ensure!(len > 4 && at + len <= body.len(), "not a git push");
        let line = &body[at + 4..at + len];
        at += len;
        let (cmd, rest) = match line.iter().position(|b| *b == 0) {
            Some(i) => (&line[..i], Some(&line[i + 1..])),
            None => (line, None),
        };
        if let Some(c) = rest {
            caps = String::from_utf8_lossy(c).split_whitespace().map(String::from).collect();
        }
        let cmd = String::from_utf8_lossy(cmd);
        // `shallow <sha>` lines come first from a shallow clone: not an update.
        let f: Vec<&str> = cmd.trim_end().splitn(3, ' ').collect();
        if let [old, new, name] = f.as_slice() {
            updates.push(Update { old: old.to_string(), new: new.to_string(), name: name.to_string() });
        }
    }
    Ok((updates, caps))
}

/// What git shows of the push as it goes: `remote:` lines, then the ref's report.
struct Said {
    tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    sideband: bool,
}

impl Said {
    fn band(&self, n: u8, data: &[u8]) {
        for part in data.chunks(65515 - 1) {
            let mut b = vec![n];
            b.extend_from_slice(part);
            let _ = self.tx.send(Bytes::from(pkt(&b)));
        }
    }

    /// A line under `remote:`. Without the sideband git shows nothing but the report.
    fn line(&self, text: &str) {
        if self.sideband {
            self.band(2, format!("{text}\n").as_bytes());
        }
    }

    /// The end: the ref taken (Ok) or refused with a short reason, as git prints it next to the ref.
    fn report(&self, name: &str, unpacked: bool, outcome: &std::result::Result<(), String>) {
        let mut r = pkt(if unpacked { b"unpack ok\n" } else { b"unpack failed\n" });
        r.extend(pkt(match outcome {
            Ok(()) => format!("ok {name}\n"),
            Err(why) => format!("ng {name} {}\n", why.replace('\n', " ")),
        }
        .as_bytes()));
        r.extend_from_slice(b"0000");
        if self.sideband {
            self.band(1, &r);
            let _ = self.tx.send(Bytes::from_static(b"0000"));
        } else {
            let _ = self.tx.send(Bytes::from(r));
        }
    }
}

/// Who pushes and where: what the landing needs beside the request's body.
pub struct Push {
    /// The repo as the URL names it: `<space>/<repo>` or `<repo>`.
    pub key: String,
    /// The person the server knows the request comes from (None: a server without accounts).
    pub person: Option<String>,
    /// The asker, for a repo the push creates (on AVP Cloud, in their space).
    pub asker: axum::http::Extensions,
    /// Where people reach this server, for the position's URL.
    pub base_url: String,
}

/// The server's address as people reach it: `AVP_PUBLIC_URL`, else the host the request came to.
pub fn base_url(h: &HeaderMap) -> String {
    if let Some(u) = std::env::var("AVP_PUBLIC_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()) {
        return u;
    }
    let v = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or("").trim().to_string()).filter(|v| !v.is_empty());
    let host = v("x-forwarded-host").or_else(|| v("host")).unwrap_or_else(|| "localhost".into());
    let https = v("x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https"));
    format!("{}://{host}", if https { "https" } else { "http" })
}

/// `POST <repo>.git/git-receive-pack`: the commits into a scratch repo, then landed. The answer
/// streams: the proof's progress under `remote:` while it runs, the ref's report at the end.
pub async fn receive(e: Engine, push: Push, body: Bytes, content_type: String) -> Response {
    let result = |b: axum::body::Body| ([(axum::http::header::CONTENT_TYPE, "application/x-git-receive-pack-result"), (axum::http::header::CACHE_CONTROL, "no-cache")], b).into_response();
    let (updates, caps) = match commands(&body) {
        Ok(c) => c,
        Err(x) => return (StatusCode::BAD_REQUEST, format!("{x}\n")).into_response(),
    };
    // git asks with an empty request first when the push is big: it only checks it may.
    if updates.is_empty() {
        return result(axum::body::Body::empty());
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let said = Said { tx, sideband: caps.iter().any(|c| c == "side-band-64k" || c == "side-band") };
    tokio::task::spawn_blocking(move || {
        let name = updates.first().map(|u| u.name.clone()).unwrap_or_default();
        let refused = |why: &str, short: &str| {
            said.line(why);
            for u in &updates {
                said.report(&u.name, true, &Err(short.to_string()));
            }
        };
        if updates.len() > 1 {
            return refused("AVP has no branches: a push lands one change on the line. Push one ref, `git push <url> HEAD:main`.", "one ref per push");
        }
        let u = &updates[0];
        if u.new == ZERO {
            return refused("A push lands a change; nothing on the line is deleted by git.", "nothing is deleted");
        }
        if u.name.starts_with("refs/tags/") {
            return refused(&format!("Tags are the line's: `avp tag <name> <position> --repo {}`.", push.key), "tags are avp tag's");
        }
        // One line, no branches: a push to another name would land on main without saying so.
        if u.name != "refs/heads/main" {
            let short = u.name.strip_prefix("refs/heads/").unwrap_or(&u.name);
            return refused(&format!("{} has one line, main, and no branches. To land this work on it: `git push origin {short}:main`.", push.key), "push to main");
        }
        let scratch = served_root(&e.data).join(format!("push-{}.git", ulid::Ulid::new()));
        let outcome = match unpack(&e, &push, &scratch, &body, &content_type, u) {
            Err(x) => {
                said.line(&format!("{x:#}"));
                said.report(&name, false, &Err("unpack failed".into()));
                let _ = std::fs::remove_dir_all(&scratch);
                return;
            }
            Ok(()) => land(&e, &push, &scratch, u, &said),
        };
        let _ = std::fs::remove_dir_all(&scratch);
        let outcome = outcome.unwrap_or_else(|x| {
            said.line(&format!("{x:#}"));
            Err("not landed".into())
        });
        said.report(&name, true, &outcome);
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx)) });
    result(axum::body::Body::from_stream(stream))
}

/// git http-backend's receive-pack takes the request into `scratch`, a bare repo that borrows the
/// served repo's objects (the `.have` the push left out).
fn unpack(e: &Engine, push: &Push, scratch: &Path, body: &[u8], content_type: &str, u: &Update) -> Result<()> {
    std::fs::create_dir_all(scratch)?;
    git(scratch, &["init", "-q", "--bare"])?;
    if let Ok(r) = e.db.lock().unwrap().repo(&push.key) {
        let objects = served_root(&e.data).join(format!("{}.git", r.id)).join("objects");
        if objects.is_dir() {
            std::fs::write(scratch.join("objects/info/alternates"), format!("{}\n", objects.display()))?;
        }
    }
    use std::io::Write;
    let name = scratch.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let mut child = std::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", served_root(&e.data))
        .env("GIT_HTTP_EXPORT_ALL", "1")
        // A known person enables receive-pack in http-backend.
        .env("REMOTE_USER", push.person.as_deref().unwrap_or("git"))
        .env("PATH_INFO", format!("/{name}/git-receive-pack"))
        .env("REQUEST_METHOD", "POST")
        .env("QUERY_STRING", "")
        .env("CONTENT_TYPE", content_type)
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|x| anyhow!("this server has no git to take pushes with: {x}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let body = body.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&body));
    let out = child.wait_with_output()?;
    let _ = writer.join();
    let got = git(scratch, &["rev-parse", "--verify", "-q", &u.name]).unwrap_or_default();
    anyhow::ensure!(got.trim() == u.new, "the pushed commits did not unpack: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

/// The commit's message and its author's email.
fn message_of(dir: &Path, sha: &str) -> Result<(String, String)> {
    let out = git(dir, &["log", "-1", "--format=%ae%x00%B", sha])?;
    let (email, msg) = out.split_once('\0').unwrap_or(("", &out));
    Ok((msg.trim().to_string(), email.trim().to_string()))
}

/// Paths the line keeps that git never carries (the served commits leave them out).
fn carried(p: &str) -> bool {
    !p.starts_with(".avp/") && !p.starts_with(".trame/")
}

fn land(e: &Engine, push: &Push, scratch: &Path, u: &Update, said: &Said) -> Result<std::result::Result<(), String>> {
    let repo = match e.db.lock().unwrap().repo(&push.key) {
        Ok(r) => Some(r),
        Err(_) => None,
    };
    let repo = match repo {
        Some(r) => r,
        None => {
            let r = crate::seam::create_pushed(e, &push.key, &push.asker)?;
            said.line(&format!("Created {} on AVP.", r.full_name()));
            r
        }
    };
    let url = |what: &str| format!("{}/r/{}/{what}", push.base_url, repo.full_name());
    let (message, email) = message_of(scratch, &u.new)?;
    if e.head(&repo.id).is_err() {
        let last = import(e, &repo, scratch, &u.new, said)?;
        said.line(&format!("Imported: positions 0 to {last}, from the pushed history. {}", url(&format!("p/{last}"))));
        said.line(&format!("Agents work on it with `avp open --repo {} --goal \"...\"`; git keeps pushing here.", repo.full_name()));
        return Ok(Ok(()));
    }
    let Some((base, base_sha)) = base_of(e, &repo.id, scratch, &u.new)? else {
        said.line(&format!("This push shares no history with the line of {}.", repo.full_name()));
        said.line(&format!("Start from it: `git pull {}/{}.git main --allow-unrelated-histories` (or clone it), then push again.", push.base_url, repo.full_name()));
        return Ok(Err("unrelated history".into()));
    };
    let writes = writes_of(e, &repo.id, scratch, base, &base_sha, &u.new)?;
    if writes.is_empty() {
        said.line(&format!("Nothing to land: position {base} has this tree already."));
        return Ok(Ok(()));
    }
    let person = push.person.clone().filter(|p| !p.is_empty()).unwrap_or(email);
    let goal = if message.is_empty() { format!("commit {}", &u.new[..12]) } else { message };
    let req = OpenTask { goal, actor: Actor { person, agent: PUSH_AGENT.into(), session: String::new() }, scope: vec![], transcript: None };
    let task = e.open_task_at(&repo.id, req, Some(base))?;
    for w in writes {
        let path = w.path.clone();
        let o = e.write(&task.id, w)?;
        if o.lease == "refused" {
            let _ = e.abandon(&task.id);
            said.line(&format!("{path} is leased by task {} and {} is strict: push again once it lands.", o.previous_holder.unwrap_or_default(), repo.full_name()));
            return Ok(Err("leased".into()));
        }
    }
    let replay = if base == e.head(&repo.id)?.position { String::new() } else { format!(", replayed on the head ({})", e.head(&repo.id)?.position) };
    said.line(&format!("Task {} opened on position {base}{replay}: proving it as `avp land` does.", task.id));
    // The proof says nothing until it ends: a line every 10 s says it still runs.
    let started = std::time::Instant::now();
    let done = std::sync::atomic::AtomicBool::new(false);
    let landed = std::thread::scope(|s| {
        s.spawn(|| {
            let mut ticks = 0;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(250));
                ticks += 1;
                if ticks % 40 == 0 && !done.load(std::sync::atomic::Ordering::Relaxed) {
                    said.line(&format!("  proving... {}s", started.elapsed().as_secs()));
                }
            }
        });
        let l = e.land(&task.id);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        l
    })?;
    let steps = |p: &Proof| {
        for st in &p.steps {
            said.line(&format!("  {} {} ({:.1}s)", if st.exit == 0 { "green" } else { "RED  " }, st.name, st.seconds));
        }
    };
    match landed {
        LandOutcome::Landed(l) => {
            if let Ok(Some(p)) = e.db.lock().unwrap().proof_in(&repo.id, &l.proof) {
                steps(&p);
            }
            if !l.replayed {
                // Exactly the pushed commit: it is the position's, so `git pull` finds nothing new.
                if let Err(x) = take(e, &repo.id, scratch, &u.name, l.position, &u.new) {
                    eprintln!("push to {}: the position's commit is AVP's own: {x:#}", repo.full_name());
                }
            }
            said.line(&format!("Landed at position {}: {}", l.position, url(&format!("p/{}", l.position))));
            if l.checks == Some(0) {
                said.line(&format!("Proven by nothing: {NO_CHECKS}."));
            }
            if l.replayed {
                said.line("The line moved since your commit: it landed replayed on the head. `git pull` brings the position's commit.");
            }
            Ok(Ok(()))
        }
        LandOutcome::NotLanded(n) => {
            if let Some(p) = &n.proof {
                steps(p);
            }
            match n.reason.as_str() {
                "approval" => {
                    said.line(&format!("Green, and it touches guarded paths ({}): it waits for a person's approval.", n.guarded.join(", ")));
                    said.line(&format!("Task {} stays open: an admin of {} lands it with `avp approve {}`. {}", task.id, repo.full_name(), task.id, url(&format!("tasks/{}", task.id))));
                    Ok(Err(format!("waits for approval (task {})", task.id)))
                }
                "empty" | "already" => {
                    let _ = e.abandon(&task.id);
                    said.line("Nothing to land: the line has this change already.");
                    Ok(Ok(()))
                }
                "red" => {
                    let _ = e.abandon(&task.id);
                    if let Some(st) = n.proof.as_ref().and_then(|p| p.steps.iter().find(|s| s.exit != 0)) {
                        said.line(&format!("The proof is red: {} ({}) failed:", st.name, st.command));
                        let lines: Vec<&str> = st.output.lines().collect();
                        for l in &lines[lines.len().saturating_sub(20)..] {
                            said.line(&format!("    {l}"));
                        }
                    }
                    if let Some(p) = &n.proof {
                        said.line(&format!("The whole log: {}", url(&format!("proofs/{}", p.id))));
                    }
                    said.line("Nothing landed. Fix it, commit, and push again.");
                    Ok(Err("proof red".into()))
                }
                "conflict" => {
                    let _ = e.abandon(&task.id);
                    said.line(&format!("It conflicts with what landed since position {base}, in {}.", n.conflicts.join(", ")));
                    said.line("Nothing landed. `git pull`, settle the conflicts, commit, and push again.");
                    Ok(Err("conflict".into()))
                }
                other => {
                    let _ = e.abandon(&task.id);
                    said.line(&format!("Not landed ({other}){}.", n.note.map(|x| format!(": {x}")).unwrap_or_default()));
                    Ok(Err(other.to_string()))
                }
            }
        }
    }
}

/// The position the push started from, and its commit: the newest commit of the served `main`
/// (one per position, first parents) that the pushed commit descends from. None: the push shares
/// no history with the line.
fn base_of(e: &Engine, repo: &str, scratch: &Path, sha: &str) -> Result<Option<(i64, String)>> {
    let served = sync_served(e, repo)?;
    let Ok(tip) = git(&served, &["rev-parse", "--verify", "-q", "refs/heads/main"]) else { return Ok(None) };
    let tip = tip.trim();
    // The served main's commits the push does not have, newest first: those after its base.
    let after = git(scratch, &["rev-list", "--first-parent", "--count", tip, &format!("^{sha}")])?.trim().parse::<usize>().unwrap_or(0);
    let log = git(scratch, &["log", "--first-parent", &format!("--skip={after}"), "-n", "1000", "--format=%H%x00%B%x01", tip])?;
    let line = e.db.lock().unwrap().line(repo, i64::MAX, i64::MAX)?;
    let by_sha: HashMap<String, i64> = line.iter().filter_map(|l| l.git.clone().map(|g| (g, l.position))).collect();
    for rec in log.split('\x01') {
        let Some((c, msg)) = rec.trim_start_matches('\n').split_once('\0') else { continue };
        // A position's commit: the one it remembers, or one the served repo wrote for it.
        let trailer = msg.lines().find_map(|l| l.strip_prefix("AVP-Position: ")).and_then(|p| p.trim().parse::<i64>().ok());
        if let Some(p) = by_sha.get(c).copied().or(trailer).filter(|p| line.iter().any(|l| l.position == *p)) {
            return Ok(Some((p, c.to_string())));
        }
    }
    Ok(None)
}

/// The writes that make position `base` (whose commit is `base_sha`) the pushed commit's tree:
/// what git says changed between the two commits, regular files only, as the line carries.
fn writes_of(e: &Engine, repo: &str, scratch: &Path, base: i64, base_sha: &str, sha: &str) -> Result<Vec<PutWrite>> {
    let tree = e.tree_at(repo, base)?;
    let out = git(scratch, &["diff-tree", "-r", "-z", "--no-renames", base_sha, sha])?;
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let regular = |mode: &str| mode == "100644" || mode == "100755";
    let mut changes = vec![];
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let m: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if m.len() >= 4 && carried(path) {
            changes.push((path.to_string(), regular(m[1]).then(|| (m[3].to_string(), u32::from_str_radix(m[1], 8).unwrap_or(0o100644)))));
        }
    }
    let shas: Vec<String> = changes.iter().filter_map(|(_, n)| n.as_ref().map(|(s, _)| s.clone())).collect();
    small_enough(scratch, &shas)?;
    let bytes = cat_blobs(scratch, &shas)?;
    let mut writes = vec![];
    for (path, now) in changes {
        match now {
            Some((s, mode)) => {
                let blob = e.objects.put(&bytes[&s])?;
                if tree.get(&path).is_none_or(|x| x.blob != blob || x.mode != mode) {
                    writes.push(PutWrite { path, kind: WriteKind::Put, blob: Some(blob), mode });
                }
            }
            None if tree.contains_key(&path) => writes.push(PutWrite { path, kind: WriteKind::Delete, blob: None, mode: default_mode() }),
            None => {}
        }
    }
    Ok(writes)
}

/// The most one pushed file may weigh, as git hosts take them.
const MOST_BYTES: u64 = 100 << 20;

/// Refuses a push whose files weigh more than MOST_BYTES each, or all together more than ten times
/// that, before any is read: a pack of a few megabytes may inflate to gigabytes (a file of zeros),
/// and the server reads each file whole into memory.
fn small_enough(scratch: &Path, shas: &[String]) -> Result<()> {
    use std::io::Write;
    if shas.is_empty() {
        return Ok(());
    }
    let mut child = std::process::Command::new("git").args(["cat-file", "--batch-check=%(objectname) %(objectsize)"]).current_dir(scratch).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let input: String = shas.iter().map(|s| format!("{s}\n")).collect();
    std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output()?;
    let mut all = 0u64;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let (sha, size) = line.split_once(' ').unwrap_or((line, "0"));
        let size: u64 = size.parse().unwrap_or(0);
        anyhow::ensure!(size <= MOST_BYTES, "{sha} weighs {} MB: a pushed file is {} MB at most", size >> 20, MOST_BYTES >> 20);
        all += size;
    }
    anyhow::ensure!(all <= 10 * MOST_BYTES, "this push's files weigh {} MB together: {} MB at most, push it in parts", all >> 20, (10 * MOST_BYTES) >> 20);
    Ok(())
}

/// A new repo from the pushed history: each first-parent commit a position, oldest first, as
/// `avp import` does; then the served repo holds the commits themselves, so `git clone` gives them.
fn import(e: &Engine, repo: &Repo, scratch: &Path, sha: &str, said: &Said) -> Result<i64> {
    let list = git(scratch, &["rev-list", "--reverse", "--first-parent", sha])?;
    let commits: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
    said.line(&format!("Importing {} commits into the line of {}...", commits.len(), repo.full_name()));
    let mut known: HashMap<String, String> = HashMap::new();
    let mut last = 0;
    for c in &commits {
        let tree = tree_of(scratch, c)?;
        let todo: Vec<String> = tree.values().map(|(s, _)| s.clone()).filter(|s| !known.contains_key(s)).collect();
        small_enough(scratch, &todo)?;
        for (s, b) in cat_blobs(scratch, &todo)? {
            known.insert(s, e.objects.put(&b)?);
        }
        let entries: Vec<ImportEntry> = tree.iter().filter(|(p, _)| carried(p)).map(|(p, (s, m))| ImportEntry { path: p.clone(), blob: known[s].clone(), mode: *m }).collect();
        let note = git(scratch, &["log", "-1", "--format=%h %s (%an, %as)", c])?;
        last = e.import_git(&repo.id, &entries, note.trim(), Some(c))?.position;
    }
    let _one = SERVED.lock().unwrap_or_else(|p| p.into_inner());
    let served = served_repo(&e.data, &repo.id)?;
    let from = scratch.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?;
    git(&served, &["fetch", "-q", "--force", from, &format!("{sha}:refs/heads/main")])?;
    let _ = std::fs::remove_file(served.join(SYNCED));
    Ok(last)
}

/// The pushed commit becomes position `position`'s: into the served repo, remembered by the line,
/// and `main` from it: the commit itself while it is the head, else the positions since on top.
fn take(e: &Engine, repo: &str, scratch: &Path, name: &str, position: i64, sha: &str) -> Result<PathBuf> {
    {
        let _one = SERVED.lock().unwrap_or_else(|p| p.into_inner());
        let served = served_repo(&e.data, repo)?;
        let from = scratch.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?;
        git(&served, &["fetch", "-q", from, name])?;
        e.set_git(repo, position, sha)?;
        if e.head(repo)?.position == position {
            git(&served, &["update-ref", "refs/heads/main", sha])?;
            std::fs::write(served.join(SYNCED), format!("{position} {sha}\n"))?;
            return Ok(served);
        }
        let _ = std::fs::remove_file(served.join(SYNCED));
    }
    sync_served(e, repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_says_its_ref_updates_and_capabilities() {
        let a = "1111111111111111111111111111111111111111";
        let mut body = pkt(format!("{ZERO} {a} refs/heads/main\0report-status side-band-64k agent=git/2.45\n").as_bytes());
        body.extend(pkt(format!("{a} {ZERO} refs/heads/old\n").as_bytes()));
        body.extend(b"0000PACK...");
        let (u, caps) = commands(&body).unwrap();
        assert_eq!(u, vec![Update { old: ZERO.into(), new: a.into(), name: "refs/heads/main".into() }, Update { old: a.into(), new: ZERO.into(), name: "refs/heads/old".into() }]);
        assert!(caps.contains(&"side-band-64k".to_string()));
        // The probe git sends before a big push: no update.
        assert!(commands(b"0000").unwrap().0.is_empty());
        assert!(commands(b"garbage").is_err());
    }
}
