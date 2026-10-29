# Facturo bench: results

Three agents (Claude Code headless, Opus) build the three domains of Facturo at once, from the
same base commit, on the same Mac (three CI runners `bench` / three AVP check slots). Judge:
18 hidden acceptance tests on the integrated result. `run-arm.sh` runs one arm; `main` is reset
to `bench-base` before each classic run; each AVP run gets a fresh repo.

| Run | Arm | All three integrated | Acceptance | Verifications | Verif. minutes | Conflicts / rebases | Sessions cost |
|---|---|---|---|---|---|---|---|
| 1 | classic | 8,1 min | 18/18 | 7 CI runs | 1,2 runner-min (wall 0,2 each) | 1 rebase (payments) | 5,00 $ |
| 1 | avp | 4,6 min | 18/18 | 7 checks, 7 green | 7 s | 1 conflict in app.ts, settled by its author; 3 replays | 3,13 $ |
| 2 | classic | 6,7 min | 18/18 | 7 CI runs | 1,1 runner-min | 0 rebase (Payments held its PR until Invoices merged) | 3,93 $ |
| 2 | avp | 5,0 min | 18/18 | 7 checks, 7 green | 7 s | 2 conflicts in app.ts on replay, settled by their authors | 3,06 $ |
| 3 | classic | 6,8 min | 18/18 | 8 CI runs | 1,2 runner-min | 1 rebase (Invoices) | 3,93 $ |
| 3 | avp | 5,4 min | 18/18 | 7 checks, 7 green | 11 s | 2 replays with a conflict in app.ts, settled by their authors | 3,15 $ |

## Over three runs

| | Classic | AVP |
|---|---|---|
| All three domains integrated, mean | 7,2 min (8,1 / 6,7 / 6,8) | 5,0 min (4,6 / 5,0 / 5,4) |
| Acceptance | 18/18 every run | 18/18 every run |
| Verification work | 22 CI runs, 3,5 runner-min | 21 checks, 25 s |
| Shared-file collisions | 2 rebases over 3 runs; one PR held until a teammate merged | 5 conflicts on replay, all settled by their author in the same turn |
| Sessions, API-equivalent | 4,29 $ mean (5,00 / 3,93 / 3,93) | 3,11 $ mean (3,13 / 3,06 / 3,15) |
| Tool failures | 9/9 `gh pr merge --delete-branch` failed after merging (worktree holds main) | 0 |

Tokens per run, every message of every session and sub-agent, deduplicated by message id:

| | Classic (mean of 3) | AVP (mean of 3) |
|---|---|---|
| Assistant messages | 94 | 69 |
| Cache reads | 5,84 M | 3,31 M |
| Cache writes | 193 k | 154 k |
| Output | 78,6 k | 60,9 k |

The gap is not verification here (a 12-second CI): it is the protocol around it. Classic agents
wait for CI, rebase, wait again, merge one at a time and clean up after `gh`. AVP agents
land when their proof is green; the line replays them and hands back the one file that
overlapped. AVP sessions also wrote more code per session (payments opened a second task in
every run to switch its tests to the real routes once invoices had landed) and still cost less.

Notes, run 1:
- Classic: every `gh pr merge --delete-branch` failed after the merge (worktree holds `main`);
  the three agents deleted their remote branch by hand. Payments merged last, after one rebase.
- AVP: Payments opened a second task after Invoices landed, to switch its tests from the store
  to the real routes; four landings in all. Five advisory lease takeovers on the shared files,
  every one of them told to the holder.

# CooperBench flash_10: git merge against AVP, same agents, same judge

CooperBench (Stanford + SAP, ICLR 2026): two agents, two features on a real repository, hidden
expert tests. Subset `flash_10` (10 pairs, 8 repositories, Python / Go / Rust). Agent:
`claude_code` (their adapter), model `claude-sonnet-5-5`, Docker backend on one WSL host
(Ryzen 5 3600, 14 GB for WSL), Redis messaging on in both coop conditions. Evaluator: theirs,
unmodified (`cooperbench eval`): the two patches are applied on two branches and merged
(naive `git merge`); on a clean merge both test suites run on the merged tree; on a conflict the
evaluator falls back to the lead agent's patch alone, which then has to pass both suites (so a
lead that merged its peer's branch, as their prompt asks, can still pass). The maintainers
dropped the paper's union fallback on purpose ("rewards lucky non-overlap") and there is no LLM
resolver in the published code.

Our only change is the `coop` condition's integration layer: `CB_AVP=1` replaces the shared git
remote by a AVP repository (one per task), each container a view, `avp_*` MCP tools, git
denied. When both agents are done, each container lays the line's head out and diffs it against
the base commit: the evaluator sees two identical patches and skips its merge. AVP ran with no
`avp.toml`, so `avp_check` verified nothing: the difference measured here is coordination and
integration only (leases, replay on a moved line, one line), not verification.

Run of 2026-10-24, 00:00 to 00:35 Paris time, one pass per condition, infrastructure failures
replayed once (two solo tasks whose container never started; counted on the replay).

| Condition | Passed / 10 | Retention vs solo | Integration | Sessions, API-equivalent | Output tokens | Cache reads |
|---|---|---|---|---|---|---|
| solo (one agent, both features) | 8 | 1 | n/a | 0,88 $ | 30,9 k | 0,97 M |
| coop, shared git remote (their protocol) | 1 | 0,125 | 8 merge conflicts, 1 clean merge with a failing test, 1 pass | 1,39 $ | 40,7 k | 1,85 M |
| coop, AVP (ours) | 8 | 1,0 | 10 identical patches, 0 merge conflicts; the 2 failures are the 2 tasks solo also fails, on the same feature-1 test | 1,68 $ | 48,6 k | 2,72 M |

Reading: with the paper's protocol the pair of agents keeps 12,5 % of what one agent does alone
(the paper reports about 60 % for Sonnet 4.5 with a stronger merge procedure); with AVP it keeps
100 % (8 of 8): the pair fails exactly the two tasks the single agent fails, nothing more. Every one of the eight git failures is an unresolved merge conflict between the
two patches whose lead patch alone did not pass both suites; not a test failure of merged code. AVP's failures are test failures of the code itself (the same
two tasks the solo agent also fails; Pillow and Typst passed once bug 3 below was fixed and the
two tasks replayed).

## The pilot pair (pallets_click_task 2068, features 1 and 2)

Eight attempts (five with Sonnet 5.5, three with Opus 5.5), same outcome every time: integration
clean (identical patches, merge skipped), feature 2 at 66/66, feature 1 at 67/68. The failing test, `test_edit_with_custom_environment`,
mocks `Popen` with a `wait()` that takes no `timeout`, and the feature 2 agent always calls
`wait(timeout=self.timeout)`. A semantic conflict between two correct features, invisible to both
agents (the tests are hidden): exactly what the benchmark is built to measure. Not an integration
failure, and not something a tool can fix without telling the agents about the hidden test; the
goal's `both_passed` on this pair was not reached. Cost of the eight attempts: 2,2 $.

## Bugs found by the benchmark and fixed (all in `Mossab28/avp`)

1. After a land, the view believed it held the line while the disk only had its own writes: a
   teammate's files never arrived in the container. Landing now lays the line out; `open --fresh`.
2. Routes took only a repo id: a repo name answered an empty list, so the second agent never
   waited for the first. Every route resolves a name.
3. The second agent attached while the first agent's import of a large repository (Pillow,
   1 477 files; Typst, 1 924 files) was still running: no view, `avp_open` failed, its work
   never landed. The adapter now waits for the head, not the repo record, and lands work left
   in the tree as a safety net.
4. `avp attach`: a view on an existing repo without opening a task (new verb, for containers
   that already hold the same checkout).

## Replay

On the host (see `bench/cooperbench/`): `setup-desktop.sh` once, `install-bench.sh` after each
AVP change, then

```sh
bash run-flash.sh solo && bash run-flash.sh git && bash run-flash.sh avp
python3 collect-flash.py
```

`pilot-avp.sh` runs the pilot pair. Logs under `CooperBench/logs/flash-*/`, one `eval.json`
per task.
