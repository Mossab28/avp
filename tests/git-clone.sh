#!/bin/bash
# Come in from Git, leave with Git: `avp import` makes a repo from a checkout with its history,
# an agent lands on it, and `git clone <server>/<repo>.git` gives the head of the line on top of
# the original commits. A push is refused; with accounts, a clone needs a reader's token.
# Usage: AVP=target/release/avp bash tests/git-clone.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/git-clone.sh <scratch dir>}/gc.XXXX") && cd "$W" || exit 1
unset AVP_TOKEN AVP_USER
export AVP_SERVER=http://127.0.0.1:7604 GIT_TERMINAL_PROMPT=0
$T serve --listen 127.0.0.1:7604 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
for _ in $(seq 50); do curl -s $AVP_SERVER/ >/dev/null && break; sleep 0.2; done
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
g(){ git -c user.name=Human -c user.email=h@x "$@"; }
git init -q -b main shop && cd shop
mkdir -p bin lib && printf 'echo hi\n' > bin/run.sh && chmod +x bin/run.sh && printf 'a\n' > lib/a.txt
g add -A && g commit -qm "first"
printf 'b\n' > lib/b.txt && g add -A && g commit -qm "second"
ORIG=$(git rev-parse HEAD); cd ..
$T --json import shop >"$W/import.json" 2>"$W/import.err"
ok "import makes the repo from the checkout, a position per commit" 'grep -q "\"positions\": 2" $W/import.json'
mkdir v && cd v && $T open --repo shop --goal "agent: change things" >/dev/null 2>&1
printf 'a2\n' > lib/a.txt && printf 'new\n' > lib/new.txt && rm lib/b.txt
$T land >"$W/land.log" 2>&1; cd ..
git clone -q $AVP_SERVER/shop.git c1 2>"$W/clone.err"
ok "a clone gives the same tree as the head of the line" 'diff -r -x .git -x .avp c1 v'
ok "on top of the original commits, one commit per landed position" 'git -C c1 merge-base --is-ancestor $ORIG HEAD && [ $(git -C c1 rev-list --count HEAD) = 3 ]'
ok "the commit says its position" 'git -C c1 log -1 --format=%B | grep -q "^AVP-Position: 2"'
ok "a space in front that is not the repo's is refused" '! git clone -q $AVP_SERVER/acme/shop.git c0'
ok "the exec bit is carried" 'git -C c1 ls-tree HEAD bin/run.sh | grep -q "^100755"'
cd c1 && printf 'x\n' > hand.txt && g add -A && g commit -qm "by hand"
git push -q origin main >"$W/push.log" 2>&1; P=$?; cd ..
ok "a push is refused, saying to land through AVP" '[ $P != 0 ] && grep -q "avp land" $W/push.log'
A=$($T user add ada 2>&1 | grep -o 'avp_[A-Za-z0-9_-]\{40,\}' | head -1)
B=$(AVP_TOKEN=$A $T user add bob 2>&1 | grep -o 'avp_[A-Za-z0-9_-]\{40,\}' | head -1)
# As from another machine: the server's own keeps every right.
far(){ git -c http.extraHeader="X-Forwarded-For: 10.0.0.9" "$@"; }
ok "with accounts, a clone without a token is refused" '! far clone -q $AVP_SERVER/shop.git c2'
ok "and one with the token of a person without a role on the repo" '! far clone -q http://bob:$B@127.0.0.1:7604/shop.git c3'
ok "a reader's token clones" 'far clone -q http://ada:$A@127.0.0.1:7604/shop.git c4 && diff -r -x .git -x .avp c4 v'
