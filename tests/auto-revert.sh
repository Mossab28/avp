#!/bin/bash
# A line that never stays red: two landings, the second breaks a shard of the full suite. Red twice,
# AVP finds it, reverts it in a task of its own, proven and landed, and the line ends green on the
# revert. Its person and agent hear why, `avp show` says it, the next agent on its paths is told,
# and the red position is never promoted.
# Usage: AVP=target/release/avp bash tests/auto-revert.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/auto-revert.sh <scratch dir>}/ar.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7446 AVP_PERSON=ada AVP_AGENT=codex
$T serve --listen 127.0.0.1:7446 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir app && cd app || exit 1
cat > avp.toml <<'EOF'
[check]
[[check.full_shards]]
name = "api"
run = "true"
[[check.full_shards]]
name = "web"
run = "test ! -f broken.txt || { echo ' FAIL  src/b.test.ts > b > breaks'; exit 1; }"
[envs.staging]
follow = "full"
EOF
$T init ar >/dev/null
cd "$W" || exit 1
# The full suite's verdict on a position, once it has one: `avp log` prints it after the position.
full(){ for _ in $(seq 1 120); do v=$($T log --repo ar | awk -v p="$1" '$1 == p { print $2 }'); [[ "$v" == Green || "$v" == Flaky || "$v" == Red ]] && { echo "$v"; return; }; sleep 0.5; done; }
events(){ curl -s "$AVP_SERVER/repos/ar/events?limit=1000" | python3 -X utf8 -c "import json,sys; print(json.dumps([e['data'] for e in json.load(sys.stdin) if e['kind'] == '$1'], ensure_ascii=False))"; }

mkdir one && $T open -C one --repo ar --goal "a first change" >/dev/null && printf 'one\n' > one/one.txt
ok "the first change lands" '[[ "$($T -C one land)" == "landed at position 1"* ]]'
ok "its full suite is green" '[ "$(full 1)" = Green ]'
mkdir two && $T open -C two --repo ar --goal "a broken change" >/dev/null && printf 'x\n' > two/broken.txt
ok "the second lands on its proof" '[[ "$($T -C two land)" == "landed at position 2"* ]]'
ok "shard web is red twice on it" '[ "$(full 2)" = Red ]'
ok "the line ends green, on a revert position" '[ "$(full 3)" = Green ]'
ok "the revert's goal names the position, the shard and the failing lines" '[[ "$($T log --repo ar | awk "\$1 == 3")" == *"Revert position 2: a broken change (full suite red twice, shard web: FAIL  src/b.test.ts > b > breaks)" ]]'
ok "it takes the broken file back and keeps the rest" '[ "$(curl -s "$AVP_SERVER/repos/ar/tree/3" | python3 -c "import json,sys; print(\" \".join(sorted(json.load(sys.stdin))))")" = "avp.toml one.txt" ]'
task3=$(curl -s "$AVP_SERVER/repos/ar/line?from=3&limit=1" | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['task'])")
ok "AVP opened it itself" '[ "$(curl -s "$AVP_SERVER/repos/ar/tasks/$task3" | python3 -c "import json,sys; a=json.load(sys.stdin)[\"task\"][\"actor\"]; print(a[\"person\"], a[\"agent\"])")" = "avp avp" ]'
said=$(events line.reverted | python3 -X utf8 -c 'import json,sys; e=json.load(sys.stdin); print(len(e), e[0]["position"], e[0]["by"], e[0]["shard"], e[0]["person"], e[0]["agent"], ",".join(e[0]["files"]), "|".join(e[0]["tests"]), "breaks" in e[0]["output"])')
ok "line.reverted hands it back to its person and agent, with the failing output" '[ "$said" = "1 2 3 web ada codex broken.txt FAIL  src/b.test.ts > b > breaks True" ]'
ok "avp show says it was reverted, and why" '[[ "$($T show 2 --repo ar)" == *"reverted by AVP at position 3"*"shard web went red twice on it: FAIL  src/b.test.ts > b > breaks"*"+++ b/broken.txt"* ]]'
mkdir three && hint=$(AVP_PERSON=bob AVP_AGENT=gemini $T open -C three --repo ar --goal "break it again" --scope broken.txt)
ok "the next agent opening on its paths is told" '[[ "$hint" == *"position 2 (a broken change, by ada via codex) was reverted by AVP at position 3"* ]]'
ok "and avp events shows it" '[[ "$($T -C three events)" == *"line.reverted"* ]]'
mkdir four && ok "an agent opening elsewhere is not" '[[ "$(AVP_PERSON=bob AVP_AGENT=gemini $T open -C four --repo ar --goal "elsewhere" --scope src)" != *"reverted"* ]]'
for _ in $(seq 1 40); do [[ "$($T envs --repo ar)" == *"serves position 3"* ]] && break; sleep 0.5; done
ok "staging follows the revert" '[[ "$($T envs --repo ar)" == *"staging"*"serves position 3"* ]]'
ok "a red position is never promoted" '! $T promote production 2 --repo ar'
