#!/bin/bash
# A restart of the server does not lose a deploy AVP runs: the server that starts runs the deploy
# the one before it cut again, once, from the start (`env.interrupted`), and leaves one cut twice
# failed, with the reason. GET /work counts the deploys in flight, which the server's updater waits for.
# Usage: AVP=target/release/avp bash tests/deploy-restart.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/deploy-restart.sh <scratch dir>}/drs.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7422 AVP_PERSON=ada
within() { local i; for i in $(seq 1 $(($1 * 10))); do eval "$2" >/dev/null 2>&1 && return 0; sleep 0.1; done; return 1; }
start() { $T serve --listen 127.0.0.1:7422 --data "$W/data" >>"$W/server.log" 2>&1 & SP=$!; within 10 'curl -sf "$AVP_SERVER/repos"'; }
# What restarting the server's container does: the server and the deploy it runs die at once.
stop() { kill -9 "$SP" "$(cat "$W/$1.pid")" 2>/dev/null; wait "$SP" 2>/dev/null; }
runs() { cat "$W/$1.runs" 2>/dev/null | wc -l | tr -d ' '; }
trap 'kill -9 $SP 2>/dev/null; touch "$W/go"' EXIT
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
start
mkdir app && cd app || exit 1
printf 'v0\n' > app.txt
# Each run says its process and counts itself, then waits for `go`.
cat > avp.toml <<EOF
[envs.staging]
deploy = { run = "echo \$\$ > '$W/staging.pid'; echo run >> '$W/staging.runs'; while [ ! -f '$W/go' ]; do sleep 0.1; done; cp app.txt '$W/served'" }
[envs.production]
deploy = { run = "echo \$\$ > '$W/production.pid'; echo run >> '$W/production.runs'; while [ ! -f '$W/go' ]; do sleep 0.1; done" }
EOF
$T init drs >/dev/null
$T promote staging --force >/dev/null 2>&1 &
within 10 '[ "$(runs staging)" = 1 ]'
ok "GET /work counts the deploy in flight" '[[ "$(curl -s "$AVP_SERVER/work")" == *"\"deploys\":1"* ]]'
stop staging
start
within 10 '[ "$(runs staging)" = 2 ]'
ok "the server that starts runs the deploy it found cut again, from the start" '[ "$(runs staging)" = 2 ]'
ok "it records env.interrupted" '[[ "$(curl -s "$AVP_SERVER/repos/drs/events?limit=100")" == *"\"env.interrupted\""* ]]'
ok "avp envs says it deploys again" '[[ "$($T envs)" == *"staging      serves nothing known yet, promoted to 0 by ada (deploying"* ]]'
touch "$W/go"
within 10 '[[ "$($T envs)" == *"staging      serves position 0"* ]]'
ok "the run again moves the environment" '[[ "$($T envs)" == *"staging      serves position 0"* ]] && [ "$(cat "$W/served")" = v0 ]'
ok "nothing is in flight once it is over" '[[ "$(curl -s "$AVP_SERVER/work")" == *"\"deploys\":0"* ]]'
rm "$W/go"
$T promote production --force >/dev/null 2>&1 &
within 10 '[ "$(runs production)" = 1 ]'
stop production
start
within 10 '[ "$(runs production)" = 2 ]'
stop production
start
sleep 1
ok "a deploy cut twice does not run a third time" '[ "$(runs production)" = 2 ]'
ok "it is left failed, and avp envs says why" '[[ "$($T envs)" == *"production   serves nothing known yet, promoted to 0 by ada (deploying: failed: a restart of the server cut it 2 times"* ]]'
ok "so does env.failed" '[[ "$(curl -s "$AVP_SERVER/repos/drs/events?limit=200")" == *"\"reason\":\"a restart of the server cut it 2 times\""* ]]'
ok "nothing is in flight" '[[ "$(curl -s "$AVP_SERVER/work")" == *"\"deploys\":0"* ]]'
