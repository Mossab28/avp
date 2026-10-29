use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use avp::client::{unreachable, Client};
use avp::model::*;
use avp::offline::{self, Op};
use avp::{engine, gitbridge, mcp, server, up, view};

/// What `avp --version` says: an AVP Cloud build says so (tests/ci.sh needs one).
const VERSION: &str = if cfg!(feature = "cloud") { concat!(env!("CARGO_PKG_VERSION"), " (AVP Cloud)") } else { env!("CARGO_PKG_VERSION") };

/// Version control for agents: open, write, check, land.
#[derive(Parser)]
#[command(name = "avp", version = VERSION, about)]
struct Cli {
    /// Server to talk to (or AVP_SERVER). Its value is never shown in the help: it may carry a password.
    #[arg(long, global = true, env = "AVP_SERVER", hide_env_values = true, default_value = "http://127.0.0.1:7117")]
    server: String,
    /// The view directory (default: current directory).
    #[arg(long, global = true, short = 'C')]
    dir: Option<PathBuf>,
    /// Print answers as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Put AVP on this git checkout without touching it: a local server if none answers, the
    /// repo imported from its history, the checks its proofs run, a mirror that writes every
    /// landed task on a branch of its own, and AVP wired for every coding agent on this machine.
    Up {
        /// Repo name on the server (default: the checkout's folder name).
        #[arg(long)]
        name: Option<String>,
        /// The branch the mirror writes.
        #[arg(long, default_value = "avp/line")]
        branch: String,
        /// Write the mirror's commits locally, push nothing.
        #[arg(long)]
        no_push: bool,
        /// How many commits of HEAD the line starts from.
        #[arg(long, default_value_t = 50)]
        history: usize,
        /// Do not guess and land the checks (avp.toml).
        #[arg(long)]
        no_checks: bool,
        /// On a team server, run a mirror from this checkout too (the server usually mirrors the line
        /// itself, and a second mirror would write the same commits twice).
        #[arg(long)]
        mirror: bool,
    },
    /// Stop this checkout's mirror, and the local server once no mirror is left.
    Down {
        #[arg(long)]
        name: Option<String>,
    },
    /// The coding agents of this machine, for this checkout: wired for AVP, found but not wired
    /// (with the line that wires it), or not installed. `avp up` wires the ones it can.
    Agents,
    /// Move this checkout's team to AVP, the first two steps of docs/MIGRATE.md in one command: AVP
    /// beside main as `avp up` puts it (server, line, mirror on avp/line, agents), avp.toml from the
    /// CI (or the one there, kept, with what differs), proven on this machine on the head of the
    /// line (each proof step and shard of the whole suite green or red, with what to change), then
    /// the plan of what is left. Run again after a fix: it starts from where it is. Never pushes to
    /// main, never changes a setting on GitHub.
    Migrate {
        /// Repo name on the server (default: the checkout's folder name).
        #[arg(long)]
        name: Option<String>,
        /// Your word for the checks: the avp.toml proven green lands without waiting for a person's
        /// approval (you are that person).
        #[arg(long)]
        yes: bool,
    },
    /// Serve the verbs as MCP tools over stdio, for an agent running in this view.
    Mcp,
    /// Wait until other tasks have landed (or been dropped) and paths are on the line: what a red
    /// proof needs that teammates have not landed yet. Then land again.
    Wait {
        /// A task id (or the start of one), as many as needed.
        #[arg(long)]
        task: Vec<String>,
        /// A path that must be on the head of the line.
        #[arg(long)]
        path: Vec<String>,
        /// Seconds before giving up.
        #[arg(long, default_value_t = 900)]
        timeout: u64,
    },
    /// Take the server's avp when it runs another version: the binary it runs, put in place of this
    /// one, when both run on the same platform. A server rolled back takes its clients back with it.
    Update,
    /// Run the server.
    Serve {
        #[arg(long, default_value = "127.0.0.1:7117")]
        listen: String,
        #[arg(long, env = "AVP_DATA", default_value = ".avp-data")]
        data: PathBuf,
    },
    /// Run proofs for a server started with AVP_WORKERS=1: claim one, lay it out, run its
    /// steps, send them back, again. Start as many as the proofs you want at once.
    Worker {
        #[arg(long, env = "AVP_DATA", default_value = ".avp-worker")]
        data: PathBuf,
    },
    /// Bring GitHub pull requests into the line, oldest first: each one's change merged three-way
    /// onto the exact commit of its task's base, proven, and with --land landed and the PR closed
    /// with its position. Run from a checkout of the repo; GH_TOKEN reads and closes the PRs.
    ImportPr {
        numbers: Vec<u64>,
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Land the ones whose proof is green (otherwise they stay open tasks).
        #[arg(long)]
        land: bool,
        /// Where the views go (default: $AVP_VIEWS or ~/avp).
        #[arg(long)]
        views: Option<PathBuf>,
        /// Keep looking at the open pull requests: each new or updated one comes in, and one whose
        /// task lands gets its position as a comment and is closed.
        #[arg(long, conflicts_with = "numbers")]
        follow: bool,
        /// Seconds between two looks of --follow.
        #[arg(long, default_value_t = 60)]
        every: u64,
    },
    /// Create a repo from a git checkout: its last commits become the first positions of the line.
    /// For a repo that exists, the commits of HEAD (with --follow, of a branch, again and again) the
    /// line does not have yet each become a task: replayed on the head, proven, landed as their
    /// author through the agent `git`. One that does not land stays open, and those after it wait.
    ImportGit {
        name: String,
        /// How many first-parent commits of HEAD to import, oldest first.
        #[arg(long, default_value_t = 100)]
        history: usize,
        #[arg(long, default_value = "advisory")]
        policy: String,
        /// Keep bringing in the commits that reach this branch by Git, fetched from --remote.
        #[arg(long, value_name = "BRANCH")]
        follow: Option<String>,
        /// Where --follow fetches the branch from (none of that name: the checkout's own branch).
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Seconds between two looks of --follow.
        #[arg(long, default_value_t = 60)]
        every: u64,
        /// Where the task of a commit that does not land is laid out (default: $AVP_VIEWS or ~/avp).
        #[arg(long)]
        views: Option<PathBuf>,
    },
    /// Propose avp.toml from the repo's CI (.github/workflows/*.yml, .gitlab-ci.yml): its push and
    /// pull request jobs as the full suite, their install, typecheck, lint and tests as the proof,
    /// its services and matrices, its deploys as environments, and a line for each thing it could
    /// not translate, with what to do. Prints it; --write writes it.
    ImportCi {
        /// Write avp.toml, or avp.imported.toml beside the one there is (never over a file).
        #[arg(long)]
        write: bool,
    },
    /// Write the line's new positions as commits on top of the last position that has one, check
    /// the result against the head of the line, and push it to a branch (fast-forward only). Run it
    /// from any checkout of the repo: the server remembers every commit, so it resumes anywhere.
    Mirror {
        #[arg(long)]
        repo: String,
        /// Branch to push to; `main` makes main a mirror of the line.
        #[arg(long, default_value = "avp/line")]
        branch: String,
        /// Remote to push to (none: write the commits, push nothing).
        #[arg(long)]
        remote: Option<String>,
        /// Committer of every commit (the author is the task's person). Fixed, so commits are
        /// the same whichever machine writes them.
        #[arg(long, env = "AVP_COMMITTER", default_value = "AVP <avp@users.avp>")]
        committer: String,
        /// Keep mirroring, every this many seconds (one line printed per change).
        #[arg(long)]
        follow: Option<u64>,
    },
    /// Create a repo on the server from this directory (position 0 of its line).
    Init {
        name: String,
        #[arg(long, default_value = "advisory")]
        policy: String,
    },
    /// Make this directory a view of an existing repo without opening a task (its files must
    /// already match the head, e.g. a checkout of the same commit).
    Attach {
        #[arg(long)]
        repo: String,
    },
    /// Open a task on the head and lay its tree out here (`--on`: on another open task's work).
    Open {
        #[arg(long)]
        goal: String,
        /// Stack the task on an open task (its id, or the start of one): it builds on that task's
        /// work before it lands, `avp refresh` replays it on that task's latest changes, and it lands
        /// after it (`avp land --stack` lands both).
        #[arg(long)]
        on: Option<String>,
        /// Paths this task intends to write (leases taken now).
        #[arg(long)]
        scope: Vec<String>,
        /// Repo name or id (needed when the directory is not a view yet; or AVP_REPO).
        #[arg(long, env = "AVP_REPO")]
        repo: Option<String>,
        /// Write every file of the tree, whatever the view thinks it already has.
        #[arg(long)]
        fresh: bool,
    },
    /// Send every change in the directory as writes.
    Sync,
    /// Send these paths as writes whatever the ignore rules say (git add -f).
    Add { paths: Vec<String> },
    /// Watch the directory and sync on every save.
    Watch,
    /// The task: writes, leases, last proof.
    Status,
    /// Take or renew leases on paths.
    Claim { paths: Vec<String> },
    /// Give leases back.
    Release { paths: Vec<String> },
    /// Verify the candidate tree.
    Check,
    /// Land the task on the line.
    Land {
        /// Land the open tasks it is stacked on first, the bottom one first, then this one.
        #[arg(long)]
        stack: bool,
        /// An open task to land with this one, as one position proven whole: tasks that need each
        /// other, red each alone (`avp wait` names them). As many as needed; each with its stack.
        #[arg(long, value_name = "TASK")]
        with: Vec<String>,
    },
    /// Undo writes after a sequence number (0 = all), or one path back to the base, or take the task
    /// to a save point (`--to`); lays the result out.
    Undo {
        #[arg(default_value_t = 0, value_name = "SEQ")]
        to: i64,
        #[arg(long)]
        path: Option<String>,
        /// A save point, by its number or its message (`avp saves`): back to it, or forward, the
        /// writes since kept in the journal.
        #[arg(long = "to", value_name = "SAVE", conflicts_with = "path")]
        save: Option<String>,
    },
    /// Name the task's state now, a save point to come back to with `avp undo --to`. Kept with the
    /// task, never landed.
    Save { message: String },
    /// The task's save points.
    Saves,
    /// What this task changes against the position it stands on (`--head`: against the head of the
    /// line), as a unified diff, colored on a terminal. `avp diff <a> <b>`: from position a to b;
    /// `avp diff <a>`: from a to this task (to the head, without one). Paths after them keep those
    /// files only (a folder: what is under it).
    Diff {
        /// Up to two positions, then paths (one that reads as a number: `./12`).
        args: Vec<String>,
        /// Against the head of the line, not the position the task stands on.
        #[arg(long)]
        head: bool,
        /// Each file with the lines it adds and removes, not the diff.
        #[arg(long)]
        stat: bool,
        #[arg(long)]
        repo: Option<String>,
    },
    /// A folder holding the head of a repo, on a machine with no git checkout of it: laid out,
    /// attached as a view, and the coding agents found here wired for it as `avp up` wires a
    /// checkout. Then start an agent there, or `avp open`.
    Clone {
        /// `<server>/<repo>` (`https://avp.example.com/shop`), or the repo's name on --server.
        source: Option<String>,
        /// The folder to make (default: the repo's name, here).
        dir: Option<PathBuf>,
        /// The repo, on --server (or AVP_SERVER): the first argument is then the folder.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Drop the task and its leases.
    Abandon,
    /// What waits in this folder's outbox (what changed something while the server did not answer),
    /// sent in order: each entry, when it was queued, why its last send failed. An entry the server
    /// refused stops those after it: `retry` sends them again, `drop` takes entries out.
    Outbox {
        #[command(subcommand)]
        cmd: Option<OutboxCmd>,
    },
    /// Say the open task's goal better before it lands: once landed, it is the line's record.
    Goal { goal: String },
    /// Bring the line's head into the task: what this folder changed is sent, the task is replayed
    /// on what landed since (not landed, not proven), and its tree is laid out here.
    Refresh,
    /// Put the git checkout this view lives in on the commit of a position once the mirror wrote it
    /// (started by `land` in a checkout view).
    #[command(hide = true)]
    Align { position: i64 },
    /// Events this task should see.
    Events {
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long)]
        follow: bool,
        /// Every event of the repo, not just this task's.
        #[arg(long)]
        all: bool,
    },
    /// Run the full suite of a position again (none: the head of the line), on the workers.
    Full {
        position: Option<i64>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// The line. With `--path`, the positions that changed that file, newest first: position,
    /// time, change, person, goal.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: i64,
        #[arg(long)]
        path: Option<String>,
        /// With `--path`, read back from this position instead of the head.
        #[arg(long, requires = "path")]
        from: Option<i64>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// A team's numbers on AVP: landings, how long a change takes to reach the line and each
    /// environment, its proof, the machine time, what went red. With --github and --before, the same
    /// window of pull requests and Actions runs on GitHub beside them, as the README's table.
    Stats {
        /// From when: how long ago (`48h`, `7d`), a date (`2026-10-25`, UTC) or an RFC3339 time.
        #[arg(long, default_value = "7d")]
        since: String,
        /// Until when, as --since (default: now).
        #[arg(long)]
        until: Option<String>,
        /// The repo on GitHub (owner/name) whose pull requests and Actions runs are the before. GH_TOKEN reads them.
        #[arg(long, requires = "before")]
        github: Option<String>,
        /// The before's window, `<from>..<to>`, each as --since: `2026-10-22..2026-10-24`.
        #[arg(long, requires = "github")]
        before: Option<String>,
        /// Keep the before on the server (its numbers, never the token): AVP Live compares to it,
        /// labeled "before AVP". A save replaces the one before it.
        #[arg(long, requires = "github")]
        save: bool,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Each line of a file at the head of the line, with the position that last changed it and its
    /// person. Reads back 500 positions at most: a line unchanged over them is marked `^`.
    Blame {
        path: String,
        /// The file as it was at this position instead of the head.
        #[arg(long)]
        at: Option<i64>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Why a file is what it is: the task that last wrote it, its goal, its proof.
    /// `path` or `path:line`: the line is followed back through every change to the task that wrote it.
    Why {
        path: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// What the line remembers of paths, before reading them: for each (a directory by its most
    /// changed files, 6 paths at most), the last 3 positions that changed it (position, age, agent,
    /// goal) and the open task holding it. `path:line` adds why that line is what it is.
    Context {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// What a position changed: its goal, who landed it and through which agent, its proof, the
    /// full suite's verdict, its git commit, then the diff file by file.
    Show {
        position: i64,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Undo a position: a task opened here that takes its changes back on the head of the line,
    /// merged with what changed since, proven like any other change when it lands.
    Revert {
        position: i64,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Find the position that broke something: the positions between a good one and a bad one are
    /// halved, each laid out in a scratch folder where the check runs (exit 0: good, anything else:
    /// bad), down to the first bad position, named with its goal, agent and proof.
    Bisect {
        /// The command that says good or bad, run by `sh -c` in the position's folder
        /// (`$AVP_BISECT_POSITION` says which): `--check "cargo test parser"`.
        #[arg(long)]
        check: String,
        /// A position where the check passes (default: the newest below --bad whose full suite is
        /// green, else 0).
        #[arg(long)]
        good: Option<i64>,
        /// A position where it fails (default: the head).
        #[arg(long)]
        bad: Option<i64>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Bring a position's change into the open task, three ways, conflicts left as markers as a
    /// replay leaves them: a change taken back comes back on the head of the line, to carry its fix.
    Pick { position: i64 },
    /// Name a position (none: the head): `avp tag v1.2`, `avp tag v1.2 41`; `-f` moves a name, `-d`
    /// deletes it. `avp log` and AVP Live show it, the mirror writes it as a git tag on the
    /// position's commit, `avp promote <env> v1.2` promotes it.
    Tag {
        name: String,
        position: Option<i64>,
        /// Delete the tag (and, on the mirror's next pass, its git tag).
        #[arg(short = 'd', long, conflicts_with_all = ["position", "force"])]
        delete: bool,
        /// Move a tag that names another position.
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
    },
    /// The line's tags, newest position first.
    Tags {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Open tasks of the repo.
    Tasks {
        #[arg(long)]
        repo: Option<String>,
    },
    /// A proof in full: each step's whole log, each line with when it came. No id: the last proof of
    /// this view's task.
    Proof {
        id: Option<String>,
        /// Only this step's log (or artifacts).
        #[arg(long)]
        step: Option<String>,
        /// Download what the steps kept (`artifacts = [..]` in avp.toml) into this folder, one folder
        /// per step.
        #[arg(long)]
        artifacts: Option<PathBuf>,
    },
    /// A person's yes on a task (the repo's landing rule or a guarded path may require it).
    Approve {
        task: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// A person's no on a task, with the reason the agent will read.
    Refuse {
        task: String,
        #[arg(long)]
        why: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// The repo's landing rule: proof, or proof_and_approval.
    Landing { rule: String },
    /// Guard paths: a task that touches one waits for a person's approval, whatever its proof
    /// says. `dir/` or a plain path covers what is under it; `*`, `**` and `?` are globs.
    Guard {
        patterns: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Lift guards.
    Unguard {
        patterns: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// The guarded paths, and the tasks waiting for a person's approval.
    Guarded {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Environments: what each serves, what was promoted there.
    Envs {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Promote a position to an environment (none: what its `after` serves, or the head). Its deploy
    /// in avp.toml is a GitHub workflow AVP's bridge starts (the environment moves once it serves the
    /// position), or a command AVP runs itself (`run`): this answers once it is over, and a restart
    /// of the server does not lose it, it runs again. No deploy: the environment moves at once.
    Promote {
        env: String,
        /// A position, or a tag's name.
        position: Option<String>,
        /// Promote without a green full suite, or other than what `after` serves.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Secrets for the tests (a payment sandbox key, an API test token), kept by the server for the
    /// repo, encrypted: a step gets those avp.toml names (`[check] secrets = ["NAME"]`), and their
    /// values read `***` in all AVP keeps and shows.
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Take an environment back to the position it served before the one it serves now, through its
    /// deploy as a promotion is, recorded as a rollback in its history. The full suite and `after`
    /// are not asked again, `approvers` are. Twice: back where the first rollback found it.
    Rollback {
        env: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// The people of a team server and their role on each repo. The first one added turns accounts
    /// on: from then on each request carries a person's token, the person on what it does is that
    /// token's, and their role says what they may do. A server without any works as before.
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// A person's tokens, what they put in AVP_TOKEN on a server with accounts: shown once, kept hashed.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Who the server takes you for, and your role on each repo.
    Whoami,
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add a person, with a first token for them (shown once, to hand over). The first person is an
    /// admin of the server.
    Add {
        name: String,
        #[arg(long)]
        email: Option<String>,
        /// The GitHub login that signs them in to AVP Live.
        #[arg(long)]
        github: Option<String>,
        /// Admin of the server: people, every repo, the workers.
        #[arg(long)]
        admin: bool,
    },
    /// The people, their roles and how many tokens work; with --repo, who has a role on that repo.
    List {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Take a person off the server: their tokens stop working, their roles go.
    Rm { name: String },
    /// Give a person a role on a repo: read (the line, AVP Live), write (open and land tasks), admin
    /// (guards, landing rule, environments, secrets, roles), or none.
    Role {
        name: String,
        role: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Make a person an admin of the server (people, every repo, the workers), or with --off no longer.
    Admin {
        name: String,
        #[arg(long)]
        off: bool,
    },
    /// Every change of people, tokens and roles: what, who, when.
    Log {
        #[arg(long, default_value_t = 0)]
        since: i64,
    },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// A new token, shown once: put it in AVP_TOKEN.
    Create {
        /// What it is for (a laptop, a CI).
        #[arg(long)]
        name: Option<String>,
        /// When it stops: 90d, 12h, a date, or never.
        #[arg(long, default_value = "never")]
        expires: String,
        /// Another person's (a server admin makes it).
        #[arg(long)]
        user: Option<String>,
    },
    /// The tokens that work: yours, or another person's (a server admin reads them).
    List {
        #[arg(long)]
        user: Option<String>,
    },
    /// Stop a token at once.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum OutboxCmd {
    /// Send the outbox now, in order.
    Retry,
    /// Take entries out by number, asking nothing. An open of a task of this machine's takes the
    /// task's entries with it; what a dropped write wrote stays in the folder, unsent.
    Drop {
        #[arg(required = true)]
        n: Vec<u64>,
    },
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Keep a secret's value, read from stdin, never from an argument (process lists and shell
    /// histories keep those): `avp secret set STRIPE_KEY < key.txt`, or typed unseen at a terminal.
    Set {
        name: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// The repo's secrets: names and when each was set, never a value.
    List {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Forget a secret: a step that names it goes red until it is set again.
    Rm {
        name: String,
        #[arg(long)]
        repo: Option<String>,
    },
}

fn actor() -> Actor {
    let person = std::env::var("AVP_PERSON").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "someone".into());
    // An agent that runs the CLI from its shell is named by what its runtime leaves in the
    // environment: Codex marks every shell it starts with CODEX_* (its home, its npm install),
    // the others with a variable of their own (in their source, October 2026), then AI_AGENT,
    // the variable agents begin to share.
    let set = |k: &str| std::env::var_os(k).is_some();
    let agent = std::env::var("AVP_AGENT").unwrap_or_else(|_| {
        if set("CLAUDECODE") { "claude-code".into() }
        else if ["CODEX_HOME", "CODEX_MANAGED_BY_NPM", "CODEX_SANDBOX", "CODEX_THREAD_ID"].iter().any(|k| set(k)) { "codex".into() }
        else if set("CURSOR_AGENT") { "cursor".into() }
        else if set("QWEN_CODE") { "qwen-code".into() }
        else if set("GEMINI_CLI") { "gemini-cli".into() }
        else if set("OPENCODE") { "opencode".into() }
        else if set("COPILOT_AGENT") || set("COPILOT_CLI") { "copilot".into() }
        else if set("CLINE_ACTIVE") { "cline".into() }
        else { std::env::var("AI_AGENT").ok().filter(|a| !a.is_empty()).unwrap_or_else(|| "human".into()) }
    });
    let session = std::env::var("AVP_SESSION").unwrap_or_else(|_| new_id("s"));
    Actor { person, agent, session }
}

fn print<T: serde::Serialize>(json: bool, v: &T, human: impl FnOnce(&T) -> String) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap());
    } else {
        println!("{}", human(v));
    }
}

/// What a directory versions: git's own answer when it is a git checkout (tracked files plus
/// untracked ones no ignore rule hides), the ignore rules alone otherwise.
fn versioned_files(dir: &Path) -> Result<std::collections::BTreeMap<String, (String, u32)>> {
    if dir.join(".git").exists() {
        let out = std::process::Command::new("git").args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).current_dir(dir).output();
        if let Ok(out) = out {
            if out.status.success() {
                let mut files = std::collections::BTreeMap::new();
                for raw in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                    let rel = String::from_utf8_lossy(raw).to_string();
                    if view::is_host_path(&rel) {
                        continue;
                    }
                    let full = dir.join(&rel);
                    let Ok(meta) = std::fs::metadata(&full) else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    let bytes = view::read_project(&full)?;
                    files.insert(rel, (avp::objects::hash_bytes(&bytes), view::mode_of(&meta)));
                }
                return Ok(files);
            }
        }
    }
    view::scan(dir, &Default::default(), &mut Default::default())
}

fn upload_dir(client: &Client, repo: &str, dir: &Path) -> Result<Vec<ImportEntry>> {
    let files = versioned_files(dir)?;
    let mut entries = Vec::with_capacity(files.len());
    for (path, (hash, mode)) in files {
        if !client.has_object(repo, &hash)? {
            client.put_object(repo, view::read_project(&dir.join(&path))?)?;
        }
        entries.push(ImportEntry { path, blob: hash, mode });
    }
    Ok(entries)
}

/// A step's log as `avp proof` prints it: when it started, then each line after the time since.
fn log_text(log: &serde_json::Value) -> String {
    let mut s = log["started"].as_i64().map(|t| format!("  started {} UTC", avp::blame::utc(t))).unwrap_or_default();
    let cut = &log["cut"];
    for (i, l) in log["lines"].as_array().into_iter().flatten().enumerate() {
        if cut["at"].as_u64() == Some(i as u64) {
            s.push_str(&format!("\n          … {} lines ({}) cut: the start and the end are kept", cut["lines"], human_bytes(cut["bytes"].as_u64().unwrap_or(0))));
        }
        let ms = l[0].as_u64().unwrap_or(0);
        s.push_str(&format!("\n{:>9}  {}", format!("{}:{:02}.{}", ms / 60_000, ms / 1000 % 60, ms / 100 % 10), l[1].as_str().unwrap_or("")));
    }
    s
}

/// What a step's artifacts hold, and how to get them.
fn artifacts_line(a: &serde_json::Value, id: &str, step: &str) -> String {
    let mut s = match a["error"].as_str() {
        Some(e) => format!("not packed ({e})"),
        None if a["files"].as_u64().unwrap_or(0) == 0 => "none made".into(),
        None => format!("{} ({}): `avp proof {id} --step {step:?} --artifacts <dir>`", file_count(a["files"].as_u64().unwrap_or(0)), human_bytes(a["bytes"].as_u64().unwrap_or(0))),
    };
    for (what, key) in [("missing", "missing"), ("left out", "left_out")] {
        let names: Vec<&str> = a[key].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
        if !names.is_empty() {
            s.push_str(&format!("; {what}: {}", names.join(", ")));
        }
    }
    s
}

fn file_count(n: u64) -> String {
    format!("{n} file{}", if n == 1 { "" } else { "s" })
}

fn human_bytes(n: u64) -> String {
    match n {
        0..1_000 => format!("{n} B"),
        1_000..1_000_000 => format!("{:.1} kB", n as f64 / 1e3),
        _ => format!("{:.1} MB", n as f64 / 1e6),
    }
}

/// Each step's pack, unpacked into `<into>/<step>/` by the machine's `tar`.
fn download_artifacts(client: &Client, repo: &str, id: &str, steps: &[(&Step, &serde_json::Value)], into: &Path) -> Result<Vec<serde_json::Value>> {
    let mut got = Vec::new();
    for (st, v) in steps {
        let Some(a) = v.get("artifacts").filter(|a| a["packed"].as_u64().unwrap_or(0) > 0) else { continue };
        let bytes = client.get_bytes(&format!("/repos/{repo}/proofs/{id}/artifacts?{}", avp::check::step_query(&st.name)))?;
        // The step's name as a folder: `web (node 20)` stays readable, nothing climbs out.
        let folder = st.name.replace(['/', '\\'], "-").trim_start_matches('.').to_string();
        let dir = into.join(if folder.is_empty() { "step" } else { &folder });
        std::fs::create_dir_all(&dir)?;
        let pack = std::env::temp_dir().join(format!("avp-artifacts-{}.tar.gz", ulid::Ulid::new()));
        std::fs::write(&pack, bytes)?;
        let out = std::process::Command::new("tar").arg("-xzf").arg(&pack).arg("-C").arg(&dir).output();
        let _ = std::fs::remove_file(&pack);
        let out = out.context("tar, which unpacks them")?;
        anyhow::ensure!(out.status.success(), "{}: tar: {}", st.name, String::from_utf8_lossy(&out.stderr).trim());
        got.push(serde_json::json!({ "step": st.name, "dir": dir, "files": a["files"], "bytes": a["bytes"] }));
    }
    Ok(got)
}

fn verdict_line(p: &Proof) -> String {
    let steps: Vec<String> = p.steps.iter().map(|s| format!("{} {} ({:.1}s)", if s.exit == 0 { "✓" } else { "✗" }, s.name, s.seconds)).collect();
    let mut s = format!("{:?} on position {}: {}", p.verdict, p.base_position, steps.join(", "));
    if p.verdict != Verdict::Green {
        for (f, h) in &p.blamed {
            s.push_str(&format!("\n  {f}{}", h.as_ref().map(|h| format!("  (held by {h})")).unwrap_or_default()));
        }
        if let Some(last) = p.steps.iter().find(|s| s.exit != 0) {
            let tail: Vec<&str> = avp::check::red_part(&last.output).lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect();
            s.push_str("\n  ---\n  ");
            s.push_str(&tail.join("\n  "));
        }
    }
    s
}

/// A position given by its number or by a tag's name (`avp promote production v1.2`).
fn position_of(client: &Client, repo: &str, at: &str) -> Result<i64> {
    if let Ok(n) = at.parse::<i64>() {
        return Ok(n);
    }
    let tags: Vec<avp::db::Tag> = client.get(&format!("/repos/{repo}/tags"))?;
    tags.into_iter().find(|t| t.name == at).map(|t| t.position).with_context(|| format!("no position nor tag {at}: `avp tags` lists the tags"))
}

/// The repo a command acts on: `--repo`, or the view this directory is.
/// The repo a command is about: named, or this view's, or AVP_REPO, or the one `avp up` made
/// from the git checkout this directory is in.
fn repo_of(dir: &Path, repo: Option<String>) -> Result<String> {
    if let Some(r) = repo.or_else(|| view::load(dir).ok().map(|s| s.repo)).or_else(|| std::env::var("AVP_REPO").ok()) {
        return Ok(r);
    }
    up::repo_name(dir).context("not in a view nor in a git checkout: pass --repo")
}

/// The view here, or, for a command that only reads the repo, an empty one on the repo of this
/// checkout (see `repo_of`).
fn view_or_repo(dir: &Path) -> Result<view::ViewState> {
    match view::load(dir) {
        Ok(s) => Ok(s),
        Err(_) => Ok(view::ViewState { repo: repo_of(dir, None)?, ..Default::default() }),
    }
}

/// `avp update`: the server's binary when this one is not the same file, written beside it, checked
/// against its hash and asked its version, then renamed over it. A process still running the old
/// binary keeps it; the next one starts the new.
fn update(client: &Client) -> Result<()> {
    let have = env!("CARGO_PKG_VERSION");
    let info: serde_json::Value = client.get("/client")?;
    let s = |k: &str| info[k].as_str().unwrap_or_default().to_string();
    let (want, platform, hash) = (s("version"), s("platform"), s("hash"));
    let exe = std::env::current_exe()?.canonicalize()?;
    if avp::objects::hash_bytes(&std::fs::read(&exe)?) == hash {
        println!("avp {have}, as the server");
        return Ok(());
    }
    if platform != avp::platform() {
        println!("avp {have}, the server {want} on {platform}: nothing to take for {}", avp::platform());
        return Ok(());
    }
    let bytes = client.get_bytes("/client/avp")?;
    anyhow::ensure!(avp::objects::hash_bytes(&bytes) == hash, "the binary the server sent is not the one it announced: this one stays");
    let new = exe.with_file_name(format!(".avp-update-{}", std::process::id()));
    std::fs::write(&new, bytes)?;
    #[cfg(unix)]
    std::fs::set_permissions(&new, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let answers = std::process::Command::new(&new).arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).trim() == format!("avp {want}"));
    if !answers.unwrap_or(false) {
        let _ = std::fs::remove_file(&new);
        anyhow::bail!("the server's binary does not answer avp {want}: this one stays");
    }
    std::fs::rename(&new, &exe)?;
    println!("avp {have} -> {want} ({})", &hash[..12]);
    Ok(())
}

/// A secret's value from stdin: piped (without the newline `echo` or a file ends it with), or typed
/// at a terminal, unseen.
fn secret_value(name: &str) -> Result<String> {
    use std::io::{IsTerminal, Read};
    let mut value = String::new();
    if std::io::stdin().is_terminal() {
        eprint!("{name} (not shown, Enter ends it): ");
        let _unseen = Unechoed::new();
        std::io::stdin().read_line(&mut value)?;
        eprintln!();
    } else {
        std::io::stdin().read_to_string(&mut value).context("the value on stdin must be text")?;
    }
    let value = value.strip_suffix('\n').map(|v| v.strip_suffix('\r').unwrap_or(v)).unwrap_or(&value);
    anyhow::ensure!(!value.is_empty(), "no value on stdin: `avp secret set {name} < file`, or `printf %s \"$VALUE\" | avp secret set {name}`");
    Ok(value.to_string())
}

/// The terminal's echo, off while this lives: a value typed is not shown.
struct Unechoed(#[cfg(unix)] Option<libc::termios>);

impl Unechoed {
    fn new() -> Self {
        #[cfg(unix)]
        unsafe {
            let mut was: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut was) != 0 {
                return Self(None);
            }
            let mut quiet = was;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
            Self(Some(was))
        }
        #[cfg(not(unix))]
        Self()
    }
}

impl Drop for Unechoed {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(was) = &self.0 {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, was);
            }
        }
    }
}

/// A land or a check that waits for the server's update says so, apart from its answer.
fn say_drain() -> std::sync::Arc<dyn Fn(&str) + Send + Sync> {
    std::sync::Arc::new(|line: &str| eprintln!("{line}"))
}

/// An answer given without the server: its first line says so. With --json, the JSON alone on
/// stdout and the text on stderr.
fn offline_said(json: bool, v: serde_json::Value, text: &str) {
    if json {
        eprintln!("{}", text.lines().next().unwrap_or_default());
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("{text}");
    }
}

/// `avp land` while the server does not answer: proven on this machine, and queued when green.
fn land_offline(server: &str, dir: &Path, state: &mut view::ViewState, json: bool) -> Result<()> {
    let notice = offline::notice_of(server, &state.repo);
    match offline::land(server, dir, state)? {
        offline::Landing::Queued(p, n) => offline_said(json, serde_json::json!({ "offline": true, "queued": n, "proof": p }), &format!("{notice}
green on this machine: {}
the land is {}: the server proves it again there, replayed on what landed meanwhile", verdict_line(&p), offline::queued_text(n).trim_start_matches("offline: "))),
        offline::Landing::Empty => offline_said(json, serde_json::json!({ "offline": true, "reason": "empty" }), &format!("{notice}
not landed: empty
  nothing to land: the task has no live write")),
        offline::Landing::Red(p) => {
            offline_said(json, serde_json::json!({ "offline": true, "reason": "red", "proof": p }), &format!("{notice}
not landed: red on this machine, nothing queued: fix it, then land again
  {}", verdict_line(&p)));
            std::process::exit(2);
        }
    }
    Ok(())
}

/// The time of day, UTC, that a follow loop puts before what it says.
fn clock() -> String {
    let t = now_ms() / 1000;
    format!("{:02}:{:02}:{:02}Z", t / 3600 % 24, t / 60 % 60, t % 60)
}

/// Where views made for imported work go: --views, else $AVP_VIEWS, else ~/avp.
fn views_dir(views: Option<PathBuf>) -> PathBuf {
    views.or_else(|| std::env::var("AVP_VIEWS").ok().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("avp"))
}

fn main() -> Result<()> {
    avp::legacy_env();
    let cli = Cli::parse();
    let dir = cli.dir.clone().unwrap_or(std::env::current_dir()?);
    let json = cli.json;

    if let Cmd::Serve { listen, data } = &cli.cmd {
        let engine = engine::Engine::open(data)?;
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(server::serve(engine, listen));
    }

    let client = Client::new(&cli.server);
    // A view's own server wins over the default, so one view talks to one server.
    let client = match view::load(&dir) {
        Ok(s) if cli.server == "http://127.0.0.1:7117" && !s.server.is_empty() => Client::new(&s.server),
        _ => client,
    };
    // The view's outbox goes first: what was queued while the server did not answer reaches it, in
    // order, before this command adds to it. A server that still does not answer is known then, and
    // what this command changes is queued behind.
    let link = match cli.cmd {
        Cmd::Serve { .. } | Cmd::Mcp | Cmd::Worker { .. } | Cmd::Up { .. } | Cmd::Down { .. } | Cmd::Agents | Cmd::Migrate { .. } | Cmd::Update | Cmd::Mirror { .. } | Cmd::ImportPr { .. } | Cmd::ImportGit { .. } | Cmd::ImportCi { .. } | Cmd::Init { .. } | Cmd::Attach { .. } | Cmd::Clone { .. } | Cmd::Align { .. } => offline::Link::default(),
        // The way out of a stuck outbox: `drop` must not send first what it drops.
        Cmd::Outbox { .. } => offline::Link::default(),
        _ => offline::flush(&client, &dir, true),
    };
    for l in &link.said {
        if json { eprintln!("{l}") } else { println!("{l}") }
    }
    let down = link.down;

    match cli.cmd {
        Cmd::Serve { .. } => unreachable!(),
        Cmd::Up { name, branch, no_push, history, no_checks, mirror } => up::up(&client, &dir, up::Up { name, branch, push: !no_push, history, checks: !no_checks, mirror })?,
        Cmd::Down { name } => up::down(&dir, name)?,
        Cmd::Agents => up::agents(&dir, json)?,
        Cmd::Migrate { name, yes } => {
            let r = avp::migrate::migrate(&client, &dir, avp::migrate::Migrate { name, yes })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("{}", avp::migrate::text(&r));
            }
            // Red on this machine: not done yet, as a red `avp land` says.
            if r.proof.verdict == "red" || matches!(r.checks.state.as_str(), "invalid" | "refused") {
                std::process::exit(2);
            }
        }
        Cmd::Mcp => {
            // Started by an agent that passes no AVP_SERVER (an editor opened from the Dock): the
            // checkout's configs never hold it, `avp up` kept it for this checkout.
            let client = match std::env::var_os("AVP_SERVER") {
                None if cli.server == up::LOCAL && view::load(&dir).is_err() => up::adopt(&dir).map(|s| Client::new(&s)).unwrap_or(client),
                // A view `avp clone` made: its own server, with the token kept for it.
                None if cli.server == up::LOCAL && up::adopt(&dir).is_some() => Client::new(&client.base),
                _ => client,
            };
            mcp::Mcp::new(&dir, client).serve()?
        }
        Cmd::Update => update(&client)?,
        Cmd::Wait { task, path, timeout } => {
            let mut state = view_or_repo(&dir)?;
            let w = avp::wait::wait(&client, &state.repo, state.task.as_deref(), &task, &path, std::time::Duration::from_secs(timeout))?;
            let folder = match w.state.as_str() {
                // Ready, with a task open here: what was waited for comes into the folder, to build on.
                "ready" if state.task.is_some() => Some(view::catch_up(&client, &dir, &mut state)?),
                // Landed with others: the folder becomes the line.
                "landed" => {
                    let tree: Tree = client.get(&format!("/repos/{}/tree/{}", state.repo, w.position))?;
                    state.task = None;
                    view::materialize(&client, &tree, &dir, &mut state)?;
                    Some(format!("this folder holds position {} of the line", w.position))
                }
                _ => None,
            };
            let mut shown = serde_json::to_value(&w)?;
            shown["folder"] = serde_json::json!(folder);
            print(json, &shown, |_| folder.as_ref().map(|f| format!("{}\n{f}", w.text)).unwrap_or_else(|| w.text.clone()));
        }
        Cmd::Init { name, policy } => {
            let repo: Repo = client.post("/repos", &serde_json::json!({ "name": name, "policy": policy }))?;
            let entries = upload_dir(&client, &repo.id, &dir)?;
            let n = entries.len();
            let entry: LineEntry = client.post(&format!("/repos/{}/import", repo.id), &serde_json::json!({ "entries": entries, "note": "imported" }))?;
            let mut state = view::ViewState { server: client.base.clone(), repo: repo.id.clone(), task: None, known: Default::default(), base: Some((entry.position, entry.tree.clone())), ..Default::default() };
            let tree = client.get::<Tree>(&format!("/repos/{}/tree/{}", repo.id, entry.position))?;
            state.known = tree.iter().map(|(p, e)| (p.clone(), (e.blob.clone(), e.mode))).collect();
            view::save(&dir, &state)?;
            print(json, &repo, |r| format!("repo {} ({}) created, position {} holds {} files. This directory is now a view.", r.name, r.id, entry.position, n));
        }
        Cmd::Attach { repo } => {
            let r: Repo = client.get(&format!("/repos/{repo}"))?;
            let head: LineEntry = client.get(&format!("/repos/{}/head", r.id))?;
            let tree: Tree = client.get(&format!("/repos/{}/tree/{}", r.id, head.position))?;
            let state = view::ViewState { server: client.base.clone(), repo: r.id.clone(), task: None, known: tree.iter().map(|(p, e)| (p.clone(), (e.blob.clone(), e.mode))).collect(), base: Some((head.position, head.tree.clone())), ..Default::default() };
            view::save(&dir, &state)?;
            println!("attached to {} at position {} ({} files known)", r.name, head.position, tree.len());
        }
        Cmd::ImportGit { name, history, policy, follow, remote, every, views } => {
            if client.get::<Repo>(&format!("/repos/{name}")).is_err() {
                let (repo, last) = gitbridge::import_git(&client, &dir, &name, &policy, history)?;
                let tree: Tree = client.get(&format!("/repos/{}/tree/{last}", repo.id))?;
                let state = view::ViewState { server: client.base.clone(), repo: repo.id.clone(), task: None, known: tree.iter().map(|(p, e)| (p.clone(), (e.blob.clone(), e.mode))).collect(), ..Default::default() };
                view::save(&dir, &state)?;
                println!("repo {} ({}) created, positions 0..{last} from git history. This checkout is now a view.", repo.name, repo.id);
                if follow.is_none() {
                    return Ok(());
                }
            }
            let views = views_dir(views);
            let open = gitbridge::open_over_http(&client);
            let mut known = gitbridge::Known::default();
            let mut last = String::new();
            loop {
                let rev = match &follow {
                    Some(branch) => gitbridge::fetch_branch(&dir, &remote, branch),
                    None => Ok("HEAD".to_string()),
                };
                let got = rev.and_then(|rev| gitbridge::follow_git(&client, &dir, &name, &rev, Some(&views), &mut known, &open));
                let fs = match got {
                    Ok(fs) => fs,
                    Err(e) if follow.is_some() => {
                        let line = format!("error: {e:#}");
                        if line != last {
                            println!("{} {line}", clock());
                            last = line;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(every));
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                let lines: Vec<String> = fs.iter().map(|f| format!("{} {}: {}", &f.sha[..12], f.subject, f.outcome)).collect();
                let Some(_) = &follow else {
                    if json {
                        let all: Vec<_> = fs.iter().map(|f| serde_json::json!({ "commit": f.sha, "subject": f.subject, "task": f.task, "outcome": f.outcome, "landed": f.landed, "open": f.open })).collect();
                        println!("{}", serde_json::to_string_pretty(&all)?);
                    } else if lines.is_empty() {
                        println!("nothing to bring in: the line has every commit of HEAD");
                    } else {
                        println!("{}", lines.join("\n"));
                    }
                    // As `avp land` says a red proof: what stays open needs someone.
                    if fs.iter().any(|f| f.open) {
                        std::process::exit(2);
                    }
                    break;
                };
                // following: what changed, not every look
                let said = lines.join("\n");
                if !said.is_empty() && said != last {
                    for l in &lines {
                        println!("{} {l}", clock());
                    }
                }
                last = said;
                std::thread::sleep(std::time::Duration::from_secs(every));
            }
        }
        Cmd::ImportCi { write } => {
            let top = avp::importci::root(&dir).with_context(|| format!("no CI in {} nor above it: avp import-ci reads .github/workflows/*.yml and .gitlab-ci.yml", dir.display()))?;
            let imported = avp::importci::import(&top)?;
            let written = if write { Some(avp::importci::write(&top, &imported)?) } else { None };
            let file = written.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string());
            let next = match file.as_deref() {
                None => "Write it: `avp import-ci --write` (avp.toml, or avp.imported.toml beside the one there is).".to_string(),
                Some("avp.toml") => "Read it, then land it like any change: its proof runs the checks it names, on the head of the line.".to_string(),
                Some(f) => format!("avp.toml is there and stays as it is: carry over from {f} what you want, then land avp.toml like any change."),
            };
            if json {
                // The report prints the `avp secret set` lines; an agent gets the names, and the way to ask for them.
                let next = if imported.secrets.is_empty() { next } else { format!("{next} A person sets each of `secrets` once: `avp secret set <name>`, the value on stdin.") };
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "sources": imported.sources, "toml": imported.toml, "not_translated": imported.not_translated, "secrets": imported.secrets, "written": written.as_ref().map(|p| p.display().to_string()), "next": next }))?);
            } else if let Some(p) = &written {
                println!("{} written from {}.", p.display(), imported.sources.join(", "));
                print!("{}", avp::importci::report(&imported));
                println!("{next}");
            } else {
                // Comments after the file: the whole output is an avp.toml as it stands.
                println!("{}", imported.toml);
                print!("{}", avp::importci::report(&imported));
                println!("# {next}");
            }
        }
        Cmd::ImportPr { mut numbers, repo, remote, land, views, follow, every } => {
            anyhow::ensure!(follow || !numbers.is_empty(), "name the pull requests to bring in, or --follow to bring in each open one, now and as they come");
            numbers.sort();
            let views = views_dir(views);
            if follow {
                let mut last = String::new();
                loop {
                    let said = match gitbridge::follow_prs(&client, &dir, &repo, &remote, Some(&views), land) {
                        Ok(prs) => prs.iter().map(|x| format!("#{}  {}  {}", x.number, x.outcome, x.title)).collect::<Vec<_>>().join("\n"),
                        Err(e) => format!("error: {e:#}"),
                    };
                    if !said.is_empty() && said != last {
                        for l in said.lines() {
                            println!("{} {l}", clock());
                        }
                    }
                    last = said;
                    std::thread::sleep(std::time::Duration::from_secs(every));
                }
            }
            for n in numbers {
                match gitbridge::import_pr(&client, &dir, &repo, n, &remote, Some(&views), land) {
                    Ok(x) => println!("#{}  {}  {}", x.number, x.outcome, x.title),
                    Err(e) => println!("#{n}  error: {e:#}"),
                }
            }
        }
        Cmd::Worker { data } => {
            avp::worker::work(&client, &data)?;
        }
        Cmd::Mirror { repo, branch, remote, committer, follow } => {
            let mut last = String::new();
            loop {
                let line = match gitbridge::mirror(&client, &dir, &repo, &branch, remote.as_deref(), &committer) {
                    Ok(x) => format!(
                        "{} new commit(s); position {} is {}, identical to the line{}{}",
                        x.written,
                        x.head,
                        &x.sha[..12.min(x.sha.len())],
                        if x.pushed { format!(", pushed to {} {branch}", remote.clone().unwrap_or_default()) } else { String::new() },
                        x.tags.iter().map(|t| format!("; {t}")).collect::<String>()
                    ),
                    Err(e) if follow.is_some() => format!("mirror failed: {e:#}"),
                    Err(e) => return Err(e),
                };
                let Some(secs) = follow else {
                    println!("{line}");
                    break;
                };
                // following: one line per change of state, not one per round
                if line != last {
                    println!("{} {line}", clock());
                    last = line;
                }
                std::thread::sleep(std::time::Duration::from_secs(secs));
            }
        }
        Cmd::Open { goal, scope, repo, fresh, on } => {
            // The clean git checkout this runs in, when the host gives each agent its own
            // (AVP_VIEW_IN_CHECKOUT): the task is laid out there, with its install, git on top.
            let in_place = avp::checkout::in_place_target(&dir);
            if let Some(top) = avp::checkout::refused_checkout(&dir) {
                anyhow::bail!("{} holds changes git has not committed: a task is never laid over them. Commit or stash them, or open in a new folder with -C", top.display());
            }
            let dir = in_place.clone().unwrap_or(dir);
            let mut state = view::load(&dir).unwrap_or_default();
            // Offline: a task of this machine's, on the position the folder holds; it opens on the
            // server, under the server's id, once the outbox reaches it.
            if down || !client.reachable() {
                anyhow::ensure!(on.is_none(), "{}\na stacked task opens on the server, which holds the writes of the task under it: open it once the server answers", offline::notice_of(&client.base, &state.repo));
                if let Some(r) = repo.clone().filter(|r| !r.is_empty() && state.repo.is_empty()) {
                    state.repo = r;
                }
                anyhow::ensure!(!state.repo.is_empty(), "no repo: pass --repo, or run in a view");
                let said = offline::open(&client.base, &dir, &mut state, goal, scope, actor(), std::env::var("AVP_TRANSCRIPT").ok())?;
                offline_said(json, serde_json::json!({ "offline": true, "task": state.task, "base": state.base.as_ref().map(|b| b.0) }), &said);
                return Ok(());
            }
            // AVP_REPO is read by clap, so a folder that is not a view yet takes the repo of the
            // environment, as the MCP server does (agents were told the repo is set there, and
            // `open` refused it: 25/10 bench).
            if let Some(r) = repo.filter(|r| !r.is_empty()) {
                let repo: Repo = client.get(&format!("/repos/{r}"))?;
                state.repo = repo.id;
                state.server = client.base.clone();
            }
            anyhow::ensure!(!state.repo.is_empty(), "no repo: pass --repo, or run in a view");
            if let Some(t) = &state.task {
                let st: TaskStatus = client.get(&format!("/repos/{}/tasks/{t}", state.repo))?;
                anyhow::ensure!(st.task.state != TaskState::Open, "task {t} is still open here: land or abandon it first");
            }
            // Changes made here and never sent: the new task takes them, merged with what the line
            // changed meanwhile, instead of the head's layout writing over them.
            let unsent = if fresh || state.known.is_empty() { vec![] } else { view::unsent(&dir, &mut state)? };
            let on = on.map(|o| o.trim().to_string()).filter(|o| !o.is_empty());
            anyhow::ensure!(on.iter().all(|o| o.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')), "--on takes a task id, or the start of one (`avp tasks`)");
            let at = on.as_ref().map(|o| format!("?on={o}")).unwrap_or_default();
            let opened: avp::blame::Opened = client.post(&format!("/repos/{}/tasks{at}", state.repo), &OpenTask { goal, actor: actor(), scope, transcript: std::env::var("AVP_TRANSCRIPT").ok() })?;
            let task = &opened.task;
            state.task = Some(task.id.clone());
            state.base = Some((task.base_position, task.base_tree.clone()));
            if fresh {
                state.known.clear();
            }
            // A stacked task stands on the tree of the task under it, not on a position.
            let tree: Tree = match &task.on {
                Some(_) => client.get(&format!("/repos/{}/tasks/{}/tree", state.repo, task.id))?,
                None => client.get(&format!("/repos/{}/tree/{}", state.repo, task.base_position))?,
            };
            let stack = if task.on.is_some() { client.get::<TaskStatus>(&format!("/repos/{}/tasks/{}", state.repo, task.id))?.stack } else { None };
            if in_place.is_some() {
                view::reset(&dir, &tree, &mut state)?;
            }
            // Files the checkout this runs from, or another view of the repo, already holds are
            // copied, not downloaded.
            let mut seeds: Vec<PathBuf> = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output().ok().filter(|o| o.status.success()).map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())).into_iter().collect();
            seeds.extend(std::fs::read_dir(avp::up::views()).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| view::load(p).is_ok_and(|v| v.repo == state.repo)));
            if in_place.is_some() {
                seeds.extend(avp::checkout::worktrees(&dir));
            }
            seeds.retain(|s| *s != dir);
            let n = view::lay_out(&client, &tree, &dir, &mut state, &seeds)?;
            // The install of a folder whose lockfiles are the same, cloned: the install has nothing to do.
            let deps = view::reuse_deps(&tree, &dir, &seeds);
            if in_place.is_some() {
                avp::checkout::align(&client, &dir, &state.repo, task.base_position);
            }
            let carried = if unsent.is_empty() {
                None
            } else {
                let c = view::carry(&client, &dir, &state.repo, &tree, unsent)?;
                view::sync(&client, &dir, &mut state)?;
                Some(c)
            };
            offline::refresh(&client, &state.repo, false);
            let open: Vec<Task> = client.get(&format!("/repos/{}/tasks?state=open", state.repo)).unwrap_or_default();
            let reverted: Vec<Event> = client.get(&format!("/repos/{}/tasks/{}/reverted", state.repo, task.id)).unwrap_or_default();
            let mut shown = serde_json::to_value(&opened)?;
            shown["dependencies"] = serde_json::json!(deps);
            print(json, &shown, |_| {
                let (o, t) = (&opened, &opened.task);
                let on = match (&t.on, &stack) {
                    (Some(p), Some(_)) => format!("on task {p}'s work as it is now, on position {}", t.base_position),
                    _ => format!("on position {}", t.base_position),
                };
                let mut s = format!("task {} opened {on} ({} files laid out). Write, then `avp land`: it proves, then lands (`avp check` shows the proof alone).", t.id, n);
                if let Some(w) = &stack {
                    s.push_str(&format!("\n  {w}"));
                }
                for d in &deps {
                    s.push_str(&format!("\n  {} cloned from {} (same {}): the install has nothing to do", d.folders.join(", "), d.from.display(), d.lockfile));
                }
                if let Some(c) = &carried {
                    s.push_str(&format!("\n  your unsent changes are in it: {} as they were, {} merged with the line", c.kept, c.merged));
                    for p in &c.conflicts {
                        s.push_str(&format!("\n  conflict, settle the markers: {p}"));
                    }
                }
                for o in &t.similar {
                    s.push_str(&format!("\n  close to an open task: {o}"));
                }
                if let Some(h) = avp::wait::open_hint(&open, &t.id) {
                    s.push_str(&format!("\n  {}", h.replace('\n', "\n  ")));
                }
                for e in &reverted {
                    s.push_str(&format!("\n  {}", engine::revert_hint(e)));
                }
                // What the line remembers of the scope: read it before exploring the code.
                let c = avp::blame::context_text(&o.context, now_ms());
                if !c.is_empty() {
                    s.push_str(&format!("\n  {}", c.replace('\n', "\n  ")));
                }
                s
            });
        }
        Cmd::Sync => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone();
            let r = match offline::ask(down, task.as_deref(), || view::sync(&client, &dir, &mut state)) {
                Err(e) if unreachable(&e) => {
                    let (lines, n) = offline::sync(&client.base, &dir, &mut state)?;
                    let text = if lines.is_empty() { format!("{}
nothing new to queue ({n} waiting)", offline::notice_of(&client.base, &state.repo)) } else { format!("{}
{}", lines.join("\n"), offline::queued_text(n)) };
                    offline_said(json, serde_json::json!({ "offline": true, "writes": lines, "waiting": n }), &text);
                    return Ok(());
                }
                r => r?,
            };
            for w in r.puts.iter().chain(r.deletes.iter()) {
                println!("{} {} [{}{}]", if w.write.kind == WriteKind::Delete { "-" } else { "+" }, w.write.path, w.lease, w.previous_holder.as_ref().map(|h| format!(" from {h}")).unwrap_or_default());
            }
            if r.puts.is_empty() && r.deletes.is_empty() {
                println!("nothing to send");
            }
            // First writes outside the scope: what the line remembers of those paths.
            let c = avp::history::first_writes(&client, &state.repo, state.task.as_deref().unwrap_or_default(), &avp::history::outside_scope(r.puts.iter().chain(r.deletes.iter())));
            if !c.is_empty() {
                println!("{c}");
            }
        }
        Cmd::Add { paths } => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            let mut sent = Vec::new();
            for p in paths.clone() {
                let full = dir.join(&p);
                let meta = std::fs::metadata(&full).with_context(|| format!("{p} is not a file here"))?;
                let bytes = view::read_project(&full)?;
                let mode = view::mode_of(&meta);
                let blob = match offline::ask(down, Some(&task), || client.put_object(&state.repo, bytes.clone())) {
                    Err(e) if unreachable(&e) => {
                        // Those sent already are sent again: the server takes a same write as unchanged.
                        let (lines, n) = offline::add_paths(&client.base, &dir, &mut state, &paths)?;
                        offline_said(json, serde_json::json!({ "offline": true, "writes": lines, "waiting": n }), &format!("{}
{}", lines.join("\n"), offline::queued_text(n)));
                        return Ok(());
                    }
                    r => r?,
                };
                let out: WriteOutcome = client.post(&format!("/repos/{}/tasks/{task}/writes", state.repo), &PutWrite { path: p.clone(), kind: WriteKind::Put, blob: Some(blob), mode })?;
                if out.lease != "refused" {
                    state.known.insert(p.clone(), (avp::objects::hash_bytes(&bytes), mode));
                }
                println!("+ {p} [{}]", out.lease);
                sent.push(out);
            }
            view::save(&dir, &state)?;
            let c = avp::history::first_writes(&client, &state.repo, &task, &avp::history::outside_scope(&sent));
            if !c.is_empty() {
                println!("{c}");
            }
        }
        Cmd::Watch => {
            let mut state = view::load(&dir)?;
            let (tx, rx) = std::sync::mpsc::channel();
            let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    // AVP's own state and git's: not the work (the outbox written would wake it again).
                    let ours = |p: &Path| p.components().any(|c| matches!(c.as_os_str().to_str(), Some(".avp" | ".trame" | ".git")));
                    if ev.paths.iter().any(|p| !ours(p)) {
                        let _ = tx.send(ev);
                    }
                }
            })?;
            notify::Watcher::watch(&mut w, &dir, notify::RecursiveMode::Recursive)?;
            eprintln!("watching {} for task {}", dir.display(), state.task.clone().unwrap_or_default());
            let mut down = down;
            loop {
                // Every few seconds without a save, the outbox goes to the server once it answers again.
                match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                    Ok(_) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) if offline::pending(&dir) > 0 => {
                        let link = offline::flush(&client, &dir, true);
                        link.said.iter().for_each(|l| eprintln!("{l}"));
                        down = link.down;
                        state = view::load(&dir)?;
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(e) => return Err(e.into()),
                }
                while rx.recv_timeout(std::time::Duration::from_millis(150)).is_ok() {}
                // Queued writes go before this one: never a newer version sent before an older.
                if down || offline::pending(&dir) > 0 {
                    let link = offline::flush(&client, &dir, true);
                    link.said.iter().for_each(|l| eprintln!("{l}"));
                    down = link.down;
                    state = view::load(&dir)?;
                }
                let task = state.task.clone();
                match offline::ask(down, task.as_deref(), || view::sync(&client, &dir, &mut state)) {
                    Ok(r) => {
                        for w in r.puts.iter().chain(r.deletes.iter()) {
                            eprintln!("{} {} [{}]", if w.write.kind == WriteKind::Delete { "-" } else { "+" }, w.write.path, w.lease);
                        }
                    }
                    Err(e) if unreachable(&e) => match offline::sync(&client.base, &dir, &mut state) {
                        Ok((lines, n)) if !lines.is_empty() => eprintln!("{}\n{}", lines.join("\n"), offline::queued_text(n)),
                        Ok(_) => {}
                        Err(e) => eprintln!("sync: {e:#}"),
                    },
                    Err(e) => eprintln!("sync: {e:#}"),
                }
            }
        }
        Cmd::Status => {
            let state = view::load(&dir)?;
            let asked = match state.task.as_deref() {
                Some(task) => offline::ask(down, Some(task), || client.get::<TaskStatus>(&format!("/repos/{}/tasks/{task}", state.repo))),
                // No task here: online or not, the outbox, its last flush.
                None => Err(offline::down()),
            };
            let st = match asked {
                Err(e) if unreachable(&e) => {
                    // An outbox stuck on a refused entry is not the server gone.
                    let (v, text) = offline::status(&client.base, &dir, &state, (!down || link.stuck) && client.reachable())?;
                    offline_said(json, v, &text);
                    return Ok(());
                }
                r => r?,
            };
            offline::refresh(&client, &state.repo, false);
            let outbox = offline::outbox_text(&dir);
            // A server from before save points has none to give.
            let saves: Vec<engine::Save> = client.get(&format!("/repos/{}/tasks/{}/saves", state.repo, st.task.id)).unwrap_or_default();
            let mut shown = serde_json::to_value(&st)?;
            shown["outbox"] = serde_json::json!(offline::outbox(&dir).map(|o| o.queued.len()).unwrap_or_default());
            shown["saves"] = serde_json::to_value(&saves)?;
            print(json, &shown, |_| {
                let s = &st;
                let live = engine::Engine::effective_writes(&s.writes);
                let mut out = format!("task {} ({:?}) on position {} (head {}): {}\n{} live writes, {} leases", s.task.id, s.task.state, s.task.base_position, s.head_position, s.task.goal, live.len(), s.leases.len());
                if let Some(at) = s.task.landed_position {
                    out.push_str(&format!("\nlanded at position {at}{}", avp::wait::together(&s.with)));
                }
                if let Some(w) = &s.stack {
                    out.push_str(&format!("\n{w}"));
                }
                for (p, w) in &live {
                    out.push_str(&format!("\n  {} {p} (seq {})", match w.kind { WriteKind::Put => "+", WriteKind::Delete => "-", WriteKind::Conflict => "!" }, w.seq));
                }
                if let Some(p) = &s.last_proof {
                    out.push_str(&format!("\nlast proof: {}", verdict_line(p)));
                }
                if !saves.is_empty() {
                    out.push_str(&format!("\nsave points:\n{}", engine::saves_text(&saves)));
                }
                out.push_str(&format!("\nonline; {outbox}"));
                out
            });
        }
        Cmd::Outbox { cmd } => {
            let (v, text) = match cmd {
                None => offline::list(&dir)?,
                Some(OutboxCmd::Retry) => offline::retry(&client, &dir)?,
                Some(OutboxCmd::Drop { n }) => offline::drop_entries(&dir, &n)?,
            };
            print(json, &v, |_| text);
        }
        Cmd::Claim { paths } => {
            let state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            let r: Vec<(String, Option<String>)> = match offline::ask(down, Some(&task), || client.post(&format!("/repos/{}/tasks/{task}/claim", state.repo), &serde_json::json!({ "paths": paths }))) {
                Err(e) if unreachable(&e) => {
                    let n = offline::queue(&dir, &state, Op::Claim { paths })?;
                    offline_said(json, serde_json::json!({ "offline": true, "waiting": n }), &offline::queued_text(n));
                    return Ok(());
                }
                r => r?,
            };
            for (p, h) in &r {
                println!("{p}: {}", h.as_ref().map(|h| format!("taken from {h}")).unwrap_or_else(|| "taken".into()));
            }
            // About to write outside the scope: what the line remembers of those paths.
            let c = avp::history::first_writes(&client, &state.repo, &task, &r.into_iter().map(|(p, _)| p).collect::<Vec<_>>());
            if !c.is_empty() {
                println!("{c}");
            }
        }
        Cmd::Release { paths } => {
            let state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            let paths = if paths.is_empty() { None } else { Some(paths) };
            let r: serde_json::Value = match offline::ask(down, Some(&task), || client.post(&format!("/repos/{}/tasks/{task}/release", state.repo), &serde_json::json!({ "paths": paths }))) {
                Err(e) if unreachable(&e) => {
                    let n = offline::queue(&dir, &state, Op::Release { paths })?;
                    offline_said(json, serde_json::json!({ "offline": true, "waiting": n }), &offline::queued_text(n));
                    return Ok(());
                }
                r => r?,
            };
            println!("released {}", r["released"]);
        }
        Cmd::Check => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone();
            let synced = offline::ask(down, task.as_deref(), || view::sync(&client, &dir, &mut state));
            let task = state.task.clone().context("no task open here")?;
            let p: Proof = match synced.and_then(|_| avp::mcp::through_drain(&client, &task, say_drain(), || client.check(&state.repo, &task))) {
                // Offline: avp.toml's steps on this machine, as the server runs them.
                Err(e) if unreachable(&e) => {
                    offline::sync(&client.base, &dir, &mut state)?;
                    let p = offline::prove(&client.base, &dir, &state)?;
                    let text = format!("{}
proven on this machine (the server proves it again when the land reaches it): {}", offline::notice_of(&client.base, &state.repo), verdict_line(&p));
                    offline_said(json, serde_json::to_value(&p)?, &text);
                    if p.verdict != Verdict::Green {
                        std::process::exit(1);
                    }
                    return Ok(());
                }
                r => r?,
            };
            print(json, &p, verdict_line);
            if p.verdict != Verdict::Green {
                std::process::exit(1);
            }
        }
        Cmd::Land { stack, with } => {
            let mut state = view::load(&dir)?;
            // The outbox just landed what was here, or holds its land still: said above, or now.
            if state.task.is_none() && (!link.said.is_empty() || offline::pending(&dir) > 0) {
                if link.said.is_empty() {
                    println!("nothing to land here: {}", offline::outbox_text(&dir));
                }
                return Ok(());
            }
            let task = state.task.clone().context("no task open here")?;
            // Landed already: a person approved it, and the approval landed it.
            let st: TaskStatus = match offline::ask(down, Some(&task), || client.get(&format!("/repos/{}/tasks/{task}", state.repo))) {
                Err(e) if unreachable(&e) => return land_offline(&client.base, &dir, &mut state, json),
                r => r?,
            };
            if st.task.state == TaskState::Landed {
                let position = st.task.landed_position.unwrap_or(st.head_position);
                println!("landed at position {position}{}{}", avp::wait::together(&st.with), st.approval.as_ref().map(|a| format!(", approved by {}", a.person)).unwrap_or_default());
                let tree: Tree = client.get(&format!("/repos/{}/tree/{position}", state.repo))?;
                state.task = None;
                view::materialize(&client, &tree, &dir, &mut state)?;
                return Ok(());
            }
            let landed = view::sync(&client, &dir, &mut state).and_then(|_| {
                avp::mcp::through_drain(&client, &task, say_drain(), || if with.is_empty() { client.land_as(&state.repo, &task, stack) } else { client.land_with(&state.repo, &task, &with) })
            });
            // The server went away during the proof: proven here, and queued.
            let landed = match landed {
                Err(e) if unreachable(&e) => return land_offline(&client.base, &dir, &mut state, json),
                r => r?,
            };
            match landed {
                Ok(l) => {
                    print(json, &l, |l| {
                        let under: String = l.stack.iter().map(|b| format!("task {} landed at position {}\n", b.task, b.position)).collect();
                        format!("{under}landed at position {}{}{} (proof {})", l.position, avp::wait::together(&l.with), if l.replayed { ", replayed on the moved line" } else { "" }, l.proof)
                    });
                    // The view becomes the line: teammates' landed files arrive here too.
                    let tree: Tree = client.get(&format!("/repos/{}/tree/{}", state.repo, l.position))?;
                    state.task = None;
                    state.base = Some((l.position, l.tree.clone()));
                    offline::refresh(&client, &state.repo, true);
                    view::materialize(&client, &tree, &dir, &mut state)?;
                    // A view in its agent's git checkout: git follows the landed position once the
                    // mirror wrote its commit, from a process of its own so the landing returns now.
                    if avp::checkout::is_checkout_view(&dir) {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).arg("-C").arg(&dir).args(["align", &l.position.to_string()])
                                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
                        }
                    }
                }
                Err(n) => {
                    print(json, &n, |n| {
                        let mut s: String = n.stack.iter().map(|b| format!("task {} landed at position {}\n", b.task, b.position)).collect();
                        s.push_str(&format!("not landed: {}", n.reason));
                        if let Some(note) = &n.note {
                            s.push_str(&format!("\n  {note}"));
                        }
                        if !n.conflicts.is_empty() && !with.is_empty() {
                            s.push_str(&format!("\n  conflicts: {}", n.conflicts.join(", ")));
                        } else if !n.conflicts.is_empty() {
                            s.push_str("\n  conflicts (markers written, settle them, then land again):");
                            for c in &n.conflicts {
                                s.push_str(&format!("\n    {c}"));
                            }
                        }
                        if let Some(p) = &n.proof {
                            s.push_str(&format!("\n  {}", verdict_line(p)));
                        }
                        if !n.waits_for.is_empty() && n.note.is_none() {
                            s.push_str(&format!("\n  waits for: {}", n.waits_for.join(", ")));
                        }
                        if n.reason == "red" && with.is_empty() {
                            if let Some(h) = avp::wait::others_hint(&client, &state.repo, &task, n.proof.as_ref()) {
                                s.push_str(&format!("\n  {}", h.replace('\n', "\n  ")));
                            }
                        }
                        if n.reason == "approval" {
                            if !n.guarded.is_empty() {
                                s.push_str(&format!("\n  guarded paths: {}", n.guarded.join(", ")));
                            }
                            s.push_str("\n  green; waiting for a person's `avp approve <task>`");
                        }
                        if n.reason == "empty" {
                            s.push_str("\n  nothing to land: the task has no live write");
                        }
                        if n.reason == "already" {
                            s.push_str("\n  nothing to land: the line already holds these changes; `avp abandon`");
                        }
                        if let Some(w) = &n.word {
                            s.push_str(&format!("\n  refused by {}: {}", w.person, w.reason));
                        }
                        s
                    });
                    // Landed with others, a conflict changes no task: no markers to lay out.
                    if n.reason == "conflict" && with.is_empty() {
                        let tree: Tree = client.get(&format!("/repos/{}/tasks/{task}/tree", state.repo))?;
                        view::materialize(&client, &tree, &dir, &mut state)?;
                    }
                    std::process::exit(2);
                }
            }
        }
        Cmd::Undo { save: Some(save), .. } => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            // What the folder holds goes to the journal first: going to a save loses nothing.
            let r: engine::Restored = match offline::ask(down, Some(&task), || {
                view::sync(&client, &dir, &mut state)?;
                client.post(&format!("/repos/{}/tasks/{task}/restore", state.repo), &serde_json::json!({ "to": save }))
            }) {
                Err(e) if unreachable(&e) => anyhow::bail!("{}\ngoing to a save point needs the server, which keeps them: `avp undo --path <file>` and `avp undo` work offline", offline::notice_of(&client.base, &state.repo)),
                r => r?,
            };
            let tree: Tree = client.get(&format!("/repos/{}/tasks/{task}/tree", state.repo))?;
            let n = view::materialize(&client, &tree, &dir, &mut state)?;
            let mut shown = serde_json::to_value(&r)?;
            shown["laid_out"] = serde_json::json!(n);
            print(json, &shown, |_| {
                let mut s = format!("at save point {} ({}): {} files changed, {n} laid out", r.save.n, r.save.message, r.files.len());
                if let Some(p) = r.replayed_on {
                    s.push_str(&format!("\n  made on position {}, replayed on position {p} with the task: the line's changes since are kept", r.save.position));
                }
                for c in &r.conflicts {
                    s.push_str(&format!("\n  conflict, settle the markers: {c}"));
                }
                s
            });
        }
        Cmd::Save { message } => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here: `avp open` first")?;
            // The folder is the task's state: what it changed and has not sent yet goes first.
            let s: engine::Save = match offline::ask(down, Some(&task), || {
                view::sync(&client, &dir, &mut state)?;
                client.post(&format!("/repos/{}/tasks/{task}/saves", state.repo), &serde_json::json!({ "message": message }))
            }) {
                Err(e) if unreachable(&e) => anyhow::bail!("{}\na save point needs the server, which holds the writes it names: keep working, save once it answers", offline::notice_of(&client.base, &state.repo)),
                r => r?,
            };
            print(json, &s, |s| format!("save point {}: {} (write {}, {} files changed on position {}). `avp undo --to {}` comes back to it", s.n, s.message, s.seq, s.files, s.position, s.n));
        }
        Cmd::Saves => {
            let state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            let saves: Vec<engine::Save> = client.get(&format!("/repos/{}/tasks/{task}/saves", state.repo))?;
            print(json, &saves, |s| engine::saves_text(s));
        }
        Cmd::Diff { args, head, stat, repo } => {
            // Up to two positions first, then the paths.
            let n = args.iter().take(2).take_while(|a| a.parse::<i64>().is_ok()).count();
            let (positions, paths) = args.split_at(n);
            let paths: Vec<String> = paths.iter().map(|p| p.trim_start_matches("./").to_string()).collect();
            anyhow::ensure!(!head || positions.is_empty(), "--head compares this task with the head of the line: no position with it");
            // The view's task, unless --repo names another repo.
            let mut state = if repo.is_some() { None } else { view::load(&dir).ok().filter(|s| s.task.is_some()) };
            let r = repo_of(&dir, repo)?;
            let task = state.as_ref().and_then(|s| s.task.clone()).filter(|_| positions.len() < 2);
            let (from, to) = match (positions, task.is_some()) {
                ([a, b], _) => (a.clone(), b.clone()),
                ([a], true) => (a.clone(), "task".to_string()),
                ([a], false) => (a.clone(), "head".to_string()),
                (_, true) => ((if head { "head" } else { "base" }).to_string(), "task".to_string()),
                (_, false) => anyhow::bail!("no task open here: `avp diff <a> <b>` compares two positions, `avp diff <a>` one with the head"),
            };
            if let (Some(s), true) = (state.as_mut(), to == "task") {
                // The folder is the task: what it changed and has not sent yet goes first.
                anyhow::ensure!(!down, "{}\navp diff reads the task from the server: ask again once it answers", offline::notice_of(&client.base, &r));
                view::sync(&client, &dir, s)?;
            }
            let files: Vec<engine::Diffed> = client.post(&format!("/repos/{r}/diff"), &serde_json::json!({ "task": task, "from": from, "to": to, "paths": paths }))?;
            if json {
                let shown: Vec<serde_json::Value> = files.iter().map(|engine::Diffed { file: f, large }| {
                    let (added, removed) = avp::client::diff_counts(f);
                    let mut v = serde_json::json!({ "path": f.path, "change": f.change, "binary": f.binary, "large": large, "added": added, "removed": removed });
                    if !stat {
                        v["patch"] = serde_json::json!(f.patch);
                    }
                    v
                }).collect();
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "from": from, "to": to, "task": task, "files": shown }))?);
            } else if files.is_empty() {
                println!("no change from {from} to {to}");
            } else {
                use std::io::IsTerminal;
                let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
                print!("{}", if stat { avp::client::diff_stat(&files, color) } else { avp::client::diff_text(&files, color) });
            }
        }
        Cmd::Clone { source, dir: into, repo } => {
            let (server, repo, into) = match (repo, source) {
                (Some(r), s) => {
                    anyhow::ensure!(s.is_none() || into.is_none(), "with --repo, the only argument is the folder");
                    (cli.server.clone(), r, s.map(PathBuf::from).or(into))
                }
                (None, Some(s)) if s.contains("://") => {
                    let (scheme, rest) = s.split_once("://").unwrap_or_default();
                    let (host, r) = rest.trim_end_matches('/').rsplit_once('/').filter(|(h, r)| !h.is_empty() && !r.is_empty()).with_context(|| format!("{s}: give the repo after the server, `<server>/<repo>`"))?;
                    (format!("{scheme}://{host}"), r.to_string(), into)
                }
                (None, Some(s)) => (cli.server.clone(), s, into),
                (None, None) => anyhow::bail!("which repo? `avp clone <server>/<repo> [dir]`, or `avp clone --server <url> --repo <name> [dir]`"),
            };
            up::clone(&Client::new(&server), &repo, into, &dir, json)?;
        }
        Cmd::Undo { to, path, save: None } => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            let r: serde_json::Value = match offline::ask(down, Some(&task), || client.post(&format!("/repos/{}/tasks/{task}/undo", state.repo), &serde_json::json!({ "to": to, "path": path }))) {
                Err(e) if unreachable(&e) => {
                    println!("{}", offline::undo(&client.base, &dir, &mut state, to, path)?);
                    return Ok(());
                }
                r => r?,
            };
            let tree: Tree = client.get(&format!("/repos/{}/tasks/{task}/tree", state.repo))?;
            let n = view::materialize(&client, &tree, &dir, &mut state)?;
            println!("undone {} writes, {} files laid out", r["undone"], n);
        }
        Cmd::Goal { goal } => {
            let state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            if let Err(e) = offline::ask(down, Some(&task), || client.post::<serde_json::Value>(&format!("/repos/{}/tasks/{task}/goal", state.repo), &serde_json::json!({ "goal": goal }))) {
                anyhow::ensure!(unreachable(&e), e);
                let n = offline::queue(&dir, &state, Op::Goal { goal: goal.trim().to_string() })?;
                offline_said(json, serde_json::json!({ "offline": true, "waiting": n }), &format!("goal of {task}: {}\n{}", goal.trim(), offline::queued_text(n)));
                return Ok(());
            }
            println!("goal of {task}: {}", goal.trim());
        }
        Cmd::Abandon => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone().context("no task open here")?;
            let r = match offline::ask(down, Some(&task), || client.post::<serde_json::Value>(&format!("/repos/{}/tasks/{task}/abandon", state.repo), &serde_json::json!({}))) {
                Err(e) => {
                    anyhow::ensure!(unreachable(&e), e);
                    println!("{}", offline::abandon(&dir, &mut state)?);
                    return Ok(());
                }
                Ok(r) => r,
            };
            state.task = None;
            view::save(&dir, &state)?;
            println!("abandoned {task}");
            for t in r["stacked"].as_array().into_iter().flatten().filter_map(|t| t.as_str()) {
                println!("  {t}, stacked on it, stays open: it lands its own changes without this task's (its agent hears of it)");
            }
        }
        Cmd::Align { position } => {
            let state = view::load(&dir)?;
            avp::checkout::align_when_written(&client, &dir, &state.repo, position, std::time::Duration::from_secs(120));
        }
        Cmd::Refresh => {
            let mut state = view::load(&dir)?;
            let task = state.task.clone();
            match offline::ask(down, task.as_deref(), || view::catch_up(&client, &dir, &mut state)) {
                Err(e) if unreachable(&e) => println!("{}\nthe line's head is on the server: keep working, a queued land is replayed on it when it answers", offline::notice_of(&client.base, &state.repo)),
                r => println!("{}", r?),
            }
        }
        Cmd::Events { since, follow, all } => {
            let state = view_or_repo(&dir)?;
            let task = if all { None } else { state.task.clone() };
            let mut cursor = since;
            loop {
                let evs: Vec<Event> = client.get(&format!("/repos/{}/events?since={cursor}&limit=200{}", state.repo, task.as_ref().map(|t| format!("&task={t}")).unwrap_or_default()))?;
                for e in &evs {
                    if json {
                        println!("{}", serde_json::to_string(e)?);
                    } else {
                        println!("#{} {} {}{}{}", e.id, e.kind, e.task.as_deref().unwrap_or(""), e.path.as_ref().map(|p| format!(" {p}")).unwrap_or_default(), if e.data.is_null() || e.data == serde_json::json!({}) { String::new() } else { format!(" {}", e.data) });
                    }
                    cursor = e.id;
                }
                if !follow {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        Cmd::Full { position, repo } => {
            let r: serde_json::Value = client.post(&format!("/repos/{}/full", repo_of(&dir, repo)?), &serde_json::json!({ "position": position }))?;
            println!("full suite of position {} started: `avp log` shows its verdict when it ends", r["position"]);
        }
        Cmd::Log { limit, path: Some(path), from, repo } => {
            let repo = repo_of(&dir, repo)?;
            let log = match offline::ask(down, None, || client.file_log(&repo, &path, from, limit.max(1) as usize)) {
                Err(e) if unreachable(&e) => {
                    let r = offline::replica(&client.base, &repo)?;
                    let log = r.file_log(&path, from, limit.max(1) as usize);
                    offline_said(json, serde_json::to_value(&log)?, &format!("{}\n{}", offline::notice(&r.line()), avp::blame::log_text(&log)));
                    return Ok(());
                }
                r => r?,
            };
            offline::refresh(&client, &repo, false);
            print(json, &log, avp::blame::log_text);
        }
        Cmd::Stats { since, until, github, before, save, repo } => {
            let r = repo_of(&dir, repo)?;
            let (from, to) = avp::stats::window(Some(&since), until.as_deref(), now_ms())?;
            let s: avp::stats::Stats = client.get(&format!("/repos/{r}/stats?since={}&until={}", avp::stats::rfc3339(from), avp::stats::rfc3339(to)))?;
            match (github, before) {
                (Some(slug), Some(window)) => {
                    let (bf, bt) = avp::stats::range(&window, now_ms())?;
                    eprintln!("reading {slug}'s pull requests and Actions runs from GitHub...");
                    let b = avp::stats::github_before(&avp::stats::github_get, &slug, bf, bt)?;
                    if save {
                        let _: avp::stats::Before = client.post(&format!("/repos/{r}/before"), &b)?;
                        eprintln!("saved on the server: AVP Live compares to it, labeled \"before AVP\"");
                    }
                    print(json, &serde_json::json!({ "github": b, "avp": s }), |_| avp::stats::side_by_side(&b, &s));
                }
                _ => print(json, &s, avp::stats::text),
            }
        }
        Cmd::Blame { path, at, repo } => {
            let repo = repo_of(&dir, repo)?;
            let blame = match offline::ask(down, None, || client.blame(&repo, &path, at)) {
                // The replica blames the head it knows; a position asked for is the server's.
                Err(e) if unreachable(&e) && at.is_none() => {
                    let r = offline::replica(&client.base, &repo)?;
                    let b = r.blame(&path)?;
                    offline_said(json, serde_json::to_value(&b)?, &format!("{}\n{}", offline::notice(&r.line()), avp::blame::blame_text(&b)));
                    return Ok(());
                }
                r => r?,
            };
            offline::refresh(&client, &repo, false);
            print(json, &blame, avp::blame::blame_text);
        }
        Cmd::Log { limit, path: None, from: _, repo } => {
            let repo = repo_of(&dir, repo)?;
            // The tags each position carries, as git's --decorate shows them. A server from before tags has none.
            let tags: Vec<avp::db::Tag> = if down { vec![] } else { client.get(&format!("/repos/{repo}/tags")).unwrap_or_default() };
            let tags_of = |p: i64| tags.iter().filter(|t| t.position == p).map(|t| t.name.clone()).collect::<Vec<_>>();
            let text = |l: &Vec<LineEntry>| l.iter().map(|e| {
                let named = tags_of(e.position);
                let decor = if named.is_empty() { String::new() } else { format!("(tag: {}) ", named.join(", tag: ")) };
                format!("{:>5}  {}  {}  {decor}{}", e.position, e.full.map(|v| format!("{v:?}")).unwrap_or_else(|| "…".into()), e.task.as_deref().unwrap_or("-"), e.note)
            }).collect::<Vec<_>>().join("\n");
            let line: Vec<LineEntry> = match offline::ask(down, None, || client.get(&format!("/repos/{repo}/line?limit={limit}"))) {
                Err(e) if unreachable(&e) => {
                    let r = offline::replica(&client.base, &repo)?;
                    let line = r.log(limit);
                    offline_said(json, serde_json::to_value(&line)?, &format!("{}\n{}", offline::notice(&r.line()), text(&line)));
                    return Ok(());
                }
                r => r?,
            };
            offline::refresh(&client, &repo, false);
            let mut shown = serde_json::to_value(&line)?;
            for e in shown.as_array_mut().into_iter().flatten() {
                let named = tags_of(e["position"].as_i64().unwrap_or(-1));
                if !named.is_empty() {
                    e["tags"] = serde_json::json!(named);
                }
            }
            print(json, &shown, |_| text(&line));
        }
        Cmd::Why { path, repo } if !json => {
            let r = repo_of(&dir, repo)?;
            match offline::ask(down, None, || avp::history::why(&client, &r, &path)) {
                Err(e) if unreachable(&e) => {
                    let rep = offline::replica(&client.base, &r)?;
                    println!("{}\n{}", offline::notice(&rep.line()), avp::history::why_text(&rep.why(&path)?));
                }
                w => {
                    println!("{}", w?);
                    offline::refresh(&client, &r, false);
                }
            }
        }
        Cmd::Why { path, repo: _ } => {
            let state = view_or_repo(&dir)?;
            let w: Why = match offline::ask(down, None, || client.get(&format!("/repos/{}/why?path={}", state.repo, path))) {
                Err(e) if unreachable(&e) => {
                    let r = offline::replica(&client.base, &state.repo)?;
                    eprintln!("{}", offline::notice(&r.line()));
                    r.why(&path)?
                }
                r => r?,
            };
            print(json, &w, |w| match &w.task {
                Some(t) => format!("{} last changed at position {} by task {}\n  goal: {}\n  by: {} via {}\n  proof: {}{}", w.path, w.position, t.id, t.goal, t.actor.person, t.actor.agent, w.proof.as_ref().map(verdict_line).unwrap_or_else(|| "none".into()), t.transcript.as_ref().map(|x| format!("\n  conversation: {x}")).unwrap_or_default()),
                None => format!("{} comes from position {} (imported)", w.path, w.position),
            });
        }
        Cmd::Context { paths, repo } => {
            let r = repo_of(&dir, repo)?;
            // In a view with a task open, that task's own leases are not named as holders.
            let task = view::load(&dir).ok().and_then(|v| v.task);
            let c = match offline::ask(down, None, || avp::history::context(&client, &r, &paths, task.as_deref())) {
                Err(e) if unreachable(&e) => {
                    let rep = offline::replica(&client.base, &r)?;
                    let c = rep.context(&paths, task.as_deref())?;
                    offline_said(json, serde_json::to_value(&c)?, &format!("{}\n{}", offline::notice(&rep.line()), avp::history::context_text(&c)));
                    return Ok(());
                }
                c => c?,
            };
            offline::refresh(&client, &r, false);
            print(json, &c, avp::history::context_text);
        }
        Cmd::Show { position, repo } => {
            let r = repo_of(&dir, repo)?;
            let mut out = match offline::ask(down, None, || avp::history::show(&client, &r, position)) {
                Err(e) if unreachable(&e) => {
                    let rep = offline::replica(&client.base, &r)?;
                    print!("{}\n{}", offline::notice(&rep.line()), rep.show(position)?);
                    return Ok(());
                }
                o => o?,
            };
            // A position AVP took back says so under its full suite, before the diff.
            let entry = client.get::<Vec<LineEntry>>(&format!("/repos/{r}/line?from={position}&limit=1"))?.into_iter().find(|e| e.position == position);
            if let Some(rv) = entry.and_then(|e| e.reverted) {
                let at = out.find("\n  full suite: ").and_then(|i| out[i + 1..].find('\n').map(|j| i + j + 2)).unwrap_or(out.len());
                out.insert_str(at, &format!("  reverted by AVP at position {} (task {}): the full suite's shard {} went red twice on it: {}\n", rv.by, rv.task, rv.shard, rv.tests.join(" | ")));
            }
            print!("{out}");
        }
        Cmd::Revert { position, repo } => {
            let r = repo_of(&dir, repo)?;
            println!("{}", avp::history::revert(&client, &dir, &r, position, actor())?);
        }
        Cmd::Bisect { check, good, bad, repo } => {
            let r = repo_of(&dir, repo)?;
            let b = avp::history::bisect(&client, &r, &check, good, bad, &|l| eprintln!("{l}"))?;
            print(json, &b, avp::history::bisect_text);
        }
        Cmd::Pick { position } => {
            let mut state = view::load(&dir)?;
            let p = avp::history::pick(&client, &dir, &mut state, position)?;
            print(json, &p, avp::history::picked_text);
        }
        Cmd::Tag { name, position, delete, force, repo } => {
            let (r, person) = (repo_of(&dir, repo)?, actor().person);
            if delete {
                let t: avp::db::Tag = client.post(&format!("/repos/{r}/tags/remove"), &serde_json::json!({ "name": name, "person": person }))?;
                print(json, &t, |t| format!("tag {} deleted (it named position {}): the mirror deletes its git tag", t.name, t.position));
            } else {
                let t: avp::db::Tag = client.post(&format!("/repos/{r}/tags"), &serde_json::json!({ "name": name, "position": position, "force": force, "person": person }))?;
                print(json, &t, |t| format!("tag {} names position {} ({}): the mirror writes it as a git tag, `avp promote <env> {}` promotes it", t.name, t.position, t.note.trim(), t.name));
            }
        }
        Cmd::Tags { repo } => {
            let tags: Vec<avp::db::Tag> = client.get(&format!("/repos/{}/tags", repo_of(&dir, repo)?))?;
            print(json, &tags, |ts| {
                if ts.is_empty() {
                    return "no tag on this line: `avp tag <name> [<position>]` names one".into();
                }
                ts.iter().map(|t| format!("{:<14} position {:<5} {}  {} UTC  {}{}", t.name, t.position, t.person, avp::blame::utc(t.at), t.note.trim(), t.commit.as_ref().map(|c| format!("  (git {})", &c[..12.min(c.len())])).unwrap_or_default())).collect::<Vec<_>>().join("\n")
            });
        }
        Cmd::Tasks { repo } => {
            let repo = repo_of(&dir, repo)?;
            let text = |ts: &Vec<Task>| engine::stack_order(ts).into_iter().map(|(depth, t)| format!("{}{}  {}  {} via {}  {}", "  ".repeat(depth), t.id, engine::stands_on(t, depth), t.actor.person, t.actor.agent, t.goal)).collect::<Vec<_>>().join("\n");
            let ts: Vec<Task> = match offline::ask(down, None, || client.get(&format!("/repos/{repo}/tasks?state=open"))) {
                Err(e) if unreachable(&e) => {
                    let r = offline::replica(&client.base, &repo)?;
                    let ts = r.tasks();
                    offline_said(json, serde_json::to_value(&ts)?, &format!("{}\n{}", offline::notice(&r.line()), text(&ts)));
                    return Ok(());
                }
                r => r?,
            };
            print(json, &ts, text);
        }
        Cmd::Proof { id, step, artifacts } => {
            let state = view_or_repo(&dir)?;
            let id = match id {
                Some(id) => id,
                None => {
                    let task = state.task.as_deref().context("no proof id given and no task open here")?;
                    let st: TaskStatus = client.get(&format!("/repos/{}/tasks/{task}", state.repo))?;
                    st.last_proof.context("this task has no proof yet: `avp check` runs one")?.id
                }
            };
            // As the server says it: each step with its log's length and its artifacts, when kept.
            let raw: serde_json::Value = client.get(&format!("/repos/{}/proofs/{id}", state.repo))?;
            let p: Proof = serde_json::from_value(raw.clone())?;
            if let Some(name) = &step {
                anyhow::ensure!(p.steps.iter().any(|s| &s.name == name), "no step {name:?} in {id}: its steps are {}", p.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "));
            }
            let chosen: Vec<(&Step, &serde_json::Value)> = p.steps.iter().zip(raw["steps"].as_array().into_iter().flatten()).filter(|(s, _)| step.as_ref().is_none_or(|n| &s.name == n)).collect();
            if let Some(into) = artifacts {
                let got = download_artifacts(&client, &state.repo, &id, &chosen, &into)?;
                print(json, &got, |got| {
                    if got.is_empty() {
                        return format!("no artifacts kept in {id}: a step keeps what it makes with `artifacts = [\"coverage/\"]` in avp.toml, for 14 days");
                    }
                    got.iter().map(|g| format!("{}: {} ({}) in {}", g["step"].as_str().unwrap_or(""), file_count(g["files"].as_u64().unwrap_or(0)), human_bytes(g["bytes"].as_u64().unwrap_or(0)), g["dir"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")
                });
                return Ok(());
            }
            // A server from before keeps no log: the output it has, undated.
            let log_of = |st: &Step| client.get::<serde_json::Value>(&format!("/repos/{}/proofs/{id}/log?{}", state.repo, avp::check::step_query(&st.name))).ok();
            if json {
                match step.as_ref().and_then(|_| chosen.first()).and_then(|(st, _)| log_of(st)) {
                    Some(log) => print(true, &log, |_| String::new()),
                    None => print(true, &raw, |_| String::new()),
                }
                return Ok(());
            }
            let mut s = if step.is_none() { verdict_line(&p) } else { String::new() };
            for (st, v) in &chosen {
                s.push_str(&format!("\n--- {} ({}, {:.1}s): {}", st.name, st.exit, st.seconds, st.command));
                match log_of(st).filter(|l| l["kept"] == true) {
                    Some(log) => s.push_str(&log_text(&log)),
                    None => s.push_str(&format!("\n{}", st.output)),
                }
                if let Some(a) = v.get("artifacts") {
                    s.push_str(&format!("\n  artifacts: {}", artifacts_line(a, &id, &st.name)));
                }
            }
            println!("{}", s.trim_start_matches('\n'));
        }
        Cmd::Guard { patterns, repo } => {
            let r = repo_of(&dir, repo)?;
            let now: Vec<String> = client.post(&format!("/repos/{r}/guarded"), &serde_json::json!({ "add": patterns }))?;
            println!("guarded: {}", now.join(", "));
        }
        Cmd::Unguard { patterns, repo } => {
            let r = repo_of(&dir, repo)?;
            let now: Vec<String> = client.post(&format!("/repos/{r}/guarded"), &serde_json::json!({ "remove": patterns }))?;
            println!("guarded: {}", if now.is_empty() { "nothing".into() } else { now.join(", ") });
        }
        Cmd::Guarded { repo } => {
            let r = repo_of(&dir, repo)?;
            let now: Vec<String> = client.get(&format!("/repos/{r}/guarded"))?;
            println!("guarded by AVP: {} (the checks: a change to them always waits for a person)", engine::CHECK_FILES.join(", "));
            println!("guarded: {}", if now.is_empty() { "nothing".into() } else { now.join(", ") });
            let open: Vec<Task> = client.get(&format!("/repos/{r}/tasks?state=open"))?;
            let evs: Vec<Event> = client.get(&format!("/repos/{r}/events?since=0&limit=100000"))?;
            let waiting: Vec<&Task> = open.iter().filter(|t| evs.iter().any(|e| e.kind == "task.awaiting_approval" && e.task.as_deref() == Some(&t.id))).collect();
            if waiting.is_empty() {
                println!("no task waits for an approval");
            }
            for t in waiting {
                let paths = evs.iter().rev().find(|e| e.kind == "task.awaiting_approval" && e.task.as_deref() == Some(&t.id)).and_then(|e| e.data.get("guarded").cloned()).unwrap_or_default();
                println!("waits: {}  {} via {}  {}  {}", t.id, t.actor.person, t.actor.agent, t.goal, paths);
            }
        }
        Cmd::Approve { task, repo } => {
            let r = repo_of(&dir, repo)?;
            let a: Approval = client.post(&format!("/repos/{r}/tasks/{task}/approve"), &serde_json::json!({ "person": actor().person, "approved": true, "reason": "" }))?;
            println!("{} approved by {}", a.task, a.person);
        }
        Cmd::Refuse { task, why, repo } => {
            let state = view::ViewState { repo: repo_of(&dir, repo)?, ..Default::default() };
            let a: Approval = client.post(&format!("/repos/{}/tasks/{task}/approve", state.repo), &serde_json::json!({ "person": actor().person, "approved": false, "reason": why }))?;
            println!("{} refused by {}: {}", a.task, a.person, a.reason);
        }
        Cmd::Landing { rule } => {
            let state = view::load(&dir)?;
            let r: Repo = client.post(&format!("/repos/{}/landing", state.repo), &serde_json::json!({ "landing": rule }))?;
            println!("{} lands on {:?}", r.name, r.landing);
        }
        Cmd::Envs { repo } => {
            let es: Vec<Environment> = client.get(&format!("/repos/{}/envs", repo_of(&dir, repo)?))?;
            print(json, &es, |es| {
                es.iter()
                    .map(|e| {
                        let serves = if e.position >= 0 { format!("serves position {}", e.position) } else { "serves nothing known yet".into() };
                        let wanted = match &e.wanted {
                            Some(w) if w.position != e.position => match &e.progress {
                                Some(p) => format!(", promoted to {} by {} (deploying: {}, {} min)", w.position, w.by, p.step, (now_ms() - p.since).max(0) / 60_000),
                                None => format!(", promoted to {} by {} (deploying)", w.position, w.by),
                            },
                            Some(w) => format!(", promoted by {}", w.by),
                            None => String::new(),
                        };
                        format!("{:<12} {serves}{wanted}", e.name)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        Cmd::Secret { cmd: SecretCmd::Set { name, repo } } => {
            let repo = repo_of(&dir, repo)?;
            let value = secret_value(&name)?;
            let set: avp::check::SecretInfo = client.post(&format!("/repos/{repo}/secrets/{name}"), &serde_json::json!({ "value": value, "person": actor().person }))?;
            print(json, &set, |s| format!("{} kept, encrypted: a step gets it once avp.toml names it (`[check] secrets = [\"{}\"]`), its value masked in what AVP shows", s.name, s.name));
        }
        Cmd::Secret { cmd: SecretCmd::List { repo } } => {
            let kept: Vec<avp::check::SecretInfo> = client.get(&format!("/repos/{}/secrets", repo_of(&dir, repo)?))?;
            print(json, &kept, |kept| {
                if kept.is_empty() {
                    return "no secret for this repo: `avp secret set <NAME>` keeps one, its value on stdin".into();
                }
                kept.iter().map(|s| format!("{:<24} set {} UTC", s.name, avp::blame::utc(s.set_at))).collect::<Vec<_>>().join("\n")
            });
        }
        Cmd::Secret { cmd: SecretCmd::Rm { name, repo } } => {
            let r: serde_json::Value = client.post(&format!("/repos/{}/secrets/{name}/remove", repo_of(&dir, repo)?), &serde_json::json!({ "person": actor().person }))?;
            print(json, &r, |_| format!("{name} forgotten: a step that names it goes red until it is set again"));
        }
        Cmd::Promote { env, position, force, repo } => {
            let (a, r) = (actor(), repo_of(&dir, repo)?);
            let position = position.map(|p| position_of(&client, &r, &p)).transpose()?;
            let e = client.promote(&r, &env, &serde_json::json!({ "position": position, "force": force, "person": a.person, "agent": a.agent }))?;
            match (&e.wanted, &e.deploy) {
                (Some(w), Some(d)) => println!("{} promoted to position {} by {}: {} carries it out, {} moves when it serves it", e.name, w.position, w.by, d.github_workflow, e.name),
                _ => println!("{} now serves position {}", e.name, e.position),
            }
        }
        Cmd::User { cmd } => users(&client, &dir, cmd, json)?,
        Cmd::Token { cmd } => tokens(&client, cmd, json)?,
        Cmd::Whoami => {
            let me: serde_json::Value = client.get("/me")?;
            print(json, &me, |me| {
                let person = me["person"].as_str().unwrap_or("nobody named");
                if me["accounts"] != true {
                    return format!("this server has no accounts: anyone who reaches it may do anything, as {person} (AVP_PERSON)\n  `avp user add <name>` turns them on");
                }
                let mut s = match me["via"].as_str() {
                    Some("token") => format!("{person}{}", if me["admin"] == true { ", admin of this server" } else { "" }),
                    Some("key") => format!("the server's key (AVP_TOKEN), as {person}: every right"),
                    _ => "the server's own machine: every right".into(),
                };
                for r in me["roles"].as_array().into_iter().flatten() {
                    s.push_str(&format!("\n  {}: {}", r["repo"].as_str().unwrap_or_default(), r["role"].as_str().unwrap_or_default()));
                }
                s
            });
        }
        Cmd::Rollback { env, repo } => {
            let (a, r) = (actor(), repo_of(&dir, repo)?);
            let from = client.get::<Vec<Environment>>(&format!("/repos/{r}/envs"))?.into_iter().find(|e| e.name == env).map(|e| e.position).unwrap_or(-1);
            // Its own route, answered once the deploy is over, as a promotion's is.
            let e = client.promote(&r, &format!("{env}/rollback"), &serde_json::json!({ "person": a.person, "agent": a.agent }))?;
            print(json, &e, |e| match (&e.wanted, &e.deploy) {
                (Some(w), Some(d)) => format!("{} rolled back from position {from} to position {} by {}: {} carries it out, {} moves when it serves it", e.name, w.position, w.by, d.github_workflow, e.name),
                _ => format!("{} rolled back from position {from}: it serves position {}", e.name, e.position),
            });
        }
    }
    Ok(())
}

/// A token as `avp token create` and `avp user add` print it: the one time it is shown.
fn token_text(t: &avp::model::NewToken) -> String {
    let ends = t.info.expires_at.map(|e| format!("until {} UTC", avp::blame::utc(e))).unwrap_or_else(|| "no end".into());
    format!("token {} for {} ({}, {ends}), shown once:\n  {}\nit goes in AVP_TOKEN; `avp token revoke {}` stops it", t.info.id, t.info.person, t.info.name, t.token, t.info.id)
}

fn users(client: &Client, dir: &Path, cmd: UserCmd, json: bool) -> Result<()> {
    match cmd {
        UserCmd::Add { name, email, github, admin } => {
            let added: serde_json::Value = client.post("/users", &serde_json::json!({ "name": name, "email": email, "github": github, "admin": admin }))?;
            let token: avp::model::NewToken = serde_json::from_value(added["token"].clone())?;
            print(json, &added, |_| {
                let what = if added["user"]["admin"] == true { ", admin of this server" } else { ": `avp user role` gives them a role on a repo" };
                format!("{name} added{what}\n{}", token_text(&token))
            });
        }
        UserCmd::List { repo: Some(repo) } => {
            let roles: Vec<serde_json::Value> = client.get(&format!("/repos/{repo}/roles"))?;
            print(json, &roles, |roles| {
                if roles.is_empty() {
                    return format!("nobody has a role on {repo}: `avp user role <name> write --repo {repo}`");
                }
                roles.iter().map(|r| format!("{:<20} {}", r["person"].as_str().unwrap_or_default(), r["role"].as_str().unwrap_or_default())).collect::<Vec<_>>().join("\n")
            });
        }
        UserCmd::List { repo: None } => {
            let people: Vec<serde_json::Value> = client.get("/users")?;
            print(json, &people, |people| {
                if people.is_empty() {
                    return "no user: this server has no accounts, `avp user add <name>` turns them on".into();
                }
                people
                    .iter()
                    .map(|p| {
                        let u = &p["user"];
                        let roles: Vec<String> = p["roles"].as_array().into_iter().flatten().map(|r| format!("{} {}", r["repo"].as_str().unwrap_or_default(), r["role"].as_str().unwrap_or_default())).collect();
                        let what = if u["admin"] == true { "admin of the server".to_string() } else if roles.is_empty() { "no role".to_string() } else { roles.join(", ") };
                        let mut s = format!("{:<16} {what}", u["name"].as_str().unwrap_or_default());
                        for (k, v) in [("email", &u["email"]), ("github", &u["github"])] {
                            if let Some(v) = v.as_str() {
                                s.push_str(&format!("  {k} {v}"));
                            }
                        }
                        s.push_str(&format!("  {} token{}", p["tokens"], if p["tokens"] == 1 { "" } else { "s" }));
                        s
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        UserCmd::Rm { name } => {
            let r: serde_json::Value = client.post(&format!("/users/{name}/remove"), &serde_json::json!({}))?;
            print(json, &r, |_| format!("{name} removed: their tokens stop working, their roles are gone"));
        }
        UserCmd::Admin { name, off } => {
            let u: avp::model::User = client.post(&format!("/users/{name}/admin"), &serde_json::json!({ "admin": !off }))?;
            print(json, &u, |u| format!("{} is {}", u.name, if u.admin { "an admin of the server" } else { "no admin of the server" }));
        }
        UserCmd::Role { name, role, repo } => {
            let repo = repo_of(dir, repo)?;
            let r: serde_json::Value = client.post(&format!("/repos/{repo}/roles"), &serde_json::json!({ "person": name, "role": role }))?;
            print(json, &r, |r| format!("{name}: {} on {} (was {})", r["role"].as_str().unwrap_or_default(), r["repo"].as_str().unwrap_or_default(), r["was"].as_str().unwrap_or_default()));
        }
        UserCmd::Log { since } => {
            let evs: Vec<Event> = client.get(&format!("/accounts/events?since={since}&limit=10000"))?;
            print(json, &evs, |evs| {
                if evs.is_empty() {
                    return "no change of people, tokens or roles yet".into();
                }
                evs.iter()
                    .map(|e| {
                        let d = &e.data;
                        let s = |k: &str| d[k].as_str().unwrap_or_default().to_string();
                        let what = match e.kind.as_str() {
                            "user.added" => format!("added {}{}", s("name"), if d["admin"] == true { ", admin of the server" } else { "" }),
                            "user.removed" => format!("removed {}", s("name")),
                            "user.admin" => format!("{} {}", s("name"), if d["admin"] == true { "made admin of the server" } else { "no longer admin of the server" }),
                            "token.created" => format!("token {} for {} ({})", s("id"), s("person"), s("name")),
                            "token.revoked" => format!("revoked token {} of {} ({})", s("id"), s("person"), s("name")),
                            "role.set" => format!("{} {} on {} (was {})", s("person"), s("role"), s("repo"), s("was")),
                            _ => format!("{} {}", e.kind, e.data),
                        };
                        format!("#{:<6} {} UTC  {what}, by {}", e.id, avp::blame::utc(e.at), s("by"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
    }
    Ok(())
}

fn tokens(client: &Client, cmd: TokenCmd, json: bool) -> Result<()> {
    match cmd {
        TokenCmd::Create { name, expires, user } => {
            let t: avp::model::NewToken = client.post("/tokens", &serde_json::json!({ "name": name, "expires": expires, "person": user }))?;
            print(json, &t, token_text);
        }
        TokenCmd::List { user } => {
            let path = match &user {
                Some(u) => format!("/tokens?person={u}"),
                None => "/tokens".into(),
            };
            let ts: Vec<avp::model::TokenInfo> = client.get(&path)?;
            print(json, &ts, |ts| {
                if ts.is_empty() {
                    return "no token that works: `avp token create` makes one".into();
                }
                ts.iter()
                    .map(|t| {
                        let ends = t.expires_at.map(|e| format!("until {}", avp::blame::utc(e))).unwrap_or_else(|| "no end".into());
                        let used = t.used_at.map(|u| format!("used {}", avp::blame::utc(u))).unwrap_or_else(|| "never used".into());
                        format!("{}  {:<12} {:<20} {ends}, {used}", t.id, t.person, t.name)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        TokenCmd::Revoke { id } => {
            let t: avp::model::TokenInfo = client.post(&format!("/tokens/{id}/revoke"), &serde_json::json!({}))?;
            print(json, &t, |t| format!("{} revoked: {}'s {} no longer works", t.id, t.person, t.name));
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: &Path) -> Result<()> {
    Err(anyhow!("unused"))
}
