#!/bin/bash
# Git keeps coming in while a team moves over: commits pushed to main by a person become tasks,
# replayed on the head, proven and landed as their author; the mirror's own commits are never taken
# back in; a red one stays open and the commits after it wait; `avp serve` does the same by itself
# from avp.toml's [git]. A local bare remote, no network.
# Usage: AVP=target/release/avp bash tests/git-inbound.sh <scratch dir>. Prints PASS/FAIL per check.
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/git-inbound.sh <scratch dir>}/gi.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7473
$T serve --listen 127.0.0.1:7473 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
FP=
trap 'kill $SP $FP 2>/dev/null' EXIT
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
upto(){ for _ in $(seq 1 ${2:-60}); do eval "$1" >/dev/null 2>&1 && return 0; sleep 0.5; done; return 1; }
upto "curl -sf $AVP_SERVER/"
g(){ git -c user.name=Human -c user.email=h@x "$@"; }
head_pos(){ curl -s $AVP_SERVER/repos/demo/head | grep -o '"position":[0-9]*' | cut -d: -f2; }
# A file of the head of the line.
at_head(){ local b; b=$(curl -s "$AVP_SERVER/repos/demo/tree/$(head_pos)" | grep -o "\"$1\":{\"blob\":\"[0-9a-f]*\"" | cut -d'"' -f6); curl -s "$AVP_SERVER/repos/demo/objects/$b"; }
agent(){ (mkdir -p v && cd v && $T open --repo demo --goal "$1" >/dev/null 2>&1 && eval "$2" && $T land >/dev/null 2>&1); }
person(){ (cd src && g pull -q --rebase origin main && eval "$1" && g add -A && g commit -qm "$2" && g push -q origin main); }

git init -q --bare -b main origin.git
git init -q -b main src && cd src
cat > avp.toml <<'EOF'
[[check.steps]]
name = "no-broken"
run = "! grep -rl BROKEN --include=*.txt ."
EOF
mkdir lib && printf 'one\ntwo\nthree\nfour\nfive\n' > lib/a.txt
g add -A && g commit -qm "first" && g remote add origin ../origin.git && g push -q origin main; cd ..
git clone -q origin.git imp
$T -C imp import-git demo --history 5 >/dev/null 2>&1
agent "agent: one in capitals" "sed -i.b 's/^one/ONE/' lib/a.txt && rm lib/a.txt.b"

# The mirror writes main; following main then finds nothing of its own to take back.
git clone -q origin.git exp && $T -C exp mirror --repo demo --branch main --remote origin >/dev/null 2>&1
git -C imp pull -q origin main
$T -C imp import-git demo > once0.log 2>&1
ok "the mirror's commits are never taken back in" "grep -q 'nothing to bring in' once0.log && [ \$(head_pos) = 1 ]"

# A person still pushes to main: the follow brings it in as a task, as its author.
$T -C imp import-git demo --follow main --every 1 --views "$W/views" > follow.log 2>&1 & FP=$!
person "printf 'by hand\n' > hand.txt" "hand: a file"
upto "[ \$(head_pos) = 2 ]"
ok "a commit pushed to main lands as a task" "[ \$(head_pos) = 2 ] && grep -q 'hand: a file: landed at 2' follow.log"
ok "its person is the commit's author, its agent git, its goal the subject" "$T -C imp show 2 | grep -q 'h@x' && $T -C imp show 2 | grep -q 'git' && $T -C imp show 2 | grep -q 'hand: a file'"
ok "exactly the commit on its parent's position: the position is that commit" "curl -s $AVP_SERVER/repos/demo/line/2/last-git | grep -q \$(git -C origin.git rev-parse main)"

# The line moved meanwhile: the commit's change is merged three-way onto the head.
agent "agent: three in capitals" "sed -i.b 's/^three/THREE/' lib/a.txt && rm lib/a.txt.b"
person "sed -i.b 's/^five/FIVE/' lib/a.txt && rm lib/a.txt.b" "hand: five in capitals"
upto "[ \$(head_pos) = 4 ]"
ok "a commit on a line that moved is merged with what landed" "[ \"\$(at_head lib/a.txt | tr '\n' ' ')\" = 'ONE two THREE four FIVE ' ]"
# main holds that commit, not the line's history: the mirror's next commit merges it.
H=$(git -C origin.git rev-parse main)
$T -C exp mirror --repo demo --branch main --remote origin > mirror.log 2>&1
ok "the mirror merges what main got by Git, its push a fast-forward" "git -C origin.git merge-base --is-ancestor $H main && [ \$(git -C origin.git log -1 --format=%P main | wc -w) = 2 ] && git -C origin.git log -1 --format=%B main | grep -q '^AVP-Position: 4'"

# Red: the task stays open, the next commit waits for it.
person "printf 'BROKEN\n' > bad.txt" "hand: something broken"
upto "grep -q 'something broken: open' follow.log"
ok "a red proof leaves the task open and says so" "grep -q 'something broken: open, its proof is red (no-broken)' follow.log && [ \$(head_pos) = 4 ] && $T tasks --repo demo | grep -q 'via git  hand: something broken'"
ok "its task is laid out to settle" "ls -d $W/views/git-*/.avp"
person "git rm -q bad.txt" "hand: unbreak"
upto "grep -q 'the commit after it waits for it' follow.log"
ok "the commits after it wait for it" "grep -q 'the commit after it waits for it' follow.log && [ \$(head_pos) = 4 ]"
(cd $W/views/git-* && $T abandon >/dev/null 2>&1)
upto "grep -q 'unbreak: nothing to bring in' follow.log"
ok "abandoned, the commit is left out and the next comes in" "grep -q 'unbreak: nothing to bring in' follow.log && [ \$(head_pos) = 4 ]"
kill $FP; FP=
ok "each commit came in once" "[ \$(curl -s \"$AVP_SERVER/repos/demo/tasks?agent=git\" | grep -o '\"id\"' | wc -l) = 4 ]"

# avp serve follows by itself once avp.toml says [git].
# avp.toml is always guarded: the agent's change waits for a person's word.
(cd v && $T open --goal "agent: the server follows main" >/dev/null 2>&1 && printf '\n[git]\nremote = "%s"\nfollow = "main"\nevery = 1\n' "$W/origin.git" >> avp.toml \
  && $T land >/dev/null 2>&1; $T approve "$(grep -o 'task_[0-9a-z]*' .avp/view.json | head -1)" --repo demo >/dev/null 2>&1; $T land >/dev/null 2>&1)
person "printf 'served\n' > served.txt" "hand: while the server follows"
upto "[ \$(head_pos) = 6 ]" 80
ok "avp serve brings main's commits in by itself, as their author" "[ \$(head_pos) = 6 ] && $T -C imp show 6 | grep -q 'h@x' && grep -q 'while the server follows: landed at 6' server.log"
echo "workdir $W"
