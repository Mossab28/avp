#!/bin/bash
# Secrets for tests: `avp secret set` reads the value on stdin, never as an argument; the server
# keeps it encrypted; only the steps avp.toml names it for get it, on the server and on a worker; and
# what they print of it reads *** in all AVP keeps and shows.
# Usage: AVP=target/release/avp bash tests/secrets.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/secrets.sh <scratch dir>}/se.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7461 AVP_PERSON=ada AVP_TOKEN=avp-s3cret-7461
# Workers on, and a proof no worker claims within a second proven by the server itself: both paths.
AVP_WORKERS=1 AVP_WORKER_CLAIM_S=1 $T serve --listen 127.0.0.1:7461 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
WP=
trap 'kill $SP $WP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
events(){ curl -s -u "ada:$AVP_TOKEN" "$AVP_SERVER/repos/se/events?limit=1000"; }
KEY=payment-sandbox-value-5c71e2
mkdir app && cd app || exit 1
printf 'v0\n' > app.txt
cat > avp.toml <<EOF
[check]
[[check.steps]]
name = "unit"
run = "env > '$W/unit-env.txt'"
[[check.steps]]
name = "integration"
run = "echo charging with \$STRIPE_KEY; printf %s \"\$STRIPE_KEY\" > '$W/seen-'\$(cat app.txt)"
secrets = ["STRIPE_KEY"]
[[check.full_shards]]
name = "e2e"
run = "echo the whole suite charges with \$STRIPE_KEY"
secrets = ["STRIPE_KEY"]
EOF
$T init se >/dev/null
cd "$W" || exit 1
ok "a value given as an argument is refused" '! $T -C app secret set STRIPE_KEY "$KEY" && [[ "$($T -C app secret list)" == "no secret"* ]]'
ok "the value is read from stdin" 'echo "$KEY" | $T -C app secret set STRIPE_KEY | grep -q "^STRIPE_KEY kept"'
ok "a name a step never gets is refused" 'out=$(printf %s aws-s3cret-7461 | $T -C app secret set AWS_SECRET_ACCESS_KEY 2>&1); [ $? != 0 ] && [[ "$out" == *"never reach a step"* ]]'
ok "the list says names and dates, never a value" 'l=$($T -C app secret list) && [[ "$l" == "STRIPE_KEY "*UTC && "$l" != *"$KEY"* ]] && [[ "$($T -C app --json secret list)" == *"\"set_at\""* ]]'
mcp=$(cd app && AVP_MCP_LOG= python3 -I -c '
import json, subprocess, sys
p = subprocess.Popen([sys.argv[1], "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def call(n, method, params):
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n"); p.stdin.flush()
    while True:
        m = json.loads(p.stdout.readline())
        if m.get("id") == n: return m
call(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})
print(call(2, "tools/call", {"name": "avp_secrets", "arguments": {"repo": "se"}})["result"]["content"][0]["text"])
p.stdin.close(); p.wait(timeout=20)' "$T" 2>&1)
ok "avp_secrets tells an agent the names and dates, never a value" '[[ "$mcp" == "STRIPE_KEY  set "*" UTC"*"avp secret set NAME"* && "$mcp" != *"$KEY"* ]]'
ok "the value is sealed with XChaCha20-Poly1305" 'grep -q "\"cipher\": \"xchacha20-poly1305\"" data/secrets.json'

$T open -C v --repo se --goal "a change proven on the server" >/dev/null && printf 'v1\n' > v/app.txt
land=$($T -C v --json land)
ok "landed, proven on the server" '[[ "$land" == *"\"position\": 1"* ]] && grep -q "proving here" server.log'
ok "the step that names it gets it, without the newline echo added" '[ "$(cat seen-v1)" = "$KEY" ]'
ok "a step that does not name it does not get it" '[ -s unit-env.txt ] && ! grep -q STRIPE_KEY unit-env.txt'
proof=$(grep -o 'proof_[a-z0-9]*' <<<"$land" | head -1)
ok "the proof masks what the step printed of it" 'p=$($T -C app proof "$proof") && [[ "$p" == *"charging with ***"* && "$p" != *"$KEY"* ]]'
for _ in $(seq 60); do events | grep -q '"line.green"' && break; sleep 0.5; done
full=$(events | grep -o '"line.green"[^}]*"proof":"proof_[a-z0-9]*"' | grep -o 'proof_[a-z0-9]*' | head -1)
ok "the whole suite's shard gets it, and its output masks it" '[ -n "$full" ] && p=$($T -C app proof "$full") && [[ "$p" == *"charges with ***"* && "$p" != *"$KEY"* ]]'

$T worker --data "$W/worker" >"$W/worker.log" 2>&1 & WP=$!
$T open -C v --repo se --goal "a change proven on a worker" >/dev/null && printf 'v2\n' > v/app.txt
land=$($T -C v --json land)
ok "landed, proven on a worker" '[[ "$land" == *"\"position\": 2"* ]] && grep -q green worker.log'
ok "the worker's step that names it gets it" '[ "$(cat seen-v2)" = "$KEY" ]'
proof=$(grep -o 'proof_[a-z0-9]*' <<<"$land" | head -1)
ok "the worker's proof masks it" 'p=$($T -C app proof "$proof") && [[ "$p" == *"charging with ***"* && "$p" != *"$KEY"* ]]'
ok "the events say who set it, never its value" 'e=$(events) && [[ "$e" == *"\"secret.set\""* && "$e" == *"\"by\":\"ada\""* && "$e" != *"$KEY"* ]]'
ok "the key is the server's user's alone" '[ "$(stat -c %a data/secrets.key 2>/dev/null || stat -f %Lp data/secrets.key)" = 600 ]'
ok "nothing the server or the worker stores or logs holds it" '! grep -rqF "$KEY" data worker server.log worker.log'

ok "removed, it is listed no more" '$T -C app secret rm STRIPE_KEY | grep -q forgotten && [[ "$($T -C app secret list)" == "no secret"* ]]'
$T open -C v --repo se --goal "a change whose step lacks its secret" >/dev/null && printf 'v3\n' > v/app.txt
ok "a step that names a secret not set is red, and says how to set it" 'out=$($T -C v check); [ $? != 0 ] && [[ "$out" == *"avp secret set STRIPE_KEY"* ]] && [ ! -e seen-v3 ]'
