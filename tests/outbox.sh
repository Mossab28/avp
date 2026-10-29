#!/bin/bash
# Offline, exact. A task opened offline opens on the server on the position its folder held, its
# write sent as it was made, and lands replayed on what landed meanwhile. Then an outbox entry the
# server refuses (a write to a task abandoned meanwhile): it stops the outbox, `avp status` says so
# with the way out, `avp outbox` says why, and `avp outbox drop` takes it out, the file unsent again.
# Usage: AVP=target/release/avp bash tests/outbox.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/outbox.sh <scratch dir>}/outbox.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7486 AVP_HOME=$W/home
up(){ $T serve --listen 127.0.0.1:7486 --data "$W/data" >>"$W/server.log" 2>&1 & SP=$!; for _ in $(seq 50); do curl -s "$AVP_SERVER/" >/dev/null && return; sleep 0.1; done; }
down(){ kill $SP 2>/dev/null; wait $SP 2>/dev/null; }
trap down EXIT
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
up
mkdir -p seed/src && cd seed && printf 'one\ntwo\nthree\n' > src/a.txt
printf '[[check.steps]]\nname = "clean"\nrun = "! grep -rq BROKEN src"\n' > avp.toml
$T init ob >/dev/null && cd "$W"
$T open -C v --repo ob --goal "a view" >/dev/null && $T -C v abandon >/dev/null

code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$AVP_SERVER/repos/ob/tasks?base=9" -H 'content-type: application/json' -d '{"goal":"x","actor":{"person":"p","agent":"a","session":"s"}}')
ok "the server refuses a base the line does not hold" '[ "$code" = 404 ]'

# Offline on position 0; the line moves to 1 before the server sees the task.
down
$T -C v open --goal "ONE, offline" >/dev/null
sed -i.bak 's/^one$/ONE/' v/src/a.txt && rm v/src/a.txt.bak
$T -C v sync >/dev/null
up
$T open -C x --repo ob --goal "THREE" >/dev/null && sed -i.bak 's/^three$/THREE/' x/src/a.txt && rm x/src/a.txt.bak && $T -C x land >/dev/null
out=$($T -C v status 2>&1)
ok "on return the task opens on the position the folder held" '[[ "$out" == *"opened on the server as task_"*"on position 0 as this folder held it"* && "$out" == *"on position 0 (head 1)"* ]]'
ok "its write went as it was made" '[ "$(cat v/src/a.txt)" = "$(printf "ONE\ntwo\nthree")" ]'
out=$($T -C v land 2>&1)
ok "its land replays it on what landed meanwhile" '[[ "$out" == "landed at position 2"* ]] && [ "$(cat v/src/a.txt)" = "$(printf "ONE\ntwo\nTHREE")" ]'

# A write queued for a task abandoned before it reaches the server.
$T -C v open --goal "c" >/dev/null
task=$($T -C v status | sed -n 's/^task \(task_[a-z0-9]*\).*/\1/p')
down
printf 'c\n' > v/src/c.txt
$T -C v sync >/dev/null
up
curl -s -X POST "$AVP_SERVER/repos/ob/tasks/$task/abandon" >/dev/null
out=$($T -C v status 2>&1)
n=$(sed -n 's/.*outbox stuck on #\([0-9]*\).*/\1/p' <<<"$out" | head -1)
ok "a refused entry stops the outbox, and status says the way out" '[ -n "$n" ] && [[ "$out" == *"online"* && "$out" == *"(+ src/c.txt)"*"avp outbox drop $n"* ]]'
out=$($T -C v outbox 2>&1)
ok "avp outbox says what, and why its last send failed" '[[ "$out" == *"#$n "*"+ src/c.txt"*"refused"*"task is Abandoned"* ]] && [[ "$($T -C v --json outbox)" == *"\"failed\""*"task is Abandoned"* ]]'
out=$($T -C v outbox drop "$n" 2>&1)
ok "drop says what it dropped, the file unsent again" '[[ "$out" == *"dropped #$n"*"+ src/c.txt"*"unsent again"*"src/c.txt"*"outbox: empty"* ]]'
out=$($T -C v open --goal "c again" 2>&1)
ok "a new task carries the dropped write, and lands it" '[[ "$out" == *"unsent changes are in it"* ]] && [[ "$($T -C v land)" == "landed at position 3"* ]]'
