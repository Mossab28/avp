//! The Git bridge, one way each.
//!
//! `import-git`: a checkout's first-parent history becomes the first positions of a line, and
//! each position remembers its commit.
//!
//! `mirror`: every position after the last one that has a commit becomes one commit on top of it,
//! so the branch descends from what was imported: a pull request against main opens normally, and
//! main itself can be the branch (a fast-forward). Commits are deterministic (landing time as
//! dates, fixed committer, trailers): the same line gives the same commits on any machine. The
//! server remembers each commit, so an export resumes from any checkout with no local state, and
//! two exporters agree. After writing, the exported tree is compared with the head of the line,
//! file by file, before anything is pushed.
//!
//! While a team moves over, Git keeps coming in: `follow_git` brings the commits that still reach a
//! branch by Git into the line as tasks, `follow_prs` the pull requests that keep coming. `avp
//! serve` runs both when avp.toml's `[git]` asks for them.

use crate::check::{GitConfig, PullRequests};
use crate::client::Client;
use crate::model::*;
use crate::objects::hash_bytes;
use crate::view;
use anyhow::{anyhow, Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// What every git command of this process finds in its environment beside its own: `avp serve`'s
/// GH_TOKEN as the credentials of github.com over https (`GIT_CONFIG_*`), never on a command line
/// nor in a file.
static GIT_ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_env(dir, args, &[])
}

fn git_env(dir: &Path, args: &[&str], env: &[(&str, String)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    for (k, v) in GIT_ENV.get().into_iter().flatten() {
        cmd.env(k, v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().with_context(|| format!("git {}", args.join(" ")))?;
    anyhow::ensure!(out.status.success(), "git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `git ls-tree -r` of one commit: path -> (git blob sha, mode), regular files only. Symlinks and
/// submodules are git objects the line does not carry: the export starts from a commit that has
/// them and never touches them, so they survive the round trip unchanged.
fn tree_of(dir: &Path, commit: &str) -> Result<BTreeMap<String, (String, u32)>> {
    let mut map = BTreeMap::new();
    for (path, (kind, mode, sha)) in ls_tree(dir, commit)? {
        if kind == "blob" && (mode == 0o100644 || mode == 0o100755) {
            map.insert(path, (sha, mode));
        }
    }
    Ok(map)
}

/// Every entry of a commit's tree: path -> (type, mode, sha).
fn ls_tree(dir: &Path, commit: &str) -> Result<BTreeMap<String, (String, u32, String)>> {
    let out = Command::new("git").args(["ls-tree", "-r", "-z", commit]).current_dir(dir).output()?;
    anyhow::ensure!(out.status.success(), "git ls-tree {commit}: {}", String::from_utf8_lossy(&out.stderr).trim());
    let mut map = BTreeMap::new();
    for rec in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec);
        let (meta, path) = rec.split_once('\t').ok_or_else(|| anyhow!("ls-tree line"))?;
        let f: Vec<&str> = meta.split(' ').collect();
        if f.len() < 3 {
            continue;
        }
        let mode = u32::from_str_radix(f[0], 8).unwrap_or(0);
        map.insert(path.to_string(), (f[1].to_string(), mode, f[2].to_string()));
    }
    Ok(map)
}

/// Uploads the git blobs not yet known, through one `git cat-file --batch`. Returns git sha -> blake3.
fn upload_blobs(client: &Client, repo: &str, dir: &Path, shas: &[String], known: &mut HashMap<String, String>) -> Result<()> {
    let todo: Vec<&String> = shas.iter().filter(|s| !known.contains_key(*s)).collect();
    if todo.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git").args(["cat-file", "--batch"]).current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let input: String = todo.iter().map(|s| format!("{s}\n")).collect();
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    for sha in todo {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let parts: Vec<&str> = header.trim_end().split(' ').collect();
        anyhow::ensure!(parts.len() == 3 && parts[0] == sha, "cat-file header for {sha}: {header}");
        let size: usize = parts[2].parse()?;
        let mut bytes = vec![0u8; size];
        reader.read_exact(&mut bytes)?;
        let mut nl = [0u8; 1];
        reader.read_exact(&mut nl)?;
        let hash = hash_bytes(&bytes);
        if !client.has_object(repo, &hash)? {
            client.put_object(repo, bytes)?;
        }
        known.insert(sha.clone(), hash);
    }
    let _ = child.wait();
    Ok(())
}

/// Creates the repo and appends the last `history` first-parent commits of HEAD as positions,
/// each one remembering its commit.
pub fn import_git(client: &Client, dir: &Path, name: &str, policy: &str, history: usize) -> Result<(Repo, i64)> {
    anyhow::ensure!(dir.join(".git").exists(), "{} is not a git checkout", dir.display());
    let repo: Repo = client.post("/repos", &serde_json::json!({ "name": name, "policy": policy }))?;
    let list = git(dir, &["rev-list", "--reverse", "--first-parent", &format!("-n{history}"), "HEAD"])?;
    let commits: Vec<&str> = list.lines().filter(|l| !l.is_empty()).collect();
    let mut known: HashMap<String, String> = HashMap::new();
    let mut last = 0;
    for (i, sha) in commits.iter().enumerate() {
        let tree = tree_of(dir, sha)?;
        let shas: Vec<String> = tree.values().map(|(s, _)| s.clone()).collect();
        upload_blobs(client, &repo.id, dir, &shas, &mut known)?;
        let entries: Vec<ImportEntry> = tree.iter().map(|(p, (s, m))| ImportEntry { path: p.clone(), blob: known[s].clone(), mode: *m }).collect();
        let subject = git(dir, &["log", "-1", "--format=%h %s (%an, %as)", sha])?;
        let entry: LineEntry = client.post(&format!("/repos/{}/import", repo.id), &serde_json::json!({ "entries": entries, "note": subject.trim(), "git": sha }))?;
        last = entry.position;
        eprint!("\r{}/{} positions", i + 1, commits.len());
    }
    eprintln!();
    Ok((repo, last))
}

pub struct Exported {
    /// Positions written as new commits by this run.
    pub written: usize,
    /// The head of the line, and its commit.
    pub head: i64,
    pub sha: String,
    pub pushed: bool,
    /// What this run did to git's tags, one line each: written, moved, deleted, or left as git has it.
    pub tags: Vec<String>,
}

/// "Name <email>" -> (name, email).
fn parse_ident(s: &str) -> Result<(String, String)> {
    let (name, rest) = s.split_once('<').ok_or_else(|| anyhow!("committer must look like `Name <email>`: {s}"))?;
    Ok((name.trim().to_string(), rest.trim_end_matches('>').trim().to_string()))
}

fn email_of(person: &str) -> String {
    if person.contains('@') { person.to_string() } else { format!("{person}@users.avp") }
}

/// The absolute git directory of a checkout (a worktree's own, when the checkout is one).
fn git_dir(dir: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"])?.trim()))
}

/// Writes every position after the last one with a commit as commits on top of it, checks the
/// result against the head of the line, and pushes it to `branch` on `remote` (fast-forward only).
pub fn mirror(client: &Client, dir: &Path, repo: &str, branch: &str, remote: Option<&str>, committer: &str) -> Result<Exported> {
    anyhow::ensure!(dir.join(".git").exists(), "{} is not a git checkout", dir.display());
    let (cname, cemail) = parse_ident(committer)?;
    let r: Repo = client.get(&format!("/repos/{repo}"))?;
    let head: LineEntry = client.get(&format!("/repos/{}/head", r.id))?;
    let base: Option<LineEntry> = client.get(&format!("/repos/{}/line/{}/last-git", r.id, head.position))?;
    let base = base.ok_or_else(|| anyhow!("no position of {} has a git commit: create the repo with `avp import-git`", r.name))?;
    let base_sha = base.git.clone().unwrap();

    // The commit to start from must be here; fetch it from the remote if it is not.
    if git(dir, &["cat-file", "-e", &format!("{base_sha}^{{commit}}")]).is_err() {
        let remote = remote.ok_or_else(|| anyhow!("commit {base_sha} (position {}) is not in this checkout: pass --remote to fetch it", base.position))?;
        if git(dir, &["fetch", "-q", remote, &base_sha]).is_err() {
            git(dir, &["fetch", "-q", remote, branch])?;
        }
        git(dir, &["cat-file", "-e", &format!("{base_sha}^{{commit}}")]).with_context(|| format!("commit {base_sha} is not on {remote} either"))?;
    }

    // A worktree of our own, reset to exactly the base commit.
    let wt = git_dir(dir)?.join("avp-export");
    let wt_s = wt.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?.to_string();
    if !wt.join(".git").exists() {
        let _ = git(dir, &["worktree", "prune"]);
        git(dir, &["worktree", "add", "-q", "--detach", &wt_s, &base_sha])?;
    } else {
        git(&wt, &["checkout", "-q", "-f", "--detach", &base_sha])?;
    }
    git(&wt, &["clean", "-q", "-ffdx"])?;

    let base_tree: Tree = client.get(&format!("/repos/{}/tree/{}", r.id, base.position))?;
    let mut vs = view::ViewState { server: client.base.clone(), repo: r.id.clone(), task: None, known: base_tree.iter().map(|(p, e)| (p.clone(), (e.blob.clone(), e.mode))).collect(), ..Default::default() };
    let mut merge = match remote {
        Some(remote) if head.position > base.position => merge_of(client, dir, &r.id, remote, branch, &base_sha, base.position)?,
        _ => None,
    };
    let mut sha = base_sha.clone();
    let mut written = 0;
    for pos in base.position + 1..=head.position {
        let entry: LineEntry = client.get::<Vec<LineEntry>>(&format!("/repos/{}/line?from={pos}&limit=1", r.id))?.into_iter().next().ok_or_else(|| anyhow!("no position {pos}"))?;
        let tree: Tree = client.get(&format!("/repos/{}/tree/{pos}", r.id))?;
        view::materialize(client, &tree, &wt, &mut vs)?;
        let _ = std::fs::remove_dir_all(wt.join(".avp"));
        let _ = std::fs::remove_dir_all(wt.join(".trame"));
        // -f: the line is the truth; a file it holds goes in even if an ignore rule matches it.
        git(&wt, &["add", "-A", "-f"])?;
        let (aname, aemail, msg) = match entry.task.as_deref() {
            Some(t) => {
                let s: TaskStatus = client.get(&format!("/repos/{}/tasks/{t}", r.id))?;
                let person = if s.task.actor.person.is_empty() { "avp".to_string() } else { s.task.actor.person.clone() };
                let agent = s.task.actor.agent.trim();
                let mut msg = format!("{}\n\nAVP-Position: {pos}\nAVP-Task: {t}\nAVP-Person: {person}\nAVP-Agent: {agent}\n", s.task.goal.trim());
                // Which instance of the agent: two Claude Code sessions are two authors.
                if !s.task.actor.session.is_empty() {
                    msg.push_str(&format!("AVP-Session: {}\n", s.task.actor.session));
                }
                if let Some(p) = &entry.proof {
                    msg.push_str(&format!("AVP-Proof: {p}\n"));
                }
                // The agent in the author's name, so that `git blame` says it on every line; the
                // email stays the person's.
                let name = if agent.is_empty() || agent == "human" { person.clone() } else { format!("{person} ({agent})") };
                (name, email_of(&person), msg)
            }
            None => (cname.clone(), cemail.clone(), format!("{}\n\nAVP-Position: {pos}\n", entry.note.trim())),
        };
        let date = format!("@{} +0000", entry.landed_at / 1000);
        let env = [
            ("GIT_AUTHOR_NAME", aname),
            ("GIT_AUTHOR_EMAIL", aemail),
            ("GIT_AUTHOR_DATE", date.clone()),
            ("GIT_COMMITTER_NAME", cname.clone()),
            ("GIT_COMMITTER_EMAIL", cemail.clone()),
            ("GIT_COMMITTER_DATE", date),
        ];
        // One commit per position, even an empty one: position N is always commit N.
        match merge.take_if(|(at, _)| *at <= pos) {
            Some((_, tip)) => {
                let tree = git(&wt, &["write-tree"])?.trim().to_string();
                let c = git_env(&wt, &["-c", "commit.gpgsign=false", "commit-tree", &tree, "-p", "HEAD", "-p", &tip, "-m", msg.trim_end()], &env)?;
                git(&wt, &["reset", "-q", "--soft", c.trim()])?;
            }
            None => {
                git_env(&wt, &["-c", "commit.gpgsign=false", "commit", "-q", "--no-verify", "--allow-empty", "--cleanup=verbatim", "-m", &msg], &env)?;
            }
        }
        sha = git(&wt, &["rev-parse", "HEAD"])?.trim().to_string();
        let _: LineEntry = client.post(&format!("/repos/{}/line/{pos}/git", r.id), &serde_json::json!({ "sha": sha }))?;
        written += 1;
        eprint!("\r{written}/{} positions written", head.position - base.position);
    }
    if written > 0 {
        eprintln!();
    }

    // The exported tree must be the head of the line, file by file, before anything leaves.
    let head_tree: Tree = client.get(&format!("/repos/{}/tree/{}", r.id, head.position))?;
    verify(&wt, &head_tree).with_context(|| format!("the export of position {} differs from the line: nothing pushed", head.position))?;

    let mut pushed = false;
    if let Some(remote) = remote {
        // From the checkout, not the worktree: a remote given as a relative path is relative to it.
        if let Err(e) = git(dir, &["push", "-q", remote, &format!("{sha}:refs/heads/{branch}")]) {
            let m = format!("{e:#}");
            if m.contains("non-fast-forward") || m.contains("fetch first") || m.contains("[rejected]") {
                // The first commit on the branch that is not ours, so the alert names what to look at.
                let foreign = git(dir, &["fetch", "-q", remote, branch]).ok().and_then(|_| git(dir, &["rev-list", "--reverse", &format!("{sha}..FETCH_HEAD")]).ok()).and_then(|l| l.lines().next().map(str::to_string));
                let _: serde_json::Value = client.post(&format!("/repos/{}/mirror", r.id), &serde_json::json!({ "blocked": true, "branch": branch, "commit": foreign, "position": head.position })).unwrap_or_default();
                anyhow::bail!("push to {remote} {branch} refused: it only fast-forwards, and {branch} holds a commit AVP did not write");
            }
            return Err(e.context(format!("push to {remote} {branch} failed")));
        }
        pushed = true;
        let _: serde_json::Value = client.post(&format!("/repos/{}/mirror", r.id), &serde_json::json!({ "blocked": false, "branch": branch, "commit": sha, "position": head.position })).unwrap_or_default();
    } else {
        // No remote: the line shows on this checkout's own branch (`git log avp/line`).
        let ref_name = format!("refs/heads/{branch}");
        let checked_out = git(dir, &["symbolic-ref", "-q", "HEAD"]).map(|h| h.trim() == ref_name).unwrap_or(false);
        anyhow::ensure!(!checked_out, "{branch} is checked out here: the mirror does not move a branch under your files");
        let old = git(dir, &["rev-parse", "-q", "--verify", &ref_name]).ok().map(|s| s.trim().to_string());
        if old.as_deref() != Some(sha.as_str()) {
            if let Some(o) = &old {
                if git(dir, &["merge-base", "--is-ancestor", o, &sha]).is_err() {
                    // Said as a refused push says it: the branch tip is the commit to look at.
                    let _: serde_json::Value = client.post(&format!("/repos/{}/mirror", r.id), &serde_json::json!({ "blocked": true, "branch": branch, "commit": o, "position": head.position })).unwrap_or_default();
                    anyhow::bail!("{branch} holds a commit AVP did not write: left as it is");
                }
            }
            git(dir, &["update-ref", &ref_name, &sha])?;
        }
    }
    let tags = mirror_tags(client, dir, &r.id, remote)?;
    Ok(Exported { written, head: head.position, sha, pushed, tags })
}

/// Makes git's tags say what the line's say: each tag on its position's commit, here and on
/// `remote`, moved from the commit AVP last wrote it on, deleted once deleted on the line, so a
/// release workflow keyed on tags runs as it did. A git tag AVP did not write is never moved nor
/// deleted: it is named instead. The server remembers where each tag was written, so a pass with
/// nothing new reads one list and does nothing.
fn mirror_tags(client: &Client, dir: &Path, repo: &str, remote: Option<&str>) -> Result<Vec<String>> {
    // A server from before tags has none to write.
    let Ok(tags) = client.get::<Vec<crate::db::Tag>>(&format!("/repos/{repo}/tags?deleted=true")) else { return Ok(vec![]) };
    let mut said = Vec::new();
    for t in tags {
        let want = if t.deleted { None } else { t.commit.clone() };
        // Written already, or on a position no commit holds (the line before the git history).
        if t.git == want || (want.is_none() && !t.deleted) {
            continue;
        }
        let name = &t.name;
        let r = format!("refs/tags/{name}");
        let ours = |sha: &Option<String>| sha.is_none() || *sha == t.git || *sha == want;
        let here = git(dir, &["rev-parse", "-q", "--verify", &format!("{r}^{{commit}}")]).ok().map(|s| s.trim().to_string());
        if !ours(&here) {
            said.push(format!("tag {name} left as it is: git's names another commit here"));
            continue;
        }
        if let Some(c) = &want {
            if git(dir, &["cat-file", "-e", &format!("{c}^{{commit}}")]).is_err() {
                if let Some(remote) = remote {
                    let _ = git(dir, &["fetch", "-q", remote, c]);
                }
            }
        }
        let local = match &want {
            Some(c) => git(dir, &["update-ref", &r, c]),
            None if here.is_some() => git(dir, &["update-ref", "-d", &r]),
            None => Ok(String::new()),
        };
        if let Err(e) = local {
            said.push(format!("tag {name} not written: {e:#}"));
            continue;
        }
        if let Some(remote) = remote {
            let there = git(dir, &["ls-remote", remote, &r])?.lines().find_map(|l| l.split_once('\t').filter(|(_, n)| *n == r).map(|(s, _)| s.to_string()));
            if there != want {
                if !ours(&there) {
                    said.push(format!("tag {name} left as it is on {remote}: it names a commit AVP did not write"));
                    continue;
                }
                // The lease: the push goes only if the remote still holds what was just read.
                let lease = format!("--force-with-lease={r}:{}", there.as_deref().unwrap_or(""));
                let target = want.as_ref().map(|c| format!("{c}:{r}")).unwrap_or_else(|| format!(":{r}"));
                if let Err(e) = git(dir, &["push", "-q", &lease, remote, &target]) {
                    said.push(format!("tag {name} not pushed to {remote}: {e:#}"));
                    continue;
                }
            }
        }
        let _: serde_json::Value = client.post(&format!("/repos/{repo}/tags/git"), &serde_json::json!({ "name": name, "sha": want }))?;
        said.push(match (&want, &t.git) {
            (Some(c), None) => format!("tag {name} written on {}", &c[..12.min(c.len())]),
            (Some(c), Some(_)) => format!("tag {name} moved to {}", &c[..12.min(c.len())]),
            (None, _) => format!("tag {name} deleted"),
        });
    }
    Ok(said)
}

/// When `branch` on `remote` moved by Git since the commit the mirror goes on from, and each of its
/// commits since is the line's (brought in by a follow and landed, or left out by a person): the
/// position to write as a merge of its tip, the one that brought that tip in (else the first to
/// write), so the push stays a fast-forward and the branch keeps its history. None otherwise: a
/// commit the line has not taken in yet still makes the push refuse, and the next pass merges it.
fn merge_of(client: &Client, dir: &Path, repo: &str, remote: &str, branch: &str, base_sha: &str, base: i64) -> Result<Option<(i64, String)>> {
    if git(dir, &["fetch", "-q", remote, branch]).is_err() {
        return Ok(None); // not on the remote yet
    }
    let tip = git(dir, &["rev-parse", "FETCH_HEAD"])?.trim().to_string();
    if git(dir, &["merge-base", "--is-ancestor", &tip, base_sha]).is_ok() {
        return Ok(None);
    }
    let foreign = git(dir, &["rev-list", "--first-parent", &tip, &format!("^{base_sha}")])?;
    let taken = git_tasks(client, repo)?;
    let settled = |c: &str| taken.get(c).is_some_and(|t| t.state != TaskState::Open);
    if !foreign.lines().all(settled) {
        return Ok(None);
    }
    let at = taken.get(&tip).and_then(|t| t.landed_position).filter(|p| *p > base).unwrap_or(base + 1);
    Ok(Some((at, tip)))
}

/// The tasks of the agent `git`, by the commit each was opened for.
fn git_tasks(client: &Client, repo: &str) -> Result<HashMap<String, Task>> {
    // A server from before `agent=` sends every task: the filter is ours too.
    let tasks: Vec<Task> = client.get(&format!("/repos/{repo}/tasks?agent={GIT_AGENT}"))?;
    Ok(tasks.into_iter().filter(|t| t.actor.agent == GIT_AGENT).filter_map(|t| t.transcript.as_deref().and_then(commit_of).map(|sha| (sha, t))).collect())
}

/// HEAD of the worktree holds exactly the line's files: same paths, same bytes, same exec bit,
/// and no other regular file (symlinks and submodules the line does not carry are left alone).
fn verify(wt: &Path, tree: &Tree) -> Result<()> {
    let committed = ls_tree(wt, "HEAD")?;
    let mut problems = Vec::new();
    for (path, e) in tree {
        match committed.get(path) {
            Some((kind, mode, _)) if kind == "blob" && (*mode == 0o100644 || *mode == 0o100755) => {
                if (*mode & 0o111 != 0) != (e.mode & 0o111 != 0) {
                    problems.push(format!("{path}: mode {mode:o}, the line says {:o}", e.mode));
                }
                match std::fs::read(wt.join(path)) {
                    Ok(bytes) if hash_bytes(&bytes) == e.blob => {}
                    Ok(_) => problems.push(format!("{path}: content differs")),
                    Err(err) => problems.push(format!("{path}: {err}")),
                }
            }
            Some((kind, mode, _)) => problems.push(format!("{path}: committed as {kind} {mode:o}")),
            None => problems.push(format!("{path}: missing from the commit")),
        }
        if problems.len() > 20 {
            break;
        }
    }
    for (path, (kind, mode, _)) in &committed {
        if kind == "blob" && (*mode == 0o100644 || *mode == 0o100755) && !tree.contains_key(path) {
            problems.push(format!("{path}: in the commit, not in the line"));
        }
    }
    anyhow::ensure!(problems.is_empty(), "{} difference(s): {}", problems.len(), problems.join("; "));
    Ok(())
}

/// What became of one pull request.
pub struct ImportedPr {
    pub number: u64,
    pub title: String,
    pub task: Option<String>,
    /// "landed at N", "open (proof green)", "conflict on ...", ...
    pub outcome: String,
    pub landed: Option<i64>,
}

/// GitHub's REST API with the token of GH_TOKEN or GITHUB_TOKEN, when there is one.
fn github(method: reqwest::Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value> {
    let token = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")).map_err(|_| anyhow!("set GH_TOKEN to read and close pull requests"))?;
    let http = reqwest::blocking::Client::new();
    let mut req = http.request(method, format!("https://api.github.com{path}")).bearer_auth(token).header("accept", "application/vnd.github+json").header("user-agent", "avp");
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send()?;
    let status = r.status();
    let v: serde_json::Value = r.json().unwrap_or(serde_json::Value::Null);
    anyhow::ensure!(status.is_success(), "GitHub {path}: {status} {}", v["message"].as_str().unwrap_or(""));
    Ok(v)
}

/// "owner/name" of a checkout's remote on GitHub.
fn github_slug(dir: &Path, remote: &str) -> Result<String> {
    let url = git(dir, &["remote", "get-url", remote])?.trim().to_string();
    let slug = url.trim_end_matches(".git").rsplit(|c| c == ':' || c == '/').take(2).collect::<Vec<_>>();
    anyhow::ensure!(slug.len() == 2, "cannot read owner/name from {url}");
    Ok(format!("{}/{}", slug[1], slug[0]))
}

/// Brings a GitHub pull request into the line as a task: its change, merged three-way onto the
/// git commit of exactly the task's base position, laid into a fresh view, proven, and landed
/// when `land` (then the PR is closed with a comment naming the position). A conflict with the
/// line abandons the task and says where. `dir` is a git checkout of the repo whose `remote`
/// reaches GitHub; the view stays under `views`, or is removed once its files are sent (the task
/// is on the server). The task's link is the PR at its head commit: `follow_prs` sees an update.
pub fn import_pr(client: &Client, dir: &Path, repo: &str, number: u64, remote: &str, views: Option<&Path>, land: bool) -> Result<ImportedPr> {
    anyhow::ensure!(dir.join(".git").exists(), "{} is not a git checkout", dir.display());
    let slug = github_slug(dir, remote)?;
    let pr = github(reqwest::Method::GET, &format!("/repos/{slug}/pulls/{number}"), None)?;
    let title = pr["title"].as_str().unwrap_or("").to_string();
    anyhow::ensure!(pr["state"] == "open", "PR #{number} is {}", pr["state"]);
    let branch = format!("avp-pr-{number}");
    git(dir, &["fetch", "-q", remote, &format!("+pull/{number}/head:{branch}")])?;

    // The task first: it fixes the base position; then the git commit of exactly that position.
    let r: Repo = client.get(&format!("/repos/{repo}"))?;
    let actor = Actor {
        person: std::env::var("AVP_PERSON").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "someone".into()),
        agent: format!("pr-{number}"),
        session: new_id("s"),
    };
    let goal = format!("{title} (PR #{number})");
    let link = pr["html_url"].as_str().map(|u| match pr["head"]["sha"].as_str() {
        Some(head) => format!("{u}/commits/{head}"),
        None => u.to_string(),
    });
    let task: Task = client.post(&format!("/repos/{}/tasks", r.id), &OpenTask { goal, actor, scope: vec![], transcript: link })?;
    let abandon = |why: String| -> Result<ImportedPr> {
        let _: serde_json::Value = client.post(&format!("/repos/{}/tasks/{}/abandon", r.id, task.id), &serde_json::json!({}))?;
        Ok(ImportedPr { number, title: title.clone(), task: None, outcome: why, landed: None })
    };
    let mut base_sha = None;
    for _ in 0..60 {
        let e: Option<LineEntry> = client.get(&format!("/repos/{}/line/{}/last-git", r.id, task.base_position))?;
        if let Some(e) = e.filter(|e| e.position == task.base_position) {
            base_sha = e.git;
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let Some(base_sha) = base_sha else { return abandon(format!("position {} is not on git yet (is the mirror running?)", task.base_position)) };
    if git(dir, &["cat-file", "-e", &format!("{base_sha}^{{commit}}")]).is_err() {
        git(dir, &["fetch", "-q", remote, &base_sha]).ok();
    }

    // The three-way merge, in a worktree of our own at the base commit.
    let wt = git_dir(dir)?.join(format!("avp-pr-{number}"));
    let wt_s = wt.to_str().ok_or_else(|| anyhow!("non-UTF-8 path"))?.to_string();
    let _ = git(dir, &["worktree", "remove", "--force", &wt_s]);
    let _ = git(dir, &["worktree", "prune"]);
    git(dir, &["worktree", "add", "-q", "--detach", &wt_s, &base_sha])?;
    let merged = git_env(&wt, &["merge", "-q", "--squash", &branch], &[("GIT_AUTHOR_NAME", "avp".into()), ("GIT_AUTHOR_EMAIL", "avp@users.avp".into()), ("GIT_COMMITTER_NAME", "avp".into()), ("GIT_COMMITTER_EMAIL", "avp@users.avp".into())]);
    let unmerged = git(&wt, &["diff", "--name-only", "--diff-filter=U"])?;
    if merged.is_err() || !unmerged.trim().is_empty() {
        let files = unmerged.split_whitespace().collect::<Vec<_>>().join(", ");
        let _ = git(dir, &["worktree", "remove", "--force", &wt_s]);
        return abandon(format!("conflict with the line on {files}"));
    }
    let changes = git(&wt, &["diff", "--cached", "--no-renames", "--name-status", &base_sha])?;

    // The view: the base laid out, then the PR's files over it.
    let vname = format!("pr-{number}-{}", &task.id[task.id.len() - 6..]);
    let vdir = match views {
        Some(v) => v.join(&vname),
        None => git_dir(dir)?.join(format!("avp-{vname}")),
    };
    std::fs::create_dir_all(&vdir)?;
    let mut vs = view::ViewState { server: client.base.clone(), repo: r.id.clone(), task: Some(task.id.clone()), known: BTreeMap::new(), ..Default::default() };
    let tree: Tree = client.get(&format!("/repos/{}/tree/{}", r.id, task.base_position))?;
    view::materialize(client, &tree, &vdir, &mut vs)?;
    let mut n = 0;
    for line in changes.lines() {
        let Some((st, path)) = line.split_once('\t') else { continue };
        let dest = vdir.join(path);
        if st == "D" {
            let _ = std::fs::remove_file(&dest);
        } else {
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            let meta = std::fs::symlink_metadata(wt.join(path))?;
            if meta.file_type().is_symlink() {
                continue; // the line does not carry symlinks; the mirror keeps git's own
            }
            std::fs::copy(wt.join(path), &dest)?;
        }
        n += 1;
    }
    let _ = git(dir, &["worktree", "remove", "--force", &wt_s]);
    view::sync(client, &vdir, &mut vs)?;
    view::save(&vdir, &vs)?;
    if views.is_none() {
        let _ = std::fs::remove_dir_all(&vdir);
    }
    let proof: Proof = client.check(&r.id, &task.id)?;
    let green = proof.verdict == Verdict::Green;
    if !land || !green {
        let at = if views.is_some() { format!("in {}", vdir.display()) } else { format!("as {}", task.id) };
        return Ok(ImportedPr { number, title, task: Some(task.id), outcome: format!("open {at} ({n} files), proof {:?}", proof.verdict), landed: None });
    }
    match client.land(&r.id, &task.id)? {
        Ok(l) => {
            close_pr(&slug, number, l.position, &task.id, &l.proof);
            Ok(ImportedPr { number, title, task: Some(task.id), outcome: format!("landed at {}", l.position), landed: Some(l.position) })
        }
        Err(nl) => Ok(ImportedPr { number, title, task: Some(task.id), outcome: format!("not landed: {}{}", nl.reason, if nl.guarded.is_empty() { String::new() } else { format!(" (guarded: {})", nl.guarded.join(", ")) }), landed: None }),
    }
}

/// Says on the pull request where its task landed, and closes it. Failures are left: the change is
/// on the line, and the next look of `follow_prs` tries again.
fn close_pr(slug: &str, number: u64, position: i64, task: &str, proof: &str) -> bool {
    let body = format!("AVP: landed at position {position} (task {task}, proof {proof}). The mirror writes it on the branch; this pull request is closed.");
    github(reqwest::Method::POST, &format!("/repos/{slug}/issues/{number}/comments"), Some(serde_json::json!({ "body": body }))).is_ok()
        && github(reqwest::Method::PATCH, &format!("/repos/{slug}/pulls/{number}"), Some(serde_json::json!({ "state": "closed" }))).is_ok()
}

/// The commit a pull request's task was opened at: the end of its link, `.../pull/12/commits/<sha>`.
fn pr_head(t: &Task) -> Option<&str> {
    t.transcript.as_deref()?.split_once("/commits/").map(|(_, sha)| sha)
}

/// One look at the open pull requests of `remote` on GitHub (GH_TOKEN), oldest first. Each new one,
/// and each one whose head moved since its task was opened (its old task, still open, abandoned
/// first), is brought in by `import_pr`, landed when `land` and green. One whose task has landed
/// since, by a person's `avp land`, gets its position as a comment and is closed. Drafts wait.
pub fn follow_prs(client: &Client, dir: &Path, repo: &str, remote: &str, views: Option<&Path>, land: bool) -> Result<Vec<ImportedPr>> {
    let slug = github_slug(dir, remote)?;
    let r: Repo = client.get(&format!("/repos/{repo}"))?;
    let prs = github(reqwest::Method::GET, &format!("/repos/{slug}/pulls?state=open&sort=created&direction=asc&per_page=100"), None)?;
    // A server from before `agent=` sends every task: the filter is ours too.
    let tasks: Vec<Task> = client.get::<Vec<Task>>(&format!("/repos/{}/tasks?agent=pr-*", r.id))?.into_iter().filter(|t| t.actor.agent.starts_with("pr-")).collect();
    let mut out = Vec::new();
    for pr in prs.as_array().into_iter().flatten() {
        let (Some(number), Some(url)) = (pr["number"].as_u64(), pr["html_url"].as_str()) else { continue };
        if pr["draft"].as_bool() == Some(true) {
            continue;
        }
        let title = pr["title"].as_str().unwrap_or("").to_string();
        let head = pr["head"]["sha"].as_str().unwrap_or("");
        let agent = format!("pr-{number}");
        let theirs = |t: &&Task| t.actor.agent == agent && t.transcript.as_deref().is_some_and(|x| x == url || x.starts_with(&format!("{url}/")));
        // Tasks come oldest first: the last one is the PR as it was last brought in.
        if let Some(t) = tasks.iter().filter(theirs).last() {
            // A task from before the link named the head: taken for the PR as it is.
            if pr_head(t).is_none_or(|h| h == head) {
                if let (TaskState::Landed, Some(p)) = (t.state, t.landed_position) {
                    let proof = client.get::<Vec<LineEntry>>(&format!("/repos/{}/line?from={p}&limit=1", r.id))?.into_iter().next().and_then(|e| e.proof).unwrap_or_default();
                    let outcome = if close_pr(&slug, number, p, &t.id, &proof) { format!("landed at {p}, closed") } else { format!("landed at {p}, not closed yet") };
                    out.push(ImportedPr { number, title, task: Some(t.id.clone()), outcome, landed: Some(p) });
                }
                continue;
            }
            if t.state == TaskState::Open {
                let _: serde_json::Value = client.post(&format!("/repos/{}/tasks/{}/abandon", r.id, t.id), &serde_json::json!({}))?;
            }
        }
        out.push(import_pr(client, dir, repo, number, remote, views, land).unwrap_or_else(|e| ImportedPr { number, title, task: None, outcome: format!("error: {e:#}"), landed: None }));
    }
    Ok(out)
}

/// The agent a commit brought in from Git lands as.
pub const GIT_AGENT: &str = "git";

/// Opens a task on a position, as `req` says, actor included: over HTTP from the CLI, in the
/// engine itself in `avp serve`, whose HTTP surface would make every task the server's own person.
pub type Opener<'a> = &'a dyn Fn(&str, &OpenTask, i64) -> Result<Task>;

/// `Opener` over HTTP. A server that names who asks (AVP_TOKEN, a proxy) puts that person instead.
pub fn open_over_http(client: &Client) -> impl Fn(&str, &OpenTask, i64) -> Result<Task> + '_ {
    move |repo: &str, req: &OpenTask, base: i64| client.post(&format!("/repos/{repo}/tasks?base={base}"), req)
}

/// What the commits of a followed branch are matched with, kept from one look to the next.
#[derive(Default)]
pub struct Known {
    loaded: bool,
    /// The line's commits: those it was imported from, those the mirror wrote.
    line: HashSet<String>,
    /// The commits brought in, by task.
    taken: HashMap<String, String>,
}

/// What became of a commit of the followed branch.
pub struct Followed {
    pub sha: String,
    pub subject: String,
    pub task: Option<String>,
    /// "landed at N", "open: ...", "nothing to bring in: ...".
    pub outcome: String,
    pub landed: Option<i64>,
    /// Its task is open: the commits after it wait for it.
    pub open: bool,
}

/// Where a commit is seen: on GitHub when `origin` is there, else its sha.
fn commit_ref(dir: &Path, sha: &str) -> String {
    match git(dir, &["remote", "get-url", "origin"]) {
        Ok(url) if url.contains("github.com") => github_slug(dir, "origin").map_or_else(|_| format!("git commit {sha}"), |s| format!("https://github.com/{s}/commit/{sha}")),
        _ => format!("git commit {sha}"),
    }
}

/// The commit a task of the agent `git` was opened for: the end of its link.
fn commit_of(link: &str) -> Option<String> {
    let last = link.rsplit(['/', ' ']).next()?;
    (last.len() >= 40 && last.bytes().all(|b| b.is_ascii_hexdigit())).then(|| last.to_string())
}

/// Fetches `branch` from `remote` into `refs/remotes/<remote>/<branch>`, and says that ref; a checkout
/// with no such remote follows its own branch.
pub fn fetch_branch(dir: &Path, remote: &str, branch: &str) -> Result<String> {
    if git(dir, &["remote", "get-url", remote]).is_err() {
        return Ok(format!("refs/heads/{branch}"));
    }
    let rev = format!("refs/remotes/{remote}/{branch}");
    git(dir, &["fetch", "-q", remote, &format!("+refs/heads/{branch}:{rev}")])?;
    Ok(rev)
}

/// One look at `rev` (a branch, `origin/main`, HEAD) of the checkout `dir`: its first-parent commits
/// the line does not have yet, oldest first, each one a task on the head, opened as its author with
/// the agent `git` and its subject as goal, its change merged three-way onto the head, proven and
/// landed. The walk back stops at a commit the line has: one it was imported from, one the mirror
/// wrote (never taken back in), one brought in already. A commit that does not land leaves its task
/// open, laid out under `views` when given, and the commits after it wait for it: they build on it.
pub fn follow_git(client: &Client, dir: &Path, repo: &str, rev: &str, views: Option<&Path>, known: &mut Known, open: Opener) -> Result<Vec<Followed>> {
    let r: Repo = client.get(&format!("/repos/{repo}"))?;
    if !known.loaded {
        let head: LineEntry = client.get(&format!("/repos/{}/head", r.id))?;
        let line: Vec<LineEntry> = client.get(&format!("/repos/{}/line?from={}&limit={}", r.id, head.position, head.position + 1))?;
        known.line = line.into_iter().filter_map(|e| e.git).collect();
        known.loaded = true;
    }
    // At each look: what another follower brought in counts too.
    known.taken.extend(git_tasks(client, &r.id)?.into_iter().map(|(sha, t)| (sha, t.id)));
    let tip = git(dir, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")]).map_err(|_| anyhow!("{rev} is not a commit of {}", dir.display()))?.trim().to_string();
    const WALK: usize = 1000;
    let log = git(dir, &["log", "--first-parent", &format!("-n{WALK}"), "--format=%H %(trailers:key=AVP-Position,valueonly,separator=%x2C)", &tip])?;
    let (mut new, mut stop) = (Vec::new(), None);
    for l in log.lines() {
        let (sha, pos) = l.split_once(' ').unwrap_or((l, ""));
        if known.line.contains(sha) || known.taken.contains_key(sha) || mirrored(client, &r.id, sha, pos, &mut known.line)? {
            stop = Some(sha.to_string());
            break;
        }
        new.push(sha.to_string());
    }
    let stop = stop.ok_or_else(|| anyhow!("none of the last {} commits of {rev} is on the line of {} (was it rewritten, or imported from another repo?): nothing brought in", new.len(), r.name))?;
    new.reverse();
    // The newest commit brought in is still an open task: what came after it waits for it.
    if let Some(t) = known.taken.get(&stop) {
        let s: TaskStatus = client.get(&format!("/repos/{}/tasks/{t}", r.id))?;
        if s.task.state == TaskState::Open {
            let laid = views.map(|v| v.join(format!("git-{}", &stop[..12]))).filter(|d| !d.join(".avp").exists());
            if let Some(d) = &laid {
                lay_out(client, &r.id, t, d)?;
            }
            let mut last: BTreeMap<&str, WriteKind> = BTreeMap::new();
            for w in s.writes.iter().filter(|w| !w.undone) {
                last.insert(&w.path, w.kind);
            }
            let conflicts: Vec<&str> = last.into_iter().filter(|(_, k)| *k == WriteKind::Conflict).map(|(p, _)| p).collect();
            let proof = match &s.last_proof {
                _ if !conflicts.is_empty() => format!("in conflict in {}", conflicts.join(", ")),
                Some(p) => format!("proof {:?}", p.verdict).to_lowercase(),
                None => "not proven".into(),
            };
            let outcome = format!("open as {t}, {proof}{}{}", laid.map(|d| format!(", laid out in {}", d.display())).unwrap_or_default(), waiting(new.len()));
            return Ok(vec![Followed { sha: stop.clone(), subject: subject_of(dir, &stop), task: Some(t.clone()), outcome, landed: None, open: true }]);
        }
    }
    let mut out = Vec::new();
    for (i, sha) in new.iter().enumerate() {
        let mut f = bring_in(client, dir, &r, sha, views, open)?;
        if let Some(t) = &f.task {
            known.taken.insert(sha.clone(), t.clone());
        }
        let stuck = f.open;
        if stuck {
            f.outcome.push_str(&waiting(new.len() - i - 1));
        }
        out.push(f);
        if stuck {
            break;
        }
    }
    Ok(out)
}

fn waiting(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => "; the commit after it waits for it".into(),
        n => format!("; the {n} commits after it wait for it"),
    }
}

fn subject_of(dir: &Path, sha: &str) -> String {
    git(dir, &["log", "-1", "--format=%s", sha]).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Whether the mirror wrote `sha`: its message names a position (`AVP-Position`), and that position
/// is this commit on the line. A squash on GitHub can carry the trailer of a commit it squashed.
fn mirrored(client: &Client, repo: &str, sha: &str, trailer: &str, line: &mut HashSet<String>) -> Result<bool> {
    let Some(pos) = trailer.split(',').next().and_then(|p| p.trim().parse::<i64>().ok()) else { return Ok(false) };
    let entry = client.get::<Vec<LineEntry>>(&format!("/repos/{repo}/line?from={pos}&limit=1"))?.into_iter().next();
    let ours = entry.is_some_and(|e| e.position == pos && e.git.as_deref() == Some(sha));
    if ours {
        line.insert(sha.to_string());
    }
    Ok(ours)
}

/// Lays an open task out in a folder of its own, its writes in place, for a person or an agent to
/// settle there: fix it and `avp land`, or `avp abandon`.
fn lay_out(client: &Client, repo: &str, task: &str, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let s: TaskStatus = client.get(&format!("/repos/{repo}/tasks/{task}"))?;
    let tree: Tree = client.get(&format!("/repos/{repo}/tasks/{task}/tree"))?;
    let mut vs = view::ViewState { server: client.base.clone(), repo: repo.to_string(), task: Some(task.to_string()), base: Some((s.task.base_position, s.task.base_tree.clone())), ..Default::default() };
    view::materialize(client, &tree, dir, &mut vs)?;
    Ok(())
}

/// One commit as a task: opened on the head, given the commit's change, landed.
fn bring_in(client: &Client, dir: &Path, r: &Repo, sha: &str, views: Option<&Path>, open: Opener) -> Result<Followed> {
    let meta = git(dir, &["log", "-1", "--format=%ae%x00%an%x00%P%x00%s", sha])?;
    let mut f = meta.trim_end_matches('\n').splitn(4, '\0');
    let (email, name, parents, subject) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or("").to_string());
    let parent = parents.split_whitespace().next();
    let said = |task: Option<String>, outcome: String, landed: Option<i64>, open: bool| Followed { sha: sha.to_string(), subject: subject.clone(), task, outcome, landed, open };
    // The writes first, on the head as it is: a commit that fails here leaves no task behind.
    let head: LineEntry = client.get(&format!("/repos/{}/head", r.id))?;
    let base: Tree = client.get(&format!("/repos/{}/tree/{}", r.id, head.position))?;
    let (writes, conflicts) = commit_writes(client, &r.id, dir, parent, sha, &base)?;
    let person = if email.is_empty() { name } else { email };
    let goal = if subject.trim().is_empty() { format!("commit {}", &sha[..12]) } else { subject.clone() };
    let req = OpenTask { goal, actor: Actor { person: person.to_string(), agent: GIT_AGENT.into(), session: String::new() }, scope: vec![], transcript: Some(commit_ref(dir, sha)) };
    let task = open(&r.id, &req, head.position)?;
    let left_out = |why: &str| -> Result<Followed> {
        // A task all the same, dropped at once: the commit counts as taken in, for the next look and
        // for the mirror, which merges a branch only once the line has each of its commits.
        let _: serde_json::Value = client.post(&format!("/repos/{}/tasks/{}/abandon", r.id, task.id), &serde_json::json!({}))?;
        Ok(said(Some(task.id.clone()), format!("nothing to bring in: {why}"), None, false))
    };
    if writes.is_empty() {
        return left_out("the line has its change already, or it changes nothing the line carries");
    }
    let left_open = |why: String| -> Result<Followed> {
        let laid = views.map(|v| v.join(format!("git-{}", &sha[..12])));
        if let Some(d) = &laid {
            lay_out(client, &r.id, &task.id, d)?;
        }
        let at = laid.map_or_else(|| format!(" (task {})", task.id), |d| format!(" (task {}, laid out in {})", task.id, d.display()));
        Ok(said(Some(task.id.clone()), format!("open, {why}{at}: fix it and land it, or abandon it to leave the commit out"), None, true))
    };
    for w in &writes {
        let o: WriteOutcome = client.post(&format!("/repos/{}/tasks/{}/writes", r.id, task.id), w)?;
        if o.lease == "refused" {
            return left_open(format!("{} is leased by {} and the repo's policy is strict", w.path, o.previous_holder.unwrap_or_default()));
        }
    }
    if !conflicts.is_empty() {
        return left_open(format!("conflicts with the line in {}", conflicts.join(", ")));
    }
    match client.land(&r.id, &task.id)? {
        Ok(l) => {
            // Exactly the commit, on top of its parent's position: it is the position's commit, so
            // the mirror goes on from it, and a branch it writes stays a fast-forward of this one.
            if !l.replayed && parent.is_some() && head.git.as_deref() == parent {
                let _ = client.post::<LineEntry>(&format!("/repos/{}/line/{}/git", r.id, l.position), &serde_json::json!({ "sha": sha }));
            }
            Ok(said(Some(task.id), format!("landed at {}", l.position), Some(l.position), false))
        }
        Err(nl) if nl.reason == "empty" || nl.reason == "already" => left_out("the line has its change already"),
        Err(nl) => left_open(match nl.reason.as_str() {
            "red" => format!("its proof is red{}", nl.proof.as_ref().and_then(|p| p.steps.iter().find(|s| s.exit != 0)).map(|s| format!(" ({})", s.name)).unwrap_or_default()),
            "conflict" => format!("conflicts with what landed meanwhile in {}", nl.conflicts.join(", ")),
            "approval" => format!("green, it waits for a person's approval (guarded: {})", nl.guarded.join(", ")),
            other => format!("not landed: {other}"),
        }),
    }
}

/// The writes that bring commit `sha`'s change (against its first parent, none for a root commit)
/// onto the tree `base`: a file the line has as the parent had it takes the commit's version; one
/// the line changed too gets both changes merged, as a landing merges them; where both touch the
/// same lines, or one deleted what the other changed, it is written in conflict (the commit's
/// version, or the line's when the commit deleted it). Regular files only, as the line carries.
/// Returns the writes and the paths in conflict.
fn commit_writes(client: &Client, repo: &str, dir: &Path, parent: Option<&str>, sha: &str, base: &Tree) -> Result<(Vec<PutWrite>, Vec<String>)> {
    let mut args = vec!["diff-tree", "-r", "-z", "--no-renames", "--no-commit-id"];
    match parent {
        Some(p) => args.push(p),
        None => args.push("--root"),
    }
    args.push(sha);
    let out = git(dir, &args)?;
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    // path -> (git blob of the parent, of the commit, and the commit's mode), regular files only.
    let regular = |mode: &str, blob: &str| (mode == "100644" || mode == "100755").then(|| blob.to_string());
    let mut changes = Vec::new();
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let m: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if m.len() < 4 {
            continue;
        }
        let (old, new) = (regular(m[0], m[2]), regular(m[1], m[3]));
        if old.is_some() || new.is_some() {
            changes.push((path.to_string(), old, new, u32::from_str_radix(m[1], 8).unwrap_or(0o100644)));
        }
    }
    let shas: Vec<String> = changes.iter().flat_map(|(_, o, n, _)| o.iter().chain(n.iter()).cloned()).collect();
    let bytes = cat_blobs(dir, &shas)?;
    let (mut writes, mut conflicts) = (Vec::new(), Vec::new());
    let put = |path: &str, kind: WriteKind, content: &[u8], mode: u32| -> Result<PutWrite> { Ok(PutWrite { path: path.to_string(), kind, blob: Some(client.put_object(repo, content.to_vec())?), mode }) };
    for (path, old, new, mode) in changes {
        let (was, now) = (old.as_ref().map(|s| &bytes[s]), new.as_ref().map(|s| &bytes[s]));
        let line = base.get(&path);
        let line_is = |b: Option<&Vec<u8>>| match (line, b) {
            (None, None) => true,
            (Some(e), Some(b)) => e.blob == hash_bytes(b),
            _ => false,
        };
        if line_is(now) && line.is_none_or(|e| e.mode == mode) {
            continue; // the line has it already
        }
        if line_is(was) {
            match now {
                Some(b) => writes.push(put(&path, WriteKind::Put, b, mode)?),
                None => writes.push(PutWrite { path: path.clone(), kind: WriteKind::Delete, blob: None, mode: default_mode() }),
            }
            continue;
        }
        let ours = match line {
            Some(e) => Some(client.get_object(repo, &e.blob)?),
            None => None,
        };
        // None as text is an empty file: two adds of the same path merge as two additions.
        fn text(b: Option<&Vec<u8>>) -> Option<&str> {
            b.map_or(Some(""), |b| std::str::from_utf8(b).ok())
        }
        match (now, &ours) {
            (Some(theirs), Some(o)) => match (text(was), text(Some(theirs)), text(Some(o))) {
                (Some(a), Some(t), Some(o)) => match diffy::merge(a, t, o) {
                    Ok(m) => writes.push(put(&path, WriteKind::Put, m.as_bytes(), mode)?),
                    Err(markers) => match crate::merge::settle_additions(&markers) {
                        Some(m) => writes.push(put(&path, WriteKind::Put, m.as_bytes(), mode)?),
                        None => {
                            writes.push(put(&path, WriteKind::Conflict, markers.as_bytes(), mode)?);
                            conflicts.push(path);
                        }
                    },
                },
                _ => {
                    writes.push(put(&path, WriteKind::Conflict, theirs, mode)?);
                    conflicts.push(path);
                }
            },
            (Some(theirs), None) => {
                writes.push(put(&path, WriteKind::Conflict, theirs, mode)?);
                conflicts.push(path);
            }
            (None, Some(o)) => {
                writes.push(put(&path, WriteKind::Conflict, o, line.map_or(mode, |e| e.mode))?);
                conflicts.push(path);
            }
            (None, None) => {}
        }
    }
    Ok((writes, conflicts))
}

/// The bytes of git blobs, by sha, through one `git cat-file --batch`.
fn cat_blobs(dir: &Path, shas: &[String]) -> Result<HashMap<String, Vec<u8>>> {
    let mut out = HashMap::new();
    if shas.is_empty() {
        return Ok(out);
    }
    let mut child = Command::new("git").args(["cat-file", "--batch"]).current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let input: String = shas.iter().map(|s| format!("{s}\n")).collect();
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    for sha in shas {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let parts: Vec<&str> = header.trim_end().split(' ').collect();
        anyhow::ensure!(parts.len() == 3 && parts[0] == sha, "cat-file header for {sha}: {header}");
        let mut bytes = vec![0u8; parts[2].parse()?];
        reader.read_exact(&mut bytes)?;
        let mut nl = [0u8; 1];
        reader.read_exact(&mut nl)?;
        out.insert(sha.clone(), bytes);
    }
    let _ = child.wait();
    Ok(out)
}

/// `[git]` of each repo's avp.toml, carried out by `avp serve`: every `every` seconds (60 by
/// default), the branch `follow` names is fetched from `remote` into the server's own clone
/// (`<data>/git/<repo>`) and its new commits brought in; with `pull_requests`, the open pull
/// requests of `remote` on GitHub (GH_TOKEN) too. The server talks to itself at `local` as any
/// client does, and opens the commits' tasks in its engine, as their authors. Its log gets one line
/// per change of what a look says.
pub fn serve_follows(engine: crate::engine::Engine, local: String) {
    if let Ok(token) = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")) {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{}", token.trim()));
        let _ = GIT_ENV.set(vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "http.https://github.com/.extraheader".into()),
            ("GIT_CONFIG_VALUE_0".into(), format!("AUTHORIZATION: basic {basic}")),
        ]);
    }
    std::thread::spawn(move || {
        let client = Client::new(&local);
        // By repo: what its follow knows, when it looks next, what it said last.
        let mut state: HashMap<String, (Known, i64, String)> = HashMap::new();
        loop {
            let repos = engine.db.lock().unwrap_or_else(|e| e.into_inner()).repos().unwrap_or_default();
            for repo in repos {
                let Ok(head) = engine.head(&repo.id) else { continue };
                let Ok(cfg) = engine.objects.get_tree(&head.tree).and_then(|t| crate::check::read_config(&t, &engine.objects)) else { continue };
                let g = cfg.git;
                if g.follow.is_none() && g.pull_requests == PullRequests::Off {
                    continue;
                }
                let (known, next, said) = state.entry(repo.id.clone()).or_default();
                if now_ms() < *next {
                    continue;
                }
                let open = |repo: &str, req: &OpenTask, base: i64| engine.open_task_at(repo, req.clone(), Some(base));
                let now_said = follow_once(&engine.data.join("git").join(&repo.id), &client, &repo, &g, known, &open);
                *next = now_ms() + g.every.unwrap_or(60).max(5) as i64 * 1000;
                if !now_said.is_empty() && now_said != *said {
                    for l in now_said.lines() {
                        eprintln!("git {}: {l}", repo.name);
                    }
                }
                *said = now_said;
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

/// One look of `[git]` for one repo, in the server's clone `dir`: what it says, a line per commit
/// or pull request it moved, empty when nothing came.
fn follow_once(dir: &Path, client: &Client, repo: &Repo, g: &GitConfig, known: &mut Known, open: Opener) -> String {
    let Some(remote) = g.remote.as_deref() else { return "[git] needs `remote`: the URL of the Git repo the server fetches from".into() };
    if let Err(e) = server_clone(dir, remote) {
        return format!("{remote}: {e:#}");
    }
    let mut said = Vec::new();
    if let Some(branch) = &g.follow {
        let got = fetch_branch(dir, "origin", branch).and_then(|rev| follow_git(client, dir, &repo.id, &rev, None, known, open));
        match got {
            Ok(fs) => said.extend(fs.iter().map(|f| format!("{} {}: {}", &f.sha[..12], f.subject, f.outcome))),
            Err(e) => said.push(format!("{branch}: {e:#}")),
        }
    }
    if g.pull_requests != PullRequests::Off {
        match follow_prs(client, dir, &repo.id, "origin", None, g.pull_requests == PullRequests::Land) {
            Ok(prs) => said.extend(prs.iter().map(|p| format!("#{} {}: {}", p.number, p.title, p.outcome))),
            Err(e) => said.push(format!("pull requests: {e:#}")),
        }
    }
    said.join("\n")
}

/// The server's own clone of `remote`: its objects only, nothing checked out, made once.
fn server_clone(dir: &Path, remote: &str) -> Result<()> {
    if !dir.join(".git").exists() {
        std::fs::create_dir_all(dir)?;
        git(dir, &["init", "-q"])?;
        git(dir, &["remote", "add", "origin", remote])?;
    } else if git(dir, &["remote", "get-url", "origin"])?.trim() != remote {
        git(dir, &["remote", "set-url", "origin", remote])?;
    }
    Ok(())
}
