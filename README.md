# AVP

**Version control that replaces Git for coding agents.** Built at [Murmell](https://murmell.com),
where it runs our own repository.

Why we built it, with the numbers and a diagram:
[AVP, version control that replaces Git for coding agents](https://murmell.com/blog/agents-outgrew-pull-requests).

An agent opens a task, writes, and lands it. Every landing is proven on the exact head of the
line before it counts, so `main` stays green, and two agents on the same file are replayed one on
the other instead of ending in a merge conflict. While a team moves over, every landing is still
written as an ordinary commit on the branch `avp/line` (`--branch main` makes `main` the mirror).

```sh
curl -fsSL https://raw.githubusercontent.com/Mossab28/avp/main/install.sh | sh
cd your-repo && avp up
```

macOS and Linux, x86_64 and arm64; on Windows, inside WSL. Then ask your coding agent for a change
in that folder: `avp up` wires each one it finds (Claude Code, Codex, Cursor, Gemini CLI, GitHub
Copilot, OpenCode, Qwen Code, Kiro, Zed, Amp, Windsurf, Cline, Aider), `avp agents` says which.
Nothing to tell the agent: it finds AVP through the MCP server and one rule in `AGENTS.md` (or the
file it reads). A team shares one server, AVP Cloud's: see [AVP and AVP Cloud](#avp-and-avp-cloud).

New to AVP? [Your first 15 minutes](docs/QUICKSTART.md) goes from install to two agents on one
file, with the output of each command. [The concepts](docs/CONCEPTS.md) say the words (line,
position, task, proof, landing...) and what each replaces in Git and GitHub.

## What it changed for us

On Murmell's repository, 48 hours of pull requests (22 and 23 October 2026) against 48 hours on
AVP (25 to 27 October), same team, same product in production:

| | Pull requests and CI | AVP |
|---|---|---|
| To staging | 91 min (median) | 8.5 min, **10x faster** |
| To `main` | 25 min (median) | 3.4 min (median) |
| To production | 122 min (median) | a 2-minute promotion of what staging serves |
| Changes shipped | 57 pull requests | 131 landings, **2.3x more** |
| CI machine time | 15,310 min | 3,475 min, **77 % less** |
| Checking one change | a CI run, 5 to 15 min | 15 s (median) |
| Staging deploys cancelled | 41 % | none |

Where each number comes from, and the command that measures it again:
[docs/CASE-MURMELL.md](docs/CASE-MURMELL.md).

## The benchmark

64 Codex agents build the same 112-section spec in 40 minutes, each model with Git and CI (a push
to main runs the typecheck and every test, and is refused when they fail), then with AVP:

| 64 agents, 40 minutes | Git + CI | AVP |
|---|---|---|
| GPT-6.1 Sol | 8 of 112, $31.58 | **all 112 in 10 minutes**, $19.77 |
| GPT-6.1 Terra | 0 of 112, $23.99 | **77 of 112**, $13.00 |
| GPT-5.6 Luna | 110 of 112, $7.16 | 105 of 112, **$4.28** |

With Git, the big models stall: sections whose tests need each other are refused by CI one by
one, and their agents wait for each other. AVP lands them together. At 32 agents, AVP delivers as
much as Git and CI for 35 to 60 % less, and GitHub's merge queue does worse than both. The method,
every number and the harness: [bench/](bench/README.md).

## How it works

- `avp up` starts a server, imports the last 50 commits, guesses the checks (Node, Python, Rust,
  Go, Java, Ruby) into `avp.toml`, and mirrors every landing to the branch `avp/line`.
- An agent calls `avp_open`: a folder of its own at the head of the line, the one a finished
  session left, with its install. It writes, then `avp_land`:
  the proof runs (typecheck and the tests related to the task), and only green lands. If the line
  moved, the task is replayed on the new head first.
- The proof starts while the agent is still running its own tests, so landing rarely waits.
- On AVP Cloud, the whole suite runs once per batch of landings. Staging can follow a green suite,
  and production is `avp promote production`.
- For people, the history stays readable: every position is a Git commit on `avp/line` (goal, author,
  agent, proof), `avp show <position>` prints what it changed and why, `avp why <file>:<line>` which
  task wrote that line, its goal, its agent, its proof and the agent's conversation, and
  `avp revert <position>` undoes a position on the head, proven before it lands like any other
  change.

Configuration, environments and the rest: [docs/GUIDE.md](docs/GUIDE.md). Moving a team off GitHub
Actions, step by step and each step reversible: [docs/MIGRATE.md](docs/MIGRATE.md). When something
does not look right: [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md).

Already on a merge queue? [AVP and a merge queue](docs/GUIDE.md#avp-and-a-merge-queue) says what
each does best. Releases, hotfixes and long-lived branches:
[Releases, hotfixes and long work](docs/GUIDE.md#releases-hotfixes-and-long-work).

## AVP and AVP Cloud

AVP is the free tool anyone runs on their machine, as Git is: a local server (`avp up`), tasks,
proofs, the line and its history (`log`, `show`, `why`, `blame`, `revert`, `bisect`), the mirror to
Git, the MCP server for every agent, the import from Git and from a CI. `cargo build` and the
release binaries are this AVP; its server answers its own machine, on 127.0.0.1.

AVP Cloud is the hosted server a team pays for, as GitHub is: what only makes sense on a team's
server. Proof workers; the full suite after each landing, its flaky reruns, the reverts it makes
and the scheduled suites; a person in the loop (approvals, the landing rule, guarded paths);
secrets for tests; environments, with promotions, rollbacks and the GitHub deploy bridge; stats and
the before and after; AVP Live; a team's accounts and tokens.

The CLI keeps every verb, as `gh` speaks to GitHub: on a free server, `avp promote` or `avp approve`
answers that it is part of AVP Cloud, and what to do. In this repository AVP Cloud's code sits in
`src/cloud/`, behind one seam (`src/seam.rs`), and builds with `cargo build --features cloud`.

## Limits

- It is new. Every defect a real run found is in [docs/RUNS.md](docs/RUNS.md) with its fix.
- A proof is not the whole suite: a break the related tests miss is caught by the next batch run.

## License

Copyright (C) 2026 IntrudR. [Elastic License 2.0](LICENSE): you may use, change and run AVP for
yourself and your team; you may not offer it to others as a hosted or managed service, or work
around its license keys. Releases up to and including 0.5.0 were published under the AGPL-3.0.
Contributions: [CONTRIBUTING.md](CONTRIBUTING.md).
