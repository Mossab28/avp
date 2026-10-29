#!/bin/bash
# Many landings at once on one shared file: every task lands, none proves more than twice (once
# beside the others, once more in its turn when the line moved), and the file holds every line.
# Usage: AVP=target/release/avp bash tests/land-queue.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/land-queue.sh <scratch dir>}/lq.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7198 AVP_SPECULATE=0
$T serve --listen 127.0.0.1:7198 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'start\n' > src/list.txt && printf '[check]\n[[check.steps]]\nname = "slow"\nrun = "sleep 2"\n' > avp.toml && $T init q >/dev/null
N=6
for i in $(seq 1 $N); do
  mkdir v$i && $T open -C v$i --repo q --goal "line $i" >/dev/null
  printf 'start\nline %s\n' $i > v$i/src/list.txt && printf '%s\n' $i > v$i/src/f$i.txt
done
start=$(date +%s)
P=()
for i in $(seq 1 $N); do $T -C v$i land > v$i.out 2>&1 & P+=($!); done
wait "${P[@]}"
took=$(( $(date +%s) - start ))
proofs=$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("select max(n) from (select count(*) n from proofs where task is not null group by task)").fetchone()[0])' "$W/data/avp.sqlite")
mkdir head && $T open -C head --repo q --goal check >/dev/null
ok "every task lands" '[ "$(grep -l "^landed at" v*.out | wc -l | tr -d " ")" = $N ]'
ok "no task proves more than twice (most: $proofs)" '[ "$proofs" -le 2 ]'
ok "the shared file holds every line" '[ "$(grep -c "^line " head/src/list.txt)" = $N ] && [ "$(ls head/src/f*.txt | wc -l | tr -d " ")" = $N ]'
ok "the landings take their turns, not more (${took}s)" '[ $took -le $(( 2 * N + 10 )) ]'
