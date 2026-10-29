#!/bin/bash
# A deploy AVP runs itself (`run` under [envs.<name>.deploy]): `avp promote` answers once it is
# over, the environment moves when it succeeds, and stays, saying why, when it fails. It sees what a
# proof's step sees: none of the server's secrets.
# Usage: AVP=target/release/avp bash tests/deploy-run.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/deploy-run.sh <scratch dir>}/dr.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7401 AVP_PERSON=ada
GH_TOKEN=gh-s3cret $T serve --listen 127.0.0.1:7401 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir app && cd app || exit 1
printf 'v0\n' > app.txt
cat > avp.toml <<EOF
[envs.staging]
deploy = { run = "cp app.txt '$W/served' && env > '$W/deploy-env.txt' && echo deployed {position}" }
[envs.broken]
deploy = { run = "echo pushing; echo no credentials for the cluster; exit 2" }
EOF
$T init dr >/dev/null
out=$($T promote staging --force 2>&1)
ok "promote answers once the deploy moved the environment" '[[ "$out" == "staging now serves position 0" ]]'
ok "the deploy ran in the promoted position laid out" '[ "$(cat "$W/served")" = v0 ]'
ok "the deploy sees what a proof's step sees, no secret" '! grep -q gh-s3cret "$W/deploy-env.txt" && grep -qx CI=true "$W/deploy-env.txt"'
ok "avp envs says what it serves" '[[ "$($T envs)" == *"staging      serves position 0, promoted by ada"* ]]'
out=$($T promote broken --force 2>&1); rc=$?
ok "a failed deploy fails the promote and says why" '[ $rc != 0 ] && [[ "$out" == *"failed (exit 2), broken serves nothing yet"* && "$out" == *"no credentials for the cluster"* ]]'
ok "avp envs says the deploy failed and why" '[[ "$($T envs)" == *"broken       serves nothing known yet, promoted to 0 by ada (deploying: failed (exit 2): no credentials for the cluster"* ]]'
id=$(grep -o 'avp proof proof_[a-z0-9]*' <<<"$out" | cut -d" " -f3)
ok "its output is kept as a proof" '[ -n "$id" ] && [[ "$($T proof "$id")" == *pushing* ]]'
ok "the event says so too" '[[ "$(curl -s "$AVP_SERVER/repos/dr/events?limit=100")" == *"\"env.failed\""* ]]'
