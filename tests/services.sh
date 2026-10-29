#!/bin/bash
# Services around the tests ([[check.services]]): a `run` service starts before a proof's steps, on
# its own port and with its own data, its env reaches every step, and it is stopped and its data
# removed after, green or red; one never ready fails the proof with that reason, before any step; a
# shard of the whole suite starts only the services it names, and a matrix runs it once per value.
# Usage: AVP=target/proof/avp bash tests/services.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/services.sh <scratch dir>}/sv.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7444 AVP_PERSON=ada
# No speculative proof: each step below runs once per `avp check`, so the runs can be counted.
AVP_SPECULATE=0 $T serve --listen 127.0.0.1:7444 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
# The service: an HTTP server on the port AVP picks, serving its data folder, where it writes who it
# is. Each start records its process; the steps reach it through DB_URL, and say what they saw.
mkdir app && cd app || exit 1
cat > avp.toml <<EOF
[check]
[[check.services]]
name = "db"
run = "echo \$\$ >> '$W/pids'; echo \$\$ > {data}/owner; exec python3 -I -m http.server {port} --bind 127.0.0.1 --directory {data}"
env = { DB_URL = "http://127.0.0.1:{port}", DB_DATA = "{data}" }

[[check.steps]]
name = "tests"
run = "curl -sf \"\$DB_URL/owner\" >/dev/null && echo \"\$DB_URL \$DB_DATA\" >> '$W/seen' && sleep 2 && test ! -f red.txt"

[[check.full_shards]]
name = "unit"
run = "echo \"\$MATRIX_NODE \${DB_URL:-none}\" >> '$W/full'"
services = []
matrix = { node = ["20", "22"] }

[[check.full_shards]]
name = "integration"
run = "curl -sf \"\$DB_URL/owner\" >/dev/null && echo \"db \$DB_URL\" >> '$W/full'"
EOF
$T init sv >/dev/null
cd "$W" || exit 1
stopped(){ for p in $(cat "$W/pids"); do kill -0 "$p" 2>/dev/null && return 1; done; return 0; }
# The services' data is kept beside the slots, under the repo's id.
no_data(){ [ -z "$(ls -d "$W"/data/work/*/slot-*.services 2>/dev/null)" ]; }
started(){ wc -l < "$W/pids" | tr -d ' '; }

mkdir a b && $T open -C a --repo sv --goal "one change" >/dev/null && $T open -C b --repo sv --goal "another change" >/dev/null
printf 'a\n' > a/a.txt && printf 'b\n' > b/b.txt
$T -C a check >"$W/a.out" 2>&1 & A=$!
$T -C b check >"$W/b.out" 2>&1 & B=$!
wait $A $B
ok "two proofs at once, each with the service up: both green" 'grep -q Green a.out && grep -q Green b.out'
ok "each proof had its own port and its own data" '[ "$(wc -l < seen | tr -d " ")" = 2 ] && [ "$(cut -d" " -f1 seen | sort -u | wc -l | tr -d " ")" = 2 ] && [ "$(cut -d" " -f2 seen | sort -u | wc -l | tr -d " ")" = 2 ]'
ok "the services are stopped and their data removed once the proofs are over" 'stopped && no_data'

printf 'x\n' > a/red.txt
ok "a red step" '! $T -C a check | grep -q Green'
ok "the service of a red proof is stopped too" '[ "$(started)" = 3 ] && stopped && no_data'

# The whole suite: the unit shard twice (node 20, node 22), without the service it does not name;
# the integration shard with it.
ok "a change lands" '[[ "$($T -C b land)" == "landed at position 1"* ]]'
for _ in $(seq 1 60); do [ "$(cat full 2>/dev/null | wc -l | tr -d ' ')" = 3 ] && break; sleep 0.5; done
ok "the matrix ran the shard once per value, without the service" 'grep -qx "20 none" full && grep -qx "22 none" full'
ok "the shard that needs the service reached it" 'grep -q "^db http://127.0.0.1:" full'
verdict(){ for _ in $(seq 1 60); do v=$($T log --repo sv | awk -v p="$1" '$1 == p { print $2 }'); [[ "$v" == Green || "$v" == Red ]] && break; sleep 0.5; done; echo "$v"; }
ok "the whole suite is green" '[ "$(verdict 1)" = Green ]'
ok "every service is stopped after the suite" 'stopped && no_data'

# A service that never answers. A task is proven under its own avp.toml: red for that reason,
# before any step runs.
mkdir c && $T open -C c --repo sv --goal "a service that is never ready" >/dev/null
python3 -I - "$W/c/avp.toml" <<'PY'
import sys
p = sys.argv[1]
t = open(p).read().replace('env = {', 'ready = "false"\nready_s = 2\nenv = {', 1).replace("exec python3", "exec sleep 60 #", 1)
open(p, "w").write(t)
PY
rm -f seen
$T -C c check >"$W/c.out" 2>&1
ok "a service never ready fails the proof with that reason" 'grep -q "service db" c.out && grep -q "was not ready after 2s" c.out'
ok "no step ran" '[ ! -f seen ]'
ok "the services that were never ready are stopped" 'stopped && no_data'
