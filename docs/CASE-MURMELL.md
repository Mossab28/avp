# The Murmell case

Murmell is built by a small team and many agents at once, in one large monorepo, and ships every
day. On 24 October 2026 its repository moved to AVP along the path of [MIGRATE.md](MIGRATE.md):
`main` became the mirror of the line, staging follows the full suite, a person promotes production,
and the deploy workflows stayed GitHub's, dispatched by a bridge that ran beside the server (now in
AVP: `AVP_GITHUB_BRIDGE=1`).
What broke on the way is in [RUNS.md](RUNS.md) and [TROUBLESHOOTING.md](TROUBLESHOOTING.md).

## The numbers

48 hours of pull requests (22 and 23 October 2026) against 48 hours on AVP (25 to 27 October), same
team, same product in production. The 24th, the day of the switch, is in neither window.

| | Pull requests and CI | AVP |
|---|---|---|
| To staging | 91 min (median) | 8.5 min, **10x faster** |
| To `main` | 25 min (median) | 3.4 min (median) |
| To production | 122 min (median) | a 2-minute promotion of what staging serves |
| Changes shipped | 57 pull requests | 131 landings, **2.3x more** |
| CI machine time | 15,310 min | 3,475 min, **77 % less** |
| Checking one change | a CI run, 5 to 15 min | 15 s (median) |
| Staging deploys cancelled | 41 % | none |

Tokens per change stayed about the same: 10.0 M against 10.6 M. AVP did not make a change cheaper
for an agent to write; what went is the wait around it (a CI run per pull request, the queue behind
`main`, the staging deploys cancelled) and the machine time of the whole CI on every change.

## How to measure them again

Two sources: GitHub's API for the pull requests and the Actions minutes, the AVP line for the
positions, the proofs and the full suites. The table above was computed by hand from them; `avp
stats` came after it and computes most rows in one command, with `AVP_SERVER` set to Murmell's
server:

```sh
GH_TOKEN=... avp stats --repo murmell --since 2026-10-25 --until 2026-10-27 \
  --github Murmell/murmell --before 2026-10-22..2026-10-24
```

Each date is its midnight, UTC: `2026-10-22..2026-10-24` is the 22nd and the 23rd. `--json` gives
every number, and `GET /repos/{repo}/stats?since=&until=` the after alone. Row by row, what each
column is:

| Row | Before, from GitHub's API | After, from the line |
|---|---|---|
| To staging | Not in `avp stats` (`-`). The after's definition, read from `/pulls` and the staging deploy workflow's `/actions/runs`: a pull request from its opening to the end of the first green deploy run on its merge commit or a later one | `avp stats`, "To staging": a task from its opening to the first time staging serves its position or a later one, median |
| To `main` | `avp stats --github`: a pull request merged in the window, from its opening to its merge, median | A task landed in the window, from its opening to its landing, median |
| To production | Not in `avp stats` (`-`): as to staging, with the production deploy workflow | How long a production promotion takes: from its `env.promoted` event to its `env.moved` (`avp events --all --json`, `at` in milliseconds). `avp stats` "To production" counts from the task's opening, the wait for a person's promotion included |
| Changes shipped | Pull requests merged in the window | Tasks landed in the window; imported commits are not landings |
| CI machine time | The minutes the jobs of the Actions runs created in the window ran, every attempt (`/actions/runs/{id}/jobs?filter=all`) | The machine minutes of proofs and full suites, on the server and on workers: a split step counts each of its shares, a shard that ran twice counts twice |
| Checking one change | Not computed (`a CI run`): a run's duration is in `/actions/runs` | A task's proof, from its first step to its last, median |
| Staging deploys cancelled | `avp stats --github`: the cancelled share of the runs of every workflow whose name or file says "deploy", staging and production together; for staging alone, count the staging workflow's runs | `avp stats` counts the deploys AVP ran itself that a restart cut. Murmell's were GitHub workflows: their cancellations are on GitHub (below) |

What GitHub still ran in the after window, the deploy workflows among them, is not in the after
column. `avp stats --github Murmell/murmell --before 2026-10-25..2026-10-27` reads it: its before
column is then GitHub's side of the after window.

AVP does not see an agent's usage: tokens per change are not in `avp stats`.

The line itself answers for any position: `avp log` lists them with their full suite's verdict,
`avp show <position>` gives a position's goal, who landed it through which agent, its proof, its
full suite and its git commit, and `avp proof <id>` a proof step by step.
