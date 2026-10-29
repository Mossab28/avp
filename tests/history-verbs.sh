#!/bin/bash
# History verbs Git users expect: `avp bisect` names the position that broke a check, `avp pick`
# brings a position's change back into a task, and a tag names a position in `avp log`, as a git tag
# on the mirror's commit (moved and deleted with it, a tag git holds of its own left alone), and for
# `avp promote`. Usage: AVP=target/proof/avp bash tests/history-verbs.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
# The caller's AVP_* (an agent's AVP_REPO) would name another repo than this test's.
for v in $(compgen -e | grep -E '^(AVP|TRAME)_'); do unset "$v"; done
W=$(mktemp -d "${1:?usage: tests/history-verbs.sh <scratch dir>}/hv.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7478 TMPDIR=$W
$T serve --listen 127.0.0.1:7478 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
g(){ git -c user.name=Human -c user.email=h@x "$@"; }
git init -q --bare -b main origin.git
git init -q -b main src && cd src
printf '# the sum\n# of two and two\n\necho 4\n' > calc.sh && printf 'n0\n' > notes.txt
g add -A && g commit -qm "first" && g remote add origin ../origin.git && g push -q origin main && cd ..
$T -C src import-git hv --history 5 >/dev/null 2>&1
mkdir v && cd v
step(){ $T open --repo hv --goal "$1" >/dev/null && eval "$2" && $T land >/dev/null; }
step "more notes" "printf 'n1\n' >> notes.txt"
AVP_PERSON=ada AVP_AGENT=codex step "sum faster" "sed -i.bak 's/4/5/' calc.sh && rm calc.sh.bak"
step "even more notes" "printf 'n2\n' >> notes.txt"
step "notes again" "printf 'n3\n' >> notes.txt"

out=$($T bisect --check '[ "$(sh calc.sh)" = 4 ]' 2>"$W/probes.log")
ok "bisect names the first bad position, its goal, person and agent" '[[ "$out" == "first bad position: 2  sum faster"* && "$out" == *"by ada via codex"* ]]'
ok "it checked both ends, then halved" '[ "$(grep -c "^position" "$W/probes.log")" -le 4 ] && grep -q "^position 4: bad" "$W/probes.log" && grep -q "^position 0: good" "$W/probes.log"'
dir=$(sed -n 's/^\(.*\) holds it\..*/\1/p' <<<"$out")
ok "the scratch folder is left on the first bad position" '[ "$(sh "$dir/calc.sh")" = 5 ]'
json=$($T bisect --check '[ "$(sh calc.sh)" = 4 ]' --good 1 --bad 3 --json 2>/dev/null)
ok "--good, --bad and --json" '[ "$(python3 -c "import json,sys; b=json.loads(sys.argv[1]); print(b[\"first_bad\"], b[\"last_good\"], [p[\"position\"] for p in b[\"probes\"]])" "$json")" = "2 1 [3, 1, 2]" ]'
ok "a check that fails everywhere blames nobody" '! $T bisect --check false 2>&1 | grep -q "first bad" && $T bisect --check false 2>&1 | grep -q "fails on position 0"'

$T revert 2 >/dev/null && $T land >/dev/null
$T open --goal "sum faster, again" >/dev/null && sed -i.bak 's/sum$/sum, fixed/' calc.sh && rm calc.sh.bak
picked=$($T pick 2)
ok "pick brings the reverted change back, merged with the task's" '[[ "$picked" == "position 2 (sum faster) picked into the task: 0 files as it has them, 1 merged"* ]] && [ "$(sed -n "1p;4p" calc.sh | tr "\n" " ")" = "# the sum, fixed echo 5 " ]'
ok "the picked change is the task's, and lands like any" '$T land | grep -q "landed at position 6"'

$T tag v1.0 1 >/dev/null
ok "a tag names a position once; -f moves it" '! $T tag v1.0 3 2>/dev/null && $T tag v1.0 3 2>&1 | grep -q "avp tag -f v1.0 3"'
$T tag -f v1.0 3 >/dev/null && $T tag v2.0 >/dev/null && $T tag v0.9 2 >/dev/null
ok "avp log shows them as git decorates" '$T log | grep -q "^    3  .*(tag: v1.0) even more notes" && $T log | grep -q "^    6  .*(tag: v2.0) sum faster, again"'
ok "avp tags lists them, newest first" '[ "$($T tags | awk "{print \$1}" | tr "\n" " ")" = "v2.0 v1.0 v0.9 " ]'
ok "AVP Live reads them with the positions" 'curl -s "$AVP_SERVER/repos/hv/positions?from=3&limit=1" | grep -q "\"tags\":\[\"v1.0\"\]"'

cd .. && git clone -q origin.git mir && cd mir && g tag v0.9 HEAD && g push -q origin v0.9 && cd ..
m1=$($T -C mir mirror --repo hv --branch main --remote origin 2>/dev/null)
commit(){ curl -s "$AVP_SERVER/repos/hv/line?from=$1&limit=1" | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['git'])"; }
remote(){ git ls-remote origin.git "refs/tags/$1" | cut -f1; }
ok "the mirror writes each tag as a git tag on its position's commit" '[ "$(remote v1.0)" = "$(commit 3)" ] && [ "$(remote v2.0)" = "$(commit 6)" ] && [[ "$m1" == *"tag v1.0 written"* ]]'
ok "a tag git holds of its own is left as it is, and said" '[ "$(remote v0.9)" = "$(commit 0)" ] && [[ "$m1" == *"tag v0.9 left as it is"* ]]'
$T -C v tag -f v1.0 4 >/dev/null && $T -C v tag -d v2.0 >/dev/null
m2=$($T -C mir mirror --repo hv --branch main --remote origin 2>/dev/null)
ok "moved and deleted on the line, moved and deleted in git" '[ "$(remote v1.0)" = "$(commit 4)" ] && [ -z "$(remote v2.0)" ] && [[ "$m2" == *"tag v1.0 moved"* && "$m2" == *"tag v2.0 deleted"* ]]'
ok "a pass with nothing new does nothing to tags" '[[ "$($T -C mir mirror --repo hv --branch main --remote origin 2>/dev/null)" != *"tag v1.0"* ]]'

ok "a tag is promoted by its name" '$T -C v promote staging v1.0 --force | grep -q "staging now serves position 4"'
ok "a name that is no tag is refused" '$T -C v promote staging v7 --force 2>&1 | grep -q "no position nor tag v7"'
