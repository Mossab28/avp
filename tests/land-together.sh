#!/bin/bash
# Tasks that need each other: each section's tests need the other's, so each proof is red alone.
# Both agents wait on the other: the wait sees the cycle and says so at once, with the command that
# lands both; one lands with `--with`, and both tasks are landed at that one position.
# Usage: AVP=target/release/avp bash tests/land-together.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/land-together.sh <scratch dir>}/lt.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7461
$T serve --listen 127.0.0.1:7461 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'app\n' > src/app.txt
printf '[[check.steps]]\nname = "tests"\nrun = "test -f src/customers.txt && test -f src/invoices.txt"\n' > avp.toml
$T init lt >/dev/null
mkdir a b
$T open -C a --repo lt --goal "Customers section" --scope src/customers.txt >/dev/null
$T open -C b --repo lt --goal "Invoices section" --scope src/invoices.txt >/dev/null
tid(){ $T -C "$1" status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])'; }
A=$(tid a); B=$(tid b)
printf 'customers\n' > a/src/customers.txt; printf 'invoices\n' > b/src/invoices.txt
$T -C a land >/dev/null; ca=$?; $T -C b land >/dev/null; cb=$?
ok "each is red alone" '[ $ca = 2 ] && [ $cb = 2 ]'
# a waits on b, then b waits on a: the cycle, told to both, at once.
$T -C a wait --task "$B" --timeout 60 >"$W/a.wait" & AW=$!
sleep 2
start=$(date +%s); out=$($T -C b wait --task "$A" --timeout 60); took=$(( $(date +%s) - start ))
ok "the second wait sees the cycle at once" '[[ "$out" == cycle:* ]] && [ $took -le 3 ]'
ok "and says the command that lands both" '[[ "$out" == *"\`avp land --with $A\`"* ]]'
wait $AW
ok "the first wait is told too" '[[ "$(cat "$W/a.wait")" == *"\`avp land --with $B\`"* ]]'
ok "an agent reads it as JSON" '[ "$($T -C b wait --task "$A" --json | python3 -c "import json,sys; d=json.load(sys.stdin); print(d[\"state\"], d[\"land\"])")" = "cycle avp land --with $A" ]'
out=$($T -C b land --with "$A")
ok "one land lands both, as one position" '[[ "$out" == "landed at position 1, together with task $A"* ]]'
ok "the other task is landed at that position" '[[ "$($T -C a status)" == *"landed at position 1, together with task $B"* ]]'
out=$($T -C a land)
ok "its land says so, and its folder is the line" '[[ "$out" == "landed at position 1, together with task $B"* ]] && [ -f a/src/invoices.txt ]'
log=$($T -C a log)
ok "the line has one position for both, naming both" '[ "$(grep -cE "^ +[0-9]+ " <<<"$log")" = 2 ] && [[ "$(grep -E "^ +1 " <<<"$log")" == *"Invoices section; together with $A: Customers section"* ]]'
