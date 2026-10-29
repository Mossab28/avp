#!/bin/bash
# A repo's routes answer only for that repo: on a server with two repos, every route that takes a
# task or proof id answers 404 for the other repo's id, word for word as for an id that does not exist.
# Usage: AVP=target/release/avp bash tests/repo-scope.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/repo-scope.sh <scratch dir>}/rs.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7442
$T serve --listen 127.0.0.1:7442 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
jq1(){ python3 -I -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
mkdir src && printf 'a\n' > src/a.txt && printf '[check]\n[[check.steps]]\nname = "ok"\nrun = "true"\n' > avp.toml
$T init alpha >/dev/null && $T init beta >/dev/null
mkdir v && $T open -C v --repo alpha --goal "lands, so it has a proof" >/dev/null && printf 'b\n' > v/src/a.txt
$T -C v land >/dev/null
mkdir w && $T open -C w --repo alpha --goal "stays open" >/dev/null
TASK=$($T -C w status --json | jq1 'd["task"]["id"]')
PROOF=$(curl -s "$AVP_SERVER/repos/alpha/line?limit=5" | jq1 '[e["proof"] for e in d if e.get("proof")][0]')
ok "alpha serves its own proof" '[ "$(curl -s -o /dev/null -w %{http_code} $AVP_SERVER/repos/alpha/proofs/$PROOF)" = 200 ]'
ok "alpha serves its own task" '[ "$(curl -s -o /dev/null -w %{http_code} $AVP_SERVER/repos/alpha/tasks/$TASK)" = 200 ]'
# asked <method> <path under /repos> <body>: the answer, then its status on the last line
asked(){ curl -s -w '\n%{http_code}' -X "$1" -H 'content-type: application/json' -d "$3" "$AVP_SERVER/repos/$2"; }
# same <method> <path with ID> <real id> <body>: beta asking for alpha's id answers a 404, word for
# word as for an id that does not exist
same(){
  local real missing
  real=$(asked "$1" "${2//ID/$3}" "$4" | sed "s/$3/ID/g")
  missing=$(asked "$1" "${2//ID/nope_0000}" "$4" | sed "s/nope_0000/ID/g")
  if [ "$real" = "$missing" ] && [ "${real##*$'\n'}" = 404 ]; then echo "PASS $1 $2: 404, same as an id that does not exist"; else echo "FAIL $1 $2: $real | $missing"; fi
}
for r in "GET tasks/ID" "GET tasks/ID/tree" "GET tasks/ID/diff" "POST tasks/ID/claim" "POST tasks/ID/release" \
         "POST tasks/ID/undo" "POST tasks/ID/abandon" "POST tasks/ID/check" "POST tasks/ID/speculate" \
         "POST tasks/ID/catch-up" "POST tasks/ID/land"; do
  same "${r% *}" "beta/${r#* }" "$TASK" '{}'
done
same POST beta/tasks/ID/goal "$TASK" '{"goal":"hijacked"}'
same POST beta/tasks/ID/writes "$TASK" '{"path":"x","kind":"delete","mode":33188}'
same POST beta/tasks/ID/approve "$TASK" '{"person":"x","approved":true}'
same GET "beta/events?task=ID" "$TASK" ''
same GET beta/proofs/ID "$PROOF" ''
ok "alpha's task is untouched by all of it" '[ "$(curl -s $AVP_SERVER/repos/alpha/tasks/$TASK | jq1 "d[\"task\"][\"state\"]+d[\"task\"][\"goal\"]")" = "openstays open" ]'
