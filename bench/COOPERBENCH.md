# CooperBench with AVP: the plan

CooperBench (Stanford + SAP, ICLR 2026, github.com/cooperbench/CooperBench): 652 tasks on 12
real repositories (Python, TypeScript, Go, Rust), two agents, two features that may conflict,
expert-written tests. Their `coop` setting: one container per agent, Redis messaging, an
optional shared git remote. Their evaluation (`eval/sandbox.py`, `test_merged`): apply each
agent's patch on its own branch, merge (naive, then union, then an LLM resolver), run both
feature test suites on the merged tree. Unresolved conflict = both fail. Headline: GPT-5 and
Sonnet 4.5 keep about 60 % of their solo success in coop (retention 0.64 / 0.60, table 5).

The harness already has a `claude_code` adapter (official CLI in the container, headless
stream-json, host OAuth credentials forwarded) and a `_coop` module for git remotes.

## The variable we add

A fourth setting, `coop-avp`: the two agents keep Redis messaging, but the repository is a
AVP repo. Each container is a view (`avp mcp`, `.mcp.json`, git and gh denied), the AVP
server runs beside the containers, both agents open tasks, write, check, land. At the end, the
line's final tree is diffed against the base commit and written as **the same patch for both
agents**: the evaluator sees identical trees, skips its merge, and runs both suites. No change
to the evaluator; the only variable is how the two agents' work became one tree.

## What it takes

1. A Docker host (we used WSL on a Windows machine). Build `avp` for
   Linux there (`cargo build --release`), or cross-compile.
2. In `cooperbench/agents/claude_code/adapter.py`: when the setting is `coop-avp`, install
   `avp` in the container, write `.mcp.json`, append the AVP system prompt, deny git/gh,
   run `avp open` on start and `avp land` as a final safety net if the agent forgot.
3. A `avp serve` per task (fresh data dir, `AVP_CHECK_SLOTS=2`), reachable from both
   containers (`host.docker.internal`, as the adapter already does for the API base URL).
   `avp.toml` per task from `run_tests.sh`: setup = the task's dependency install, steps =
   the task's test command restricted to the touched files when possible, full = the task's
   whole test run.
4. After the run: export the line, `git diff <base>` in a checkout, write `patch.diff` for
   both agents, then `cooperbench eval` as usual.
5. Three conditions on the same tasks: `solo`, `coop --git` (their protocol), `coop-avp`.
   Subset: 60 tasks drawn across the 12 repositories (the Harbor "flash" subset is 50 pairs
   and is a known reference: use it). Model: Sonnet 5.5 (the paper used Sonnet 4.5).

Cost order of magnitude: 50 tasks × 2 agents × 3 conditions ≈ 300 sessions, plus the
solo ones. To be announced before launch.

## What we would publish

Retention (coop / solo) per condition, per repository and per difficulty bucket, with
Wilson intervals like the paper; the conflict count at integration (their merge log versus
AVP's replays and settled conflicts); verification time per task. Harness and patches in
this repository so anyone reruns it.
