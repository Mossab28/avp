//! The six nouns. Everything on the wire is one of these.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Hash = String;

/// Who acts: the person the account belongs to, the agent runtime, and its session.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Actor {
    pub person: String,
    pub agent: String,
    pub session: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub blob: Hash,
    /// Unix mode bits that matter: 0o100644 or 0o100755.
    pub mode: u32,
}

/// A tree is the whole repository at one instant: path -> entry. Stored as an object, named by
/// the hash of its canonical JSON.
pub type Tree = BTreeMap<String, Entry>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LeasePolicy {
    /// A write outside the task's leases goes through, takes the lease, and the previous holder is told.
    Advisory,
    /// A write outside the task's leases is refused.
    Strict,
}

/// What a landing needs besides a green proof.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LandingRule {
    /// A green proof lands.
    Proof,
    /// A green proof waits for a person's approval.
    ProofAndApproval,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub name: String,
    pub policy: LeasePolicy,
    #[serde(default = "default_landing")]
    pub landing: LandingRule,
    pub created_at: i64,
}

pub fn default_landing() -> LandingRule {
    LandingRule::Proof
}

/// A person's word on a task: approved, or refused with a reason the agent receives.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Approval {
    pub task: String,
    pub person: String,
    pub approved: bool,
    pub reason: String,
    pub at: i64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Landed,
    Abandoned,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub repo: String,
    pub goal: String,
    pub actor: Actor,
    pub scope: Vec<String>,
    /// The line position the task builds on.
    pub base_position: i64,
    pub base_tree: Hash,
    pub state: TaskState,
    pub created_at: i64,
    pub landed_position: Option<i64>,
    /// Where the agent's conversation for this task lives (a path or a URL), when it says.
    #[serde(default)]
    pub transcript: Option<String>,
    /// Open tasks whose goal is close to this one, said when it opens: duplicated work is the
    /// most expensive thing agents do without a shared substrate. Not stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub similar: Vec<String>,
    /// The task this one is stacked on (`avp open --on`): it builds on that task's tree, not on the
    /// line's, and lands after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    Put,
    Delete,
    /// The server rewrote the file while replaying the task on a moved line and could not merge it;
    /// the content carries conflict markers and the task must settle it.
    Conflict,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Write {
    pub seq: i64,
    pub task: String,
    pub path: String,
    pub kind: WriteKind,
    pub blob: Option<Hash>,
    pub mode: u32,
    pub at: i64,
    pub undone: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub path: String,
    pub task: String,
    pub taken_at: i64,
    pub renewed_at: i64,
    pub expires_at: i64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Green,
    Red,
    /// The failure is in a file another open task holds: this task waits for that one.
    Blocked,
    /// A full suite's only: a shard went red, then green when it ran again on the same tree. It
    /// counts as green for promotions and `follow`, never as red. A task's proof never runs again:
    /// it gates a landing, and a red there is the agent's to read.
    Flaky,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub command: String,
    pub exit: i32,
    pub seconds: f64,
    pub output: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proof {
    pub id: String,
    pub task: Option<String>,
    /// The line position the candidate was built on.
    pub base_position: i64,
    pub candidate_tree: Hash,
    pub files: Vec<String>,
    pub steps: Vec<Step>,
    pub verdict: Verdict,
    /// Files the failure names, with the open task that holds each (if any).
    pub blamed: Vec<(String, Option<String>)>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineEntry {
    pub position: i64,
    pub task: Option<String>,
    pub tree: Hash,
    pub landed_at: i64,
    pub proof: Option<String>,
    /// The full suite after landing: none yet, green, flaky, or red.
    pub full: Option<Verdict>,
    pub note: String,
    /// The git commit whose tree is this position: the imported commit, or the commit the export
    /// wrote for it. The export starts from the last position that has one.
    #[serde(default)]
    pub git: Option<String>,
    /// AVP took this position back by itself: its full suite went red twice and named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverted: Option<Reverted>,
}

/// A position AVP reverted on its own (`line.reverted`): what took it back, and why.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reverted {
    /// The position that took it back, and the task AVP opened to land it.
    pub by: i64,
    pub task: String,
    /// The shard of the full suite that went red, and the first lines it failed on.
    pub shard: String,
    pub tests: Vec<String>,
    /// The red run.
    pub proof: String,
}

/// Who AVP is when it acts on the line by itself (a revert of a position that broke the full suite).
pub fn avp_actor() -> Actor {
    Actor { person: "avp".into(), agent: "avp".into(), session: String::new() }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub task: Option<String>,
    pub path: Option<String>,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Environment {
    pub name: String,
    /// What the environment serves: observed from it, or moved by a promotion that has no deploy.
    /// -1 while nothing is known.
    pub position: i64,
    pub moved_at: i64,
    /// The last promotion here, when a deploy carries promotions out: what, who, when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wanted: Option<Promotion>,
    /// How a promotion is carried out (avp.toml `[envs.<name>].deploy`), for the bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<DeployConfig>,
    /// What the deploy of the promotion is doing now, as the bridge last read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<DeployProgress>,
}

/// The step a promotion's deploy is at (building its images, waiting for a runner or an approval,
/// installing), read from the CI by the bridge: an agent sees where the wait is without leaving AVP.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeployProgress {
    pub position: i64,
    pub step: String,
    /// When that step started (ms).
    pub since: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When the bridge read it (ms), set by the server.
    #[serde(default)]
    pub at: i64,
}

/// A position promoted to an environment, waiting to be served there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Promotion {
    pub position: i64,
    /// Its git commit, for a deploy that takes a sha.
    pub git: Option<String>,
    /// The person who promoted (the proxy's identity), or `avp` for a promotion the line made.
    pub by: String,
    pub agent: String,
    pub at: i64,
}

/// A deploy AVP's GitHub bridge runs for a promotion: a workflow dispatched on the repo's branch,
/// its inputs (`{git}` and `{position}` filled in), and the GitHub environment whose required
/// review the promotion stands for, when there is one.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DeployConfig {
    pub github_workflow: String,
    #[serde(default)]
    pub inputs: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approve_environment: Option<String>,
}

// ---- requests and answers ----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenTask {
    pub goal: String,
    pub actor: Actor,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub transcript: Option<String>,
}

/// The words of a goal that carry meaning: lowercase, three letters or more, common words out.
pub fn goal_words(goal: &str) -> std::collections::BTreeSet<String> {
    const COMMON: &[&str] = &["the", "and", "for", "with", "from", "into", "that", "this", "les", "des", "une", "pour", "sur", "dans", "avec", "par", "qui", "que", "est", "pas"];
    goal.split(|c: char| !c.is_alphanumeric()).map(|w| w.to_lowercase()).filter(|w| w.chars().count() >= 3 && !COMMON.contains(&w.as_str())).collect()
}

/// How close two goals are: shared meaningful words over all of them (0 to 1).
pub fn goal_likeness(a: &str, b: &str) -> f64 {
    let (a, b) = (goal_words(a), goal_words(b));
    let shared = a.intersection(&b).count();
    let all = a.union(&b).count();
    if all == 0 || shared < 2 { 0.0 } else { shared as f64 / all as f64 }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PutWrite {
    pub path: String,
    pub kind: WriteKind,
    pub blob: Option<Hash>,
    #[serde(default = "default_mode")]
    pub mode: u32,
}

pub fn default_mode() -> u32 {
    0o100644
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteOutcome {
    pub write: Write,
    /// What the lease did: "held", "taken", "taken_over" (someone else held it, advisory), "refused".
    pub lease: String,
    pub previous_holder: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskStatus {
    pub task: Task,
    pub head_position: i64,
    pub writes: Vec<Write>,
    pub leases: Vec<Lease>,
    pub last_proof: Option<Proof>,
    #[serde(default)]
    pub approval: Option<Approval>,
    /// For a stacked task, where the task it is stacked on stands, and what to do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// One file of a task's change, for a person to read before approving it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    /// "added", "modified" or "deleted".
    pub change: String,
    /// The unified diff, without its header; empty for a binary or very large file.
    pub patch: String,
    pub binary: bool,
}

/// An open task moved onto the head of the line without landing (`catch_up`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaughtUp {
    pub position: i64,
    /// False when the task already sat on the head.
    pub moved: bool,
    /// Paths left with conflict markers, to settle before landing.
    pub conflicts: Vec<String>,
    /// The open task it stands on, for a task stacked on one: its tree as it is now, not the head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// For a stacked task, where the task it is stacked on stands, and what to do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

/// A task of a stack that landed under the one `avp land --stack` was asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackLanded {
    pub task: String,
    pub position: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Landed {
    pub position: i64,
    pub tree: Hash,
    pub proof: String,
    pub replayed: bool,
    /// With `--stack`: the tasks under this one that landed before it, bottom first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<StackLanded>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotLanded {
    /// Guarded paths the task touches: why a person's approval is needed.
    #[serde(default)]
    pub guarded: Vec<String>,
    /// "conflict": paths rewritten with markers; "red": the proof failed; "blocked": waits for
    /// tasks; "approval": green, waiting for a person; "refused": a person said no (see `word`);
    /// "stacked": the task it is stacked on has not landed (see `note`).
    pub reason: String,
    #[serde(default)]
    pub word: Option<Approval>,
    pub conflicts: Vec<String>,
    pub proof: Option<Proof>,
    pub waits_for: Vec<String>,
    /// What a stacked task waits for, and what to do next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// With `--stack`: the tasks under this one that landed before the stack stopped, bottom first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<StackLanded>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Why {
    pub path: String,
    pub position: i64,
    pub task: Option<Task>,
    pub proof: Option<Proof>,
    /// For a line (1-based): which one, and what it reads at the position asked about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportEntry {
    pub path: String,
    pub blob: Hash,
    pub mode: u32,
}

/// A path a tree may hold: relative, `/`-separated, with no empty, `.` or `..` part, no
/// backslash and no NUL. Anything else could write outside the folder it is laid out in.
pub fn safe_path(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && !p.contains('\\') && !p.contains('\0') && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid::Ulid::new().to_string().to_lowercase())
}

/// A guarded path pattern against a repo path. `dir/` or a plain path covers everything under
/// it; `*` stays inside one segment, `**` crosses segments, `?` is one character.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let p = pattern.trim_start_matches('/');
    if !p.contains(['*', '?']) {
        let dir = p.trim_end_matches('/');
        return path == dir || path.starts_with(&format!("{dir}/"));
    }
    let chars: Vec<char> = p.chars().collect();
    let mut re = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 2;
                if chars.get(i) == Some(&'/') {
                    i += 1;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    regex::Regex::new(&re).map(|r| r.is_match(path)).unwrap_or(false)
}
