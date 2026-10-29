//! A task opened in the git checkout its agent stands in, instead of a folder of its own.
//!
//! On Murmell, 25/10/2026, every agent had two folders: the copy its window was given, with the
//! project's dependencies, and an AVP view where it worked and installed them again (677 MB). With
//! AVP_VIEW_IN_CHECKOUT=1 (a host that gives each agent a checkout of its own sets it), `open` lays
//! the task out in that checkout when git says it holds nothing of its own: what `.gitignore` hides
//! (the install, the builds) stays, git keeps working on top. The checkout is then moved to the git
//! commit of the position it was laid on, so `git status` and `git diff` show the agent its own work
//! and nothing else. Off by default: `avp up` promises a person's checkout is never touched.

use crate::client::Client;
use crate::model::LineEntry;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// The top of the clean git checkout `dir` is in, when the host asked for tasks in place and that
/// checkout is not an AVP view yet. A checkout with anything git would call a change, tracked or
/// not, keeps the separate folder: nothing of the agent's is ever laid over.
pub fn in_place_target(dir: &Path) -> Option<PathBuf> {
    if std::env::var("AVP_VIEW_IN_CHECKOUT").map_or(true, |v| v != "1") {
        return None;
    }
    let top = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?);
    if crate::view::load(&top).is_ok() {
        return None;
    }
    if !clean(&top)? {
        return None;
    }
    exclude_state(&top)?;
    Some(top)
}

/// Whether git's changes in the checkout are all the host's: its paths (AVP_HOST_PATHS, the
/// runtime's own files) and its blocks in tracked files (AVP_HOST_BLOCKS). Murmell writes its
/// instructions into AGENTS.md at every window it opens: a checkout whose only change is that
/// block holds nothing of the agent's.
fn clean(top: &Path) -> Option<bool> {
    let out = git(top, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    for entry in out.split('\0').filter(|e| e.len() > 3) {
        let (code, path) = entry.split_at(3);
        if crate::view::is_host_path(path) {
            continue;
        }
        if code == " M " {
            let head = Command::new("git").arg("-C").arg(top).args(["show", &format!("HEAD:{path}")]).stderr(Stdio::null()).output().ok()?;
            if head.status.success() && crate::view::read_project(&top.join(path)).ok()? == head.stdout {
                continue;
            }
        }
        return Some(false);
    }
    Some(true)
}

/// A git checkout that is not a view and holds changes of its own, where the host asked for tasks
/// in place: the CLI refuses to lay a task over it (the MCP server takes a separate folder).
pub fn refused_checkout(dir: &Path) -> Option<PathBuf> {
    if std::env::var("AVP_VIEW_IN_CHECKOUT").map_or(true, |v| v != "1") {
        return None;
    }
    let top = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?);
    (crate::view::load(&top).is_err() && !clean(&top)?).then_some(top)
}

/// The view's own files stay out of git, in the repository's local excludes (never its .gitignore).
fn exclude_state(top: &Path) -> Option<()> {
    let common = PathBuf::from(git(top, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    let file = common.join("info").join("exclude");
    let now = std::fs::read_to_string(&file).unwrap_or_default();
    let missing: Vec<&str> = ["/.avp/", "/.avp-laid"].into_iter().filter(|l| !now.lines().any(|x| x.trim() == *l)).collect();
    if !missing.is_empty() {
        std::fs::create_dir_all(file.parent()?).ok()?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file).ok()?;
        let sep = if now.is_empty() || now.ends_with('\n') { "" } else { "\n" };
        write!(f, "{sep}# AVP's view state (a task opened in this checkout)\n{}\n", missing.join("\n")).ok()?;
    }
    Some(())
}

/// Puts git on the commit of `position`, whose files the checkout now holds: the branch and the
/// index move, the files do not. Silent when the position has no commit yet (the mirror writes it
/// a few seconds after the landing) or the commit cannot be fetched: git then shows the line's
/// newer files as changes, and the next task in this checkout tries again.
pub fn align(client: &Client, top: &Path, repo: &str, position: i64) -> bool {
    let Ok(Some(entry)) = client.get::<Option<LineEntry>>(&format!("/repos/{repo}/line/{position}/last-git")) else { return false };
    let Some(sha) = entry.git.filter(|_| entry.position == position) else { return false };
    let commit = format!("{sha}^{{commit}}");
    if git(top, &["cat-file", "-e", &commit]).is_none() && !fetch(top, &sha) {
        return false;
    }
    git(top, &["reset", "-q", "--mixed", &sha]).is_some()
}

/// `git fetch origin <sha>`, given at most 30 seconds: an open must not hang on a slow remote.
fn fetch(top: &Path, sha: &str) -> bool {
    let Ok(mut child) = Command::new("git").arg("-C").arg(top).args(["fetch", "-q", "origin", sha]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return false };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(30) => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The other worktrees of the repository `top` is a checkout of: on a host that gives each agent a
/// worktree, the other agents' checkouts, whose files and installs a task laid out here can take
/// (a file when its content is the same, an install when its lockfile is).
pub fn worktrees(top: &Path) -> Vec<PathBuf> {
    let Some(out) = git(top, &["worktree", "list", "--porcelain"]) else { return vec![] };
    let me = top.canonicalize().ok();
    out.lines().filter_map(|l| l.strip_prefix("worktree ")).map(PathBuf::from).filter(|p| p.canonicalize().ok() != me).collect()
}

/// Whether `dir` is a view laid out in a git checkout by a host that asked for tasks in place.
pub fn is_checkout_view(dir: &Path) -> bool {
    std::env::var("AVP_VIEW_IN_CHECKOUT").is_ok_and(|v| v == "1") && dir.join(".git").exists() && crate::view::load(dir).is_ok()
}

/// After a landing: the mirror writes the position's commit a few seconds later, git follows it
/// then. Waits up to `wait` for the commit, so the checkout ends with nothing git calls a change and
/// its host can recycle it once its window closes.
pub fn align_when_written(client: &Client, top: &Path, repo: &str, position: i64, wait: Duration) -> bool {
    let start = Instant::now();
    loop {
        let written = client.get::<Option<LineEntry>>(&format!("/repos/{repo}/line/{position}/last-git")).ok().flatten().is_some_and(|e| e.position == position && e.git.is_some());
        if written {
            return align(client, top, repo, position);
        }
        if start.elapsed() >= wait {
            return false;
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}
