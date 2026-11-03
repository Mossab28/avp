#!/bin/bash
# What a tester driving agents on AVP Cloud found: an agent takes its task back after its MCP server
# restarts, a claim names the task it takes a lease from and that task hears it, `avp guarded`
# shows avp.toml alone, and `avp whoami` says it is the AVP_TOKEN that is refused.
# Usage: AVP=target/release/avp bash tests/agent-fixes.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/agent-fixes.sh <scratch dir>}/af.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7491 AVP_MCP_LOG= AVP_SPECULATE=0 AVP_PERSON=ada
unset AVP_VIEW_IN_CHECKOUT AVP_TOKEN AVP_REPO
$T serve --listen 127.0.0.1:7491 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
for _ in $(seq 50); do curl -s $AVP_SERVER/ >/dev/null && break; sleep 0.2; done
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir -p src && printf 'a\n' > src/a.txt && printf 'b\n' > src/b.txt && $T init af >/dev/null

# A task opened before the MCP server (re)started: a new session takes it back by its id.
mkdir v && id=$($T open -C v --repo af --goal "first" --scope src/ --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
mcp(){ (cd "$W" && printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"e2e"}}}' "$@" | AVP_REPO=af AVP_VIEWS="$W/views" $T mcp); }
answer(){ python3 -c "import json,sys; print([json.loads(l) for l in sys.argv[1].splitlines() if '\"id\"' in l][int(sys.argv[2])]['result']['content'][0]['text'])" "$1" "$2"; }
out=$(mcp '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"avp_status","arguments":{"task":"'"$id"'"}}}')
ok "avp_status with its task id takes back an open task of the same person, in its folder" '[[ "$(answer "$out" 1)" == *"folder: $W/v"*"task $id (Open)"* ]]'
out=$(AVP_PERSON=bo mcp '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"avp_status","arguments":{"task":"'"$id"'"}}}')
ok "another person's is not" '[[ "$out" == *"is ada'"'"'s, not yours (bo)"* ]]'

# A claim on a path another task's scope holds (`src/`, with its slash) names that task, which hears it.
mkdir w && $T open -C w --repo af --goal "second" >/dev/null
said=$($T -C w claim src/a.txt)
ok "a claim names the task it takes the lease from" '[[ "$said" == *"src/a.txt: yours now, taken from task $id"* ]]'
ok "the task that held it sees it in its status" '[[ "$($T -C v status)" == *"lease lost: src/a.txt: task task_"*"writes it too"* ]] && [ "$($T -C v events --json | grep -c lease.taken_over)" -ge 1 ]'

ok "avp guarded shows avp.toml, not its old name" '[[ "$($T guarded --repo af)" == *"guarded by AVP: avp.toml ("* ]] && [[ "$($T guarded --repo af)" != *trame* ]]'
$T user add ada >/dev/null 2>&1
far=$(AVP_TOKEN=avp_bogus $T whoami 2>&1)
ok "whoami with a bad AVP_TOKEN says the token is refused" '[[ "$far" == *"refuses the AVP_TOKEN set in your environment"* ]]'
