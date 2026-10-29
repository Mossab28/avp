# From GitHub Actions to AVP in an afternoon

Run `avp migrate` in a checkout of `main`, on the machine that will serve the team: steps 1 and 2
below in one command, then what is left.

```sh
avp migrate          # --json for an agent; --yes lands the checks it proved, on your word
```

- It reads the repo: its CI (as `avp import-ci` does), its coding agents (`avp agents`), its server.
- It puts AVP beside `main` as `avp up` does: the server (started here when none answers), the line
  imported from the last 50 commits, the mirror on `avp/line` (never `main`), the agents wired. It
  writes `avp.toml` from the CI in the checkout, or keeps the one there and lists what the CI says
  otherwise.
- It proves it before anyone relies on it: the proof steps and the full suite, once, on the head of
  the line with that `avp.toml`, on this machine, the secrets the server keeps included when it runs
  here. Each is green or red, with its first failing lines and what to change: a secret not set (the
  `avp secret set` line), a service never ready, a tool not installed here.
- Green, it lands `avp.toml` as a task, which the server proves again. A change of the checks waits
  for a person: `--yes` is your word, or `avp approve <task>` later.
- It prints the plan of what is left, numbered as below (protect `main`, `main` becomes the mirror,
  the environments, `avp stats --github`), each with its command, and what it did not do: it never
  pushes to `main` and never changes a setting on GitHub.

Run it again after a fix: it starts from where it is (the server, the line, the mirror, the agents,
the task it opened and a green proof of the same tree are kept). `--json` gives the same report to
an agent, and the MCP tool `avp_migrate_report` returns the last one.

The steps below are what it does, by hand (1 and 2), and what it leaves to a person (3 to 5). They
are the path Murmell took, in its order; where Murmell wrote its checks and measured its before and
after by hand, `avp import-ci` and `avp stats` now do it in one command. Nobody changes how they work
until step 3: before it, AVP runs beside `main` and writes on a branch of its own. Each step gives
the command, what to check, and how to go back. When something does not look as described, see
[TROUBLESHOOTING.md](TROUBLESHOOTING.md); what it changed for Murmell is in
[CASE-MURMELL.md](CASE-MURMELL.md).

## 1. The line beside `main`

On the machine that will serve the team (its address and its token: the guide's
[A team server](GUIDE.md#a-team-server) and [Security](GUIDE.md#security)), in a checkout of `main`:

```sh
avp serve --listen 0.0.0.0:7117 --data ~/avp-data &   # AVP_TOKEN set, or 127.0.0.1 behind a proxy
avp up --name shop
```

`avp up` imports the last 50 commits of `main` as the first positions of the line, guesses the
checks into `avp.toml` and lands it, and mirrors every landing as a commit on `avp/line`, pushed to
`origin` within 15 s. Then whoever tries it, in their own checkout:

```sh
AVP_SERVER=https://avp.example.com AVP_TOKEN=... AVP_USER=<you> avp up --name shop
```

It connects their agents to the team's line and starts no mirror: the server's machine runs the
only one. Pull requests, CI and deploys go on as before; what agents land here reaches `avp/line`,
not `main`.

**`main` keeps moving.** Pull requests are still merged during steps 1 and 2: the line takes each
new commit of `main` in as it comes, as a task proven on its head and landed as its author. Add to
`avp.toml`, in a task (a person approves it: `avp.toml` is always guarded), the server's `[git]`
(the guide's [Git keeps coming in](GUIDE.md#git-keeps-coming-in)):

```toml
[git]
remote = "https://github.com/acme/shop"   # GH_TOKEN in the server's environment for a private repo
follow = "main"
```

A commit whose proof is red stays an open task, and those after it wait: `avp import-git shop` from
a checkout lays it out to fix and land, or to abandon.

**Check.** `avp log` lists the imported commits and the `avp.toml` landing. After a first task,
`git fetch origin avp/line && git log -1 origin/avp/line` shows its commit, with the trailers
`AVP-Position` and `AVP-Task`. AVP has not moved `main`. A workflow that runs on every push runs on
`avp/line` too: that is your CI judging the line's commits, at the price of its runner minutes
(`branches: [main]` under `push:` keeps it off).

**Go back.** `avp down` in the server's checkout stops the mirror; stop `avp serve` (its data stays
in `~/avp-data`), then `git push origin --delete avp/line`. Remove the `avp` MCP server from the
agents `avp up` connected (it printed which). Nothing else changed.

## 2. Your CI as checks

In a task of its own, so that the new checks are proven on the head like any change:

```sh
mkdir ci && cd ci && avp open --repo shop --goal "avp.toml from our CI" --scope avp.toml
avp import-ci            # prints the proposed avp.toml, then what it could not translate
avp import-ci --write    # avp.imported.toml beside the avp.toml avp up landed, never over a file
```

`avp import-ci` reads `.github/workflows/*.yml` (and `.gitlab-ci.yml`). The jobs a push or a pull
request runs become the full suite, one shard each, with their matrix; their install, typecheck,
lint and tests become the proof (`[check]`, the tests related to the task's files); their
`services:` become `[[check.services]]`; their deploy workflows become `[envs]`. It lists the tools
the CI set up on its runners (a Node version, pnpm): the machines that prove, the server and the
workers, need them, and docker or podman for the services. Each thing it could not translate is a
line with what to do. Carry what you keep into `avp.toml`, delete `avp.imported.toml`, then
`avp land`. `avp import-ci --json` gives the same to an agent.

**Services.** Each runs around every proof and every shard of the full suite, on its own free
ports and with its own data, and is removed after (the guide's
[Your checks, your environments](GUIDE.md#your-checks-your-environments)). A service not ready in
`ready_s` seconds fails the run as the step `service <name>`, with the lines it printed.

**Test secrets.** Each `secrets.X` (or `vars.X`) a job reads is named where it is read, on its
shard and on the step that stands for its command: `secrets = ["X"]`. A test key (a payment sandbox,
an API test token) is kept by AVP, per repo, encrypted, and handed only to the steps that name it.
Its value stays in GitHub: what `avp import-ci` prints ends with one line per secret, run once:

```sh
avp secret set STRIPE_KEY < stripe-test-key.txt   # or typed unseen at the prompt
```

A step that names a secret not set goes red and says how to set it; an agent reads the names with
the MCP tool `avp_secrets` and tells a person which one is missing. Never a production key: a test
can print what it gets.

**Workflows on a clock.** A `schedule:` workflow becomes a `[[check.schedule]]` at its cron's hour
and minute (UTC), naming its shards (the guide's [Scheduled suites](GUIDE.md#scheduled-suites)). A
schedule runs once a day, every day: a cron of several times a day, or of weekdays only, is listed
with the time it becomes. Its jobs are shards like any other, so they also run after each batch of
landings; one too slow for that stays in GitHub.

On a shared server, `avp guard avp.toml`: a task that changes the checks waits for a person.

Then let agents land a few real tasks. Each proof runs on the exact head of the line, and the full
suite runs once per batch of landings after them.

**Check.** `avp proof` (in the task's folder) shows each step, its time and its output; `avp log`
shows the full suite's verdict on each position (`Green`, `Red`, `Flaky`); `avp show <position>`
gives the proof, the full suite and the git commit. Where your CI runs on `avp/line`, compare its
verdict on that commit with AVP's: Murmell kept both running and compared them over days.
`avp full <position>` runs a position's full suite again.

**Go back.** `avp revert <position>` on the `avp.toml` landing takes it back, proven like any other
task. `avp secret rm STRIPE_KEY`, `avp unguard avp.toml`.

## 3. `main` becomes the mirror

The switch, the only step that changes how people work: from here, a change reaches `main` through
`avp open` and `avp land`, never through a pull request.

1. Protect `main` on GitHub so that only the mirror writes it: a ruleset (or branch protection)
   with no force pushes and no deletions, where only the mirror's identity may push (its deploy key
   or account in the bypass list), with no pull request or status check required of it. From now
   on, nobody merges a pull request.
2. The mirror only fast-forwards. `main` moved since the import (pull requests merged during
   steps 1 and 2): the follow of step 1 has taken its commits in, so the mirror's first commit on
   `main` merges its tip, and the push is a fast-forward. Wait until `avp tasks` shows no open task
   through `git`: a commit the line has not taken in keeps the push refused (`mirror.blocked`).
3. Stop the mirror of `avp/line` (`avp down` in the server's checkout), and start the one of `main`
   on the server's machine, kept running there:

   ```sh
   avp mirror --repo shop --branch main --remote origin --follow 15
   ```

   Keep `[git] follow = "main"`: a merge that reaches `main` anyway (a ruleset not in place yet, a
   person not switched) is taken in, and the mirror's next commit merges it.
4. Bring in the open pull requests: `pull_requests = true` under `[git]`, with `GH_TOKEN` in the
   server's environment. Each one is a task, merged three-way onto the commit of its task's base
   (the running mirror writes it) and proven; a person lands a green one with `avp approve <task>`,
   or `pull_requests = "land"` lands each green one at once. Once landed, the pull request gets its
   position as a comment and is closed. Dependabot's and Renovate's keep coming in this way. When
   one conflicts with the line, its task is dropped and the files are named: merge the line into its
   branch, and the update brings it in again. `avp import-pr 123 --repo shop --land` does it for one,
   from a checkout.

**Check.** `git log -1 origin/main` is the head of the line, `AVP-Position` in its message; `avp
events --all` shows `mirror.pushed`; a person's direct push is refused by GitHub. When `main` holds
a commit AVP did not write, the mirror refuses to push, overwrites nothing, records
`mirror.blocked` with that commit, and AVP Live shows a red alert (the guide's
[Protect the mirror's branch](GUIDE.md#protect-the-mirrors-branch)). Your workflows on `push` to
`main` now run on every mirrored commit: at Murmell the CI of `main` stopped catching up until it ran
one run at a time ([TROUBLESHOOTING.md](TROUBLESHOOTING.md#the-ci-of-main-never-catches-up)).

**Go back.** Stop the mirror of `main` and lift the protection: `main` holds every landed position
as an ordinary commit, and people merge pull requests again on top of it.

## 4. Staging and production by promotion

```toml
[envs.staging]
follow = "full"                                         # each position whose whole suite is green
deploy = { run = "./scripts/deploy.sh staging {git}" }

[envs.production]
after = "staging"                                       # only what staging serves
approvers = ["<you>", "<a teammate>"]                   # their promotion is the approval
deploy = { run = "./scripts/deploy.sh production {git}" }
```

A `run` deploy is carried out by AVP on the server: it lays the promoted position out in the
environment's own folder and runs the command there, `{git}` and `{position}` filled in. Its
credentials are named in `AVP_DEPLOY_ENV` on the server (`AVP_DEPLOY_ENV=KUBECONFIG,REGISTRY_TOKEN`),
and reach deploys only, never a proof. Exit 0 moves the environment. The command must be safe to run
twice: a restart of the server runs a cut deploy again from the start.

`avp import-ci` writes deploys as `github_workflow`, the workflow dispatched with the promoted
commit. AVP's bridge to GitHub carries them out, inside the server: start `avp serve` with
`AVP_GITHUB_BRIDGE=1` and a `GH_TOKEN` that may run the workflows, approve their environments and
write commit statuses, and name the repo in avp.toml:

```toml
[github]
repo = "acme/shop"

[envs.staging]
follow = "full"
deploy = { github_workflow = "deploy-staging.yml", inputs = { sha = "{git}" } }

[envs.production]
after = "staging"
approvers = ["<you>", "<a teammate>"]
deploy = { github_workflow = "deploy-production.yml", inputs = { sha = "{git}" }, approve_environment = "production" }
```

It dispatches the workflow on `main` (`branch = "..."` under `[github]` names another), approves
the GitHub environment `approve_environment` names in the name of who promoted, follows the run (its
step is in `avp envs`), and moves the environment once the run is green, or once the environment's
`serves` URL answers the commit. It writes the full suite's verdict on each position's commit as the
status `avp/full`, which a workflow or a branch rule can wait for. The guide's
[Your checks, your environments](GUIDE.md#your-checks-your-environments) says the rest.

The bridge is now in AVP: deploy/k8s/scripts/watch.mjs, which did it beside Murmell's server, is
not needed any more. A server that runs it stops its GitHub loop before `AVP_GITHUB_BRIDGE=1`: two
bridges would dispatch every deploy twice. A promotion on its way at the switch keeps its run: the
bridge takes the run of the workflow started since the promotion rather than dispatching another.
One older than 30 minutes with no run (a deploy that failed before the switch) is not dispatched
again: promote it again if it is still wanted.

Either way, take `push:` out of the deploy workflows' triggers. At Murmell, the CI kept starting
staging too, took the place of AVP's deploy in GitHub's queue, and AVP's was cancelled.

```sh
avp envs                     # what each environment serves, what was promoted there
avp promote production       # what staging serves; answers once the deploy is over
avp rollback production      # back to what it served before, through its deploy
```

`avp promote` needs a green full suite on the position, and `after` and `approvers` apply.
`avp rollback` asks again only for `approvers`: the position it goes back to passed the rest to be
served (the guide's [Rollback](GUIDE.md#rollback)). A second rollback goes back where the first one
found it.

**Check.** Once a batch's full suite is green, `avp envs` shows staging promoted to it by `avp`,
then serving it. `avp promote production` from someone not in `approvers` is refused.
`avp rollback production`, then `avp envs`: the position before, recorded as a rollback. With
the bridge, `avp events --all` shows `deploy.dispatched` with the run's page, and the position's
commit on GitHub carries the status `avp/full`.

**Go back.** Put the deploy workflows' `push:` triggers back and take `[envs]` out of `avp.toml` in a
task. A bad deploy needs none of this: `avp rollback`.

## 5. The before and the after

```sh
GH_TOKEN=... avp stats --since 2026-10-25 --until 2026-10-27 \
  --github acme/shop --before 2026-10-22..2026-10-24
```

Two windows of the same length, of ordinary days: the before ends ahead of step 1, the after starts
once step 4 is done. The before is read from GitHub's API with `GH_TOKEN`, never printed: the pull
requests merged in it (count, opened to merged), the job minutes of its Actions runs, reruns
included, and the share of runs of a workflow whose name or file says "deploy" that were cancelled.
The after is what the server recorded: each landing's time from its task's opening to the line and
to each environment, and from its landing to the full suite; the proofs; the machine minutes of
proofs and full suites; what went red. When the windows do not last as long, "more" and "less"
compare per day. `--json` gives the numbers; agents read them with `avp_stats` (the guide's
[Measure it](GUIDE.md#measure-it)).

**Check.** Both columns are there. "To staging" and "To production" before AVP read `-`: `avp stats`
does not read deploy runs per pull request. [CASE-MURMELL.md](CASE-MURMELL.md) says how Murmell's
were taken.

**Go back.** Nothing to undo: it only reads.
