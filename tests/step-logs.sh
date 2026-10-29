#!/bin/bash
# What a proof keeps beside its output: each step's whole log, each line dated (`avp proof --step`),
# and the files its `artifacts` name, packed and downloaded (`avp proof --artifacts`), on the server
# and from a worker; never a file outside the step's folder; `avp_proof` reads the red step where it
# first fails, not the whole log.
# Usage: AVP=target/release/avp bash tests/step-logs.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/step-logs.sh <scratch dir>}/sl.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7475 AVP_PERSON=ada
# Workers on, and a proof no worker claims within a second proven by the server itself: both paths.
AVP_WORKERS=1 AVP_WORKER_CLAIM_S=1 $T serve --listen 127.0.0.1:7475 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
WP=
trap 'kill $SP $WP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir app && cd app || exit 1
printf 'fine\n' > app.txt
# The tests print more than the 60 kB a proof's output keeps (the line 2000 falls in what it cuts),
# leave a coverage report with a link to a file of the machine in it, and fail on "bug".
cat > avp.toml <<'EOF'
[check]
[[check.steps]]
name = "build"
run = "echo compiling; sleep 1.2; echo built"
[[check.steps]]
name = "tests"
run = "mkdir -p coverage && echo '<html>covered</html>' > coverage/index.html && ln -sf /etc/hosts coverage/hosts && seq -f 'line %g of the test run' 1 6000 && echo 'test parse ... FAILED' && echo 'test result: FAILED. 1 failed' && ! grep -q bug app.txt"
artifacts = ["coverage/", "playwright-report/"]
[[check.full_shards]]
name = "e2e"
run = "mkdir -p shots && echo png > shots/home.png && echo e2e green"
artifacts = ["shots/"]
EOF
$T init sl >/dev/null
cd "$W" || exit 1

$T open -C v --repo sl --goal "a change the tests refuse" >/dev/null && printf 'bug\n' > v/app.txt
$T -C v land >/dev/null 2>&1
ok "red, proven on the server" 'grep -q "proving here" server.log'
whole=$($T -C v proof)
ok "avp proof prints each step's whole log, each line after the time since the step started" '[[ "$whole" == *"started "*" UTC"* ]] && grep -Eq "^ +0:00\.[0-9]  compiling$" <<<"$whole" && grep -Eq "^ +0:01\.[0-9]  built$" <<<"$whole"'
tests=$($T -C v proof --step tests)
ok "--step prints that step's log whole, past what its output keeps" '[[ "$tests" == *"line 2000 of the test run"* && "$tests" != *"compiling"* ]] && ! $T -C v --json proof | grep -q "line 2000 of"'
ok "the artifacts are named, and what was not made" '[[ "$tests" == *"artifacts: 1 file ("*"missing: playwright-report/"*"left out: coverage/hosts"* ]]'
json=$($T -C v --json proof --step tests)
ok "--json gives the dated lines and where it first fails" 'python3 -I -c "import json,sys; l=json.load(sys.stdin); assert l[\"kept\"] and l[\"total\"]==6002 and l[\"lines\"][l[\"first\"]][1]==\"test parse ... FAILED\" and l[\"started\"]" <<<"$json"'
ok "--artifacts downloads them, a folder per step, never the link out of the folder" '$T -C v proof --artifacts out | grep -q "tests: 1 file (" && [ "$(cat out/tests/coverage/index.html)" = "<html>covered</html>" ] && [ ! -e out/tests/coverage/hosts ]'
proof=$($T -C v --json proof | python3 -I -c "import json,sys; print(json.load(sys.stdin)['id'])")
ok "AVP Live reads the step's log count and artifacts, and links the pack" 'curl -sf "$AVP_SERVER/repos/sl/proofs/$proof" | grep -q "\"log\":6002" && curl -sf -o pack.tgz "$AVP_SERVER/repos/sl/proofs/$proof/artifacts?step=tests" && tar -tzf pack.tgz | grep -q coverage/index.html'
mcp=$(cd v && AVP_MCP_LOG= python3 -I -c '
import json, subprocess, sys
p = subprocess.Popen([sys.argv[1], "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def call(n, method, params):
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n"); p.stdin.flush()
    while True:
        m = json.loads(p.stdout.readline())
        if m.get("id") == n: return m
call(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})
print(call(2, "tools/call", {"name": "avp_proof", "arguments": {}})["result"]["content"][0]["text"])
p.stdin.close(); p.wait(timeout=20)' "$T" 2>&1)
ok "avp_proof reads the red step where it first fails, in few lines" '[[ "$mcp" == *"tests: lines 5991-6002 of 6002, it first fails at line 6001"*"test parse ... FAILED"*"artifacts kept: tests (1 file)"* && "$mcp" != *"line 2000 of"* ]] && [ "$(wc -l <<<"$mcp")" -lt 30 ]'

$T worker --data "$W/worker" >"$W/worker.log" 2>&1 & WP=$!
sleep 1
printf 'still a bug\n' > v/app.txt
$T -C v land >/dev/null 2>&1
ok "red again, proven on a worker" 'grep -q red worker.log'
ok "the worker sent the whole log and the artifacts back, and keeps none" '$T -C v proof --step tests | grep -q "line 2000 of the test run" && $T -C v proof --artifacts out2 >/dev/null && [ -f out2/tests/coverage/index.html ] && [ -z "$(ls worker/kept 2>/dev/null)" ]'

printf 'fixed\n' > v/app.txt
ok "fixed, it lands" '$T -C v land | grep -q "landed"'
for _ in $(seq 60); do curl -s "$AVP_SERVER/repos/sl/events?limit=1000" | grep -q '"line.green"' && break; sleep 0.5; done
full=$(curl -s "$AVP_SERVER/repos/sl/events?limit=1000" | grep -o '"line.green"[^}]*"proof":"proof_[a-z0-9]*"' | grep -o 'proof_[a-z0-9]*' | head -1)
ok "a shard of the whole suite keeps its artifacts too" '[ -n "$full" ] && $T -C app proof "$full" --artifacts out3 >/dev/null && [ "$(cat out3/e2e/shots/home.png)" = png ]'
