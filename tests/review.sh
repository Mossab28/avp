#!/bin/bash
# A repo whose landings need a person's word: the land waits, the change reads as a diff, and the
# approval lands it without the agent landing again; a refusal reaches the agent.
# Usage: AVP=target/release/avp bash tests/review.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/review.sh <scratch dir>}/rv.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7195
$T serve --listen 127.0.0.1:7195 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'one\n' > src/a.txt && printf '[check]\n[[check.steps]]\nname = "ok"\nrun = "true"\n' > avp.toml && $T init rv >/dev/null
mkdir v && $T open -C v --repo rv --goal "say two" >/dev/null && $T -C v landing proof_and_approval >/dev/null
printf 'one\ntwo\n' > v/src/a.txt
out=$($T -C v land)
ok "the land waits for a person" '[[ "$out" == *"not landed: approval"* ]]'
TASK=$($T -C v status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')
diff=$(curl -s "$AVP_SERVER/repos/rv/tasks/$TASK/diff")
ok "the diff names the file and the added line" '[[ "$diff" == *"src/a.txt"* && "$diff" == *"+two"* && "$diff" == *modified* ]]'
curl -s -X POST -H 'content-type: application/json' -d '{"person":"ada","approved":true,"reason":""}' "$AVP_SERVER/repos/rv/tasks/$TASK/approve" >/dev/null
for i in $(seq 1 20); do [[ "$($T -C v status --json)" == *'"landed"'* ]] && break; sleep 0.5; done
ok "the approval lands it" '[[ "$($T -C v status --json)" == *"\"landed\""* ]]'
ok "the agent's next land says so" '[[ "$($T -C v land)" == "landed at position 1, approved by ada"* ]] && [ "$(cat v/src/a.txt | wc -l | tr -d " ")" = 2 ]'
mkdir w && $T open -C w --repo rv --goal "say three" >/dev/null && printf 'one\ntwo\nthree\n' > w/src/a.txt && $T -C w land >/dev/null
T2=$($T -C w status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')
curl -s -X POST -H 'content-type: application/json' -d '{"person":"ada","approved":false,"reason":"three is too many"}' "$AVP_SERVER/repos/rv/tasks/$T2/approve" >/dev/null
ok "a refusal reaches the agent with its reason" '[[ "$($T -C w land)" == *"refused"* ]] && [[ "$($T -C w status --json)" == *"three is too many"* ]]'
