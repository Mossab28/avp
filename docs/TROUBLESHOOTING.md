# Troubleshooting

The traps real runs fell into, on Murmell's repository and on others, one entry each: what you
see, why, the fix. The defects of AVP itself, with the version that fixed each, are in
[RUNS.md](RUNS.md); moving a team over step by step is [MIGRATE.md](MIGRATE.md).

## Moving over

### The mirror stops pushing: `main` holds a commit AVP did not write

**What you see.** The mirror prints `push to origin main refused: it only fast-forwards, and main
holds a commit AVP did not write`; AVP Live shows a red alert, and `avp events --all` a
`mirror.blocked` naming the branch and the first foreign commit. The line keeps moving; the branch
does not.

**Why.** The mirror only fast-forwards and overwrites nothing. Someone pushed to the branch, or
merged a pull request on it, outside AVP.

**The fix.** Look at the commit it names. If it should not be there, reset the branch to the last
commit AVP wrote (the `AVP-Position` trailers say which): the next mirror pass pushes again. If it
holds work worth keeping, bring it in with `avp open` and those files, land it, then reset the branch
the same way. Then protect the branch so that only the mirror writes it (no direct pushes, no force
pushes).

### No `avp/line` branch in my checkout

**What you see.** `avp up` said the mirror writes `avp/line`, and `git branch` does not list it.

**Why.** With a remote, the mirror pushes the branch there: it is `origin/avp/line`. Without one, it
moves the local branch, fast-forward only, and never the branch checked out (`avp/line is checked
out here: the mirror does not move a branch under your files`). On a team server, `avp up` starts no
mirror at all: the server's machine runs the only one.

**The fix.** `git fetch origin avp/line`, or `git log avp/line` without a remote; check out another
branch if `avp/line` is the one checked out.

### The CI of `main` never catches up

**What you see.** Once `main` is the mirror, its CI runs pile up: at Murmell, 9 runs and 144 jobs
interleaved, none finishing.

**Why.** The mirror writes one commit per landing, and a workflow on `push` to `main` starts a whole
CI for each, every one in a group of its own.

**The fix.** One run at a time on `main`, only the newest waiting: a `concurrency` group shared by
the runs of the branch, without `cancel-in-progress`:

```yaml
concurrency:
  group: ci-${{ github.ref }}
```

Once AVP's full suite runs what this CI runs, its `push` trigger on `main` can go.

### A staging deploy AVP started is cancelled

**What you see.** `avp envs` shows staging promoted by `avp`, then the deploy workflow's run
cancelled on GitHub, and another run in its place.

**Why.** The deploy workflow still runs on `push` to `main`: every mirrored commit starts staging
too, and GitHub's queue gives that run the place of AVP's.

**The fix.** Take `push:` out of the deploy workflows' triggers: staging follows AVP's full suite
(`follow = "full"`) and nothing else.

### `avp up` says the guessed checks did not pass on the head

**What you see.** `the guessed checks did not pass on the head (..., tests failed: sh: cargo: command
not found): fix ~/.avp/<repo>.avp.toml and land it as a task`. The mirror and the agents come up
anyway.

**Why.** `avp up` proves the checks it guessed on the head before landing them. A red there is a
tool missing on the machine that proves (`cargo`, `pnpm`), a command that does not fit the project,
or the project's own tests failing on the head (`ruby/rake` on Ruby 4: AVP rightly refused).

**The fix.** Install the tool where proofs run, or fix the file it names, then land it as a task
(`avp open --scope avp.toml`, write, `avp land`). `avp import-ci` proposes the checks from your CI
instead of guessing them.

## Proofs

### A step misses a variable it has in CI

**What you see.** A step that passes in CI, or in the agent's folder, fails in the proof: a
`NODE_OPTIONS`, an `HTTPS_PROXY`, a `JAVA_HOME` is not there.

**Why.** A step runs what an agent wrote, so it gets only what a build needs (`PATH`, `HOME`,
`LANG`, `LC_*`, `TERM`, `TMPDIR`, `USER`, `SHELL`, `CARGO_HOME`, `RUSTUP_HOME`, `CARGO_TARGET_DIR`,
`XDG_CACHE_HOME`, `COREPACK_HOME`, `npm_config_store_dir`), plus `CI=true` and the secrets it names.
Steps used to inherit the server's whole environment: on 26 October 2026 the server that develops
AVP ran with `AVP_CHECK_SLOTS=1`, every step got it, and AVP's own unit tests, which start engines in
the test process, shared one proof slot and went red on timings.

**The fix.** Name what steps need in `AVP_PROOF_ENV`, comma separated, where proofs run (the server
and the workers), and restart them (drain the server first, below):
`AVP_PROOF_ENV=HTTPS_PROXY,NODE_OPTIONS`. `AVP_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `AWS_*`,
`CLAUDE_*`, `ANTHROPIC_*` and `OPENAI_*` never reach a step, even listed. A test key goes in
`avp secret set`; a deploy's credentials in `AVP_DEPLOY_ENV`.

### A step says a secret it names is not set

**What you see.** A red step whose output says `the secret STRIPE_KEY this step names is not set
for this repo`, and how a person sets it.

**Why.** `avp.toml` names it (`[check] secrets`, or the step's `secrets`) and the server keeps no
value for it in this repo. A step that names a missing secret goes red rather than run without it.

**The fix.** `avp secret set STRIPE_KEY < key.txt`, then land again; `avp secret list` shows the
names set. An agent sees the names with `avp_secrets` and asks a person: it never handles a value.

### Timing tests fail only in proofs

**What you see.** Tests that measure time go red in a proof and green on a rerun of the same tree.

**Why.** A proof ran beside another check on the same machine, a shard of the full suite among
them: a server runs 4 checks at once by default (`AVP_CHECK_SLOTS`).

**The fix.** `AVP_CHECK_SLOTS=1` on a machine whose tests time themselves, or proofs on workers:
start the server with `AVP_WORKERS=1` and run `avp worker` on other machines, one proof at a time
each.

### Every proof builds the whole project again

**What you see.** Each proof spends its time compiling what did not change: 28 to 30 s for AVP's own
crate.

**Why.** Every proof starts from what a fresh checkout would hold: what an earlier proof wrote in its
folder is gone, `target/` with the rest, save the setup's links and what `keep` names.

**The fix.** `keep = ["target"]` in `[check]` (what `avp up` writes for Cargo), or the folder your
build tool caches in: it stays from one proof to the next in the same slot, like a CI cache. The tool
must know what in it is stale. The same proofs then take 10 s.

### A workflow, a chart or a migration is proven in seconds, then refused

**What you see.** A task that changes `.github/workflows/`, a Helm chart, a Dockerfile or a SQL
migration is proven green in seconds; the CI's security scan refuses it afterwards, or two
migrations land in the wrong order.

**Why.** `{files}` gives a step the task's code files (and the tests that name a file it touched):
the rest reaches no step. And a scan the CI runs but the proof does not never sees the change.

**The fix.** Put the scan in the proof as a step, with the CI's version and rules, and give it
`{all_files}`, every file the task leaves in the tree. Same for a migration-order check. Where a
change to `.github/` should run the tests of the CI's own scripts, make that a step too.

### A proof takes minutes: the task reaches hundreds of tests

**What you see.** Tasks that touch central modules take 7 minutes to prove (the median of 16 at
Murmell, against 15 s for other tasks), and a red comes back after 13.

**Why.** The test runner relates 77 to 324 test files to such a change, and a step runs them in one
job.

**The fix.** `split = 40` on the step, with `{shard}` in its command (`vitest related --run
--shard={shard} {files}`): AVP counts the related tests through the imports and runs that many
shares side by side (8 at most, `AVP_SPLIT_MAX`), beside one job for the other steps. The first red
part stops the others, on workers too.

### Proofs are much slower on some machines

**What you see.** The same family of tests takes 566 s in a proof on one pool of workers and 227 s on
another.

**Why.** Those workers ran in containers capped at 3 processors and 4 GB, beside six CI runners on
the same machine.

**The fix.** Give workers the processors and memory a proof needs (Murmell went to 5 and 5 GB), or
put proofs first on the faster pool. `GET /work` says how many proofs wait and run.

### A proof is green and the full suite is red

**What you see.** A task lands green; the full suite after its batch goes red on tests the proof
did not run. At Murmell, a stylesheet change, then two tests that read that stylesheet as text.

**Why.** A proof runs the typecheck and the tests related to the change, not the whole suite. Since
then, a test that names a touched file (a stylesheet, a fixture, a migration) is related to it; a
break nothing names is still the full suite's to catch.

**The fix.** Nothing to do for the line: a full suite red twice finds the position that broke it and
reverts it (below). For the gap, a test that reads a file by its path names it, and a step can take
`{all_files}`.

## The full suite

### `avp log` says `Flaky`

**What you see.** A position whose full suite is `Flaky`, not `Green` nor `Red`; at Murmell, a
browser test whose browser did not start, green on the same code just before.

**Why.** A red shard runs once more on the same tree. Green the second time, the position counts as
green for `avp promote` and `follow`; the event `line.flaky` names the shard and the failing lines.

**The fix.** Fix the test: AVP Live lists the flaky tests of the last 7 days. Red twice is red.

### Shards of the full suite run on the server, not on the workers

**What you see.** With workers configured, the first full suite's shards run on the server's own
machine.

**Why.** A job waits `AVP_WORKER_CLAIM_S` (120 s by default) for a worker to claim it, then the
server runs it itself. New worker machines took longer than that to start.

**The fix.** `AVP_WORKER_CLAIM_S=900` on the server where workers start on demand.

### AVP reverted my position

**What you see.** A task by `avp` whose goal starts `Revert position 57:` and names the red shard
and its first failing lines; for a day, `avp open` (and `avp_open`) says it to the person and agent
who landed 57, and to any task opened on the paths it changed.

**Why.** The full suite went red twice after a batch, and AVP found the position that broke it,
halving the batch, then took it back in a task of its own, proven like any other. The event
`line.reverted` hands it back with the failing output.

**The fix.** `avp show 57` says why; fix it in a new task and land again. `[line] auto_revert =
false` in avp.toml turns this off. When the revert cannot land green, the line stays red for a
person (`line.revert_failed` says why). A revert of a guarded path (avp.toml, always) runs its proof
and, green, waits for a person's approval (`avp approve <task>`, the task `line.revert_failed`
names); whoever approves a revert of avp.toml reads its diff first (`avp diff <task>`), as it takes
back the line's checks.

## Environments

### Staging stays "deploying" for half an hour while newer positions go green

**What you see.** `avp envs` says `staging serves position 108, promoted to 109 by avp (deploying)`
for 30 minutes, while `avp log` shows positions after 109 `Green`; staging takes none of them.

**Why.** An environment that follows the full suite takes the newest green position only once it
serves the one it was promoted to: promoted again while a deploy ran, each new deploy took the place
of the last, and Murmell's staging stayed on 108 while 109 to 121 went green. A deploy that is never
seen served (its workflow failed or was cancelled, or nothing reports what the environment serves)
holds staging until `AVP_FOLLOW_STUCK_S` (1800 s by default) has passed; then it counts as lost and
the newest green goes. A `run` deploy is not held this way: AVP runs one at a time per environment,
and the newest green position waits for its turn.

**The fix.** Find out why that deploy was never served (its workflow run), then promote the latest
green position yourself: the newest `Green` in `avp log`, `avp promote staging <position>`.
`AVP_FOLLOW_STUCK_S=600` on the server makes the wait shorter.

## Agents and their folders

### An agent edits the checkout and never calls AVP

**What you see.** Changes left in the git checkout, no task opened, nothing landed. At first, 2
Codex agents in 3.

**Why.** Some agents do not pass an MCP server's instructions to their model, and a rule in the
tools' descriptions was not enough (0 in 3).

**The fix.** The rule where the agent reads its instructions: `avp up` writes it in an untracked
`AGENTS.md`, kept out of `git status`, when the repo has none (3 in 3). When the repo has its own,
`avp up` writes the rule to `~/.avp/<repo>.AGENTS.md`: add it to the repo's `AGENTS.md`.

### pnpm installs everything again in each folder

**What you see.** Every new agent folder spends minutes in `pnpm install` before its first test; in
a proof, `pnpm <script>` starts by reinstalling `node_modules`.

**Why.** A new folder had no install. And pnpm 11 reinstalls `node_modules` before `pnpm <command>`
when the folder looks foreign to it: in a proof, `node_modules` is a link into the setup's cache,
made once per lockfile.

**The fix.** In a proof, AVP writes `verify-deps-before-run=false` in the proof folder's `.npmrc`
and `verifyDepsBeforeRun: false` in its `pnpm-workspace.yaml` (the proof's copy: the line's files do
not change). Keep `setup`, `setup_keys` and `setup_links = ["node_modules"]` as `avp up` and `avp
import-ci` write them, and the install runs once per lockfile. For agents, a session takes a folder
another session left free, with its install, and a new folder clones the install of a folder whose
`pnpm-lock.yaml` is the same: `avp_open` (and `avp open`) says so, and that the install has nothing
to do. Where the disk cannot clone (no APFS, no reflinks), it takes none, and the agent installs once
in that folder.

### One agent's tests run another agent's build

**What you see.** An agent's tests pass or fail on code that is not in its folder: the binary, the
bundle or the virtualenv they run was built from another agent's task, and the verdict changes with
whoever built last.

**Why.** Several folders write one build folder: an install or a `target/` linked between views, or
one `CARGO_TARGET_DIR` (or another tool's output folder) set for every agent. A build tool names its
outputs by project and profile, not by the folder the sources are in, so `target/release/app` is the
last build, from any folder. In a proof, `CARGO_TARGET_DIR` is one of the variables a step gets from
the machine: set where proofs run, every proof slot builds into it.

**The fix.** One build folder per folder. AVP never shares an install between two views: a new view
clones the install of a view whose lockfile is the same (`node_modules`, `target`, `.venv`,
`vendor`), and rewrites the paths the tools wrote into it, or takes none. Do not link installs
between views yourself; give each agent its own `CARGO_TARGET_DIR`, or none. Where proofs run, leave
it unset: `keep = ["target"]` keeps each slot's own.

### Every landing waits for the whole proof

**What you see.** `avp land` waits for the proof each time, right after the agent ran the same tests
itself: 233 s against 151 s on Git for three tasks.

**Why.** Through the MCP server, AVP proves early: once a task's files are quiet for 3 s, they go to
the server, which proves that tree while the agent runs its own tests, and the landing reuses the
verdict; a newer tree stops the older proof, on workers too. The CLI alone does not: `avp land`
proves then.

**The fix.** Let agents work through the MCP server, which `avp up` connects, and check that
`AVP_SPECULATE=0` (which turns the early proof off) is not set. With it, the same tasks took 146 s.

### Agents take twice the turns they take with Git

**What you see.** 15 to 24 turns on a task an agent does in 5 or 6 with Git.

**Why.** Half was the benchmark: a tool allowlist on the AVP side that a shell hook got around, and
the refusals the agent paid for. Half was the protocol the agents were told: `avp_tasks` and
`avp_events` first, `avp_check` before `avp_land`.

**The fix.** The same permissions on both sides, and the rule `avp up` writes: `avp_open`, edit,
`avp_land`. `avp_open` already says who else is working, and `avp_land` proves.

## The server

### `avp serve` refuses to listen beyond this machine

**What you see.** `0.0.0.0:7117 is reachable from other machines, and nothing would check who is
asking: set AVP_TOKEN ...`.

**Why.** Anyone who reaches the server reads the code and lands changes. It listens on `127.0.0.1`
unless something checks who asks.

**The fix.** `AVP_TOKEN` on the server (clients set the same, and `AVP_USER`), or keep it on
`127.0.0.1` behind a proxy that authenticates and sets `X-Avp-User`. `AVP_INSECURE=1` only when the
network is yours alone.

### `avp land` is cut after about four minutes

**What you see.** A landing from a laptop, through a load balancer, ends after 271 s with the
connection closed, while the proof goes on on the server.

**Why.** Nothing crossed the connection while the proof ran, and a load balancer or a NAT drops a
connection it takes for idle.

**The fix.** A client that sends `x-avp-keepalive` gets a space every 15 s while a land or a check
is worked out: the CLI and the MCP server send it. An older client: `avp update` takes the server's
binary.

### An update waits: the server drains

**What you see.** A new version is not taken for minutes. `GET /work` says `draining: true`, and an
agent's `avp land` prints `the server is updating to a new version: your proof starts in a moment,
once the proofs already running end (nothing to do, this waits)`.

**Why.** A restart cuts the proofs and deploys in flight. With agents landing all the time there is
no pause to wait for, so the update makes one: `POST /drain` starts no new proof nor deploy, lets
those that run end, and holds what comes.

**The fix.** Nothing, for the agents: their land asks again once the new server answers (`the server
restarts on its new version: asking again once it answers`), and a deploy that waited runs on it.
For the person updating, from the server's own machine or with its `AVP_TOKEN`:
`curl -X POST http://127.0.0.1:7117/drain`, then restart once `GET /work` says `running` and
`deploying` are 0. The drain lifts with `DELETE /drain`, with the process, or at its bound
(`?minutes=`, 10 by default, 60 at most): a proof longer than that keeps it waiting, so drain again
with a longer bound. A restart before that cuts the proofs that run; their agents land again.

### Agents, deploys or tests cannot reach what they need once the server is in a cloud

**What you see.** After the server moves to a cloud machine, agents in sandboxes time out on
`avp_open` while a laptop reaches it; or a `run` deploy, a test or the mirror's push is refused by
what it calls.

**Why.** Both ways go by address. A sandbox with an egress allowlist lets out only the addresses it
names, and the server's new one is not among them: at Murmell, the agents' allowlist named the old
server's address, and the cloud server's had to go beside it. And what AVP calls sees it by its
outbound address:

- the server: the mirror's push (to the Git remote, over SSH or HTTPS, from the machine that runs
  `avp mirror`), `run` deploys (whatever the command calls: a cluster's API, a registry), the
  webhooks of `[notify]`, and the proofs no worker claimed in time;
- the workers: the setup's installs (package registries), the services' images (a container
  registry), what the tests call;
- whoever runs `avp import-pr` or `avp stats --github`: GitHub's API.

**The fix.** Give the server a fixed public address, add it to the agents' egress rules (the port
the server answers on), and to every allowlist that guards something it calls. Workers whose
addresses change (machines started and removed with the queue) reach a target that filters by
address only through a fixed outbound address; and since the server proves what no worker takes,
allow its address wherever the workers' is.
