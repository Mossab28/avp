#!/bin/bash
# The everyday commands: `avp diff` (a task against its base or the head, --stat, --json, paths, two
# positions), save points (`avp save`, `avp saves`, `avp undo --to`, never landed), and `avp clone`
# (a folder holding the head, a view with no git, its agents wired and their files kept out of the
# task's writes).
# Usage: AVP=target/release/avp bash tests/everyday.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/everyday.sh <scratch dir>}/ev.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7477 AVP_HOME=${AVP_HOME:-$W/home}
$T serve --listen 127.0.0.1:7477 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir -p r/src && printf 'one\ntwo\nthree\n' > r/src/a.txt && printf 'b\n' > r/src/b.txt && (cd r && $T init ev >/dev/null)

$T open -C v --repo ev --goal "two to TWO" >/dev/null
$T open -C w --repo ev --goal "b to B" >/dev/null
printf 'one\nTWO\nthree\n' > v/src/a.txt && printf 'new\n' > v/src/c.txt
d=$($T -C v diff)
ok "diff: the task against its base, unsent edits included" '[[ "$d" == *"--- a/src/a.txt"*"-two"*"+TWO"*"--- /dev/null"*"+++ b/src/c.txt"*"+new"* ]]'
ok "diff --stat counts the lines" '[[ "$($T -C v diff --stat)" == *"src/a.txt | 2 +-"*"2 files changed, 2 insertions(+), 1 deletion(-)"* ]]'
ok "diff takes paths as filters" '[[ "$($T -C v diff src/c.txt)" == *c.txt* && "$($T -C v diff src/c.txt)" != *a.txt* ]]'
touch v/src/empty.txt && python3 -c "print('x' * 300000)" > v/src/big.txt
ok "diff tells an empty file from one too large to show" '[[ "$($T -C v diff src/big.txt src/empty.txt)" == *"src/big.txt is too large to show"* && "$($T -C v diff src/empty.txt)" != *"too large"* ]]'
rm v/src/empty.txt v/src/big.txt
ok "diff --json" '[ "$($T -C v diff --json | python3 -c "import json,sys; d=json.load(sys.stdin); print([(f[\"path\"], f[\"added\"], f[\"removed\"]) for f in d[\"files\"]])")" = "[('"'"'src/a.txt'"'"', 1, 1), ('"'"'src/c.txt'"'"', 1, 0)]" ]'

ok "save names the task's state" '[[ "$($T -C v save "TWO and c")" == *"save point 1: TWO and c"* ]]'
printf 'one\nTWO\nTHREE\n' > v/src/a.txt && rm v/src/c.txt
$T -C v save THREE >/dev/null
ok "saves lists them, status too" '[[ "$($T -C v saves)" == *"1  write"*"TWO and c"*"2  write"*THREE* && "$($T -C v status)" == *"save points:"*THREE* ]]'
$T -C v undo --to 1 >/dev/null
ok "undo --to goes back to a save" '[ "$(cat v/src/a.txt)" = "$(printf "one\nTWO\nthree")" ] && [ -f v/src/c.txt ]'
$T -C v undo --to THREE >/dev/null
ok "and forward to a later one, by its message" '[ "$(cat v/src/a.txt)" = "$(printf "one\nTWO\nTHREE")" ] && [ ! -e v/src/c.txt ]'
$T -C v land >/dev/null
ok "save points never land as positions" '[ "$($T -C v log | wc -l | tr -d " ")" = 2 ]'
ok "diff between two positions" '[[ "$($T -C v diff 0 1)" == *"+THREE"* && "$($T -C v diff 0 1)" != *c.txt* ]]'

printf 'B\n' > w/src/b.txt
ok "diff --head: against the head of the line" '[[ "$($T -C w diff)" != *a.txt* && "$($T -C w diff --head)" == *"--- a/src/a.txt"*"-THREE"*"+B"* ]]'

mkdir -p fresh/.cursor
HOME=$W/fresh PATH=/usr/bin:/bin $T clone "$AVP_SERVER/ev" c >clone.out 2>&1
ok "clone lays out the head, a view with no git" '[ "$(cat c/src/a.txt)" = "$(printf "one\nTWO\nTHREE")" ] && [ -f c/.avp/view.json ] && [ ! -e c/.git ] && grep -q "at position 1" clone.out'
ok "clone wires the agents found, as avp up does" 'grep -q "\"-C\"" c/.cursor/mcp.json && grep -q avp_land c/AGENTS.md && grep -Eq "Cursor +wired" clone.out'
$T open -C c --goal "from the clone" >/dev/null && printf 'b2\n' > c/src/b.txt
s=$($T -C c sync)
ok "what clone wired stays out of the task's writes" '[[ "$s" == *"+ src/b.txt"* && "$s" != *cursor* && "$s" != *AGENTS* ]]'
ok "clone --repo: the folder takes the repo's name" 'mkdir in && (cd in && HOME=$W/fresh PATH=/usr/bin:/bin $T clone --repo ev) && [ -f in/ev/src/a.txt ]'
ok "clone into a folder that holds files refuses" '! $T clone "$AVP_SERVER/ev" c'
