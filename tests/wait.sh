#!/bin/bash
# avp wait: a task waits for another to land and for the path it writes, then goes on.
# Usage: AVP=target/release/avp bash tests/wait.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/wait.sh <scratch dir>}/wt.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7199
$T serve --listen 127.0.0.1:7199 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'a\n' > src/a.txt && $T init w >/dev/null
mkdir a b && $T open -C a --repo w --goal "payments" --scope src/payments.ts >/dev/null && $T open -C b --repo w --goal "webhooks" >/dev/null
A=$($T -C a status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')
( sleep 4; printf 'p\n' > a/src/payments.ts; $T -C a land >/dev/null ) &
start=$(date +%s); out=$($T -C b wait --task "$A" --path src/payments.ts --timeout 60); took=$(( $(date +%s) - start ))
ok "wait returns once the other task landed" '[[ "$out" == ready* ]]'
ok "it waited for it, not more" '[ $took -ge 3 ] && [ $took -le 15 ]'
ok "a timeout says what is still missing" '[[ "$($T -C b wait --path src/nope.ts --timeout 1)" == *"src/nope.ts not on the line"* ]]'
