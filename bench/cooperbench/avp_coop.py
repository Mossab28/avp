"""AVP integration for the Claude Code adapter (CooperBench `coop` setting + CB_AVP=1).

The two agents share one AVP repository instead of a git remote: each container is a view,
the agent opens a task, writes, checks, lands through the `avp_*` MCP tools. When both agents
are done, each container lays the line's final tree out and diffs it against the base commit, so
both submit the same `patch.txt` and the evaluator skips its merge.
"""

from __future__ import annotations

import json
import os
import shlex
import time
from typing import Any

from cooperbench.agents._coop.prompt import _coop_block
from cooperbench.agents._coop.runtime import CONTAINER_REPO_PATH, read_file_from_container, write_file_in_container

CONTAINER_MCP_CONFIG = "/tmp/cb-avp-mcp.json"
CONTAINER_AVP_BIN = "/usr/local/bin/avp"


def enabled() -> bool:
    return os.environ.get("CB_AVP", "").strip() not in ("", "0", "false")


def server_url() -> str:
    """The server as the containers reach it."""
    return os.environ.get("CB_AVP_SERVER", "http://host.docker.internal:7117").strip()


def host_binary() -> str:
    return os.environ.get("CB_AVP_BIN", "/root/bench/avp-musl").strip()


def repo_name(comm_url: str | None, image: str) -> str:
    """One AVP repo per task run: the run id the runner put in the Redis URL."""
    run_id = ""
    if comm_url and "#run:" in comm_url:
        run_id = comm_url.split("#run:", 1)[1].strip()
    if not run_id:
        run_id = image.replace("/", "-").replace(":", "-")
    return f"cb-{run_id}"


def run_args() -> list[str]:
    return ["-v", f"{host_binary()}:{CONTAINER_AVP_BIN}:ro"]


def mcp_config(agent_id: str) -> dict[str, Any]:
    return {
        "mcpServers": {
            "avp": {
                "command": CONTAINER_AVP_BIN,
                "args": ["mcp"],
                "env": {"AVP_SERVER": server_url(), "AVP_AGENT": "claude-code", "AVP_PERSON": agent_id},
            }
        }
    }


def claude_flags() -> str:
    return (
        f"--mcp-config {CONTAINER_MCP_CONFIG} --strict-mcp-config "
        f"--disallowedTools {shlex.quote('Bash(git:*),Bash(gh:*)')} "
    )


_AVP_BLOCK = """## Version control: AVP, not git

This repository is a AVP view shared with your peer. AVP is exposed to you as MCP tools
named avp_*. Never run git or gh; never write patch.txt: the bench exports the landed line
itself when both of you are done.

The round is: avp_open (one line goal, and the paths you intend to change as scope) before
you write anything; write files normally with your editing tools; avp_check when you want
the server's verdict; avp_land to land your work on the shared line. Land as soon as your
feature works and its tests pass: your peer's land will replay on top of yours. If avp_land
reports a conflict, the conflict markers are now in the files it names: settle them (keep both
intents) and land again. If it reports red or blocked, read the proof, fix, land again.
Call avp_events at the start of your work and before landing to see what your peer did on
your files; avp_tasks shows who holds what. A lease on a file is advisory: writing a file
your peer holds goes through, but tells them. Prefer coop-send for intent, AVP for facts.

You are done only when avp_land has answered "landed". Your final message states the task
id and the position it landed at.
"""


def instruction(task: str, *, agent_id: str, agents: list[str]) -> str:
    partners = [a for a in agents if a != agent_id]
    return f"{task}\n\n{_coop_block(agent_id, partners)}\n{_AVP_BLOCK}"


ADAPTER_LOG = os.environ.get("CB_AVP_LOG", "/root/bench/avp-adapter.log")


def _sh(env, cmd: str, timeout: int = 300) -> dict[str, Any]:
    r = env.execute({"command": cmd}, timeout=timeout)
    try:
        with open(ADAPTER_LOG, "a") as f:
            f.write(f"\n=== {time.strftime('%H:%M:%S')} rc={r.get('returncode')}\n$ {cmd[:400]}\n{(r.get('output') or '')[-1500:]}\n")
    except OSError:
        pass
    return r


def prepare(env, *, agent_id: str, agents: list[str], repo: str, logger) -> None:
    """Before Claude starts: the first agent imports the repo, the others attach to it."""
    write_file_in_container(env, CONTAINER_MCP_CONFIG, json.dumps(mcp_config(agent_id), indent=2))
    srv = server_url()
    base = f"export AVP_SERVER={shlex.quote(srv)} AVP_PERSON={shlex.quote(agent_id)} AVP_AGENT=cooperbench; cd {CONTAINER_REPO_PATH} && "
    _sh(env, base + "git rev-parse HEAD > /tmp/cb-base-sha.txt")
    if agent_id == agents[0]:
        r = _sh(env, base + f"{CONTAINER_AVP_BIN} init {shlex.quote(repo)} 2>&1")
        logger.info("avp init: %s", (r.get("output") or "")[-300:])
    else:
        r = _sh(
            env,
            base
            # The repo record appears before its import finishes: wait for the head (position 0).
            + f"for i in $(seq 1 300); do curl -sf {shlex.quote(srv)}/repos/{shlex.quote(repo)}/head >/dev/null 2>&1 && break; sleep 2; done; "
            f"{CONTAINER_AVP_BIN} attach --repo {shlex.quote(repo)} 2>&1",
            timeout=700,
        )
        logger.info("avp attach: %s", (r.get("output") or "")[-300:])


def finish(env, *, agent_id: str, agents: list[str] | None = None, repo: str, logger, wait_s: int = 1200) -> str:
    """After Claude exits: land what the agent left (a task still open, or writes it never put
    in a task), mark this agent done, wait for every peer's mark and for the repo to have no open
    task, lay the line's head out and write patch.txt from it. Returns the patch text."""
    srv = server_url()
    base = f"export AVP_SERVER={shlex.quote(srv)} AVP_PERSON={shlex.quote(agent_id)} AVP_AGENT=cooperbench; cd {CONTAINER_REPO_PATH} && "
    # No view at all (attach failed): attach now, known = head, local edits untouched.
    _sh(env, base + f"test -f .avp/view.json || {CONTAINER_AVP_BIN} attach --repo {shlex.quote(repo)} 2>&1")
    st = _sh(env, base + f"{CONTAINER_AVP_BIN} status 2>&1 | head -2")
    if "no task open" in (st.get("output") or ""):
        # Open a task and land: the sync inside `land` sends only what differs from the view's
        # known state, and an empty task is refused by the server ("empty"), then abandoned.
        r = _sh(env, base + f"{CONTAINER_AVP_BIN} open --repo {shlex.quote(repo)} --goal {shlex.quote(agent_id + ': work left in the tree (safety net)')} 2>&1 | tail -1; {CONTAINER_AVP_BIN} land 2>&1 | tail -3", timeout=900)
        out = r.get("output") or ""
        logger.info("avp safety net for %s: %s", agent_id, out[-300:])
        if "landed at" not in out:
            _sh(env, base + f"{CONTAINER_AVP_BIN} abandon 2>&1")
    else:
        r = _sh(env, base + f"{CONTAINER_AVP_BIN} land 2>&1 | tail -3", timeout=900)
        logger.info("avp land (task left open) for %s: %s", agent_id, (r.get("output") or "")[-300:])
        if "landed at" not in (r.get("output") or ""):
            _sh(env, base + f"{CONTAINER_AVP_BIN} abandon 2>&1")
    # Mark this agent done: an environment pointer named after it.
    _sh(env, base + f"POS=$(curl -sf {shlex.quote(srv)}/repos/{shlex.quote(repo)}/head | grep -o '\"position\":[0-9]*' | head -1 | tr -cd 0-9); {CONTAINER_AVP_BIN} promote --force done-{shlex.quote(agent_id)} ${{POS:-0}} 2>&1 | tail -1")
    peers = [a for a in (agents or []) if a != agent_id]
    # Wait for every peer's mark and no open task left, then export.
    cond = " && ".join([f"curl -sf '{srv}/repos/{repo}/envs' | grep -q 'done-{p}'" for p in peers] + [f"[ \"$(curl -sf '{srv}/repos/{repo}/tasks?state=open' | tr -cd '{{' | wc -c)\" = 0 ]"])
    _sh(env, base + f"for i in $(seq 1 {wait_s // 5}); do if {cond}; then break; fi; sleep 5; done", timeout=wait_s + 60)
    r = _sh(
        env,
        base
        + f"{CONTAINER_AVP_BIN} open --fresh --repo {shlex.quote(repo)} --goal export 2>&1 | tail -1 && {CONTAINER_AVP_BIN} abandon >/dev/null 2>&1; "
        "git add -A >/dev/null 2>&1; git reset -q -- .avp .mcp.json patch.txt avp.toml 2>/dev/null; git diff --cached $(cat /tmp/cb-base-sha.txt) > patch.txt; git diff --cached --stat $(cat /tmp/cb-base-sha.txt) | tail -1; "
        f"{CONTAINER_AVP_BIN} log --limit 6 2>&1",
        timeout=600,
    )
    logger.info("avp export for %s: %s", agent_id, (r.get("output") or "")[-400:])
    return read_file_from_container(env, f"{CONTAINER_REPO_PATH}/patch.txt")
