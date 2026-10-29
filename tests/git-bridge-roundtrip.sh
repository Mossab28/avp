#!/bin/bash
# Round trip of the Git bridge on a local bare remote: import, an agent lands, export to main
# from fresh clones. Usage: AVP=target/release/avp bash tests/git-bridge-roundtrip.sh <scratch dir>
# Prints PASS/FAIL per check.
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
# Without a scratch dir it would write into the current folder (a source tree): refuse.
W=$(mktemp -d "${1:?usage: tests/git-bridge-roundtrip.sh <scratch dir>}/rt.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7198
$T serve --listen 127.0.0.1:7198 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
g(){ git -c user.name=Human -c user.email=h@x "$@"; }
git init -q --bare -b main origin.git
git init -q -b main src && cd src
printf 'gen/\n' > .gitignore; mkdir -p bin gen lib
printf 'echo hi\n' > bin/run.sh; chmod +x bin/run.sh
printf 'a\n' > lib/a.txt; printf 'old\n' > lib/old.txt; printf 'kept\n' > gen/tracked.d.ts
ln -s lib/a.txt link-to-a
g add -A; g add -f gen/tracked.d.ts
g update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,vendor/sub
g commit -qm "first"; printf 'b\n' > lib/b.txt; g add lib/b.txt; g commit -qm "second"
g remote add origin ../origin.git; g push -q origin main; BASE=$(git rev-parse HEAD); cd ..
git clone -q origin.git imp
$T -C imp import-git demo --history 5 >/dev/null 2>&1
ok "import: position 1 remembers its commit" "curl -s $AVP_SERVER/repos/demo/line/1/last-git | grep -q $BASE"
mkdir v && cd v && $T open --repo demo --goal "agent: change things" >/dev/null 2>&1
printf 'a2\n' > lib/a.txt; printf 'new\n' > lib/new.txt; rm lib/old.txt; chmod +x lib/b.txt
mkdir -p gen && printf 'generated\n' > gen/new.d.ts
$T sync >/dev/null 2>&1; $T add gen/new.d.ts >/dev/null 2>&1; $T land >"$W/land.log" 2>&1; cd ..
git clone -q origin.git exp1
$T -C exp1 mirror --repo demo --branch main --remote origin > "$W/m1.log" 2>&1; cat "$W/m1.log"
git clone -q origin.git check && cd check
ok "main fast-forwarded from the imported commit" "git merge-base --is-ancestor $BASE origin/main && [ \$(git rev-parse HEAD) != $BASE ]"
ok "one commit per landed position" "[ \$(git rev-list --count $BASE..HEAD) = 1 ]"
ok "trailers name position, task, proof" "git log -1 --format=%B | grep -q '^AVP-Position: 2' && git log -1 --format=%B | grep -q '^AVP-Task: task_' && git log -1 --format=%B | grep -q '^AVP-Proof: proof_'"
ok "symlink kept as a symlink" "git ls-tree HEAD link-to-a | grep -q '^120000'"
ok "submodule kept" "git ls-tree HEAD vendor/sub | grep -q '^160000'"
ok "exec bit carried" "git ls-tree HEAD lib/b.txt | grep -q '^100755'"
ok "edit carried" "grep -q a2 lib/a.txt"
ok "new file carried" "[ -f lib/new.txt ]"
ok "delete carried" "[ ! -e lib/old.txt ]"
ok "ignored file the line holds is exported" "git ls-tree HEAD gen/new.d.ts | grep -q blob"
ok "tracked ignored file kept" "git ls-tree HEAD gen/tracked.d.ts | grep -q blob"
ok "exe file still exec" "git ls-tree HEAD bin/run.sh | grep -q '^100755'"
cd ..; SHA1=$(git -C check rev-parse HEAD)
git clone -q origin.git exp2
$T -C exp2 mirror --repo demo --branch main --remote origin > "$W/m2.log" 2>&1
ok "second export from a fresh clone writes nothing" "grep -q '^0 new commit' $W/m2.log"
cd v && $T open --goal "agent: second" >/dev/null 2>&1 && printf 'c\n' > lib/c.txt && $T land >/dev/null 2>&1; cd ..
$T -C exp2 mirror --repo demo --branch main --remote origin > "$W/m3.log" 2>&1
ok "export resumes from another clone, no local state" "grep -q '^1 new commit' $W/m3.log && git -C origin.git merge-base --is-ancestor $SHA1 main"
cd src && g pull -q origin main && printf 'x\n' > hand.txt && g add -A && g commit -qm "written by hand" && g push -q origin main; cd ..
cd v && $T open --goal "agent: third" >/dev/null 2>&1 && printf 'd\n' > lib/d.txt && $T land >/dev/null 2>&1; cd ..
$T -C exp1 mirror --repo demo --branch main --remote origin > "$W/m4.log" 2>&1
ok "a hand-written commit on main makes the push refuse, nothing overwritten" "grep -q 'refused' $W/m4.log && git -C origin.git log -1 --format=%s main | grep -q 'written by hand'"
ok "a refused push is recorded as mirror.blocked, with the branch and the foreign commit" "curl -s $AVP_SERVER/repos/demo/events?since=0\&limit=1000 | grep -q '\"kind\":\"mirror.blocked\"' && curl -s $AVP_SERVER/repos/demo/events?since=0\&limit=1000 | grep -q $(git -C origin.git rev-parse main)"
$T -C exp1 mirror --repo demo --branch main --remote origin > "$W/m5.log" 2>&1
ok "a second refusal records nothing new" "[ \$(curl -s $AVP_SERVER/repos/demo/events?since=0\&limit=1000 | grep -o 'mirror.blocked' | wc -l) = 1 ]"
git -C origin.git update-ref refs/heads/main $SHA1
$T -C exp1 mirror --repo demo --branch main --remote origin > "$W/m6.log" 2>&1
ok "a later successful push is recorded as mirror.pushed" "curl -s $AVP_SERVER/repos/demo/events?since=0\&limit=1000 | grep -q '\"kind\":\"mirror.pushed\"'"
echo "workdir $W"