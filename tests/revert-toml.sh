#!/bin/bash
# A landing that breaks avp.toml's full suite is taken back like any other: the bisection runs each
# position's own suite, AVP's revert is proven under the avp.toml it brings back, green, and since it
# changes avp.toml it waits, open, for a person. Approved, it lands as AVP's revert, and the line is
# green again.
# Usage: AVP=target/proof/avp bash tests/revert-toml.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/revert-toml.sh <scratch dir>}/rt.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7487 AVP_PERSON=ada AVP_AGENT=codex
$T serve --listen 127.0.0.1:7487 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir app && cd app || exit 1
printf 'v0\n' > app.txt
cat > avp.toml <<'EOF'
[check]
full = "test -f app.txt"
[[check.steps]]
name = "build"
run = "test -f app.txt"
EOF
$T init rt >/dev/null
cd "$W" || exit 1
full(){ for _ in $(seq 1 120); do v=$($T log --repo rt | awk -v p="$1" '$1 == p { print $2 }'); [[ "$v" == Green || "$v" == Flaky || "$v" == Red ]] && { echo "$v"; return; }; sleep 0.5; done; }
events(){ curl -s "$AVP_SERVER/repos/rt/events?limit=1000" | python3 -X utf8 -c "import json,sys; print(json.dumps([e['data'] for e in json.load(sys.stdin) if e['kind'] == '$1'], ensure_ascii=False))"; }
landed(){ for _ in $(seq 1 40); do [[ "$(curl -s "$AVP_SERVER/repos/rt/tasks/$1")" == *'"state":"landed"'* ]] && return 0; sleep 0.5; done; return 1; }

# The checks move to scripts: the steps run ci/smoke.sh, which the change adds, and the full suite
# ci/e2e.sh, which it forgot. Its steps are green; avp.toml is guarded, a person approves it.
mkdir a && $T open -C a --repo rt --goal "the checks run from ci/" >/dev/null
mkdir a/ci && printf 'test -f app.txt\n' > a/ci/smoke.sh
cat > a/avp.toml <<'EOF'
[check]
full = "bash ci/e2e.sh"
[[check.steps]]
name = "build"
run = "bash ci/smoke.sh"
EOF
ok "a change of avp.toml is proven by its own steps, green, and waits for a person" '[[ "$($T -C a land)" == *"not landed: approval"* ]]'
TASK=$($T -C a status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["task"]["id"])')
$T approve "$TASK" --repo rt >/dev/null
ok "approved, it lands at position 1" 'landed "$TASK"'
ok "its full suite is red, twice" '[ "$(full 1)" = Red ]'

# Position 0 under its own avp.toml is green: position 1 broke it. Its revert, proven under the
# avp.toml it brings back (not the head's, whose step runs the ci/smoke.sh it removes), is green.
for _ in $(seq 1 120); do [[ "$(events line.revert_failed)" == *'"revert"'* ]] && break; sleep 0.5; done
waits=$(events line.revert_failed | python3 -X utf8 -c 'import json,sys; e=json.load(sys.stdin)[-1]; print(e["revert"]["revert_task"], "|", e["reason"])')
REVERT=${waits%% *}
ok "AVP's revert waits for a person, and says how to land it" '[[ "$waits" == *"the revert of position 1 is proven green and waits for a person'"'"'s approval, as it changes avp.toml"*"avp approve $REVERT"* ]]'
st=$(curl -s "$AVP_SERVER/repos/rt/tasks/$REVERT")
ok "its proof is green, under the avp.toml it brings back" '[[ "$st" == *"\"state\":\"open\""* && "$st" == *"\"verdict\":\"green\""* && "$st" == *"\"command\":\"test -f app.txt\""* ]]'
ok "nothing landed without the person" '[ "$(curl -s "$AVP_SERVER/repos/rt/line?from=99&limit=1" | python3 -c "import json,sys; print(json.load(sys.stdin)[0][\"position\"])")" = 1 ]'

$T approve "$REVERT" --repo rt >/dev/null
ok "approved, the revert lands at position 2" 'landed "$REVERT"'
ok "the line is green again" '[ "$(full 2)" = Green ]'
ok "avp.toml is back as it was, ci/ gone" '[ "$(curl -s "$AVP_SERVER/repos/rt/tree/2" | python3 -c "import json,sys; print(\" \".join(sorted(json.load(sys.stdin))))")" = "app.txt avp.toml" ] && [ "$(curl -s "$AVP_SERVER/repos/rt/tree/2" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"avp.toml\"][\"blob\"])")" = "$(curl -s "$AVP_SERVER/repos/rt/tree/0" | python3 -c "import json,sys; print(json.load(sys.stdin)[\"avp.toml\"][\"blob\"])")" ]'
said=$(events line.reverted | python3 -X utf8 -c 'import json,sys; e=json.load(sys.stdin); print(len(e), e[0]["position"], e[0]["by"], e[0]["person"], e[0]["agent"], ",".join(sorted(e[0]["files"])), json.dumps(e[0]["probes"]))')
ok "landed, it is recorded as AVP's revert of position 1" '[ "$said" = "1 1 2 ada codex avp.toml,ci/smoke.sh [{\"position\": 0, \"red\": false}]" ]'
ok "avp show says so" '[[ "$($T show 1 --repo rt)" == *"reverted by AVP at position 2"* ]]'
