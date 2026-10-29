#!/bin/bash
# A new view is a clone of this machine's ready copy of the head, with the install of a view whose
# lockfile is the same: nothing of it is shared, and a ready copy someone touched is laid again.
# Usage: AVP=target/release/avp bash tests/instant-views.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/instant-views.sh <scratch dir>}/iv.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7447 AVP_HOME=$W/home AVP_VIEWS=$W/views
$T serve --listen 127.0.0.1:7447 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir -p repo/src/lib views/a views/b && cd repo || exit 1
printf 'a\n' > src/a.txt && printf 'b\n' > src/lib/b.txt && printf 'node_modules\n' > .gitignore && printf 'lockfileVersion: 9\n' > pnpm-lock.yaml
$T init iv >/dev/null && cd "$W" || exit 1
# Where the filesystem cannot clone, a view takes no install at all: never a shared folder.
if [ "$(uname)" = Darwin ]; then cp -c repo/src/a.txt probe; else cp --reflink=always repo/src/a.txt probe; fi 2>/dev/null && clones=1 || clones=0

$T open -C views/a --repo iv --goal a >/dev/null
R=home/ready/$(ls home/ready)
ok "a new view holds the head, laid out from the ready copy" '[ "$(cat views/a/src/lib/b.txt)" = b ] && [ -f $R/files/src/lib/b.txt ]'
mkdir -p views/a/node_modules/dep && printf 'dep\n' > views/a/node_modules/dep/index.js
printf 'touched\n' > $R/files/src/a.txt
out=$($T open -C views/b --repo iv --goal b)
ok "a file touched in the ready copy is laid again" '[ "$(cat views/b/src/a.txt)" = a ]'
if [ $clones = 1 ]; then
  ok "the install of a view with the same lockfile is cloned" '[ "$(cat views/b/node_modules/dep/index.js)" = dep ] && [[ "$out" == *"node_modules cloned from"* ]]'
else
  ok "no clones here: no install taken" '[ ! -e views/b/node_modules ]'
fi
printf 'mine\n' > views/b/src/lib/b.txt && printf 'mine\n' > views/b/node_modules/dep/index.js 2>/dev/null
ok "a write in one view stays in it" '[ "$(cat views/a/src/lib/b.txt)" = b ] && [ "$(cat $R/files/src/lib/b.txt)" = b ] && [ "$(cat views/a/node_modules/dep/index.js)" = dep ]'
ok "and is its task's change" '[[ "$($T -C views/b sync)" == *"src/lib/b.txt"* ]]'
