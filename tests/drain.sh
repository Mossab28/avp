#!/bin/bash
# The drain a server takes before a new version (POST /drain): the proof that runs goes to its end,
# the one that comes waits and its `avp land` says why, and once the drain is lifted it starts. A
# restart under a waiting land (the new version taking over) does not lose it: the CLI lands again
# once the server answers. GET /work counts the proofs in the server's own slots. Only the server's
# own machine drains it. Over MCP, avp_land says it too.
# Usage: AVP=target/release/avp bash tests/drain.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/drain.sh <scratch dir>}/dr.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7441 AVP_PERSON=ada AVP_SPECULATE=0 AVP_MCP_LOG=
within() { local i; for i in $(seq 1 $(($1 * 10))); do eval "$2" >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
start() { $T serve --listen 127.0.0.1:7441 --data "$W/data" >>"$W/server.log" 2>&1 & SP=$!; within 10 'curl -sf "$AVP_SERVER/repos"'; }
trap 'kill -9 $SP 2>/dev/null; touch "$W/go-a" "$W/go-b" "$W/go-c" "$W/go-d"' EXIT
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
work() { curl -s "$AVP_SERVER/work"; }
start
mkdir app && cd app || exit 1
printf 'app\n' > app.txt
# Tasks a, b, c and d land in that order, each adding <name>.txt: a proof is the newest one's, says
# so in `started`, then waits for `go-<name>` (a minute at most).
cat > avp.toml <<EOF
[[check.steps]]
name = "gate"
run = "who=nobody; for f in a b c d; do [ -f \$f.txt ] && who=\$f; done; echo \$who >> '$W/started'; for i in \$(seq 1 600); do [ -f '$W/go-'\$who ] && exit 0; sleep 0.1; done; exit 1"
EOF
$T init dr >/dev/null
cd "$W"
for v in a b c d; do mkdir $v && $T open -C $v --repo dr --goal "task $v" >/dev/null && printf '%s\n' $v > $v/$v.txt; done
$T -C a land > a.out 2>&1 & A=$!
within 30 'grep -qx a "$W/started"'
ok "GET /work counts the proof in a slot of the server without workers" '[[ "$(work)" == *"\"workers\":\"off\""* && "$(work)" == *"\"running\":1"* ]]'
ok "a request a proxy forwards may not drain the server" '[ "$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "X-Forwarded-For: 203.0.113.9" "$AVP_SERVER/drain")" = 403 ]'
ok "its own machine may" '[[ "$(curl -s -X POST "$AVP_SERVER/drain?minutes=5")" == *"\"draining\":true"* ]]'
$T -C b land > b.out 2>&1 & B=$!
within 30 '[[ "$(work)" == *"\"waiting\":1"* ]]'
ok "GET /work says it drains, with its counts" '[[ "$(work)" == *"\"draining\":true"* && "$(work)" == *"\"running\":1"* && "$(work)" == *"\"waiting\":1"* ]]'
within 30 'grep -q "the server is updating" b.out'
ok "the waiting land says the server is updating and its proof starts in a moment" 'grep -q "the server is updating to a new version: your proof starts in a moment" b.out'
touch "$W/go-a"; wait $A
ok "the proof that ran goes to its end, and its task lands" 'grep -q "^landed at position 1" a.out'
within 10 '[[ "$(work)" == *"\"running\":0"* ]]'
ok "the one that waits has not started" '[ "$(grep -c . "$W/started")" = 1 ]'
ok "DELETE /drain lifts the drain" '[[ "$(curl -s -X DELETE "$AVP_SERVER/drain")" == *"\"draining\":false"* ]]'
within 30 'grep -qx b "$W/started"'
touch "$W/go-b"; wait $B
ok "then its proof starts, and its task lands" 'grep -q "^landed at position 2" b.out'

# The update itself: a land waits for the drain, and the server restarts under it.
curl -s -X POST "$AVP_SERVER/drain?minutes=5" >/dev/null
$T -C c land > c.out 2>&1 & C=$!
within 30 'grep -q "the server is updating" c.out'
kill -9 $SP; wait $SP 2>/dev/null
touch "$W/go-c"
start
wait $C
ok "a land the restart cut says so, and lands again once the server answers" 'grep -q "the server restarts on its new version" c.out && grep -q "^landed at position 3" c.out'
ok "the server that starts does not drain" '[[ "$(work)" == *"\"draining\":false"* ]]'
ok "a drain lifts by itself at its bound" 'curl -s -X POST "$AVP_SERVER/drain?minutes=0.03" | grep -q "\"draining\":true" && sleep 2.5 && [[ "$(work)" == *"\"draining\":false"* ]]'

# Over MCP: avp_land while the server drains, its progress said to the host and in the answer.
curl -s -X POST "$AVP_SERVER/drain?minutes=5" >/dev/null
python3 -I -c '
import json, subprocess, sys
mcp = subprocess.Popen([sys.argv[1], "mcp"], cwd=sys.argv[2], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def send(m):
    mcp.stdin.write(json.dumps(m) + "\n")
    mcp.stdin.flush()
send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e"}}})
mcp.stdout.readline()
send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "avp_land", "arguments": {}, "_meta": {"progressToken": "land"}}})
for line in mcp.stdout:
    print(line, end="", flush=True)
    if json.loads(line).get("id") == 2:
        break
mcp.stdin.close()
' "$T" "$W/d" > d.out 2>&1 & D=$!
within 30 'grep -q "notifications/progress" d.out'
curl -s -X DELETE "$AVP_SERVER/drain" >/dev/null
touch "$W/go-d"; wait $D
ok "avp_land tells the host by a progress notification" 'grep "notifications/progress" d.out | grep "\"progressToken\":\"land\"" | grep -q "the server is updating to a new version"'
ok "and the agent in its answer, then it lands" 'grep "\"id\":2" d.out | grep -q "the server is updating to a new version.*landed at position 4"'
