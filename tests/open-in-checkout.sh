#!/bin/bash
# A task opened in the agent's own git checkout (AVP_VIEW_IN_CHECKOUT=1), on a local bare remote:
# the head is laid there, the install stays, git moves to the position's commit, a checkout with
# changes of its own is refused. Usage: AVP=target/release/avp bash tests/open-in-checkout.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/open-in-checkout.sh <scratch dir>}/oc.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7197
$T serve --listen 127.0.0.1:7197 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
g(){ git -c user.name=Human -c user.email=h@x "$@"; }
git init -q --bare -b main origin.git
git init -q -b main src && cd src
printf 'node_modules/\n' > .gitignore; mkdir -p lib
printf 'a\n' > lib/a.txt; printf 'old\n' > lib/old.txt; printf '# Rules\n\nbe kind\n' > AGENTS.md
g add -A; g commit -qm "first"; g remote add origin ../origin.git; g push -q origin main; cd ..
git clone -q origin.git imp && $T -C imp import-git demo --history 5 >/dev/null 2>&1
# The window's copy, made before the line moves: an install of its own, nothing else.
git clone -q origin.git window && mkdir -p window/node_modules/dep && echo installed > window/node_modules/dep/index.js
# Another agent lands; the mirror writes its commit on main.
mkdir v && (cd v && $T open --repo demo --goal "another agent" >/dev/null 2>&1 && printf 'a2\n' > lib/a.txt && printf 'new\n' > lib/new.txt && rm lib/old.txt && $T land >"$W/land.log" 2>&1)
git clone -q origin.git exp && $T -C exp mirror --repo demo --branch main --remote origin >"$W/mirror.log" 2>&1
HEAD2=$(git ls-remote origin.git refs/heads/main | cut -f1); BASE=$(git -C imp rev-parse HEAD)
cd window && AVP_VIEW_IN_CHECKOUT=1 $T open --repo demo --goal "in my own checkout" >"$W/open.log" 2>&1; cd ..
ok "the task opened in the checkout" "grep -q 'opened on position 1' $W/open.log && [ -f window/.avp/view.json ]"
ok "the head is laid there" "grep -q a2 window/lib/a.txt && [ -f window/lib/new.txt ] && [ ! -e window/lib/old.txt ]"
ok "the install stays" "[ -f window/node_modules/dep/index.js ]"
ok "git is on the position's commit" "[ \$(git -C window rev-parse HEAD) = $HEAD2 ]"
ok "git sees nothing, AVP's state included" "[ -z \"\$(git -C window status --porcelain)\" ]"
(cd window && printf 'mine\n' > lib/mine.txt && $T sync >"$W/sync.log" 2>&1)
ok "the agent's change is a write, and git shows it alone" "grep -q 'lib/mine.txt' $W/sync.log && [ \"\$(git -C window status --porcelain)\" = '?? lib/mine.txt' ]"
git clone -q origin.git dirty && echo scratch > dirty/notes.txt
(cd dirty && AVP_VIEW_IN_CHECKOUT=1 $T open --repo demo --goal "over my changes" >"$W/dirty.log" 2>&1)
ok "a checkout with changes of its own is refused" "grep -q 'holds changes git has not committed' $W/dirty.log && [ ! -e dirty/.avp ]"
(cd "$W" && git clone -q origin.git plain && cd plain && git reset -q --hard $BASE && $T open --repo demo --goal "without the switch" >"$W/plain.log" 2>&1)
ok "without the switch, open behaves as before (git left where it was)" "[ \$(git -C plain rev-parse HEAD) = $BASE ] && grep -q 'opened on position' $W/plain.log"
# The agent lands from its checkout; once the mirror writes the commit, git follows it.
(cd window && AVP_VIEW_IN_CHECKOUT=1 $T land >"$W/land2.log" 2>&1)
$T -C exp mirror --repo demo --branch main --remote origin >"$W/mirror2.log" 2>&1
HEAD3=$(git ls-remote origin.git refs/heads/main | cut -f1)
for i in $(seq 1 20); do [ "$(git -C window rev-parse HEAD)" = "$HEAD3" ] && break; sleep 1; done
ok "after its landing, git is on the new position's commit and sees nothing" "grep -q 'landed at position 2' $W/land2.log && [ \$(git -C window rev-parse HEAD) = $HEAD3 ] && [ -z \"\$(git -C window status --porcelain)\" ]"
# A host that writes its own block into a tracked file and its state into the checkout (Murmell).
git clone -q origin.git host && (cd host && printf '\n<!-- murmell:start -->\n# Murmell canvas\nclaim files\n<!-- murmell:end -->\n' >> AGENTS.md && mkdir -p .murmell && echo '{}' > .murmell/desk.json)
export AVP_HOST_BLOCKS=murmell AVP_HOST_PATHS=.murmell/
(cd host && AVP_VIEW_IN_CHECKOUT=1 $T open --repo demo --goal "in a host's checkout" >"$W/host-open.log" 2>&1)
ok "a checkout whose only changes are the host's opens in place" "[ -f host/.avp/view.json ] && grep -q 'opened on position' $W/host-open.log"
ok "the host's block stays in AGENTS.md" "grep -q 'murmell:start' host/AGENTS.md && grep -q 'be kind' host/AGENTS.md"
(cd host && printf 'be kind, always\n' > /dev/null && sed -i.bak 's/be kind/be kind, always/' AGENTS.md && rm AGENTS.md.bak && AVP_VIEW_IN_CHECKOUT=1 $T sync >"$W/host-sync.log" 2>&1 && AVP_VIEW_IN_CHECKOUT=1 $T land >"$W/host-land.log" 2>&1)
P3=$(grep -o 'landed at position [0-9]*' "$W/host-land.log" | grep -o '[0-9]*$')
curl -s "$AVP_SERVER/repos/demo/tree/$P3" | python3 -c 'import json,sys; t=json.load(sys.stdin); print(t["AGENTS.md"]["blob"])' > "$W/agents-blob" 2>/dev/null
curl -s "$AVP_SERVER/repos/demo/objects/$(cat $W/agents-blob)" > "$W/agents-landed"
ok "the agent's edit of AGENTS.md lands, without the host's block" "grep -q 'be kind, always' $W/agents-landed && ! grep -q 'murmell' $W/agents-landed"
ok "the host's state is never sent" "! grep -q '.murmell' $W/host-sync.log"
# A window that is a git worktree (Murmell's): the host's files listed in the shared info/exclude
# are neither deleted by the open nor sent as the task's writes.
git clone -q origin.git wt-main && git -C wt-main worktree add -q ../wt -b wt >/dev/null 2>&1
printf '.claude/settings.json\n.cursor/hooks.json\n' >> "$(git -C wt rev-parse --path-format=absolute --git-path info/exclude)"
mkdir -p wt/.claude wt/.cursor && echo '{"hooks":1}' > wt/.claude/settings.json && echo '{}' > wt/.cursor/hooks.json
(cd wt && AVP_VIEW_IN_CHECKOUT=1 $T open --repo demo --goal "in a worktree" >"$W/wt-open.log" 2>&1)
ok "in a worktree, the excluded host files stay" "[ -f wt/.avp/view.json ] && [ -f wt/.claude/settings.json ] && [ -f wt/.cursor/hooks.json ]"
(cd wt && echo '{"hooks":2}' > .claude/settings.json && echo b > lib/a.txt && AVP_VIEW_IN_CHECKOUT=1 $T sync >"$W/wt-sync.log" 2>&1)
ok "in a worktree, the excluded host files are never sent" "grep -q lib/a.txt $W/wt-sync.log && ! grep -q -e settings.json -e hooks.json $W/wt-sync.log"
