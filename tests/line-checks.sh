#!/bin/bash
# A task is proven by its own checks, as a pull request runs its own CI, and a change to them lands
# only with a person's word: one that weakens avp.toml to get a bad change through waits for a
# person, and one that breaks the checks is red under them.
# Usage: AVP=target/release/avp bash tests/line-checks.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/line-checks.sh <scratch dir>}/lc.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7193
$T serve --listen 127.0.0.1:7193 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'good\n' > src/a.txt && printf '[check]\n[[check.steps]]\nname = "no-bad"\nrun = "! grep -rq BAD src"\n' > avp.toml && $T init lc >/dev/null
WEAK='[check]\n[[check.steps]]\nname = "no-bad"\nrun = "true"\n'
mkdir cheat && $T open -C cheat --repo lc --goal "bad change, weaker checks" >/dev/null
printf 'BAD\n' > cheat/src/a.txt && printf "$WEAK" > cheat/avp.toml
out=$($T -C cheat land)
ok "weakening the checks does not get a bad change through without a person" '[[ "$out" == *"not landed: approval"* && "$out" == *"avp.toml"* ]]'
mkdir broken && $T open -C broken --repo lc --goal "checks that cannot pass" >/dev/null
printf '[check]\n[[check.steps]]\nname = "no-bad"\nrun = "test -f nowhere.txt"\n' > broken/avp.toml
ok "a change that breaks the checks is red under them" '[[ "$($T -C broken land)" == *"not landed: red"* ]]'
mkdir tune && $T open -C tune --repo lc --goal "change the checks only" >/dev/null && printf "$WEAK" > tune/avp.toml
out=$($T -C tune land)
ok "a change to the checks waits for a person" '[[ "$out" == *"not landed: approval"* && "$out" == *"avp.toml"* ]]'
TASK=$($T -C tune status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')
curl -s -X POST -H 'content-type: application/json' -d '{"person":"ada","approved":true,"reason":""}' "$AVP_SERVER/repos/lc/tasks/$TASK/approve" >/dev/null
for i in $(seq 1 20); do [[ "$($T -C tune status --json)" == *'"landed"'* ]] && break; sleep 0.5; done
ok "approved, it lands" '[[ "$($T -C tune status --json)" == *"\"landed\""* ]]'
mkdir plain && $T open -C plain --repo lc --goal "ordinary change" >/dev/null && printf 'better\n' > plain/src/a.txt
ok "a task that leaves the checks alone lands on its proof" '[[ "$($T -C plain land)" == "landed at position 2"* ]]'
