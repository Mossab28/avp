# Your first 15 minutes

You put AVP on a repo, ask an agent for a change, watch it land, make two agents change the same
file, then read the line. The words are explained in [CONCEPTS.md](CONCEPTS.md).

Every output below is real. It comes from a small Node project, `shop`: three commits, a
`src/price.js` with `formatPrice` and `applyDiscount`, and `npm test` running `node --test`. Use
your own repo: only the names change. You need git, and the tools your tests need.

## 1. Install (1 minute)

```sh
curl -fsSL https://raw.githubusercontent.com/Mossab28/avp/main/install.sh | sh
avp --version
```

```
avp 0.5.0
```

macOS and Linux, x86_64 and arm64; on Windows, inside WSL. The binary goes in `~/.local/bin`.

## 2. `avp up` in your repo (1 minute)

```sh
cd shop
avp up
```

```
3/3 positions
landing avp.toml (tests): its first proof installs the dependencies once...
a change to the checks waits for a person's word: approved as alice, who ran avp up
AVP is on for ~/shop
  server   http://127.0.0.1:7117 (started, data in ~/.avp/data)
  repo     shop, imported: the last 3 commits of HEAD are positions 0 to 2
  checks   avp.toml landed at position 3: tests
  mirror   every landed task becomes a commit on the local branch avp/line (no push)
  agents   Claude Code  wired: MCP server for this folder
           Codex        wired: .codex/config.toml, rule in AGENTS.md (Codex loads it once it trusts this folder)
           OpenCode     wired: opencode.json, rule in AGENTS.md
           not installed: Cursor, Gemini CLI, Copilot (VS Code), Copilot CLI, Qwen Code, Kiro, Zed, Amp, Windsurf, Cline, Aider
  folders  agents work in ~/avp/<repo>-<n>, one each at a time: taken again with its install, or cloned from the head with the install of a folder whose lockfiles are the same
Your branches, main and the files here are untouched. Start Claude Code, Codex or OpenCode in this folder and ask for a change:
it opens a task, proves it, lands it, and the mirror writes it on avp/line. `avp log` shows the line, `avp down` stops the mirror.
```

It took under 2 seconds. Line by line:

- **server**: a local AVP server, on this machine only.
- **repo**: your last commits (50 at most) became the first positions of the line.
- **checks**: what every change must pass before it lands, guessed from the project and landed as
  `avp.toml`. A change to the checks always waits for a person; you ran `avp up`, so you are that
  person. Read it with `avp show 3`, and edit it as any other file, in a task.
- **mirror**: every landing becomes a git commit on the branch `avp/line`. With a remote `origin`,
  it is pushed there too.
- **agents**: each coding agent installed on this machine, wired through its own project config:
  the MCP server it reaches AVP through, and the AVP rule in `AGENTS.md` (or the file it reads).
  `avp agents` prints this list again.
- **folders**: an agent never works in your checkout. Its task is laid out in a folder of its own
  under `~/avp`, which the next task takes again with its install.

`git status` says nothing: the files `avp up` wrote (here `.claude/settings.local.json`,
`.codex/config.toml`, `opencode.json`, `AGENTS.md`) are listed in `.git/info/exclude`.

## 3. Ask your agent for a change (3 minutes)

Start your agent in the repo's folder and ask for a change, as you always do. Nothing to say about
AVP: it finds the tools by itself. Here, Claude Code was asked:

> formatPrice should put thousands separators: 1234567 cents is $12,345.67

It calls `avp_open` first. The answer it reads:

```
task task_01m61x8m0c5bb143hakfsyytwy opened on position 3 (5 files laid out).
Your folder: ~/avp/shop-1
Work there, with absolute paths: read and edit files, run tests if useful (a JS project needs its install there once), then avp_land: it proves the change, then lands it. No git commit, push or pull request on this repository.
No other open task on this repo.
The line's last changes to these paths, newest first (position, age, agent, goal):
src/price.js
     1   1m  imported  ff1a96c applyDiscount (Alice, 2026-10-28)
     0   1m  imported  d086e6e formatPrice (Alice, 2026-10-28)
test/price.test.js
     0   1m  imported  d086e6e formatPrice (Alice, 2026-10-28)
```

The agent works in a folder of its own, `~/avp/shop-1`, not in your checkout. Meanwhile, in a
terminal in your repo:

```sh
avp tasks
```

```
task_01m61x8m0c5bb143hakfsyytwy  pos 3  alice via claude-code  formatPrice: thousands separators, $12,345.67
```

The agent edits `src/price.js` and its test. Each save goes to the server, which starts the proof
while the agent is still working. Then it calls `avp_land`:

```
sync: no new writes
landed at position 4 (proof proof_01m61x8qc00a1922ty75b7gsdf). The task is closed; open a new one for the next piece of work.
```

The proof ran `npm test` on the head of the line plus the agent's change, it was green, and the
change is position 4. Had it been red, the agent would have read the failing output in that answer,
fixed, and landed again.

## 4. Two agents, one file (3 minutes)

Start a second agent beside the first: Claude Code in one terminal, Codex in another. Ask each for
a change to `src/price.js`:

> applyDiscount should refuse a percent outside 0 to 100

> formatPrice should show a negative amount as -$5.00

Both open on position 4. The second one is told who else is on that file:

```
task task_01m61x8r0s478tq3a5fc7ksg7t opened on position 4 (5 files laid out).
Your folder: ~/avp/shop-2
Work there, with absolute paths: read and edit files, run tests if useful (a JS project needs its install there once), then avp_land: it proves the change, then lands it. No git commit, push or pull request on this repository.
Other tasks are open on this line:
  task_01m61x8r0q5px80kryk0dr9j9q applyDiscount refuses a percent outside 0 to 100 (holds src/price.js)
If this task needs the work of one of them, do not poll the line: `avp wait --task <id>` (or the avp_wait tool) waits until it lands and brings it into your folder.
The line's last changes to these paths, newest first (position, age, agent, goal):
src/price.js  held by open task task_01m61x8r0q5px80kryk0dr9j9q (claude-code): applyDiscount refuses a percent outside 0 to 100
     4   1m  claude-code  formatPrice: thousands separators, $12,345.67
     1   1m  imported     ff1a96c applyDiscount (Alice, 2026-10-28)
     0   1m  imported     d086e6e formatPrice (Alice, 2026-10-28)
```

Each edits its own copy of `src/price.js`. Claude Code lands first:

```
landed at position 5 (proof proof_01m61x8v5749823qdsysbev704). The task is closed; open a new one for the next piece of work.
```

Codex lands next, on a line that moved under it. Its last saves go with the landing: `sync` sends
them, and `[taken]` says the task now holds those paths (the first task let go of `src/price.js`
when it landed):

```
sync: + src/price.js [taken]
+ test/negative.test.js [taken]
landed at position 6, replayed on the moved line (proof proof_01m61x8w4jf3wt7qmybfcj1mxf). The task is closed; open a new one for the next piece of work.
```

No conflict, no rebase. AVP replayed the second task on the new head, proved the result, and
landed it. `src/price.js` now holds both changes:

```js
export function formatPrice(cents) {
  const sign = cents < 0 ? "-" : "";
  const [whole, part] = (Math.abs(cents) / 100).toFixed(2).split(".");
  return `${sign}$${whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",")}.${part}`;
}

export function applyDiscount(cents, percent) {
  if (percent < 0 || percent > 100) throw new RangeError(`no discount of ${percent}%`);
  return Math.round((cents * (100 - percent)) / 100);
}
```

Only two changes to the same lines of the same file end in conflict markers. Then the second
agent's `avp_land` answers `not landed: conflict` with the files, the markers are in its folder, it
settles them and lands again.

## 5. AVP Live (1 minute)

Open <http://127.0.0.1:7117/live> in a browser. It shows the line as it moves: each landing, the
tasks at work, and what each position changed (click one). Its left rail has six views: Overview,
Line, Review (what waits for a person), Stats, Environments and Setup. It shows the newest repo of
the server; `/live?repo=shop` picks one.

## 6. Read the line (5 minutes)

The line, newest first:

```sh
avp log
```

```
    6  …  task_01m61x8r0s478tq3a5fc7ksg7t  formatPrice shows a negative amount as -$5.00
    5  …  task_01m61x8r0q5px80kryk0dr9j9q  applyDiscount refuses a percent outside 0 to 100
    4  …  task_01m61x8m0c5bb143hakfsyytwy  formatPrice: thousands separators, $12,345.67
    3  …  task_01m61x8jkgm6wsy23et84j2vyc  AVP: the checks a task proves before it lands (avp.toml, guessed by avp up)
    2  …  -  5719b8c ignore node_modules (Alice, 2026-10-28)
    1  …  -  ff1a96c applyDiscount (Alice, 2026-10-28)
    0  …  -  d086e6e formatPrice (Alice, 2026-10-28)
```

The second column is the full suite's verdict on each position (`Green`, `Red`, `Flaky`). `…` says
no full suite ran: the `avp.toml` that `avp up` guesses has the proof's steps only. Add
`full = "npm test"` under `[check]`, in a task (a change to the checks waits for your
`avp approve <task>`), and the whole suite runs after each batch of landings.

What a position changed, who landed it, through which agent, with which proof:

```sh
avp show 6
```

```
position 6  formatPrice shows a negative amount as -$5.00
  task task_01m61x8r0s478tq3a5fc7ksg7t, by alice via codex
  proof proof_01m61x8w4jf3wt7qmybfcj1mxf: Green, ✓ tests
  full suite: not run yet

--- a/src/price.js
+++ b/src/price.js
@@ -1,8 +1,9 @@
 // Prices are integers in cents.

 export function formatPrice(cents) {
-  const [whole, part] = (cents / 100).toFixed(2).split(".");
-  return `$${whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",")}.${part}`;
+  const sign = cents < 0 ? "-" : "";
+  const [whole, part] = (Math.abs(cents) / 100).toFixed(2).split(".");
+  return `${sign}$${whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",")}.${part}`;
 }

 export function applyDiscount(cents, percent) {

--- a/test/negative.test.js
+++ b/test/negative.test.js
@@ -0,0 +1,7 @@
+import { test } from "node:test";
+import assert from "node:assert/strict";
+import { formatPrice } from "../src/price.js";
+
+test("a negative amount", () => {
+  assert.equal(formatPrice(-500), "-$5.00");
+});
```

Why one line is what it is: the task that wrote it, its goal, its person and agent, its proof
(and the agent's conversation, when AVP found it):

```sh
avp why src/price.js:5
```

```
src/price.js:5  const [whole, part] = (Math.abs(cents) / 100).toFixed(2).split(".");
  written at position 6 by task task_01m61x8r0s478tq3a5fc7ksg7t
  goal: formatPrice shows a negative amount as -$5.00
  by: alice via codex
  proof: proof_01m61x8w4jf3wt7qmybfcj1mxf Green, ✓ tests
```

`avp proof <id>` prints a proof in full, each step with its output.

How the line is doing, over the last 7 days:

```sh
avp stats
```

```
shop on AVP, 2026-10-21 18:45 to 2026-10-28 18:45 UTC (7 days)

  To main              4 s (median), 4 s (p90), from task opened to landed
  Changes shipped      4 landings
    by agent           claude-code 2, avp-up 1, codex 1
    by person          alice 4
  Checking one change  0 s (median), 0 s (p90); 4 proofs, 0 red, 0 blocked
  Full suite           - (median) after landing; 0 runs, 0 red, 0 flaky reruns
  CI machine time      0 min: 0 in proofs, 0 in full suites
  The line moved       1 replay of 1 task, 0 conflicts handed back
```

And git still has it all. Each landing is a commit on `avp/line`, with the task, the agent and the
proof in its trailers:

```sh
git log --oneline avp/line
git log -1 avp/line
```

```
666e351 formatPrice shows a negative amount as -$5.00
7afa52e applyDiscount refuses a percent outside 0 to 100
35bb2bc formatPrice: thousands separators, $12,345.67
cd012cb AVP: the checks a task proves before it lands (avp.toml, guessed by avp up)
5719b8c ignore node_modules
ff1a96c applyDiscount
d086e6e formatPrice
```

```
commit 666e35141afaba67cb1f1673e672ffc90fb1ace4
Author: alice (codex) <alice@users.avp>
Date:   Wed Oct 28 18:45:00 2026 +0000

    formatPrice shows a negative amount as -$5.00

    AVP-Position: 6
    AVP-Task: task_01m61x8r0s478tq3a5fc7ksg7t
    AVP-Person: alice
    AVP-Agent: codex
    AVP-Session: s_01m61x8r0mtx4cv0artzd1w44z
    AVP-Proof: proof_01m61x8w4jf3wt7qmybfcj1mxf
```

## When you are done

```sh
avp down
```

```
mirror of shop stopped (pid 70740)
local server stopped (pid 70639)
The line and its data stay in ~/.avp/data: `avp up` starts again where it was.
```

## Next

- [CONCEPTS.md](CONCEPTS.md): the words, a task's life to production, what replaces what.
- [GUIDE.md](GUIDE.md): your checks and the full suite, environments and promotion, a person in
  the loop, a team server.
