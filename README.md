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
file it reads). A team shares one server: see [docs/GUIDE.md](docs/GUIDE.md#a-team-server).

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

On [go-chi/chi](https://github.com/go-chi/chi), with agents never told about AVP: Claude Code and
Codex used it on their own (3 of 3 each); two agents on the same function were replayed with no
conflict, where Git needed a third agent to settle one; one agent alone costs the same as with Git
(146 s and $0.330 against 151 s and $0.332 for three tasks). On CooperBench `flash_10`, two agents
pass 1 task in 10 through Git and 8 in 10 through AVP.

## How it works

- `avp up` starts a server, imports the last 50 commits, guesses the checks (Node, Python, Rust,
  Go, Java, Ruby) into `avp.toml`, and mirrors every landing to the branch `avp/line`.
- An agent calls `avp_open`: a folder of its own at the head of the line, the one a finished
  session left, with its install. It writes, then `avp_land`:
  the proof runs (typecheck and the tests related to the task), and only green lands. If the line
  moved, the task is replayed on the new head first.
- The proof starts while the agent is still running its own tests, so landing rarely waits.
- The whole suite runs once per batch of landings. Staging can follow a green suite, and
  production is `avp promote production`.
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

## Limits

- It is new. Every defect a real run found is in [docs/RUNS.md](docs/RUNS.md) with its fix.
- A proof is not the whole suite: a break the related tests miss is caught by the next batch run.
- With one agent on a small repository with fast tests, it saves nothing: it costs what Git costs.

## License

Copyright (C) 2026 IntrudR. [Elastic License 2.0](LICENSE): you may use, change and run AVP for
yourself and your team; you may not offer it to others as a hosted or managed service, or work
around its license keys. Releases up to and including 0.5.0 were published under the AGPL-3.0.
Contributions: [CONTRIBUTING.md](CONTRIBUTING.md).
