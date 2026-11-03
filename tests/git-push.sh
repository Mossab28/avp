#!/bin/bash
# `git push` lands: a push to a repo that does not exist creates it with its history, a push of a
# commit is a task proven and landed (the position's commit is the pushed one), a push whose proof
# is red is refused with what failed, one behind the head is replayed, and a reader's push is refused.
# Usage: AVP=target/release/avp bash tests/git-push.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/git-push.sh <scratch dir>}/gp.XXXX") && cd "$W" || exit 1
unset AVP_TOKEN AVP_USER AVP_REPO
export AVP_SERVER=http://127.0.0.1:7703 GIT_TERMINAL_PROMPT=0
$T serve --listen 127.0.0.1:7703 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
for _ in $(seq 50); do curl -s $AVP_SERVER/ >/dev/null && break; sleep 0.2; done
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
g(){ git -c user.name=Ada -c user.email=ada@x "$@"; }
# As from another machine: the server's own keeps every right.
far(){ git -c http.extraHeader="X-Forwarded-For: 10.0.0.9" "$@"; }
A=$($T user add ada 2>&1 | grep -o 'avp_[A-Za-z0-9_-]\{40,\}' | head -1)
B=$(AVP_TOKEN=$A $T user add bob 2>&1 | grep -o 'avp_[A-Za-z0-9_-]\{40,\}' | head -1)
URL=http://ada:$A@127.0.0.1:7703/shop.git
git init -q -b main shop && cd shop
printf '[check]\n[[check.steps]]\nname = "tests"\nrun = "! grep -q broken app.txt"\n' > avp.toml
printf 'one\n' > app.txt && g add -A && g commit -qm "first"
printf 'two\n' > app.txt && g commit -qam "second"
far push -q "$URL" main >"$W/create.log" 2>&1
git checkout -q -b feature && printf 'f\n' > f.txt && g add -A && g commit -qm "feature work"
far push "$URL" feature >"$W/branch.log" 2>&1; P=$?
ok "a push to another branch is refused, saying how to land it on main" '[ $P != 0 ] && grep -q "feature:main" $W/branch.log && ! git ls-remote $URL | grep -q feature'
git checkout -q main && git branch -q -D feature
ok "a push to a repo that does not exist creates it, its history imported" 'grep -q "positions 0 to 1" $W/create.log && [ "$(git ls-remote $URL refs/heads/main | cut -f1)" = "$(git rev-parse HEAD)" ]'
printf 'three\n' > app.txt && g commit -qam "make it three"
far push "$URL" HEAD:main >"$W/push.log" 2>&1; P=$?
ok "a push lands through the proof, and says its position" '[ $P = 0 ] && grep -q "green tests" $W/push.log && grep -q "Landed at position 2: http://127.0.0.1:7703/r/shop/p/2" $W/push.log'
ok "its task has the commit message as goal, the person who pushed as author" 'curl -s -u ada:$A "$AVP_SERVER/repos/shop/tasks?state=landed" | grep -q "\"goal\":\"make it three\".*\"person\":\"ada\",\"agent\":\"git-push\""'
ok "the position's commit is the pushed one: nothing to pull" '[ "$(git ls-remote $URL refs/heads/main | cut -f1)" = "$(git rev-parse HEAD)" ]'
printf 'broken\n' > app.txt && g commit -qam "break it"
far push "$URL" HEAD:main >"$W/red.log" 2>&1; P=$?
ok "a push whose proof is red is refused, with what failed and what to do" '[ $P != 0 ] && grep -q "proof is red: tests" $W/red.log && grep -q "push again" $W/red.log'
ok "and nothing landed" 'curl -s -u ada:$A $AVP_SERVER/repos/shop/head | grep -q "\"position\":2"'
AVP_TOKEN=$A $T user role bob read --repo shop >/dev/null 2>&1
far push "http://bob:$B@127.0.0.1:7703/shop.git" HEAD:main >"$W/reader.log" 2>&1; P=$?
ok "a reader's push is refused, saying the role it needs" '[ $P != 0 ] && grep -q "remote: bob has read on shop" $W/reader.log'
g reset -q --hard HEAD~1 && far clone -q "$URL" "$W/other" && (cd "$W/other" && printf 'b\n' > b.txt && g add -A && g commit -qm "add b" && far push -q origin HEAD:main) >/dev/null 2>&1
printf 'c\n' > c.txt && g add -A && g commit -qm "add c"
far push "$URL" HEAD:main >"$W/behind.log" 2>&1; P=$?
ok "a push behind the head is replayed on it, and lands" '[ $P = 0 ] && grep -q "replayed on the head" $W/behind.log && far clone -q $URL $W/last && [ -f $W/last/b.txt ] && [ -f $W/last/c.txt ]'
