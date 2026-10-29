//! The local view: a directory that mirrors a task's candidate tree, and the sync that turns
//! saves into writes. State in `.avp/view.json`. Honors `.gitignore` (the `ignore` crate).

use crate::client::Client;
use crate::model::*;
use crate::objects::hash_bytes;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ViewState {
    pub server: String,
    pub repo: String,
    pub task: Option<String>,
    /// What the directory held after the last sync: path -> (blob, mode).
    pub known: BTreeMap<String, (String, u32)>,
    /// What each file looked like when it was last hashed, with that hash: a file whose stamp has
    /// not moved is not read again. On a large repository, a sync reads what changed, not all.
    #[serde(default)]
    pub stamps: BTreeMap<String, (Stamp, String)>,
    /// What the open task builds on (with none, what the folder was laid out from): its position
    /// and its tree. Offline, a task opens there and `undo` goes back to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<(i64, String)>,
}

/// Size, modification and change times, inode: what tells that a file changed since it was read.
pub type Stamp = (u64, i64, i64, u64);

pub fn stamp_of(m: &fs::Metadata) -> Stamp {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (m.len(), m.mtime() * 1_000_000_000 + m.mtime_nsec(), m.ctime() * 1_000_000_000 + m.ctime_nsec(), m.ino())
    }
    #[cfg(not(unix))]
    (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64), 0, 0)
}

/// A file's hash, from `stamps` when its stamp has not moved since. A file changed in the last two
/// seconds is read and not remembered: a second change within the same tick of the clock would
/// leave its times as they were (Git calls such an entry racy).
fn hash_of(full: &Path, rel: &str, meta: &fs::Metadata, stamps: &mut BTreeMap<String, (Stamp, String)>) -> Result<String> {
    let st = stamp_of(meta);
    if let Some((s, h)) = stamps.get(rel) {
        if *s == st {
            return Ok(h.clone());
        }
    }
    let hash = hash_bytes(&read_project(full)?);
    remember(stamps, rel, st, &hash);
    Ok(hash)
}

fn remember(stamps: &mut BTreeMap<String, (Stamp, String)>, rel: &str, st: Stamp, hash: &str) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64);
    if st.1 < now - 2_000_000_000 && st.2 < now - 2_000_000_000 {
        stamps.insert(rel.to_string(), (st, hash.to_string()));
    } else {
        stamps.remove(rel);
    }
}

/// The hashes of `files`, read side by side: the first scan of a folder laid out by a clone reads
/// every file from the disk (none is in the page cache yet), eight at a time four times sooner.
fn hash_files(files: &[PathBuf]) -> Result<Vec<String>> {
    let each = |part: &[PathBuf]| part.iter().map(|f| Ok(hash_bytes(&read_project(f)?))).collect::<Result<Vec<String>>>();
    if files.len() < 64 {
        return each(files);
    }
    std::thread::scope(|scope| {
        let parts: Vec<_> = files.chunks(files.len().div_ceil(8)).map(|part| scope.spawn(move || each(part))).collect();
        let mut out = Vec::with_capacity(files.len());
        for p in parts {
            out.extend(p.join().map_err(|_| anyhow::anyhow!("a read stopped"))??);
        }
        Ok(out)
    })
}

/// Paths that belong to the view's host (the agent runtime, AVP itself), never to the project:
/// never sent as writes, never deleted, never removed by a materialize. Narrow on purpose: a
/// project may track `.claude/skills`, so only the local, per-machine files are the host's.
pub fn is_host_path(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    matches!(first, ".avp" | ".git" | ".avp-laid" | ".trame" | ".trame-laid")
        || matches!(rel, ".mcp.json" | "CLAUDE.local.md" | ".claude/settings.local.json")
        || host_paths().iter().any(|h| if h.ends_with('/') { rel.starts_with(h.as_str()) || format!("{rel}/") == *h } else { rel == h })
}

/// Never a project's source, ignored even where no `.gitignore` says so: what running tests and
/// tools leaves in a folder. 28/10, a lead's three subagents ran their Python tests in a project with
/// no `.gitignore`: AVP sent `__pycache__/*.pyc` as writes, two of them conflicted on one, and the
/// lead spent two more tasks taking them out. A path the line already holds is still read (`scan`).
pub fn is_junk(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rel.split('/').any(|c| matches!(c, "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | "node_modules")) || name == ".DS_Store" || name.ends_with(".pyc") || name.ends_with(".pyo")
}

/// More of the host's own paths, from AVP_HOST_PATHS (comma separated; `dir/` covers what is
/// under it): a host that writes its state into the agent's checkout (Murmell's `.murmell/`).
fn host_paths() -> &'static Vec<String> {
    static P: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    P.get_or_init(|| std::env::var("AVP_HOST_PATHS").unwrap_or_default().split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
}

/// The host's blocks in project files, from AVP_HOST_BLOCKS (comma separated names): a block runs
/// from a line `<!-- NAME:start -->` to a line `<!-- NAME:end -->`. Murmell writes its instructions
/// so into AGENTS.md, a file the project tracks and its agents may edit: what AVP compares, sends
/// and lands is the file without the block, and a file AVP lays out keeps the block it had.
fn host_blocks() -> &'static Vec<(Vec<u8>, Vec<u8>)> {
    static B: std::sync::OnceLock<Vec<(Vec<u8>, Vec<u8>)>> = std::sync::OnceLock::new();
    B.get_or_init(|| {
        std::env::var("AVP_HOST_BLOCKS").unwrap_or_default().split(',').map(str::trim).filter(|x| !x.is_empty())
            .map(|n| (format!("<!-- {n}:start -->").into_bytes(), format!("<!-- {n}:end -->").into_bytes())).collect()
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Splits `bytes` into the project's content and the host's blocks (with the blank line written
/// before each). Without a block, the content is `bytes` and the blocks are empty.
pub fn split_host(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut content = bytes.to_vec();
    let mut blocks = Vec::new();
    for (start, end) in host_blocks() {
        while let Some(s) = find(&content, start, 0) {
            let line_start = s == 0 || content[s - 1] == b'\n';
            let Some(e) = find(&content, end, s) else { break };
            if !line_start {
                break;
            }
            let mut stop = e + end.len();
            if content.get(stop) == Some(&b'\n') {
                stop += 1;
            }
            // The blank line before the block is the host's too.
            let from = if s >= 2 && content[s - 1] == b'\n' && content[s - 2] == b'\n' { s - 1 } else { s };
            blocks.extend_from_slice(&content[from..stop]);
            content.drain(from..stop);
        }
    }
    (content, blocks)
}

/// A project file as AVP sees it: without the host's blocks.
pub fn read_project(full: &Path) -> std::io::Result<Vec<u8>> {
    let bytes = fs::read(full)?;
    Ok(if host_blocks().is_empty() { bytes } else { split_host(&bytes).0 })
}

pub fn state_path(dir: &Path) -> PathBuf {
    dir.join(".avp").join("view.json")
}

pub fn load(dir: &Path) -> Result<ViewState> {
    let mut p = state_path(dir);
    // A view laid out before AVP was named AVP keeps its state in .trame/ until its next save.
    if !p.exists() && dir.join(".trame/view.json").exists() {
        p = dir.join(".trame/view.json");
    }
    let bytes = fs::read(&p).with_context(|| format!("{} is not an AVP view (no .avp/view.json)", dir.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn save(dir: &Path, state: &ViewState) -> Result<()> {
    fs::create_dir_all(dir.join(".avp"))?;
    fs::write(state_path(dir), serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

pub fn mode_of(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 { 0o100755 } else { 0o100644 }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o100644
    }
}

/// Every file under `dir` the project would version: respects `.gitignore`, skips the host's
/// files. A path the view already knows is read even if an ignore rule matches it: a project can
/// track a file its `.gitignore` would hide (Murmell's `vite-env.d.ts` under `src/**/*.d.ts`).
pub fn scan(dir: &Path, known: &BTreeMap<String, (String, u32)>, stamps: &mut BTreeMap<String, (Stamp, String)>) -> Result<BTreeMap<String, (String, u32)>> {
    let mut out = scan_unignored(dir, stamps)?;
    for path in known.keys() {
        if out.contains_key(path) || is_host_path(path) {
            continue;
        }
        let full = dir.join(path);
        if let Ok(meta) = fs::metadata(&full) {
            if meta.is_file() {
                out.insert(path.clone(), (hash_of(&full, path, &meta, stamps)?, mode_of(&meta)));
            }
        }
    }
    stamps.retain(|p, _| out.contains_key(p));
    Ok(out)
}

/// The `info/exclude` of the repository `dir` is a worktree of. The walker reads that file only where
/// `.git` is a folder; in a worktree `.git` is a file pointing at the shared one, and its excludes
/// went unread: Murmell's windows are worktrees whose exclude lists the files the host writes there
/// (`.claude/settings.json`, `.cursor/hooks.json`), so `open` deleted them and `sync` sent them.
fn worktree_exclude(dir: &Path) -> Option<ignore::gitignore::Gitignore> {
    let dot = fs::read_to_string(dir.join(".git")).ok()?;
    let git_dir = PathBuf::from(dot.lines().next()?.strip_prefix("gitdir: ")?.trim());
    let git_dir = if git_dir.is_absolute() { git_dir } else { dir.join(git_dir) };
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) if PathBuf::from(c.trim()).is_absolute() => PathBuf::from(c.trim()),
        Ok(c) => git_dir.join(c.trim()),
        Err(_) => git_dir,
    };
    let mut b = ignore::gitignore::GitignoreBuilder::new(dir);
    b.add(common.join("info").join("exclude"));
    b.build().ok().filter(|g| !g.is_empty())
}

/// What the view itself keeps out of the project, as `.git/info/exclude` does for a checkout:
/// `.avp/exclude`, where `avp clone` names the agents' configs it wrote in a folder git does not hold.
fn view_exclude(dir: &Path) -> Option<ignore::gitignore::Gitignore> {
    let p = dir.join(".avp").join("exclude");
    if !p.exists() {
        return None;
    }
    let mut b = ignore::gitignore::GitignoreBuilder::new(dir);
    b.add(p);
    b.build().ok().filter(|g| !g.is_empty())
}

fn scan_unignored(dir: &Path, stamps: &mut BTreeMap<String, (Stamp, String)>) -> Result<BTreeMap<String, (String, u32)>> {
    let mut out = BTreeMap::new();
    let root = dir.to_path_buf();
    let excludes: Vec<_> = [worktree_exclude(dir), view_exclude(dir)].into_iter().flatten().collect();
    let walker = ignore::WalkBuilder::new(dir).hidden(false).git_ignore(true).git_exclude(true).require_git(false).filter_entry(move |e| {
        let rel = e.path().strip_prefix(&root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        rel.is_empty() || (!is_host_path(&rel) && !is_junk(&rel) && !excludes.iter().any(|g| g.matched(e.path(), e.file_type().is_some_and(|t| t.is_dir())).is_ignore()))
    }).build();
    let mut unread = Vec::new();
    for entry in walker {
        let entry = entry?;
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
        let st = stamp_of(&meta);
        match stamps.get(&rel).filter(|(s, _)| *s == st) {
            Some((_, hash)) => {
                out.insert(rel, (hash.clone(), mode_of(&meta)));
            }
            None => unread.push((rel, entry.into_path(), st, mode_of(&meta))),
        }
    }
    let files: Vec<PathBuf> = unread.iter().map(|(_, full, _, _)| full.clone()).collect();
    for ((rel, _, st, mode), hash) in unread.into_iter().zip(hash_files(&files)?) {
        remember(stamps, &rel, st, &hash);
        out.insert(rel, (hash, mode));
    }
    Ok(out)
}

/// Writes `tree` into `dir`: files that differ from what is known are rewritten, known files
/// absent from the tree are removed. Local files the view never knew are left alone.
/// Sends what the folder changed, moves its task onto the head of the line (replayed, not landed)
/// and lays the result out: the work teammates landed since arrives in the folder, the task's own
/// changes on top of it.
pub fn catch_up(client: &Client, dir: &Path, state: &mut ViewState) -> Result<String> {
    sync(client, dir, state)?;
    let task = state.task.clone().context("no task open here")?;
    let c: CaughtUp = client.post(&format!("/repos/{}/tasks/{task}/catch-up", state.repo), &serde_json::json!({}))?;
    let tree: Tree = client.get(&format!("/repos/{}/tasks/{task}/tree", state.repo))?;
    if c.moved {
        let st: TaskStatus = client.get(&format!("/repos/{}/tasks/{task}", state.repo))?;
        state.base = Some((st.task.base_position, st.task.base_tree));
    }
    let n = materialize(client, &tree, dir, state)?;
    let mut s = match (&c.on, c.moved) {
        (Some(on), true) => format!("the task now stands on task {on} as it is now, on position {} (its changes replayed on that task's latest); {n} files laid out", c.position),
        (Some(on), false) => format!("the task already stands on task {on} as it is now; {n} files laid out"),
        (None, true) => format!("the task now sits on position {} (its changes replayed on what landed since); {n} files laid out", c.position),
        (None, false) => format!("the task already sits on the head, position {}; {n} files laid out", c.position),
    };
    // Stacked: where the task under it stands (not landed yet, or gone), and what to do.
    if let Some(w) = &c.stack {
        s.push_str(&format!("\n{w}"));
    }
    if !c.conflicts.is_empty() {
        s.push_str("\nconflict markers (<<<<<<< ours / ||||||| original / ======= / >>>>>>> theirs) are now in:");
        for p in &c.conflicts {
            s.push_str(&format!("\n  {p}"));
        }
        s.push_str("\nsettle them, then land");
    }
    Ok(s)
}

pub fn materialize(client: &Client, tree: &Tree, dir: &Path, state: &mut ViewState) -> Result<usize> {
    materialize_from(client, tree, dir, state, &[])
}

/// `materialize`, taking a file from one of `seeds` (the agent's checkout, another view of the
/// repo) when it holds the same content, and the rest from the server, a batch at a time.
pub fn materialize_from(client: &Client, tree: &Tree, dir: &Path, state: &mut ViewState, seeds: &[PathBuf]) -> Result<usize> {
    let repo = state.repo.clone();
    let n = lay(tree, dir, state, &|need, write| fetch(client, &repo, need, seeds, write))?;
    crate::offline::keep_laid(&client.base, &repo, tree, dir);
    Ok(n)
}

/// `materialize` from this machine's replica of the line (`offline`), when the server does not answer.
pub fn materialize_offline(objects: &crate::objects::Objects, tree: &Tree, dir: &Path, state: &mut ViewState) -> Result<usize> {
    lay(tree, dir, state, &|need, write| {
        for &(path, entry) in need {
            write(path, entry, &objects.get(&entry.blob).with_context(|| format!("{path} is not on this machine"))?)?;
        }
        Ok(())
    })
}

/// How a layout gets the files it writes: given those it needs, it writes each with `write`.
type Fill<'a> = dyn Fn(&[(&String, &Entry)], &(dyn Fn(&str, &Entry, &[u8]) -> Result<()> + Sync)) -> Result<()> + 'a;

fn lay(tree: &Tree, dir: &Path, state: &mut ViewState, fill: &Fill) -> Result<usize> {
    // The tree comes from a server: no path of it may reach outside this folder.
    if let Some(bad) = tree.keys().find(|p| !safe_path(p)) {
        anyhow::bail!("the server sent a path outside the view: {bad:?}");
    }
    let need: Vec<(&String, &Entry)> = tree.iter().filter(|(path, entry)| !(state.known.get(*path).is_some_and(|(b, m)| b == &entry.blob && *m == entry.mode) && dir.join(path).exists())).collect();
    let write = |path: &str, entry: &Entry, bytes: &[u8]| -> Result<()> {
        let dest = dir.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let kept = if host_blocks().is_empty() { Vec::new() } else { fs::read(&dest).map(|b| split_host(&b).1).unwrap_or_default() };
        if kept.is_empty() {
            fs::write(&dest, bytes)?;
        } else {
            let mut all = bytes.to_vec();
            all.extend_from_slice(&kept);
            fs::write(&dest, all)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(if entry.mode & 0o111 != 0 { 0o755 } else { 0o644 }))?;
        }
        Ok(())
    };
    fill(&need, &write)?;
    let mut changed = need.len();
    let gone: Vec<String> = state.known.keys().filter(|p| !tree.contains_key(*p) && !is_host_path(p)).cloned().collect();
    for p in gone {
        let _ = fs::remove_file(dir.join(&p));
        changed += 1;
    }
    state.known = tree.iter().map(|(p, e)| (p.clone(), (e.blob.clone(), e.mode))).collect();
    save(dir, state)?;
    Ok(changed)
}

/// Writes each of `need` with `write`: from one of `seeds` when it holds the same content, the rest
/// from the server, a batch at a time.
fn fetch(client: &Client, repo: &str, need: &[(&String, &Entry)], seeds: &[PathBuf], write: &(dyn Fn(&str, &Entry, &[u8]) -> Result<()> + Sync)) -> Result<()> {
    let mut missing = Vec::new();
    for &(path, entry) in need {
        match seeds.iter().find_map(|s| read_project(&s.join(path)).ok().filter(|b| hash_bytes(b) == entry.blob)) {
            Some(bytes) => write(path, entry, &bytes)?,
            None => missing.push((path, entry)),
        }
    }
    // Four batches in flight: a first layout of thousands of files is a few round trips.
    let chunks: Vec<&[(&String, &Entry)]> = missing.chunks(200).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    while let Some(chunk) = chunks.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                        let hashes: Vec<String> = chunk.iter().map(|(_, e)| e.blob.clone()).collect();
                        for ((path, entry), bytes) in chunk.iter().zip(client.get_objects(repo, &hashes)?) {
                            write(path, entry, &bytes)?;
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        for w in workers {
            w.join().map_err(|_| anyhow::anyhow!("a download stopped"))??;
        }
        Ok(())
    })
}

/// Lays `tree` out in `dir`. A folder that holds nothing of the project yet is a clone of the
/// ready copy of the tree (`from_ready`), every file at once; any other is brought to the tree by
/// writing only the files that differ. Returns how many files were laid out.
pub fn lay_out(client: &Client, tree: &Tree, dir: &Path, state: &mut ViewState, seeds: &[PathBuf]) -> Result<usize> {
    // A clone that fails leaves the view knowing nothing: every file is then written as before.
    if state.known.is_empty() && holds_nothing(dir) && from_ready(client, tree, dir, state, seeds).is_ok() {
        return Ok(tree.len());
    }
    materialize_from(client, tree, dir, state, seeds)
}

/// Whether `dir` holds nothing but the host's files: a folder made for a view, not laid out yet.
fn holds_nothing(dir: &Path) -> bool {
    fs::read_dir(dir).map(|d| d.flatten().all(|e| is_host_path(&e.file_name().to_string_lossy()))).unwrap_or(true)
}

/// What the ready copy holds at a path: the blob, its mode, and the file's stamp once written. A
/// file whose stamp moved since was touched by someone: it is written again before any clone.
#[derive(Serialize, Deserialize)]
struct Laid {
    blob: String,
    mode: u32,
    stamp: Stamp,
}

/// Lays `tree` out in `dir`, which holds nothing yet, as a clone of this machine's ready copy of
/// the repo: a folder of AVP's own (`$AVP_HOME/ready/<repo>`), never a view, brought to the tree
/// first by writing what differs. On APFS the whole tree is one clonefile, on Linux each file is
/// a reflink (FICLONE) where the filesystem has them, a copy elsewhere. The views share no folder:
/// a clone's files part from the ready copy's at the first write.
fn from_ready(client: &Client, tree: &Tree, dir: &Path, state: &mut ViewState, seeds: &[PathBuf]) -> Result<()> {
    if let Some(bad) = tree.keys().find(|p| !safe_path(p)) {
        anyhow::bail!("the server sent a path outside the view: {bad:?}");
    }
    let all = crate::up::home().join("ready");
    let root = all.join(&state.repo);
    fs::create_dir_all(&root)?;
    fs::create_dir_all(dir.join(".avp"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // A clone stays on its volume: across two, the ready copy would cost one copy more.
        anyhow::ensure!(fs::metadata(&root)?.dev() == fs::metadata(dir)?.dev(), "the ready copy is on another volume");
    }
    forget_stale_ready(&all, &root);
    let tmp = dir.join(".avp").join(format!("laying-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    // One open at a time brings the copy to its tree and clones it: two would write it at once.
    let lock = fs::File::create(root.join("lock"))?;
    lock.lock()?;
    let cloned = ready(client, &state.repo, tree, &root, seeds).and_then(|_| clone_files(&root.join("files"), &tmp, tree));
    drop(lock);
    let laid = cloned.and_then(|_| dated_now(&tmp, tree)).and_then(|_| Ok(move_into(&tmp, dir)?));
    let _ = fs::remove_dir_all(&tmp);
    laid?;
    state.known = tree.iter().map(|(p, e)| (p.clone(), (e.blob.clone(), e.mode))).collect();
    crate::offline::keep_laid(&client.base, &state.repo, tree, dir);
    save(dir, state)
}

/// Ready copies no open used for a week go: their repo was dropped, or lives on another server.
fn forget_stale_ready(all: &Path, mine: &Path) {
    let week = std::time::Duration::from_secs(7 * 24 * 3600);
    for e in fs::read_dir(all).into_iter().flatten().flatten().filter(|e| e.path() != mine) {
        let lock = e.path().join("lock");
        let stale = fs::metadata(&lock).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > week);
        if stale && fs::File::open(&lock).is_ok_and(|f| f.try_lock().is_ok()) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

/// Brings the ready copy under `root` to `tree`: what is not in the tree goes, a file is written
/// again when its blob differs or its stamp moved, from the seeds or the server.
fn ready(client: &Client, repo: &str, tree: &Tree, root: &Path, seeds: &[PathBuf]) -> Result<()> {
    let files = root.join("files");
    let record = root.join("laid.json");
    let mut laid: BTreeMap<String, Laid> = fs::read(&record).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    fs::create_dir_all(&files)?;
    let mut dirs = HashSet::new();
    for p in tree.keys() {
        let mut s = p.as_str();
        while let Some((parent, _)) = s.rsplit_once('/') {
            if !dirs.insert(parent) {
                break;
            }
            s = parent;
        }
    }
    prune(&files, "", tree, &dirs)?;
    let stamp = |p: &str| fs::symlink_metadata(files.join(p)).ok().filter(|m| m.is_file()).map(|m| stamp_of(&m));
    let need: Vec<(&String, &Entry)> = tree.iter().filter(|(p, e)| !laid.get(*p).is_some_and(|l| l.blob == e.blob && l.mode == e.mode && stamp(p) == Some(l.stamp))).collect();
    if need.is_empty() {
        return Ok(());
    }
    laid.retain(|p, _| tree.contains_key(p));
    let write = |path: &str, entry: &Entry, bytes: &[u8]| -> Result<()> {
        let dest = files.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        // A new file, never the old one written over: its blocks may be a view's too.
        let _ = fs::remove_file(&dest);
        fs::write(&dest, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(if entry.mode & 0o111 != 0 { 0o755 } else { 0o644 }))?;
        }
        Ok(())
    };
    fetch(client, repo, &need, seeds, &write)?;
    for (p, e) in need {
        laid.insert(p.clone(), Laid { blob: e.blob.clone(), mode: e.mode, stamp: stamp(p).with_context(|| format!("{p} was not laid out"))? });
    }
    fs::write(&record, serde_json::to_vec(&laid)?)?;
    Ok(())
}

/// Removes from `files`/`rel` what is neither a file of `tree` nor a folder on the way to one.
fn prune(files: &Path, rel: &str, tree: &Tree, dirs: &HashSet<&str>) -> Result<()> {
    for entry in fs::read_dir(files.join(rel))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let kind = entry.file_type()?;
        if kind.is_dir() && dirs.contains(path.as_str()) {
            prune(files, &path, tree, dirs)?;
        } else if kind.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else if !(kind.is_file() && tree.contains_key(&path)) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The ready copy's files cloned to `dst`, a new folder: whole in one call where the system clones
/// folders (APFS), else file by file, each a clone where the filesystem has them, or a copy.
fn clone_files(src: &Path, dst: &Path, tree: &Tree) -> Result<()> {
    if clone_path(src, dst).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_dir_all(dst);
    let mut made = HashSet::new();
    let mut clones = true;
    for path in tree.keys() {
        let (from, to) = (src.join(path), dst.join(path));
        if let Some(parent) = to.parent() {
            if made.insert(parent.to_path_buf()) {
                fs::create_dir_all(parent)?;
            }
        }
        // The first clone refused says the filesystem has none: the rest are copies.
        clones = clones && clone_path(&from, &to).is_ok();
        if !clones {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// The files of a clone dated now, as if just written: the ready copy's dates are older than a
/// build the view may take with a cloned install (`target/`), and a tool that goes by dates (cargo,
/// make) would take a file the line changed since for the one it built.
fn dated_now(dir: &Path, tree: &Tree) -> Result<()> {
    for path in tree.keys() {
        date_now(&dir.join(path))?;
    }
    Ok(())
}

/// A file dated now, as if just written.
pub fn date_now(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let p = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        // No times given: both are now.
        if unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), std::ptr::null(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(unix))]
    fs::File::options().write(true).open(path)?.set_modified(std::time::SystemTime::now())?;
    Ok(())
}

/// Moves what `src` holds into `dir`, a folder within the same volume: a folder `dir` has already
/// (the host's `.claude/`) takes the entries one by one.
fn move_into(src: &Path, dir: &Path) -> std::io::Result<()> {
    for e in fs::read_dir(src)? {
        let e = e?;
        let to = dir.join(e.file_name());
        if e.file_type()?.is_dir() && fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
            move_into(&e.path(), &to)?;
        } else {
            fs::rename(e.path(), &to)?;
        }
    }
    Ok(())
}

/// Clones `src` to `dst`, a path that does not exist yet: on macOS clonefile (a folder whole, a
/// link as a link), on Linux FICLONE for a file. Fails where the filesystem cannot share blocks.
fn clone_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        // CLONE_NOFOLLOW (sys/clonefile.h): a link is cloned as a link, never followed.
        const CLONE_NOFOLLOW: u32 = 0x0001;
        let (s, d) = (std::ffi::CString::new(src.as_os_str().as_bytes())?, std::ffi::CString::new(dst.as_os_str().as_bytes())?);
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } == 0 {
            return Ok(());
        }
        Err(std::io::Error::last_os_error())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;
        let from = fs::File::open(src)?;
        let meta = from.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a folder is cloned file by file"));
        }
        use std::os::unix::fs::PermissionsExt;
        let to = fs::OpenOptions::new().write(true).create_new(true).mode(meta.permissions().mode()).open(dst)?;
        if unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) } == 0 {
            // The mode as it was, whatever the umask took from it at the creation.
            return to.set_permissions(meta.permissions());
        }
        let e = std::io::Error::last_os_error();
        drop(to);
        let _ = fs::remove_file(dst);
        Err(e)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (src, dst);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no clones on this system"))
    }
}

/// Clones the folder `src` to `dst`, a path that does not exist yet: whole on macOS, on Linux
/// folder by folder, links made again and each file a reflink. Any file that cannot be cloned
/// fails it all: the caller removes what was made.
fn clone_folder(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        fs::create_dir(dst)?;
        fs::set_permissions(dst, fs::metadata(src)?.permissions())?;
        for e in fs::read_dir(src)? {
            let e = e?;
            let (from, to) = (e.path(), dst.join(e.file_name()));
            let kind = e.file_type()?;
            if kind.is_dir() {
                clone_folder(&from, &to)?;
            } else if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
            } else {
                clone_path(&from, &to)?;
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    clone_path(src, dst)
}

/// Lockfiles, and the folder an install fills beside each.
const LOCKFILES: [(&str, &str); 9] = [
    ("pnpm-lock.yaml", "node_modules"),
    ("package-lock.json", "node_modules"),
    ("yarn.lock", "node_modules"),
    ("bun.lock", "node_modules"),
    ("bun.lockb", "node_modules"),
    ("Cargo.lock", "target"),
    ("uv.lock", ".venv"),
    ("poetry.lock", ".venv"),
    ("go.sum", "vendor"),
];

/// Dependencies a view took from another: the lockfile they match, the folder they come from, the
/// folders cloned.
#[derive(Clone, Debug, Serialize)]
pub struct Reused {
    pub lockfile: String,
    pub from: PathBuf,
    pub folders: Vec<String>,
}

/// What an install would put in `dir` for the lockfiles of `tree`, cloned from the first of
/// `sources` (other views of the repo, the agent's checkout) that holds the same lockfile, so that
/// the install finds nothing to do: `node_modules` (each workspace package's too), `target`,
/// `.venv`, `vendor`. A folder `dir` has already is its own install and stays. Never shared between
/// two views: a clone (APFS, a filesystem with reflinks), or nothing.
pub fn reuse_deps(tree: &Tree, dir: &Path, sources: &[PathBuf]) -> Vec<Reused> {
    let mut out = Vec::new();
    for (lock, entry) in tree {
        let (base, name) = lock.rsplit_once('/').unwrap_or(("", lock.as_str()));
        let Some(&(_, folder)) = LOCKFILES.iter().find(|(l, _)| *l == name) else { continue };
        let at = |p: &str| if base.is_empty() { p.to_string() } else { format!("{base}/{p}") };
        let mut folders = vec![at(folder)];
        if folder == "node_modules" {
            // Each package of a workspace beneath the lockfile has its own, unless it has its own lockfile.
            let within = if base.is_empty() { String::new() } else { format!("{base}/") };
            let own_lock = |pkg: &str| LOCKFILES.iter().any(|(l, f)| *f == "node_modules" && tree.contains_key(&format!("{pkg}/{l}")));
            folders.extend(tree.keys().filter_map(|p| p.strip_suffix("/package.json")).filter(|p| p.starts_with(&within) && *p != base && !own_lock(p)).map(|p| format!("{p}/node_modules")));
        }
        // What the tree tracks is laid out, not cloned.
        folders.retain(|f| {
            let inside = format!("{f}/");
            !tree.range::<String, _>(inside.clone()..).next().is_some_and(|(p, _)| p.starts_with(&inside)) && fs::symlink_metadata(dir.join(f)).is_err()
        });
        if folders.is_empty() {
            continue;
        }
        let same = |src: &Path| read_project(&src.join(lock)).is_ok_and(|b| hash_bytes(&b) == entry.blob);
        // A folder, not a link: a link would share the other view's install.
        let real = |p: PathBuf| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
        let Some(src) = sources.iter().find(|s| s.as_path() != dir && same(s) && folders.iter().any(|f| real(s.join(f)))) else { continue };
        let mut cloned = Vec::new();
        for f in folders.iter().filter(|f| real(src.join(f))) {
            let to = dir.join(f);
            if !to.parent().is_some_and(|p| p.is_dir()) {
                continue;
            }
            if clone_folder(&src.join(f), &to).is_err() {
                let _ = fs::remove_dir_all(&to);
                break;
            }
            relocate(&to, src, dir);
            cloned.push(f.clone());
        }
        if !cloned.is_empty() {
            out.push(Reused { lockfile: lock.clone(), from: src.clone(), folders: cloned });
        }
    }
    out
}

/// A cloned install still names the folder it was made in, in the small files tools write their
/// paths into: a venv's scripts and `.pth` files, the shims in `.bin`, pnpm's state at the top.
/// They are made to name `to`, so that nothing of the view runs or reads in the other folder.
fn relocate(folder: &Path, from: &Path, to: &Path) {
    let mut pairs = vec![(from.to_path_buf(), to.to_path_buf())];
    if let (Ok(f), Ok(t)) = (from.canonicalize(), to.canonicalize()) {
        pairs.push((f, t));
    }
    pairs.dedup();
    let list = |d: PathBuf| fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path());
    let mut files: Vec<PathBuf> = list(folder.to_path_buf()).chain(list(folder.join(".bin"))).chain(list(folder.join("bin"))).collect();
    for lib in list(folder.join("lib")) {
        files.extend(list(lib.join("site-packages")).filter(|p| p.extension().is_some_and(|x| x == "pth")));
    }
    for f in files {
        if !fs::symlink_metadata(&f).is_ok_and(|m| m.is_file() && m.len() < 1 << 20) {
            continue;
        }
        let Ok(bytes) = fs::read(&f) else { continue };
        if bytes.contains(&0) {
            continue;
        }
        let now = pairs.iter().fold(bytes.clone(), |b, (a, z)| replace_path(&b, a, z));
        if now != bytes {
            let _ = fs::write(&f, now);
        }
    }
}

/// `bytes` with the path `from` made `to` where it stands whole: not where it begins a longer name
/// (`views/repo-1` in `views/repo-12`).
fn replace_path(bytes: &[u8], from: &Path, to: &Path) -> Vec<u8> {
    let (a, z) = (from.as_os_str().as_encoded_bytes(), to.as_os_str().as_encoded_bytes());
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(i) = find(bytes, a, at) {
        let whole = !bytes.get(i + a.len()).is_some_and(|c| c.is_ascii_alphanumeric() || b"._-".contains(c));
        out.extend_from_slice(&bytes[at..i]);
        out.extend_from_slice(if whole { z } else { a });
        at = i + a.len();
    }
    out.extend_from_slice(&bytes[at..]);
    out
}

/// Before a new task in a folder AVP owns (a view under $AVP_VIEWS): every file the project would
/// version is checked against `tree` by its content. One a task changed without sending it is laid
/// again; one no task landed goes. What `.gitignore` hides stays (node_modules, builds), so the
/// agent that takes the folder keeps its install.
pub fn reset(dir: &Path, tree: &Tree, state: &mut ViewState) -> Result<()> {
    for (path, (hash, mode)) in scan(dir, &state.known, &mut state.stamps)? {
        match tree.get(&path) {
            Some(e) if e.blob == hash && e.mode == mode => {
                state.known.insert(path, (hash, mode));
            }
            Some(_) => {
                state.known.remove(&path);
            }
            None => {
                let _ = fs::remove_file(dir.join(&path));
                state.known.remove(&path);
            }
        }
    }
    Ok(())
}

/// A change this folder holds that no task has: the version it was made on, and what it is now.
pub struct Unsent {
    pub path: String,
    /// The file as the view last synced it; none for a new file.
    pub base: Option<String>,
    /// None when it was deleted here.
    pub now: Option<(Vec<u8>, u32)>,
}

/// What changed here since the last sync and was never sent as a write.
pub fn unsent(dir: &Path, state: &mut ViewState) -> Result<Vec<Unsent>> {
    let now = scan(dir, &state.known, &mut state.stamps)?;
    let mut out = vec![];
    for (path, (hash, mode)) in &now {
        let base = state.known.get(path);
        if base == Some(&(hash.clone(), *mode)) {
            continue;
        }
        out.push(Unsent { path: path.clone(), base: base.map(|(b, _)| b.clone()), now: Some((read_project(&dir.join(path))?, *mode)) });
    }
    for (path, (blob, _)) in &state.known {
        if !now.contains_key(path) && !is_host_path(path) && !dir.join(path).exists() {
            out.push(Unsent { path: path.clone(), base: Some(blob.clone()), now: None });
        }
    }
    Ok(out)
}

pub struct Carried {
    pub kept: usize,
    pub merged: usize,
    pub conflicts: Vec<String>,
}

/// Lays unsent changes back over the head just laid out: as they were where the line left the file
/// alone, merged three ways where it changed it too (`git merge-file`), with conflict markers where
/// both changed the same lines. The caller then sends them as the new task's writes.
pub fn carry(client: &Client, dir: &Path, repo: &str, tree: &Tree, changes: Vec<Unsent>) -> Result<Carried> {
    let mut report = Carried { kept: 0, merged: 0, conflicts: vec![] };
    for c in changes {
        let head = tree.get(&c.path).map(|e| e.blob.clone());
        let dest = dir.join(&c.path);
        let Some((bytes, mode)) = c.now else {
            // Deleted here: deleted still, unless the line changed the file meanwhile.
            if head == c.base {
                let _ = fs::remove_file(&dest);
                report.kept += 1;
            } else if head.is_some() {
                report.conflicts.push(format!("{} (deleted here, changed on the line: the line's version is laid)", c.path));
            }
            continue;
        };
        let content = if head == c.base {
            report.kept += 1;
            bytes
        } else {
            let (merged, clean) = merge3(client, repo, &bytes, c.base.as_deref(), head.as_deref())?;
            if clean {
                report.merged += 1;
            } else {
                report.conflicts.push(c.path.clone());
            }
            merged
        };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 }))?;
        }
    }
    Ok(report)
}

/// Mine merged with the line's version on their common base: clean, or with markers (`false`). A file
/// the line deleted, or a binary one, keeps mine as it is and counts as a conflict.
fn merge3(client: &Client, repo: &str, mine: &[u8], base: Option<&str>, head: Option<&str>) -> Result<(Vec<u8>, bool)> {
    let Some(head) = head else { return Ok((mine.to_vec(), false)) };
    let theirs = client.get_object(repo, head)?;
    let base = match base {
        Some(b) => client.get_object(repo, b)?,
        None => vec![],
    };
    merge_bytes(mine, &base, &theirs)
}

/// `mine` and `theirs`, both changed from `base`, merged three ways (`git merge-file`): clean, or
/// with markers (`false`). A binary file keeps mine as it is and counts as a conflict.
pub fn merge_bytes(mine: &[u8], base: &[u8], theirs: &[u8]) -> Result<(Vec<u8>, bool)> {
    if [mine, theirs, base].iter().any(|b| b.contains(&0)) {
        return Ok((mine.to_vec(), false));
    }
    let tmp = std::env::temp_dir().join(format!("avp-merge-{}-{}", std::process::id(), hash_bytes(mine)));
    fs::create_dir_all(&tmp)?;
    let (m, b, t) = (tmp.join("mine"), tmp.join("base"), tmp.join("line"));
    fs::write(&m, mine)?;
    fs::write(&b, base)?;
    fs::write(&t, theirs)?;
    let out = std::process::Command::new("git").args(["merge-file", "-p", "-L", "here", "-L", "base", "-L", "line"]).arg(&m).arg(&b).arg(&t).output();
    let _ = fs::remove_dir_all(&tmp);
    match out {
        Ok(o) if o.status.code() == Some(0) => Ok((o.stdout, true)),
        Ok(o) if o.status.code().is_some_and(|n| n > 0) => Ok((o.stdout, false)),
        _ => Ok((mine.to_vec(), false)),
    }
}

pub struct SyncReport {
    pub puts: Vec<WriteOutcome>,
    pub deletes: Vec<WriteOutcome>,
}

/// Sends every difference between the directory and what the view knows as writes.
pub fn sync(client: &Client, dir: &Path, state: &mut ViewState) -> Result<SyncReport> {
    let task = state.task.clone().context("no open task in this view: avp open first")?;
    let now = scan(dir, &state.known, &mut state.stamps)?;
    let mut report = SyncReport { puts: vec![], deletes: vec![] };
    for (path, (hash, mode)) in &now {
        if state.known.get(path) == Some(&(hash.clone(), *mode)) {
            continue;
        }
        let bytes = read_project(&dir.join(path))?;
        let blob = client.put_object(&state.repo, bytes)?;
        let out: WriteOutcome = client.post(&format!("/repos/{}/tasks/{}/writes", state.repo, task), &PutWrite { path: path.clone(), kind: WriteKind::Put, blob: Some(blob), mode: *mode })?;
        if out.lease != "refused" {
            state.known.insert(path.clone(), (hash.clone(), *mode));
        }
        if out.lease != "unchanged" {
            report.puts.push(out);
        }
    }
    let gone: Vec<String> = state.known.keys().filter(|p| !now.contains_key(*p) && !is_host_path(p)).cloned().collect();
    for path in gone {
        let out: WriteOutcome = client.post(&format!("/repos/{}/tasks/{}/writes", state.repo, task), &PutWrite { path: path.clone(), kind: WriteKind::Delete, blob: None, mode: 0o100644 })?;
        if out.lease != "refused" {
            state.known.remove(&path);
        }
        report.deletes.push(out);
    }
    save(dir, state)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What tests and tools leave in a folder is not sent, with no `.gitignore` to say so; a path the
    /// line already holds is.
    #[test]
    fn what_tests_leave_is_not_the_agents_work() {
        let w = std::env::temp_dir().join(format!("avp-junk-{}", ulid::Ulid::new()));
        for p in ["pkg/stats.py", "pkg/__pycache__/stats.cpython-314.pyc", "tests/.pytest_cache/v/x", "node_modules/a/index.js", ".DS_Store", "old.pyc", "web/app.js"] {
            fs::create_dir_all(w.join(p).parent().unwrap()).unwrap();
            fs::write(w.join(p), p).unwrap();
        }
        let known: BTreeMap<String, (String, u32)> = [("old.pyc".to_string(), (String::new(), 0o100644))].into();
        let got: Vec<String> = scan(&w, &known, &mut BTreeMap::new()).unwrap().into_keys().collect();
        assert_eq!(got, ["old.pyc", "pkg/stats.py", "web/app.js"]);
        let _ = fs::remove_dir_all(&w);
    }

    /// A new view takes the install of a view whose lockfile is the same: a clone of its own, whose
    /// shims name the new folder. A view whose lockfile differs gives nothing, a view that has an
    /// install keeps it, and where the filesystem cannot clone, nothing is taken at all.
    #[test]
    fn a_view_takes_the_install_of_one_with_the_same_lockfile() {
        let w = std::env::temp_dir().join(format!("avp-deps-{}", ulid::Ulid::new()));
        let (a, b, c) = (w.join("a"), w.join("b"), w.join("c"));
        let mut tree = Tree::new();
        for (path, body) in [("pnpm-lock.yaml", "lockfileVersion: '9.0'\n"), ("package.json", "{}"), ("web/package.json", "{}")] {
            tree.insert(path.into(), Entry { blob: hash_bytes(body.as_bytes()), mode: 0o100644 });
            for v in [&a, &b, &c] {
                fs::create_dir_all(v.join(path).parent().unwrap()).unwrap();
                fs::write(v.join(path), if v == &c && path == "pnpm-lock.yaml" { "another lockfile" } else { body }).unwrap();
            }
        }
        for v in [&a, &c] {
            fs::create_dir_all(v.join("node_modules/dep")).unwrap();
            fs::create_dir_all(v.join("node_modules/.bin")).unwrap();
            fs::create_dir_all(v.join("web/node_modules")).unwrap();
            fs::write(v.join("node_modules/dep/index.js"), format!("from {}\n", v.display())).unwrap();
            fs::write(v.join("node_modules/.bin/dep"), format!("#!/bin/sh\nexec node {}/node_modules/dep/index.js\n", v.display())).unwrap();
            fs::write(v.join("web/node_modules/x"), "x").unwrap();
        }
        fs::write(w.join("probe"), "p").unwrap();
        let clones = clone_path(&w.join("probe"), &w.join("probe-clone")).is_ok();

        let got = reuse_deps(&tree, &b, &[c.clone(), a.clone()]);
        if !clones {
            assert!(got.is_empty() && !b.join("node_modules").exists(), "a clone, or nothing");
            let _ = fs::remove_dir_all(&w);
            return;
        }
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].lockfile.as_str(), &got[0].from, got[0].folders.clone()), ("pnpm-lock.yaml", &a, vec!["node_modules".to_string(), "web/node_modules".to_string()]));
        assert_eq!(fs::read_to_string(b.join("web/node_modules/x")).unwrap(), "x");
        let shim = fs::read_to_string(b.join("node_modules/.bin/dep")).unwrap();
        assert!(shim.contains(&format!("{}/node_modules", b.display())) && !shim.contains(&a.display().to_string()), "{shim}");
        // A clone, not a shared folder: what the new view writes, the other never sees.
        fs::write(b.join("node_modules/dep/index.js"), "changed\n").unwrap();
        assert_eq!(fs::read_to_string(a.join("node_modules/dep/index.js")).unwrap(), format!("from {}\n", a.display()));

        assert!(reuse_deps(&tree, &b, &[a.clone()]).is_empty(), "a view with an install keeps it");
        fs::remove_dir_all(b.join("node_modules")).unwrap();
        fs::remove_dir_all(b.join("web/node_modules")).unwrap();
        assert!(reuse_deps(&tree, &b, &[c.clone()]).is_empty() && !b.join("node_modules").exists(), "another lockfile gives nothing");
        let _ = fs::remove_dir_all(&w);
    }
}
