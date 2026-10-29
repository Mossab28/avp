#!/bin/bash
# `avp stats`: a team's numbers in one command, as a person reads them, as JSON, through the route
# AVP Live charts, and through the MCP tool an agent calls. The GitHub before is tested on recorded
# answers (tests/stats.rs): here only that it asks for its token without reaching the network.
# Usage: AVP=target/release/avp bash tests/stats.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/stats.sh <scratch dir>}/st.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7442 AVP_PERSON=ada AVP_AGENT=codex
$T serve --listen 127.0.0.1:7442 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
# Fields of a JSON answer, space separated: field "$json" "v['a'], v['b']['c']".
field(){ python3 -c "import json,sys; v=json.loads(sys.argv[1]); r=eval(sys.argv[2]); print(*(r if isinstance(r, tuple) else (r,)))" "$1" "$2"; }
mkdir app && cd app || exit 1
cat > avp.toml <<'EOF'
[check]
full = "true"
[[check.steps]]
name = "lint"
run = "true"
[envs.staging]
follow = "full"
EOF
$T init st >/dev/null
cd "$W" || exit 1
mkdir one && $T open -C one --repo st --goal "a first change" >/dev/null && printf 'one\n' > one/one.txt && $T -C one land >/dev/null
# Staging follows the full suite: the landing is served once the suite is green.
for _ in $(seq 1 120); do [[ "$($T envs --repo st)" == *"serves position 1"* ]] && break; sleep 0.5; done

j=$($T stats --repo st --json)
ok "--json counts the landing, its agent and its person" '[ "$(field "$j" "v[\"landings\"], v[\"by_agent\"][\"codex\"], v[\"by_person\"][\"ada\"]")" = "1 1 1" ]'
ok "and times it to the line, the full suite and staging" '[ "$(field "$j" "v[\"opened_to_landed\"][\"count\"], v[\"landed_to_full\"][\"count\"], v[\"envs\"][\"staging\"][\"landed_to_served\"][\"count\"], v[\"proofs\"]")" = "1 1 1 1" ]'
t=$($T stats --repo st)
ok "the table reads in the README's words" '[[ "$t" == *"To main"*"To staging"*"Changes shipped      1 landing"*"Checking one change"*"CI machine time"* ]]'
ok "a window before the line is empty" '[ "$(field "$($T stats --repo st --since 2020-01-01 --until 2020-01-02 --json)" "v[\"landings\"], v[\"since\"]")" = "0 2020-01-01T00:00:00Z" ]'
ok "the route takes how long ago, and says what a time looks like" '[ "$(field "$(curl -s "$AVP_SERVER/repos/st/stats?since=48h")" "v[\"landings\"]")" = 1 ] && [ "$(curl -s -o /dev/null -w "%{http_code}" "$AVP_SERVER/repos/st/stats?since=yesterday")" = 400 ]'
ok "--github needs its window" '! $T stats --repo st --github acme/shop'
ok "the before asks for GH_TOKEN" '[[ "$(env -u GH_TOKEN -u GITHUB_TOKEN $T stats --repo st --github acme/shop --before 2026-10-22..2026-10-24 2>&1)" == *"set GH_TOKEN"* ]]'
m=$(cd one && python3 -c '
import json, subprocess, sys
p = subprocess.Popen([sys.argv[1], "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def call(n, method, params):
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n"); p.stdin.flush()
    while True:
        m = json.loads(p.stdout.readline())
        if m.get("id") == n: return m
call(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})
r = call(2, "tools/call", {"name": "avp_stats", "arguments": {"since": "7d"}})
print(json.loads(r["result"]["content"][0]["text"])["landings"])
p.stdin.close(); p.wait(timeout=20)' "$T" 2>&1)
ok "avp_stats gives an agent the same numbers" '[ "$m" = 1 ]'
