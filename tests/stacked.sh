#!/bin/bash
# Stacked tasks: a task opened on another open task (`avp open --on`) builds on its work before it
# lands. The parent changes, the child's refresh replays it on those changes, and `land --stack`
# lands both, in order.
# Usage: AVP=target/release/avp bash tests/stacked.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/stacked.sh <scratch dir>}/st.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7479
$T serve --listen 127.0.0.1:7479 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'base\n' > src/a.txt && $T init st >/dev/null
mkdir parent child other
$T open -C parent --repo st --goal "parser" >/dev/null
printf 'parse v1\n' > parent/src/parse.txt && $T -C parent sync >/dev/null
P=$($T -C parent status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')
out=$($T open -C child --repo st --goal "uses the parser" --on "${P:0:14}")
ok "a task opens on another's work, named by the start of its id" '[[ "$out" == *"opened on task $P"* ]] && [ "$(cat child/src/parse.txt)" = "parse v1" ]'
printf 'uses it\n' > child/src/use.txt
out=$($T -C child land); code=$?
ok "it does not land before the task it is stacked on" '[ $code = 2 ] && [[ "$out" == *"not landed: stacked"* ]] && [[ "$out" == *"--stack"* ]]'
ok "avp tasks shows the stack" '$T -C child tasks | grep -q "^  task_.*  on $P  "'
# The line moves under both, and the parent changes.
$T open -C other --repo st --goal "other" >/dev/null && printf 'x\n' > other/src/x.txt && $T -C other land >/dev/null
printf 'parse v2\n' > parent/src/parse.txt && $T -C parent sync >/dev/null
out=$($T -C child refresh)
ok "refresh replays the child on the parent's latest" '[[ "$out" == *"now stands on task $P"* ]] && [ "$(cat child/src/parse.txt)" = "parse v2" ]'
ok "its own changes kept on top" '[ "$(cat child/src/use.txt)" = "uses it" ]'
out=$($T -C child land --stack)
ok "land --stack lands the parent, then the child" '[[ "$out" == *"task $P landed at position 2"* ]] && [[ "$out" == *"landed at position 3"* ]]'
log=$($T -C child log)
ok "in that order on the line" '[[ "$(grep -E "^ +2 " <<<"$log")" == *"$P"*parser ]] && [[ "$(grep -E "^ +3 " <<<"$log")" == *"uses the parser" ]]'
ok "the child's folder is the line: both changes and the other's" '[ "$(cat child/src/parse.txt)" = "parse v2" ] && [ -f child/src/use.txt ] && [ -f child/src/x.txt ]'
