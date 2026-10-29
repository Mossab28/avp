# CooperBench with AVP: the adapter

Files to drop into a CooperBench checkout (`src/cooperbench/agents/claude_code/`): `adapter.py`
(their Claude Code adapter with four hooks) and `avp_coop.py` (the hooks' implementation).
`CB_AVP=1` on the `coop` setting turns the shared git remote into a AVP repository: the first
agent imports `/workspace/repo`, the second attaches once the head exists, both get `avp mcp`
through `.mcp.json`, git and gh are denied, and when both are done each container lays the line's
head out and diffs it against the base commit, so the evaluator sees identical patches.

Host side: `avp serve --listen 0.0.0.0:7117` (static binary `avp-musl` mounted read-only into
every container), `CB_AVP_BIN`, `CB_AVP_SERVER=http://host.docker.internal:7117`.
`setup-desktop.sh` and `install-bench.sh` prepare a WSL host; `run-flash.sh <solo|git|avp>` runs
the flash_10 subset in one condition; `collect-flash.py` builds the table.
