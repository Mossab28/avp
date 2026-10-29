#!/bin/bash
# A flake in the full suite (Murmell's line, 24/10: a position blamed for a test its change did not
# reach): a red suite runs again once on the same tree. Green the second time, the position reads
# Flaky, `line.flaky` names the shard and the failing tests, staging follows it and a person promotes
# it; red twice is red, as before.
# Usage: AVP=target/release/avp bash tests/flaky-full.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/flaky-full.sh <scratch dir>}/ff.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7423 AVP_PERSON=ada
$T serve --listen 127.0.0.1:7423 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
# The suite fails the first time it runs, as vitest prints a test that fails under load, and passes
# after; with broken.txt in the tree, it fails every time.
mkdir app && cd app || exit 1
cat > avp.toml <<EOF
[check]
full = "test ! -f broken.txt || { echo ' FAIL  src/b.test.ts > b > breaks'; exit 1; }; [ -f '$W/ran' ] || { touch '$W/ran'; echo ' FAIL  src/clock.test.ts > clock > ticks'; echo '   × clock > ticks 12ms'; exit 1; }"
[envs.staging]
follow = "full"
EOF
$T init ff >/dev/null
cd "$W" || exit 1
# The full suite's verdict on a position, once it has one: `avp log` prints it after the position.
full(){ for _ in $(seq 1 120); do v=$($T log --repo ff | awk -v p="$1" '$1 == p { print $2 }'); [[ "$v" == Green || "$v" == Flaky || "$v" == Red ]] && { echo "$v"; return; }; sleep 0.5; done; }
events(){ curl -s "$AVP_SERVER/repos/ff/events?limit=1000" | python3 -X utf8 -c "import json,sys; print(json.dumps([e['data'] for e in json.load(sys.stdin) if e['kind'] == '$1'], ensure_ascii=False))"; }

mkdir one && $T open -C one --repo ff --goal "a first change" >/dev/null && printf 'one\n' > one/one.txt
ok "it lands" '[[ "$($T -C one land)" == "landed at position 1"* ]]'
ok "red, then green on the same tree: the log says Flaky" '[ "$(full 1)" = Flaky ]'
said=$(events line.flaky | python3 -X utf8 -c 'import json,sys; e=json.load(sys.stdin); print(len(e), e[0]["position"], e[0]["shard"], " | ".join(e[0]["tests"]))')
ok "line.flaky names the shard and the failing test lines" '[ "$said" = "1 1 full FAIL  src/clock.test.ts > clock > ticks | × clock > ticks" ]'
ok "a flaky position is not red" '[ "$(events line.red)" = "[]" ]'
for _ in $(seq 1 40); do [[ "$($T envs --repo ff)" == *"serves position 1"* ]] && break; sleep 0.5; done
ok "staging follows a flaky position" '[[ "$($T envs --repo ff)" == *"staging"*"serves position 1"* ]]'
ok "a person promotes it without --force" '$T promote production 1 --repo ff'
ok "AVP Live tells flaky positions apart" '[[ "$(curl -s "$AVP_SERVER/live")" == *"line.flaky"* ]]'

mkdir two && $T open -C two --repo ff --goal "a broken change" >/dev/null && printf 'x\n' > two/broken.txt
ok "a change the suite fails lands on its proof" '[[ "$($T -C two land)" == "landed at position 2"* ]]'
ok "red twice is red" '[ "$(full 2)" = Red ]'
ok "line.red says so, and no flake is told" '[[ "$(events line.red)" == *"\"position\": 2"* ]] && [ "$(events line.flaky | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")" = 1 ]'
ok "a red position is not promoted" '! $T promote production 2 --repo ff'
