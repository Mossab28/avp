#!/bin/bash
# Offline: the server killed under a view, the view keeps working (log, why, blame and show from its
# replica, writes and a land queued, a red land not), and once the server is back the next command
# sends the outbox in order and the land lands. Then a task opened offline while the line moves:
# it opens on the position it was made on, and its land is replayed on what landed meanwhile.
# Usage: AVP=target/release/avp bash tests/offline.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/offline.sh <scratch dir>}/off.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7476 AVP_HOME=$W/home
up(){ $T serve --listen 127.0.0.1:7476 --data "$W/data" >>"$W/server.log" 2>&1 & SP=$!; for _ in $(seq 50); do curl -s "$AVP_SERVER/" >/dev/null && return; sleep 0.1; done; }
down(){ kill $SP 2>/dev/null; wait $SP 2>/dev/null; }
trap down EXIT
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
up
mkdir -p seed/src && cd seed && printf 'one\ntwo\nthree\n' > src/a.txt
printf '[[check.steps]]\nname = "clean"\nrun = "! grep -rq BROKEN src"\n' > avp.toml
$T init off >/dev/null && cd "$W"
$T open -C v --repo off --goal "b, offline" >/dev/null

down
printf 'b\n' > v/src/b.txt
out=$($T -C v sync 2>&1)
ok "a sync without the server is queued" '[[ "$out" == *"+ src/b.txt"* && "$out" == *"offline: queued (1 waiting)"* ]]'
out=$($T -C v log 2>&1)
ok "log answers from the replica, and says it once" '[[ "$out" == "offline: the server does not answer; as of position 0"* && "$out" == *"imported"* ]]'
out=$($T -C v blame src/a.txt 2>&1)
ok "blame answers from the replica" '[[ "$out" == *"imported  1  one"* ]]'
printf 'BROKEN\n' > v/src/c.txt
out=$($T -C v check 2>&1); code=$?
ok "check proves on this machine: red" '[ $code = 1 ] && [[ "$out" == *"proven on this machine"*"Red on position 0"* ]]'
out=$($T -C v land 2>&1); code=$?
ok "a red land is not queued" '[ $code = 2 ] && [[ "$out" == *"red on this machine, nothing queued"* ]] && [[ "$($T -C v status)" == *"outbox: 2 waiting"* ]]'
rm v/src/c.txt
out=$($T -C v land 2>&1)
ok "a green land is queued" '[[ "$out" == *"green on this machine"*"queued (4 waiting)"* ]]'
out=$($T -C v status 2>&1)
ok "status says offline, the outbox, the land in it" '[[ "$out" == offline:* && "$out" == *"outbox: 4 waiting"* && "$out" == *"land (green on this machine)"* ]]'

up
out=$($T -C v status 2>&1)
ok "the server back, the next command lands the outbox" '[[ "$out" == *"landed at position 1"* && "$out" == *"online"*"outbox: empty"* ]]'
ok "what was written offline is on the line, the red file is not" '[[ "$($T -C v log --path src/b.txt)" == *"    1 "* ]] && [[ "$($T -C v log --path src/c.txt)" == *"no position"* ]]'

# The line moves while a task opened offline waits in the outbox: replayed on return.
down
out=$($T -C v open --goal "ONE, offline" 2>&1)
ok "open works offline, on the position the folder holds" '[[ "$out" == *"task local_"*"opened on this machine, on position 1"* ]]'
ok "show answers from the replica" '[[ "$($T -C v show 1 2>&1)" == *"+b"* ]]'
sed -i.bak 's/^one$/ONE/' v/src/a.txt && rm v/src/a.txt.bak
out=$($T -C v land 2>&1)
ok "its land is queued with it" '[[ "$out" == *"queued (3 waiting)"* ]]'
up
$T open -C x --repo off --goal "THREE" >/dev/null && sed -i.bak 's/^three$/THREE/' x/src/a.txt && rm x/src/a.txt.bak
ok "meanwhile a teammate lands on the same file" '[[ "$($T -C x land)" == "landed at position 2"* ]]'
out=$($T -C v status 2>&1)
ok "on return the offline task opens on the position it was made on and lands, replayed" '[[ "$out" == *"opened on the server as task_"*"on position 1 as this folder held it"* && "$out" == *"landed at position 3, replayed on the moved line"* ]]'
ok "both changes are in the folder" '[ "$(cat v/src/a.txt)" = "$(printf "ONE\ntwo\nTHREE")" ]'
ok "and on the line" '[[ "$($T -C v why src/a.txt:1)" == *"ONE"*"position 3"* ]] && [[ "$($T -C v why src/a.txt:3)" == *"position 2"* ]]'
