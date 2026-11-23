# Move a repository's CI to AVP: steps for a coding agent

You are a coding agent. A person asked you to move this repository off its CI, GitHub Actions or
GitLab CI, onto AVP, in one session. Follow the steps in order. Stop only at the lines marked
**[person]**: say exactly what they do (the command, or each click), and wait for them.

Each step is told for people in [MIGRATE.md](../MIGRATE.md); every command is in
[GUIDE.md](../GUIDE.md).

## Rules

- Never push to `main` yourself, never force a push, never change a setting on GitHub or GitLab:
  say what to run or click.
- Never read, print or set the value of a secret: give the person the command, they type the value.
- When AVP is wrong or fails, do not work around it: stop, and give the person the command, its
  exact output and `avp --version`, for https://github.com/Mossab28/avp/issues.
- Every change of the repository during the move goes through a task: `avp_open`, then `avp_land`.

## Where it runs

On the machine that will serve. Working alone, that is the person's own machine: the line moves
while it is on, nothing lands while it is off, and a scheduled suite runs once it is back. For a
team, an always-on machine the team reaches: read [A team server](../GUIDE.md#a-team-server) first
(`AVP_TOKEN`, accounts, TLS in front). Below, "this machine" is that one.

## 0. Before anything

- Install AVP: `curl -fsSL https://raw.githubusercontent.com/Mossab28/avp/main/install.sh | sh`
- Check: a checkout of `main`, nothing uncommitted, level with `origin/main`. Note whether `origin`
  is on GitHub or GitLab: steps 2, 3 and 5 differ.
- Docker or podman on this machine when the CI runs services (a database, a cache).
- List the open pull requests (GitHub) or merge requests (GitLab). GitHub: AVP takes them in after
  the switch. GitLab: AVP does not; **[person]** merges or closes the ones that matter now.
- Measure the before, the last 14 days of CI:
  - GitHub: `GH_TOKEN=$(gh auth token) avp stats --github <owner>/<repo> --before <from>..<to>`.
  - GitLab: the pipelines of `main` from its API (`glab api`, or a read-only token the person gives):
    how many, median duration, failed, minutes.

## 1. A server that stays up

- A key for the mirror: `ssh-keygen -t ed25519 -N "" -f ~/.ssh/avp_mirror`. **[person]** adds
  `~/.ssh/avp_mirror.pub` to the repository as a deploy key that may write:
  - GitHub: Settings > Deploy keys > Add deploy key, "Allow write access".
  - GitLab: Settings > Repository > Deploy keys, "Grant write permissions to this key".
- `avp serve --listen 127.0.0.1:7117 --data ~/.avp/data` as a service that starts with the machine
  (a LaunchAgent on macOS, a systemd unit on Linux), its log in `~/.avp/server.log`. Its
  environment:
  - the `PATH` of the person's shell (`echo $PATH` in their terminal): a service's own `PATH` lacks
    node, pnpm, cargo, and every proof would fail on a tool not found;
  - `GIT_SSH_COMMAND="ssh -i ~/.ssh/avp_mirror -o IdentitiesOnly=yes"`;
  - the file `~/.avp/server.env`, mode 600, that the person fills: create it with comments only,
    naming what goes in (step 2). Never read it.

## 2. The line and the checks

- In the checkout: `avp migrate`. It imports the history, turns the CI (`.github/workflows/*.yml`,
  `.gitlab-ci.yml`) into `avp.toml`, proves the checks and the full suite once on this machine, and
  mirrors to the branch `avp/line` only.
- Each red step: fix it in a task, then `avp migrate` again, until all is green. A tool not found:
  install it here (`avp import-ci` lists the tools the CI set up, at the top).
- **[person]** sets the test secrets: give them the `avp secret set <NAME>` lines it printed.
- A scheduled pipeline: set its `[[check.schedule]]` `at` to its real time, in UTC. GitLab keeps that
  time under CI/CD > Schedules, not in the file, so the import writes 04:00.
- Show the person `avp.toml` and the "Not translated" list, and name the jobs that stay in the old CI:
  GitLab `include:` and `trigger:`, jobs on tags, manual jobs, jobs that publish (a package, an image,
  a release), an environment per branch (review apps), a GitHub action only GitHub runs.
  **[person]** says yes: then `avp migrate --yes` lands `avp.toml`.
- In a task, the server's link to the remote, in `avp.toml`:

  ```toml
  [git]
  remote = "<the repository's SSH URL>"
  follow = "main"            # a commit pushed to main anyway comes into the line
  ```

  **[person]** approves the task: `avp approve <task>`.
- Deploys: check `[envs]`. Each deploy job became a `run` command the server carries out, production
  after staging. Tell the person what those commands need in `~/.avp/server.env`: the variables, and
  `AVP_DEPLOY_ENV=` naming them. On GitHub, a deploy that needs an action only GitHub runs stays a
  `github_workflow`, with `AVP_GITHUB_BRIDGE=1` and `GH_TOKEN` there too. **[person]** fills the file;
  then restart the service.

## 3. The switch: `main` becomes the mirror

- **[person]** protects `main` so that only the mirror's key writes it:
  - GitHub: the `gh api ... rulesets` command `avp migrate` printed (its step 3.1): no deletion, no
    force push, no update but by a deploy key.
  - GitLab: Settings > Repository > Protected branches > `main`: Allowed to merge "No one", Allowed
    to push and merge the deploy key `avp_mirror` alone, Allowed to force push off.
- Wait until `avp tasks` shows no open task through `git`: every commit of `main` is in the line.
- `avp down` in the checkout: the mirror of `avp/line` stops.
- Clone the repository over SSH into `~/.avp/mirror/<repo>`, and run, as a second service that
  starts with the machine, with the same `PATH` and `GIT_SSH_COMMAND`:

  ```sh
  avp -C ~/.avp/mirror/<repo> mirror --repo <repo> --branch main --remote origin --follow 15
  ```

  When `main` may only move by a pull request (a rule of the team, an audit), on GitHub: follow
  [Keep pull requests on `main`](../MIGRATE.md#3-main-becomes-the-mirror) instead of the
  protection above.
- Delete the branch `avp/line` on the remote.
- GitHub only, now that the mirror writes `main`: in a task, `pull_requests = true` under `[git]`,
  with `GH_TOKEN` in `~/.avp/server.env` (a token that reads, comments on and closes pull requests).
  Each open pull request becomes a task, proven; a green one lands on a person's
  `avp approve <task>`, and the pull request is closed with its position. Not before the switch: a
  pull request landed then is closed while its code is only on `avp/line`. **[person]** approves the
  task. On GitLab, merge requests do not come in: they were settled in step 0.

## 4. Check it works

One small real change, through `avp_open` and `avp_land`. Then:

- `git fetch origin && git log -1 origin/main` carries `AVP-Position`;
- `avp events --all` shows `mirror.pushed`;
- `avp log` shows the full suite green on that position;
- `avp envs` shows staging on it, when there is a staging.

## 5. The old CI off

- In a task: delete the CI files, or keep only the jobs that stay (step 2), started by tags alone:
  GitHub `on: push: tags: ["*"]`, GitLab `rules: - if: $CI_COMMIT_TAG`. **[person]** reads the diff;
  land it on their word.
- Check that the next landing starts no run of the old CI on `main`.

## 6. Report

- A before and after table: from a change to `main`, checking one change, machine minutes, runs
  failed. The after is `avp stats --since <today>`: run it again after a few ordinary days.
- What stays in the old CI, and why.
- The way back, written to `~/.avp/WAY-BACK.md`: stop both services, lift the protection of `main`,
  put the CI files back with a `git revert` of the commit that removed them. `main` holds every
  landing as an ordinary commit: nothing is lost.
