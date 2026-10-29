#!/bin/bash
# The line's memory handed to an agent up front, so it does not read and grep to rebuild it: `avp
# open --scope` ends with the last positions that changed each scoped path and the open task holding
# it, `avp claim` says the same of paths outside the scope, `avp context` (the avp_context tool) of
# any path, and `path:line` adds why that line is what it is.
# Usage: AVP=target/release/avp bash tests/context.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/context.sh <scratch dir>}/cx.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7445 AVP_MCP_LOG= AVP_SPECULATE=0
unset AVP_VIEW_IN_CHECKOUT
$T serve --listen 127.0.0.1:7445 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir -p src docs && printf 'one\ntwo\n' > src/a.txt && printf 'b\n' > src/b.txt && printf 'd\n' > docs/d.md && printf 'r\n' > README.md && $T init cx >/dev/null
mkdir a && AVP_AGENT=codex $T open -C a --repo cx --goal "say TWO" >/dev/null
printf 'one\nTWO\n' > a/src/a.txt && $T -C a land >/dev/null
mkdir h && AVP_AGENT=gemini $T open -C h --repo cx --goal "rework b" --scope src/b.txt >/dev/null

mkdir c && open=$(AVP_AGENT=claude-code $T open -C c --repo cx --goal "more of a" --scope src/)
ok "open with a scope ends with the last changes of each of its paths" '[[ "$open" == *"newest first (position, age, agent, goal):"*"src/a.txt"*" 1 "*"codex"*"say TWO"*" 0 "*imported* ]]'
ok "and names the open task that holds one" '[[ "$open" == *"src/b.txt  held by open task task_"*"(gemini): rework b"* ]]'
mkdir n && ok "a task without a scope is told nothing of the line" '! [[ "$($T open -C n --repo cx --goal "anything")" == *"last changes"* ]]'
mkdir m && json=$($T open -C m --repo cx --goal "again" --scope src/a.txt --json)
ok "open --json keeps the task at the top and adds its context" '[ "$(python3 -c "import json,sys; o=json.loads(sys.argv[1]); print(o[\"id\"][:5], [(p[\"path\"], [c[\"position\"] for c in p[\"changes\"]]) for p in o[\"context\"]])" "$json")" = "task_ [('"'"'src/a.txt'"'"', [1, 0])]" ]'

cx=$($T -C c context src/a.txt:2 docs/)
ok "context says the same of any path, and why a line is what it is" '[[ "$cx" == *"src/a.txt"*"say TWO"*"docs/d.md"*"src/a.txt:2  TWO"*"written at position 1 by task"* ]]'
ok "a path with no history is said so" '[[ "$($T -C c context nowhere.txt)" == "no position of the line changed these paths" ]]'
ok "claiming a path outside the scope tells its history" '[[ "$($T -C c claim docs/d.md)" == *"docs/d.md: taken"*"docs/d.md"*imported* ]]'
printf 'r2\n' > c/README.md && sent=$($T -C c sync)
ok "so does the first write outside the scope, once" '[[ "$sent" == *"+ README.md [taken]"*"newest first"*"README.md"*imported* ]] && printf "r3\n" > c/README.md && [ "$($T -C c sync)" = "+ README.md [held]" ]'

mkdir agent && mcp=$(cd agent && printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"e2e"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"avp_context","arguments":{"paths":["src/"]}}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"avp_open","arguments":{"goal":"docs over mcp","scope":["docs/"]}}}' | AVP_REPO=cx AVP_VIEWS="$W/views" $T mcp)
answer(){ python3 -c "import json,sys; print([json.loads(l) for l in sys.argv[1].splitlines()][int(sys.argv[2])]['result']['content'][0]['text'])" "$mcp" "$1"; }
ok "the avp_context tool answers the same lines, before any task is open" '[[ "$(answer 1)" == *"src/a.txt"*"say TWO"*"src/b.txt  held by open task"* ]]'
ok "avp_open ends with them" '[[ "$(answer 2)" == *"newest first"*"docs/d.md  held by open task task_"*"(claude-code): more of a"* ]]'
ok "and the MCP instructions point to it" '[[ "$mcp" == *"avp_context says the same of any path: read it before exploring the code"* ]]'
