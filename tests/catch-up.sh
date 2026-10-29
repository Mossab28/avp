#!/bin/bash
# A task that depends on a teammate's: once that one landed, `avp refresh` (and `avp wait`, once
# ready) bring it into the folder, the task's own changes kept on top, before landing.
# Usage: AVP=target/release/avp bash tests/catch-up.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/catch-up.sh <scratch dir>}/cu.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7197
$T serve --listen 127.0.0.1:7197 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'base\n' > src/a.txt && $T init cu >/dev/null
mkdir ids dep && $T open -C ids --repo cu --goal "line ids" >/dev/null && $T open -C dep --repo cu --goal "uses line ids" >/dev/null
printf 'mine\n' > dep/src/dep.txt && $T -C dep sync >/dev/null
printf 'ids\n' > ids/src/ids.txt && $T -C ids land >/dev/null
out=$($T -C dep refresh)
ok "refresh brings the landed work into the folder" '[ -f dep/src/ids.txt ]'
ok "and keeps the task's own changes" '[ "$(cat dep/src/dep.txt)" = mine ]'
ok "and says the task moved" '[[ "$out" == *"now sits on position 1"* ]]'
ok "the task lands on top of it" '[[ "$($T -C dep land)" == "landed at position 2"* ]]'
mkdir more next && $T open -C more --repo cu --goal "more" >/dev/null && $T open -C next --repo cu --goal "needs more" >/dev/null
M=$($T -C more status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')
( sleep 4; printf 'more\n' > more/src/more.txt; $T -C more land >/dev/null ) &
out=$($T -C next wait --task "$M" --timeout 60)
ok "wait, once ready, brings what it waited for into the folder" '[ -f next/src/more.txt ] && [[ "$out" == *"now sits on position 3"* ]]'
