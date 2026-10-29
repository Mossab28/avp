# AVP guide

New to AVP? Start with [Your first 15 minutes](QUICKSTART.md), and [the concepts](CONCEPTS.md) for
the words this guide uses.

What the README leaves out: the checks, the environments, a person in the loop, AVP without
`avp up`.

## Your checks, your environments

`avp.toml` is yours to edit:

```toml
[check]
setup = "pnpm install --frozen-lockfile --prefer-offline"   # once per lockfile, kept between proofs
setup_keys = ["package.json", "pnpm-lock.yaml"]
setup_links = ["node_modules"]

[[check.steps]]
name = "typecheck"
run = "node_modules/.bin/tsc --noEmit -p ."

[[check.steps]]
name = "tests"
run = "node_modules/.bin/vitest related --run --passWithNoTests --shard={shard} {files}"
needs_files = true
split = 40                     # about 40 related test files per job, the jobs side by side

[[check.full_shards]]          # the whole suite, after each batch of landings, one job per shard
name = "web"
run = "pnpm --filter web test"

[[check.services]]             # up before the steps of each proof and shard, removed after
name = "db"
image = "postgres:16"          # by docker or podman; or `run = "<command>"`, without containers
ports = [5432]                 # each mapped to a free port of the machine, for this run only
ready = "pg_isready -U postgres"   # inside the container; or a port: ready = 5432
env = { POSTGRES_PASSWORD = "avp", DATABASE_URL = "postgres://postgres:avp@127.0.0.1:{port}/postgres" }

[github]
repo = "acme/shop"             # where AVP's bridge dispatches deploys and writes avp/full

[envs.staging]
follow = "full"                # AVP promotes each position whose whole suite is green
deploy = { github_workflow = "deploy-staging.yml", inputs = { sha = "{git}" } }
serves = "https://staging.acme.com/release.json"   # answers the commit it serves: it moves then

[envs.production]
after = "staging"              # only what staging serves
approvers = ["alice", "bob"]   # their promotion is the approval
deploy = { github_workflow = "deploy-production.yml", approve_environment = "production" }

[envs.preview]
follow = "full"
deploy = { run = "./scripts/deploy.sh preview {position}" }   # AVP runs it itself
```

A step with `split` runs as several jobs when the task reaches more related tests than one job's
share: AVP counts them through the imports, runs that many shares side by side with `{shard}` as
`i/n`, beside one job for the other steps, and stops every part as soon as one is red. The test
runner still decides which tests are related, so the proof runs the same tests, sooner.

Services are what the tests talk to: a database, a cache. AVP starts each `[[check.services]]`
before the steps of a proof or of a shard of the whole suite, on the machine that runs them (the
server or a worker), and stops and removes it after, green, red or cancelled. Each run gets its
own, on its own ports and with its own data: two proofs at once never share a database. A service
is an `image`, a container run by docker or podman (whichever the machine has), each of its `ports`
mapped to a free port of 127.0.0.1; or a `run` command, for a machine without containers, started
in the candidate's folder and stopped with what it started (keep it in the foreground), `{port}` a
free port and `{data}` an empty folder of its own: `run = "redis-server --port {port} --dir
{data}"`. Its `env` goes to the service and to every step, `{port}` (an image's first port,
`{port:6432}` another) and `{data}` filled in: it is how the tests find it.

`ready` says when a service is up: a command that exits 0 once it is (inside the container for an
image, where `pg_isready` and `redis-cli` are; in the candidate's folder for a `run` service), or
one of an image's ports, up once it answers. Without it, AVP waits for the service's first port. A
service not ready within `ready_s` seconds (60 by default) fails the run as the step `service
<name>`, with the reason and the last lines it printed, before any step runs. A shard of the whole
suite starts every service, unless it names those it needs: `services = ["db"]`, or `services = []`
for none.

A shard with a `matrix` runs once per value, each its own job: `matrix = { node = ["20", "22"] }` on
the shard `web` makes `web (node 20)` and `web (node 22)`, whose command reads the value as
`$MATRIX_NODE` (a key `node-version`, as `$MATRIX_NODE_VERSION`). With several keys, every
combination runs. A value is a string, a number or a boolean.

A full suite, or one of its `full_shards`, that goes red runs once more on the same tree: a browser
that did not start or a test that fails under load is not the position's fault. Green the second
time, the position's full suite is flaky: it counts as green for `avp promote` and `follow`, never
as red. `avp log` says `Flaky`, the event `line.flaky` names the shard and the failing test lines of
the red run (the lines with FAIL, `×` or `✗`), and AVP Live marks the position apart and lists the
flaky tests of the last 7 days. Red twice is red. A task's proof never runs again: it gates a
landing, and a red there is the agent's to read.

A full suite red twice after a landing does not leave the line red: with agents landing every few
minutes, the next ones would build on it. AVP finds the position that broke it. The batch is what
landed since the last position whose suite ran: one position, it is that one; several, AVP runs the
red shard on the positions in between, halving, down to the first red one. It takes that position
back as `avp revert` does, in a task of its own (person and agent `avp`) whose goal names the
position, the shard and the first failing lines, proven and landed like any other task. The event
`line.reverted` hands it back to the position's person and agent, with the failing output: `avp
events` shows it, `avp open` (and `avp_open`) says it for a day to the person and agent who landed
it and to any task opened on the paths it changed, and `avp show <position>` says it was reverted, by
which position, and why. The revert's full suite runs next, and environments that follow the full
suite take it once green: a red position, or one that landed after it before the revert, is never
promoted. A revert that changes a guarded path (avp.toml always is one) runs its proof under the
avp.toml it brings back and, green, stays open for a person's approval: `line.revert_failed` names
its task, `avp approve <task>` lands it as AVP's revert. Whoever approves a revert of avp.toml reads
its diff first (`avp diff <task>`): it takes back what that position changed in the checks,
environments and webhooks the line runs on. When the revert does not land green (a later position
builds on what it takes back, its lines changed again since), when the shard is red before the batch
too, or when the revert's own suite is red, AVP stops there: the line stays red for a person, as AVP
Live shows, and the event `line.revert_failed` says why. `[line] auto_revert = false` in avp.toml
turns it off.

Every proof starts from what a fresh checkout would hold: whatever an earlier proof wrote in its
folder is gone, save the setup's links and the folders `keep` names. `keep = ["target"]` (what
`avp up` writes for Cargo) works like a CI cache: the tool that builds a kept folder must know what
in it is stale.

Proofs run on the server, or on workers that claim them from a queue: start the server with
`AVP_WORKERS=1`, then `avp worker` on as many machines as you want. `GET /work` says how many proofs
wait and run (`waiting`, `running`: on workers and in the server's own slots), how many workers are
wanted (`demand`, what an autoscaler reads), and the deploys AVP runs (`deploys`, of which
`deploying` run now).

Before a server restarts on a new version, drain it: `curl -X POST http://127.0.0.1:7117/drain`, from
its own machine (not through the proxy in front of it), or with its `AVP_TOKEN`. It starts no new
proof nor deploy, what runs goes to its end, what comes waits, and `GET /work` says `draining: true`.
Restart it once `running` and `deploying` are 0: an `avp land` (or `avp_land`) that waited says so to
its agent while it waits, and asks again once the server answers, and a deploy that waited runs on
the server that starts. The drain lifts with `DELETE /drain`, at its bound (`?minutes=`, 10 by
default, 60 at most) and with the process.

A `run` deploy, AVP carries out itself, on the server: it lays the promoted
position out in the environment's own folder (what `keep` names stays from one deploy to the next)
and runs the command there as it runs a proof's steps, `{position}` and `{git}` filled in. It sees
what a step sees (Security, below), and the variables `AVP_DEPLOY_ENV` names, comma separated, where
the server runs: `AVP_DEPLOY_ENV=KUBECONFIG,REGISTRY_TOKEN,AWS_PROFILE`. Those reach deploys only,
never a proof's step, a full suite or a worker's job; a credential can also come from a file of the
server's user (`~/.kube/config`). Each line the command prints is the deploy's progress in `avp envs`
and AVP Live. There, in the output AVP keeps and in the events, the value of each variable
`AVP_DEPLOY_ENV` passed reads `***` when it is longer than 6 characters. Exit 0 moves the
environment; anything else leaves it where it was, and the `env.failed` event and `avp envs` say
why. `avp promote` answers once the deploy is over, as `avp land` answers once its proof is, and
like it keeps the connection alive, so a proxy in between does not cut a long deploy; the output is
kept as a proof (`avp proof <id>`, the id is in the event). One deploy runs at a time per
environment: a promotion made while a deploy waits for its turn goes instead of it, so an
environment that follows the full suite is promoted at once to each newest green position, and
deploys the newest when the deploy before it is over.

A deploy's state lives in the server's database, so a restart of the server does not lose it. A
deploy that waited for its turn runs once the server is back. One that was running is cut: the
server records `env.interrupted` and runs it again from the start, once, since the environment is
still asked to serve that position. Cut a second time, it is left failed (`env.failed` says why):
promote again. **A deploy command must be safe to run twice**: the server may stop halfway through
it, and the second run starts on whatever the first one left (what `kubectl apply`, `helm upgrade
--install` or `terraform apply` do, and what a script that creates without checking does not).

A `github_workflow` deploy is carried out by AVP's bridge to GitHub, inside `avp serve` when the
server has `AVP_GITHUB_BRIDGE=1` and a `GH_TOKEN` (or `GITHUB_TOKEN`) that may run the workflows,
approve their environments and write commit statuses. `[github] repo` names the repo on GitHub, and
`branch` the one workflows are dispatched on (the mirror's, `main` by default). Every 30 seconds,
for each promotion still on its way, the bridge dispatches the workflow on that branch with its
`inputs` (`{git}` the position's commit, `{position}` its number; a position the mirror has not
written yet waits for its commit), finds its run and records it: `deploy.dispatched`, with the run's
page. A run waiting for the GitHub environment `approve_environment` names is approved in the name
of the person who promoted, with a comment that names the position: their promotion is the approval
(`approvers`). Then the bridge follows the run: the step it is at (waiting for a runner, waiting for
an approval, `deploy: helm upgrade`) is the deploy's progress in `avp envs` and AVP Live, with the
run's page. Green, the environment moves (`env.moved`). Anything else leaves it where it was, and
`env.failed` names the job that failed (`deploy-staging.yml: job "deploy" failed`). A server
restarted in the middle follows the run it had found, never dispatches it twice, and does not tell a
failure twice. A promotion from before the bridge started that has no run and is older than a deploy
is given (`AVP_FOLLOW_STUCK_S`, 30 minutes) is not dispatched: its step says to promote again.

An environment's `serves` is a URL that answers the commit it serves: `{"sha": "<commit>"}`, or the
commit alone. The bridge reads it every round, four times, and when the four agree (during a
rollout, old and new pods answer in turn) and a position of the line has that commit, the
environment moves there, whoever deployed it. With `serves`, a GitHub deploy's environment moves
once that URL answers the promoted commit, not when the run is green.

The bridge also writes the full suite's verdict of each position as the commit status `avp/full` on
its commit (`success` when green or flaky, `failure` when red): a workflow or a branch rule waits for
it. It keeps within GitHub's rate limits: once GitHub says the token's limit is spent, it asks
nothing more until the reset, and it writes 10 statuses a round at most. Without
`AVP_GITHUB_BRIDGE=1` it does nothing, for a server whose GitHub side runs elsewhere.

## Logs and artifacts

Each step's output is kept whole, as a CI keeps it: every line with the time it came since the step
started, as a terminal shows it (a progress bar drawn again with `\r`, in its last state). Past 4 MB,
its first megabyte and its last three are kept, the lines between counted. `avp proof <id>` prints
each step's log (no id: the last proof of this folder's task), `--step <name>` one step's, and
`--json` its lines as `[milliseconds, text]` with `first`, the line where it first fails: a
compiler's `error`, a runner's FAIL, `×` or `✗`, a panic, a traceback. AVP Live shows each step of a
proof folded, with its duration; the red one is open at its first failing line. The proof itself
still holds the end of each output, what `avp land` and `avp check` print.

```toml
[[check.steps]]
name = "tests"
run = "node_modules/.bin/vitest run --coverage"
artifacts = ["coverage/"]

[[check.full_shards]]
name = "e2e"
run = "pnpm exec playwright test"
artifacts = ["playwright-report/", "test-results/"]
```

What a step or a shard names in `artifacts` is packed once it has run, green or red, and kept with
the proof for 14 days: a coverage report, Playwright's traces and screenshots. Each path is a file or
a folder of the step's folder, relative and without `..` (an avp.toml that names another is refused),
and a link is never followed: nothing outside the folder is packed. A proof keeps 100 MB of files at
most (as much for each shard of the whole suite, and each job of a split step); `avp proof` names the
files past it and the paths the step did not make. `avp proof <id> --artifacts <dir>` downloads them,
a folder per step (`<dir>/tests/coverage/index.html`), unpacked by the machine's `tar`; AVP Live links
each step's pack, a `.tar.gz` (`GET /repos/<repo>/proofs/<id>/artifacts?step=<name>`). A worker sends
a step's log and artifacts back with its result and keeps none. A secret a step prints reads `***`
in its log, as in its output; a file it writes is packed as written. The routes: `GET
/repos/<repo>/proofs/<id>` says for each step how many lines its log holds (`log`) and what its
artifacts hold (`artifacts`); `.../log?step=<name>&window=<n>` gives its lines, `n` of them from just
before the first failing one when `window` is set.

An agent reads a red proof with the MCP tool `avp_proof`: the steps with their durations, then 60
lines of the red step's log from just before where it first fails (the end of a test run is its
summary, rarely the cause), and the artifacts kept, for a person to download. `step` reads another
step, `lines` more or fewer.

## Secrets for tests

Integration tests need keys: a payment sandbox key, an API test token. AVP keeps them per repo, on
the server, and hands each to the steps that name it:

```sh
avp secret set STRIPE_KEY < stripe-test-key.txt   # or: printf %s "$KEY" | avp secret set STRIPE_KEY
avp secret list                                   # names and dates, never a value
avp secret rm STRIPE_KEY
```

The value is read on stdin, never as an argument (process lists and shell histories keep those),
without the newline a file or `echo` ends it with; at a terminal it is typed unseen. It is 7
characters or more, else it could not be masked. The server keeps it encrypted in its data folder
(`secrets.json`, sealed with `secrets.key`, both 0600): back `secrets.key` up with the rest, without
it the values are set again. Each value is sealed with XChaCha20-Poly1305 (the RustCrypto
`chacha20poly1305` crate), under a random 24-byte nonce of its own, with the repo and the name as
associated data: a value changed, or moved to another name or repo, does not open. Values an
earlier server sealed with its own blake3 construction are resealed this way when it starts. No route
returns a value, and `avp secret` has `--json` like the rest. An agent reads the names and dates
with the MCP tool `avp_secrets`, to tell a person which one a red step is missing.

```toml
[check]
secrets = ["STRIPE_KEY"]          # every step and the whole suite get it

[[check.steps]]
name = "integration"
run = "pnpm test:integration"
secrets = ["SENDGRID_TEST_KEY"]   # this step only

[[check.full_shards]]
name = "e2e"
run = "pnpm test:e2e"
secrets = ["BROWSER_TOKEN"]       # this shard only
```

A step gets only the secrets `[check] secrets` and its own `secrets` name, as variables, on the
server or on a worker: the server sends their values with the job a worker claims, and the worker
writes them nowhere. A step that names one not set goes red and says how to set it, rather than
running without it. What a step prints of a value reads `***` in all AVP keeps and shows: the proof
(`avp proof`, `avp land`), the whole suite, the events, AVP Live; each line of a value written on
several (a key) too. Printed changed (encoded, cut), it shows. The names a step never gets stay
refused here too (`AVP_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `AWS_*`, `CLAUDE_*`, `ANTHROPIC_*`,
`OPENAI_*`), and so are `AVP_*` and what AVP gives every step (`PATH`, `HOME`, `CI`...). The events
`secret.set` and `secret.removed` say who changed which name.

## Rollback

`avp rollback <env>` (`POST /repos/<repo>/envs/<env>/rollback`) takes an environment back to the
position it served before the one it serves now, through its deploy as `avp promote` does, and
answers as it does, once a `run` deploy is over.
It is recorded as a rollback in the environment's history: its `env.promoted` and `env.moved` events
say `"rollback": true` and the position it came `from`. A rollback is what an emergency needs at
once: the full suite and `after` are not asked again (that position passed them to be served), the
`approvers` are. A second rollback takes the environment back where the first one found it.

## Notifications

```toml
[notify]
webhooks = ["$AVP_NOTIFY_SLACK", "https://hooks.internal.example/avp"]
```

AVP posts a short JSON to each webhook when the full suite is red (on its retry too), a position is
reverted (by AVP, or by the landing of a person's `avp revert`), a task waits for a person's
approval, a deploy is done or failed (an environment with no deploy moves without a word):

```json
{"text": "app: the full suite is red on position 12 (Add Apple Pay), on its retry too. `avp proof proof_01...` says why.",
 "repo": "app", "event": "line.red", "position": 12, "task": "task_01...", "proof": "proof_01..."}
```

`text` is what Slack shows (Mattermost and Google Chat read it too, Discord at its `/slack` URL);
`event` is the kind of AVP event (`line.red`, `line.reverted`, `task.landed`,
`task.awaiting_approval`, `env.moved`, `env.failed`), with `env` for the last two, and for
`line.reverted` `by` (the revert's position) and `shard` (the one that was red). Each post is tried
4 times over half a minute, on a thread of its own: a webhook that does not answer never holds the
line. A task that waits for a person is told once per proof, however many times its agent lands
again.

A webhook's URL often carries its token (a Slack URL is one). avp.toml is read by every task, the
mirror's git and AVP Live: put the URL in the server's environment instead, in a variable whose
name starts with `AVP_NOTIFY_`, and name that variable in avp.toml (`"$AVP_NOTIFY_SLACK"`); avp.toml
reaches no other variable. No proof's step sees it (no `AVP_*` does), it is in no event, and the
server's log names only the host of a webhook that failed.

## Scheduled suites

```toml
[[check.schedule]]
at = "02:00"          # UTC, every day
shards = ["e2e"]      # of [[check.full_shards]] (`full` for `full`); none: all of them
```

At that time AVP runs the full suite, or the shards named, on the head of the line, whatever landed:
what a nightly CI run catches (a date, a dependency, a service that changed under a green line). A
`line.scheduled` event says it started. It runs as any full suite does, its red shards once more,
and a red is the position's, as `line.red` (with `"scheduled": "02:00"`) and as a notification.
Some of the shards green say nothing of the whole: the position's full suite stays what it was. It
runs once a day, within the hour after `at`: a server down at that minute runs it once back, within
the hour.

## Your CI, as avp.toml

`avp import-ci` reads the CI a team already has, `.github/workflows/*.yml` and `.gitlab-ci.yml`,
and proposes the avp.toml that does its work:

```sh
avp import-ci            # prints it, then what it could not translate (--json for an agent)
avp import-ci --write    # writes avp.toml, or avp.imported.toml beside the one there is
```

- Each job a push or a pull request runs becomes a `[[check.full_shards]]`: its commands, its
  `services:` as `[[check.services]]` (a URL that named `localhost:5432` names the port AVP picks,
  `{port}`), its matrix as the shard's `matrix` where its commands read the value
  (`${{ matrix.shard }}` becomes `${MATRIX_SHARD}`). A matrix that only sets up the runner (a Node
  or Python version) runs once, on the machine that proves.
- From those jobs, the install becomes the setup, and the typecheck, the lint and the tests the
  proof, as `avp up` would write them: vitest and jest run the tests related to the task's files,
  Python tools run from the setup's virtualenv. End-to-end tests stay in the full suite.
- A deploy job (an `environment:`, or a name that says deploy) becomes `[envs.<name>]`: a GitHub
  workflow, which AVP's bridge dispatches with the promoted commit as its `sha` input, or a GitLab
  job's commands as a `run` deploy. One that ran on every push follows the full suite; production
  comes after staging.
- What a job reads that the CI keeps (`secrets.X` and `vars.X` on GitHub, a CI/CD variable on
  GitLab) becomes `secrets = ["X"]` on the shard and the step that read it
  ([Secrets for tests](#secrets-for-tests)). The values stay in the CI: what it prints ends with the
  `avp secret set X` a person runs once for each, the value on stdin (`secrets` in `--json`). A
  deploy's stay the server's (`AVP_DEPLOY_ENV`).
- A schedule (GitHub's `on.schedule`, GitLab's `only: schedules` or a rule on
  `$CI_PIPELINE_SOURCE == "schedule"`) becomes a `[[check.schedule]]` at the cron's hour and minute,
  UTC, with the shards of that workflow ([Scheduled suites](#scheduled-suites)). A schedule runs
  once a day, every day: a cron of several times a day, or of some days only, becomes its first time
  of the day, every day, and is listed. GitLab keeps a schedule's time out of the file: `at` is
  04:00, its every-day preset, until you put yours. A job that ran only on a clock is a shard like
  any other, run after each batch of landings too: listed as well.
- `actions/checkout`, `actions/setup-node`, `-python`, `-go`, `-java` and the caches have their
  AVP equivalent: the versions they set up are listed at the top of the file, for the machines that
  prove. Any other action, a GitLab `include`, a name a step never gets (`GITHUB_TOKEN`, `AWS_*`)
  is listed under "Not translated", one line each with what to do.

What it prints is an avp.toml as it stands, the list and the `avp secret set` lines in comments
after it. It writes over no file. `avp up` mentions it when the repo has a CI.

## AVP and a merge queue

A merge queue (GitHub merge queue, Bors, Mergify) and AVP want the same thing: a trunk that stays
green when many people land at once. They get there differently.

What a merge queue does well:

- It works with what you have: branches, pull requests, your CI as it is, human review.
- It tests the exact result of merging, one pull request on top of the next, so two green pull
  requests cannot make a red trunk.
- It runs your whole CI on each one. That is slower, and it is also the most complete check.
- It is a product people know, with support, and it does not ask a team to change how it works.

What AVP does differently:

- A task is proven in seconds, on the exact head of the line, while the agent is still writing:
  the proof starts as soon as the agent asks, and only the typecheck and the tests related to the
  change run (`[check]`). The whole suite runs once per batch of landings, after them.
- If the line moved under a task, AVP replays the task on the new head, and proves it again when
  it must. A rebase conflict is rare: only a change to the same lines of the same file ends in
  markers, which the agent settles, then lands again.
- There is no branch and no pull request. A task is a folder, a goal and a proof.
- Agents get an API made for them: the MCP server (`avp_open`, `avp_check`, `avp_land`,
  `avp_wait`, `avp_why`, and more), not a web page made for people.

The price: a proof is not the whole suite. A break the related tests miss is caught by the next
batch run, not before the landing. A merge queue that runs everything catches it first, but takes
minutes per pull request. Choose by what you want to wait for.

## Releases, hotfixes and long work

AVP keeps one line, the trunk. There is no release branch and no maintenance branch.

**A release** is a tag: `avp tag v1.2 41` names position 41 (Bisect, pick, tags, below). The
mirror writes every position as a git commit, with the trailers `AVP-Position` and `AVP-Task`, and
each tag as a git tag on its position's commit: a workflow keyed on tags builds the release as it
did. `avp show <position>` prints the commit of a position, once the mirror has written it. To
serve a position, `avp promote production v1.2` (or the position's number) moves an environment to
it: it needs a green full suite on that position, and `after` and `approvers` in `avp.toml` apply.

**A hotfix** is a task on the line. Open it, fix, land it: it is proven like any other task.
Then promote it, as above. Two things to know. The line is linear, so a hotfix carries every
landing before it: it cannot be cut from an older release. And the whole suite must be green on
its position before `avp promote` accepts it. In an emergency, `avp promote <env> <position>
--force` skips the full suite and the `after` rule; it does not skip `approvers`. `avp rollback
<env>` takes an environment back to what it served before, the same way.

**Long work** is a stack of tasks. A task is made for minutes; work that takes days is several
tasks, each one building on the one under it before that one lands, each proven on its own:

```sh
avp open --goal "Parser: the new syntax tree"                           # task_01k8...
avp open -C ../editor --goal "Editor reads the new tree" --on task_01k8   # another folder
avp land --stack                                                          # from ../editor
```

`--on` takes an open task's id, or the start of one. The new task stands on that task's work as it
is now, not on the head of the line: its folder holds both, and its writes go on top. It lands only
after the task under it: `avp land` answers `stacked` while that one is open, and `avp land --stack`
lands the whole stack, the bottom task first, each one proven on the head the one under it just
made. It lands what the server holds of each: changes still unsent in their folders stay there. The
first task that does not land stops the stack and the answer says which one and why; what landed
under it stays landed. A stack is as tall as the work needs.

When the task under it changes (its agent writes more, or it is replayed on a line that moved),
`avp refresh` replays the stacked task on its latest: its own changes merged on top, markers where
both changed the same lines, as a landing replays a task on the line. Once the task under it has
landed, the stacked task is a task like another: its land replays it on the head.

If the task under it is abandoned, or lands and is reverted, the stacked task stays open: it lands
its own changes without that work, and proves them again. Its agent hears of it
(`task.parent_abandoned`, `task.parent_reverted` in `avp events`, a line in `avp status`), and `avp
refresh` lays it out; what needed that work goes red until it comes back.

A stack shares its leases: a stacked task writes the files the task under it holds, under
`--policy strict` too, and takes nothing from it. `avp tasks` lists each stack together, a stacked
task right under the one it builds on, and so does AVP Live. Agents have `avp_open` with `on`,
`avp_land` with `stack`, and `avp_refresh`. `avp wait --task <id>` is for the other case: a task that
needs another's work on the line, without building on it before it lands.

Work that already lives in a git branch comes in by its pull request. Run, from a checkout:

```sh
avp import-pr 123 --repo my-project --land
```

`import-pr` merges the change three-way onto the git commit of the task's base position, proves
it, and with `--land` lands it and closes the pull request with its position. Without `--land`
it stays an open task. If it conflicts with the line, the task is abandoned and the files are
named: merge the line into the branch, then import again. It needs `GH_TOKEN` and a mirror that
has written the base position. To take every pull request as it comes, see
[Git keeps coming in](#git-keeps-coming-in).

## Git keeps coming in

A team moves over in days, not at once. Meanwhile commits still reach `main` by Git (someone not
switched yet, a merge on GitHub), and pull requests keep coming (Dependabot, Renovate, outside
contributors). The server brings both into the line by itself when `avp.toml` says so:

```toml
[git]
remote = "https://github.com/acme/shop"   # the Git repo the server fetches from
follow = "main"                           # each new commit of main becomes a task
pull_requests = true                      # each open pull request too ("land": landed when green)
every = 60                                # seconds between two looks, the default
```

Nothing else has to run. The server keeps a clone of its own under its data folder. A private
repo on GitHub over https is fetched with `GH_TOKEN` from the server's environment, given to git in
its environment only; pull requests need `GH_TOKEN` in any case, to read, comment and close them.
Each change of what the follow does is a line in the server's log (`git shop: ...`).

**Commits.** Each first-parent commit of `main` the line does not have yet, oldest first, becomes a
task on the head, opened as the commit's author (its email) through the agent `git`, its subject as
the goal. Its change against its parent is merged three-way onto the head, as a landing merges: what
the line changed meanwhile stays. Then it is proven and landed like any task. A commit the line was
imported from, or one the mirror wrote, is never taken back in. When the line has not moved since
the commit's parent, the new position is that very commit: the mirror goes on from it.

A commit that does not land (a red proof, a conflict, `avp.toml` or a guarded path waiting for a
person) leaves its task open, and the commits after it wait for it: they build on it. `avp tasks`
shows it, through `git`. To settle it, run `avp import-git shop` from a checkout: it lays the waiting
task out under `~/avp` (or `--views`). Fix it there and `avp land`, or `avp abandon` to leave the
commit out. The next commits then come in.

**Pull requests.** Each open pull request that is not a draft becomes a task, as `avp import-pr`
makes one, and comes in again once updated (its earlier task, still open, is dropped). With
`pull_requests = "land"`, the green ones land at once. Otherwise they wait for a person: `avp
approve <task>` lands a green one. Once its task lands, the pull request gets the position as a
comment and is closed.

**The mirror.** When `main` got commits by Git that the line has taken in (landed, or left out by a
person), the mirror's next commit on `main` merges them: its push stays a fast-forward, and `main`
keeps its own history. Mirroring on `main` while following it is how a team keeps both ways open
during the move (MIGRATE.md, step 3). A commit not taken in yet still makes the push refuse
(`mirror.blocked`) until the follow brings it in.

The same from a checkout, without the server doing it:

```sh
avp import-git shop                          # once: the commits of HEAD the line does not have
avp import-git shop --follow main            # again and again: main fetched from origin every 60 s
avp import-pr --repo shop --follow           # open pull requests as they come (--land: landed when green)
```

On a server that names who is asking (`AVP_TOKEN`, a proxy), a task opened from a checkout is that
person's: only the server's own follow opens them as the commits' authors. `--json` gives an agent
each commit, its task and what became of it; `avp import-git` exits 2 when one is left open.

## A team server

Everyone runs `avp up` in their own checkout with `AVP_SERVER` set to the team's server, a plain
address, and the secret in `AVP_TOKEN`: their own token on a server with accounts (below), or the
server's one token with their name in `AVP_USER` (or `AVP_PERSON`). The client sends them as a
header, so the secret never sits in a URL that ends up in configs and logs. A name and password in
the URL still work. The repo is
already there and so is `avp.toml`: `avp up` only wires the coding agents it finds, and starts no
mirror, since the server mirrors its own line (`--mirror` to run one from a checkout anyway).

**People, tokens and roles.** A server without accounts trusts whoever reaches it: one token for
everyone, or a proxy that authenticates. To know who did what and limit who may do what, give each
person an account. The first `avp user add`, from the server's own machine or with its
`AVP_TOKEN`, turns accounts on:

```sh
avp user add ada --email ada@example.com --github ada-l  # the first person: an admin of the server
avp user add bob                         # prints bob's first token, once: hand it over
avp user role bob write --repo shop      # read, write, admin, or none
avp user list                            # the people, their roles, their tokens
avp user log                             # every change of people, tokens and roles: what, who, when
```

Each person then puts their own token in `AVP_TOKEN`. The person on a task, an approval or a
promotion is the token's, whatever `AVP_PERSON` says, and a request that carries no person's token
is refused.

```sh
avp token create --name laptop --expires 90d  # shown once, kept hashed: it goes in AVP_TOKEN
avp token list                                # yours that work: when each ends, when it was last used
avp token revoke tok_01...                    # stops it at once
avp whoami                                    # who the server takes you for, and your roles
```

| Role | May |
|---|---|
| `read` | read the line: `log`, `show`, `why`, `blame`, `envs`, `tasks`, AVP Live |
| `write` | that, and open, check and land tasks, approve or refuse one |
| `admin` | that, and guards, the landing rule, environments, secrets, the repo's roles |

A repo you have no role on answers as one that does not exist; whoever creates a repo is its
admin. An admin of the server (`--admin`, or `avp user admin <name>`) has every right:
the people, every repo, the workers (a worker's job carries the secrets its steps name). Promoting
and rolling back go per environment, in avp.toml:

```toml
[envs.production]
after = "staging"
promoters = ["ada", "bob"]   # the same list as `approvers`: their promotion is the approval
```

The people listed promote and roll back that environment whatever their role on the repo; an
environment that lists nobody is promoted by the repo's admins.

`AVP_TOKEN` on the server stays its own key, with every right: give it to its machines (workers, a
mirror), not to people. A request from the server's own machine (127.0.0.1, through no proxy) has
every right too: the way back in when the last admin's token is lost. A proxy in front keeps TLS
but drops its own password, since the token travels in the same header. Every change of people,
tokens and roles is an event: `user.added`, `user.removed`, `user.admin`, `token.created`,
`token.revoked` (`avp user log`), and `role.set` in the repo's events.

AVP Live asks a person to sign in on a server with accounts: with a token of theirs (the page keeps
a session of its own for 14 days, until Sign out), or with GitHub when the server has an OAuth app:
`AVP_GITHUB_CLIENT_ID` and `AVP_GITHUB_CLIENT_SECRET` in its environment, and
`https://<server>/auth/github/callback` as the app's callback URL. A GitHub login signs in the person
added with `--github <login>`.

## Diff, clone, save points

```sh
avp diff                     # what this task changes against the position it stands on
avp diff --head src/         # against the head of the line, only what is under src/
avp diff --stat              # each file with the lines it adds and removes
avp diff 41 42               # from position 41 to 42; `avp diff 41`: from 41 to this task
avp clone https://avp.example.com/shop   # the head in a new folder, on a machine with no checkout
avp save "parser done, tests green"      # a save point in the task
avp saves                                # its save points
avp undo --to 1                          # back to one, or forward, by number or message
```

`avp diff` sends what the folder changed first, then prints the task's change as git prints a diff,
colored on a terminal (`git apply` takes it as it is): against the position the task stands on, or
with `--head` against the head of the line, where what landed since shows too, reversed. Paths after
the positions keep those files only (a folder: what is under it). `--stat` gives each file with its
lines added and removed, `--json` the files, their counts and their diffs. The MCP tool `avp_diff`
gives an agent the same diff, compact: one line of context, cut past 400 lines with the way to ask
for the rest.

`avp clone <server>/<repo> [dir]` (or `--server` and `--repo`) is for a machine with no git checkout
of the repo: a new laptop, a container, the sandbox an agent runs in. It lays the head out in a new
folder (the repo's name by default), makes it a view, and wires the coding agents it finds as `avp
up` wires a checkout. The files that takes (`.cursor/mcp.json`, `AGENTS.md`...) are named in
`.avp/exclude`, never sent as a task's writes, and `AVP_TOKEN` is kept outside the folder as `avp up`
keeps it. Then start an agent there, or `avp open`: the task opens in that folder.

A save point names a state of a task while it works. `avp save "<message>"` sends the folder's
changes, then numbers the state (1, 2, 3, with the sequence number of its last write); `avp saves`
lists them, `avp status` too. `avp undo --to <number or message>` takes the task to one, back or
forward: each file as the save had it, written again, and a file the save had not changed back to
the base. Nothing leaves the journal, so later saves stay reachable. When the task was replayed on
the line since (`avp refresh`, a land that met a conflict), the save is replayed too: its changes
merged with the line's, markers where both changed the same lines. Save points are kept with the
task and never land: the line gets the task whole. Agents have `avp_save`, and `avp_undo` with
`save`. They need the server, which keeps them: offline, `avp undo --path` and `avp undo` still work.

## Bisect, pick, tags

```sh
avp bisect --check "cargo test parser"      # the first position the check fails on
avp bisect --check "pnpm vitest run src/price.test.ts" --good 120 --bad 140
avp pick 42                                 # position 42's change, into the open task
avp tag v1.2                                # names the head; `avp tag v1.2 41` names position 41
avp tag -f v1.2 43                          # moves the name; `avp tag -d v1.2` deletes it
avp tags                                    # the tags, newest position first
avp promote production v1.2
```

`avp bisect` finds the position that broke something, as `git bisect run` does. The check is a
command, run by `sh -c` in a scratch folder of its own (under `$TMPDIR`), where each position is laid
out as a fresh checkout would hold it, with `$AVP_BISECT_POSITION` set: exit 0 says the position is
good, any other exit bad. It checks `--bad` first (the head by default), then `--good` (by default
the newest position below whose full suite is green, else 0; when the check fails there, position
0), then halves the positions in between, as AVP's own revert of a red full suite does, and names
the first bad one: its goal, its task, person and agent, its proof, the check's last lines. What the
ignore rules hide (a `target/`, a `node_modules/`) stays from one position to the next, so each
build starts warm; whatever else the check wrote is gone before the next. The folder is left on the
first bad position, to look at. A check that passes on the bad end, or fails on the good one, blames
nobody and says so. `--json` adds each position checked with its exit and time; agents have the MCP
tool `avp_bisect`.

`avp pick <position>` brings a position's change into the open task, as `git cherry-pick` does,
three ways, as a replay merges: a file the task holds as it was before the position takes the
position's version, one changed since gets the change merged in, and where both changed the same
lines it is laid out with conflict markers to settle, as after a replay. A file deleted on one side,
or binary, keeps the task's version and is named. It is how a change AVP took back
(`line.reverted`) comes back on the head with its fix: open a task, `avp pick 42`, fix, `avp land`,
proven like any other. What the folder changed is sent first, and the task's tree is laid out after.
Agents have `avp_pick`.

A tag names a position of the line: `avp tag v1.2` the head, `avp tag v1.2 41` position 41. A name
given stays where it is unless `-f` moves it; `-d` deletes it. A name is one git takes as a tag
(letters, digits, `.`, `_`, `-`, `/`, `+`), never a number alone, which reads as a position. `avp
log` shows each position's tags as `git log --decorate` does, `(tag: v1.2)`, AVP Live beside its
goal, and `avp promote <env> v1.2` promotes the position a tag names. The events `line.tagged` and
`line.untagged` say who named or deleted what; the routes are `GET` and `POST
/repos/<repo>/tags` (`{"name", "position", "force"}`) and `POST /repos/<repo>/tags/remove`.

The mirror writes each tag as a git tag on its position's commit, and pushes it after the branch,
so a release workflow keyed on tags (`on: push: tags: ['v*']`) runs as it did. A tag moved or
deleted on the line is moved or deleted in git on the next pass, from the commit AVP wrote it on
only (a push with `--force-with-lease`): a git tag of that name AVP did not write is left as it is,
and the mirror's output says so. The server remembers where each tag was written: a pass with
nothing new does nothing to git's tags.

## Measure it

`avp stats` prints a team's numbers on AVP over a window (the last 7 days by default), in the
README's words; `--json` gives them as JSON:

```sh
avp stats --since 48h          # or 7d, 2026-10-25, an RFC3339 time; --until the same way
avp stats --since 2026-10-25 --until 2026-10-27 --github acme/shop --before 2026-10-22..2026-10-24
```

What it reads, the server already records: each landing's time from its task's opening to the line
(to `main`), to the first full suite verdict on it or a later position, and to each environment
that serves it (to staging), median and p90; how long a task's proof takes (checking one change);
the machine minutes of proofs and full suites, on the server and on workers alike (a split step
counts each of its shares, a shard that ran twice counts twice); red proofs, tasks replayed
because the line moved and the conflicts handed back, red full suites, flaky reruns, reverts;
landings per agent and per person.

With `--github owner/name --before <from>..<to>` (`2026-10-22..2026-10-24` is the 22nd and the
23rd), it reads the window before AVP from GitHub's API with `GH_TOKEN`, which it never prints: the
pull requests merged in it (count, opened to merged median and p90), the job minutes of its Actions
runs (reruns included), and the share of runs of a workflow whose name or file says "deploy" that
were cancelled. Both columns come side by side, as the README's table; when the two windows do not
last as long, "more" and "less" compare per day. `--save` keeps that before on the server (its
numbers, never the token; a new save replaces it): AVP Live's Overview and Stats compare to it,
labeled "before AVP", `avp stats` adds it as a last row, and `GET /repos/{repo}/before` reads it.
Without it, AVP Live compares to the same window just before.

AVP Live and agents read the same numbers: `GET /repos/{repo}/stats?since=&until=` (each an RFC3339
time, a date, or `48h`) and the MCP tool `avp_stats`.

## Your agents

`avp up` wires each coding agent installed on the machine (its program on `PATH` or its folder in
your home) for this checkout, through the agent's own project config: Claude Code with `claude mcp
add` (local scope), Codex in `.codex/config.toml`, Cursor in `.cursor/mcp.json`, Gemini CLI in
`.gemini/settings.json`, Copilot in `.vscode/mcp.json` (VS Code) and `.github/mcp.json` (the CLI),
OpenCode in `opencode.json`, Qwen Code in `.qwen/settings.json`, Kiro in `.kiro/settings/mcp.json`,
Zed in `.zed/settings.json`, Amp in `.amp/settings.json`, Windsurf (Devin Desktop) in
`.devin/mcp_config.json`. A file you have is merged into, never written over, and every file AVP
writes stays out of `git status` (`.git/info/exclude`); a config git tracks is left as it is. Cline
reads only a global file: `avp up` prints the entry to add there. The rule goes where each agent
reads it: `AGENTS.md`, `GEMINI.md` for Gemini CLI unless its `context.fileName` names `AGENTS.md`,
and for Aider, which has no MCP, a rule in CLI form (`avp open`, `avp land`) its `.aider.conf.yml`
reads. A repo with its own `AGENTS.md` keeps it: the rule waits in `~/.avp/` for you to add it.

No config in the checkout holds the server's address: it may carry a password. `avp mcp` takes it
from its environment, or else from what `avp up` kept for the checkout in `~/.avp/checkouts/`,
readable by you only (with `AVP_TOKEN` when it was set). Some agents load a project's MCP config
only once you trust the folder (Codex, Gemini CLI, VS Code, Copilot CLI, Zed) or approve the server
(`amp mcp approve avp`). `avp agents` (`--json` too) lists each agent: wired, found but not wired
with the line that wires it, or not installed. The agent's own name (its MCP client's) is the
position's agent: `avp log` and AVP Live say cursor, gemini-cli, copilot.

### Subagents

Give one agent a big job and let it split the work among its subagents: they share the agent's
`avp mcp`, and each one is an agent like any other. Each subagent calls `avp_open` itself and gets
a task and a folder of its own; it passes that `task` to the other tools (with a single task open,
nothing to pass). With several open and no `task`, a tool lists them and acts on none. Their
proofs and lands run side by side: a long proof holds nobody else. Overlaps settle as between
separate agents, by leases and replays. What tests and tools leave in a folder (`__pycache__`,
`*.pyc`, `.pytest_cache`, `node_modules`, `.DS_Store`) is never sent, even without a `.gitignore`.

## Protect the mirror's branch

The mirror only fast-forwards: when its branch holds a commit AVP did not write, it refuses to
push, overwrites nothing, and the line keeps moving without it. The server records `mirror.blocked`
(the branch and the first foreign commit) and AVP Live shows a red alert until a later push
succeeds (`mirror.pushed`). Protect the branch on the remote (no direct pushes, no force pushes)
so that only AVP writes it; work that belongs in the line goes through `avp open` and `avp land`.

When it is blocked: look at the commit the alert names. If it should not be there, reset the branch
to the last commit AVP wrote (the `AVP-Position` trailers say which) and the next mirror pass
pushes again. If it holds work worth keeping, bring it in: `avp import-git <repo> --follow <branch>`
(or `[git] follow` on the server) takes it in as a task, and once it lands the mirror's next commit
merges it, without a reset ([Git keeps coming in](#git-keeps-coming-in)).

## Offline

A dropped Wi-Fi, a plane, a server restarting: AVP keeps going without its server, for a person
and for an agent alike, and lands what was done once the server answers again.

Each view keeps, outside the repo, a replica of what it laid out (`$AVP_HOME/<server>/<repo>/`,
`~/.avp/` by default): the objects of every tree it laid out, and the line's last 100 positions
(goal, person, agent, proof, and what each one changed), refreshed whenever the server answers.
Offline, `avp log` (and `log --path`), `avp show`, `avp why`, `avp blame`, `avp context` and `avp
tasks` answer from it, and say so once on their first line, with the position and time they are
as of (`--json` keeps the JSON alone on stdout and says it on stderr). A blame follows the file
back through the positions the replica keeps; a line older than those is marked `^`.

What changes something while the server does not answer goes to the view's outbox
(`.avp/outbox.json`), in order, and answers `offline: queued (N waiting), lands when the server
answers`: the writes of `avp sync`, `avp add` and `avp watch`, and `avp undo`, `goal`, `claim`,
`release`, `abandon` and `land`. Each file written is kept in the replica, so the outbox holds all
it needs. `avp open` offline opens a task of this machine's (`local_...`) on the position the
folder holds (in an empty folder, the newest position the replica keeps whole); it opens on the
server, under the server's id, when the outbox reaches it. Offline, `avp undo` takes one path back
to the task's base (`--path`) or every write back (no number): a sequence number is the server's.

Offline, `avp check` and `avp land` run avp.toml's proof steps on this machine, as the server runs
them (a folder of the replica's, the setup, what `keep` names), and say green or red. A red land is
not queued: it is the agent's to fix, exactly as online. A green one is queued and the task closes
in the folder, so the next one can open, offline too. The local proof is advice: the server proves
the task again before it lands it. A step that needs a secret the server keeps is red here.

The next command that reaches the server sends the outbox first, in order; `avp watch` and the MCP
server try every 5 seconds. A server that does not connect within 5 seconds is offline; once
connected, nothing cuts a request (a land waiting for its proof, a big download). A task opened
offline opens on the server on the position the folder held (`POST /repos/<repo>/tasks?base=<n>`,
any position of the line, refused if unknown), and its writes go as they were made. Its land then
replays it on what landed since, exactly as for a task the line moved under: proven, conflicts
handed back. A server from before `base` opens it on the head instead, and each write is merged
here with what the server's task holds (three ways where the line changed the file too, with
conflict markers where both changed the same lines). A land that does not go (a conflict, a red
proof on the server) brings its task back into the folder to fix, and what was queued after it
waits until it lands. An entry leaves the outbox only once the server took it: a flush that stops
keeps the rest, nothing in it is ever lost. `avp status` says online or offline, lists the outbox,
and says what the last flush sent and landed; it works with no task open too.

An entry the server refuses (a write to a task abandoned meanwhile, an open on a position it does
not hold) stops those after it, so nothing goes out of order. `avp status` and the MCP `avp_status`
say so in one line, with the commands to run. `avp outbox` lists every entry: its number, when it
was queued, what it does, why its last send failed. `avp outbox retry` sends them again now. `avp
outbox drop <n>...` takes entries out, asking nothing, and says what it dropped: an open of a task
of this machine's takes that task's entries with it, and what a dropped write wrote stays in the
folder, unsent again (the next sync sends it, `avp open` carries it into a new task). All three
take `--json`; the MCP tool `avp_outbox` does the same (`retry`, `drop`).

The MCP tools do the same: offline, each answer says so in its first line and the agent keeps
working, edits queued, `avp_check` and `avp_land` proven on the machine, `avp_open` opening a local
task. What a flush between two calls sent and landed is said at the start of the next answer.

## A person in the loop

```sh
avp landing proof_and_approval   # a green proof waits for a person
avp guard infra/ migrations/     # or only for these paths
avp approve task_…               # or: avp refuse task_… --why "rename it"
```

## Without `avp up`

```sh
avp serve --data ~/.avp-data &
cd my-project && avp init my-project
avp open --goal "price: thin space before €" --scope src/price.ts
avp land
avp import-git my-project --history 200                          # from a Git checkout
avp mirror --repo my-project --branch avp/line --remote origin   # back to Git
```

`avp mcp` keeps a journal of what agents do with it (`~/.local/state/avp/mcp.jsonl`): which
client, which tool, how long, what came back. It is how we know agents reach for AVP by themselves.

## Install

`install.sh` puts the binary of the latest release in `~/.local/bin` (macOS and Linux, x86_64 and
arm64), or builds it with cargo when there is no release for your machine. From source:
`cargo install --git https://github.com/Mossab28/avp`.

## Security

- `avp serve` listens on `127.0.0.1`. To share a server, either set `AVP_TOKEN` on it (clients
  then set the same `AVP_TOKEN`, and `AVP_USER` says who they are),
  or keep it on `127.0.0.1` behind a proxy that authenticates and sets `X-Avp-User`. It refuses to
  listen anywhere else without one of the two, unless `AVP_INSECURE=1` says the network is yours.
  Accounts (A team server, above) give each person a token of their own and a role per repo: the
  server keeps only a token's hash, and AVP Live a session cookie of its own (HttpOnly, SameSite,
  Secure behind HTTPS), never the token.
- A proof runs the steps and the services of the `avp.toml` in the task's tree, on the machine that proves it, like
  a CI configuration in a pull request. On a shared server, `avp guard avp.toml` makes any task
  that changes it wait for a person.
- A step runs without the machine's secrets: it sees only what a build needs (`PATH`, `HOME`,
  `LANG`, `LC_*`, `TERM`, `TMPDIR`, `USER`, `SHELL`, `CARGO_HOME`, `RUSTUP_HOME`,
  `CARGO_TARGET_DIR`, `XDG_CACHE_HOME`, `COREPACK_HOME`, `npm_config_store_dir`), plus `CI=true`.
  Nor the server's own settings: a server run with `AVP_CHECK_SLOTS=1` would hand it to the
  project's tests. Name what else your steps need in `AVP_PROOF_ENV`, comma separated, where the
  proofs run (the server, the workers): `AVP_PROOF_ENV=HTTPS_PROXY,NODE_OPTIONS`. No `AVP_*`
  reaches a step unless listed there. `AVP_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `AWS_*`,
  `CLAUDE_*`, `ANTHROPIC_*` and `OPENAI_*` never do, even listed. A key the tests need is better
  kept by AVP (`avp secret set`, Secrets for tests, above): per repo, set without a restart, and
  only to the steps that name it.
- A secret for tests reaches code a task can change (a test file the step runs), and that code may
  print it changed or send it away: keep test keys there, never production ones. Changing which
  steps get one means changing avp.toml, which waits for a person. Who may claim a worker's job
  (the server's token, or an admin of the server) receives the values its steps name with it.
- A deploy's credentials go in `AVP_DEPLOY_ENV`, not `AVP_PROOF_ENV`: what it names reaches `run`
  deploys only, never a proof's step, a full suite or a worker's job, even when `AVP_PROOF_ENV`
  names it too. The names steps never get (`AWS_*`, `GH_TOKEN`...) reach a deploy when listed
  there; `AVP_TOKEN` never does, even listed. In what AVP keeps and streams of a deploy's output
  (`avp proof`, the events, `avp envs`, AVP Live), each of their values longer than 6 characters
  reads `***`. A shorter one, or one the deploy prints changed (encoded, cut), shows: a deploy
  should not print its credentials.
- The bridge's `GH_TOKEN` stays on the server: it goes to GitHub in a header, never into an event,
  an error, a step or a full suite. A `run` deploy gets it only when `AVP_DEPLOY_ENV` names it.
- A `run` deploy runs on the server the command of the line's avp.toml, which no task changes
  without a person's approval, in the promoted tree: a script that command calls is a file of the
  tree, which tasks change like any other, and it runs with what `AVP_DEPLOY_ENV` lists. `avp
  guard scripts/` puts it behind a person too.
- Every path a tree holds is checked, on the server and in the client that lays it out: nothing is
  written outside the task's folder.

AVP itself is developed through AVP: its own avp.toml proves every change with the release build and
the unit tests, and runs the end-to-end tests once it lands.
