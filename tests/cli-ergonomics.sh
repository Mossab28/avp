#!/bin/bash
# Small CLI conveniences: `open` and `tasks` take the repo from AVP_REPO or --repo, `proof` with no
# id shows this view's last proof, and `mirror` without --remote records mirror.blocked when the
# local branch holds a commit AVP did not write.
# Usage: AVP=target/release/avp bash tests/cli-ergonomics.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/cli-ergonomics.sh <scratch dir>}/ce.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7425
$T serve --listen 127.0.0.1:7425 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
g(){ git -c user.name=Human -c user.email=h@x "$@"; }
git init -q -b main src && cd src
printf 'a\n' > a.txt; g add -A; g commit -qm first; printf 'b\n' > b.txt; g add -A; g commit -qm second
$T import-git demo --history 5 >/dev/null 2>&1; cd ..

mkdir v
ok "open reads the repo from AVP_REPO" "AVP_REPO=demo $T open -C v --goal 'env repo' | grep -q 'opened on position'"
ok "tasks reads it from --repo outside a view" "mkdir -p out && $T tasks -C out --repo demo | grep -q 'env repo'"
ok "tasks reads it from AVP_REPO outside a view" "AVP_REPO=demo $T tasks -C out | grep -q 'env repo'"
ok "proof with no id and no proof yet says so" "[[ \"\$($T proof -C v 2>&1)\" == *'no proof yet'* ]]"
printf 'c\n' > v/c.txt
$T -C v check >/dev/null 2>&1
ok "proof with no id shows the last proof of this view's task" "$T proof -C v | grep -q 'on position'"

# A local branch with a commit AVP did not write: the mirror refuses and says so.
$T -C src mirror --repo demo --branch avp/line >m1.log 2>&1
ok "mirror writes the local branch" "grep -q 'identical to the line' m1.log"
$T -C v land >/dev/null 2>&1
g -C src commit -q --allow-empty -m "not AVP" 2>/dev/null
g -C src branch -f avp/line HEAD
FOREIGN=$(git -C src rev-parse avp/line)
ok "mirror leaves a branch with a foreign commit alone" "! $T -C src mirror --repo demo --branch avp/line && [ \$(git -C src rev-parse avp/line) = $FOREIGN ]"
ok "and records mirror.blocked with the foreign commit" "curl -s $AVP_SERVER/repos/demo/events?since=0\&limit=1000 | grep -q '\"kind\":\"mirror.blocked\"' && curl -s $AVP_SERVER/repos/demo/events?since=0\&limit=1000 | grep -q $FOREIGN"
