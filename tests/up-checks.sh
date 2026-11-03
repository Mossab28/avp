#!/bin/bash
# `avp up` on a fresh checkout lands the checks it guessed: a change to avp.toml waits for a person's
# word, and the person who ran `avp up` gives it.
# Usage: AVP=target/release/avp bash tests/up-checks.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/up-checks.sh <scratch dir>}/up.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7465 AVP_HOME=$W/avp AVP_VIEWS=$W/views AVP_PERSON=ada
# The agents `avp up` wires keep their settings under HOME: this test's, never the caller's.
export HOME=$W/home && mkdir -p "$HOME"
$T serve --listen 127.0.0.1:7465 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap '$T -C "$W/shop" down >/dev/null 2>&1; kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir shop && cd shop && git init -q -b main && printf '{"name":"shop","scripts":{"test":"node --test"}}\n' > package.json
git add -A && git -c user.name=Ada -c user.email=ada@example.com commit -q -m start
ORIG=$(git rev-parse HEAD)
out=$($T up --no-push 2>&1)
ok "avp up lands the checks it guessed" '[[ "$out" == *"avp.toml landed at position 1"* ]] && [[ "$($T log --repo shop --limit 1)" == *"guessed by avp up"* ]]'
ok "with the word of the person who ran it" '[[ "$out" == *"approved as ada"* ]] && curl -s "$AVP_SERVER/repos/shop/events?since=0&limit=100" | grep "task.approved" | grep -q "\"ada\""'
ok "a git clone from AVP gives back the original commit, as it was" 'git clone -q $AVP_SERVER/shop.git "$W/c" && [ "$(git -C "$W/c" rev-parse HEAD~1)" = "$ORIG" ] && [ "$(git -C "$W/c" log -1 --format=%an HEAD~1)" = Ada ]'
# Another project in a folder of the same name: its history shares no commit with the repo's line.
mkdir -p "$W/elsewhere/shop" && cd "$W/elsewhere/shop" && git init -q -b main && echo other > README.md
git add -A && git -c user.name=Bo -c user.email=bo@example.com commit -q -m other
out=$($T up --no-push --no-checks 2>&1)
ok "avp up refuses to attach an unrelated checkout to the repo of its name" '[[ "$out" == *"shares no commit with this checkout"* ]] && [[ "$out" == *"avp up --as <name>"* ]] && [ ! -e .mcp.json ]'
