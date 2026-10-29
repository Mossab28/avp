#!/bin/bash
# run-arm.sh <classic|avp> <run number>
# Three agents build Facturo's three domains at once. Classic: worktrees, PRs, CI, merges on the
# bench repo (main reset to the bench-base tag first). AVP: a fresh AVP repo imported from the
# same base, three views. Results in ~/.avp-bench/facturo/<arm>-<n>/.
set -u
ARM=$1; N=$2
H=$HOME/Projects/avp/bench
OUT=$HOME/.avp-bench/facturo/$ARM-$N; rm -rf "$OUT"; mkdir -p "$OUT"
REPO=https://github.com/Mossab28/avp-bench.git
export AVP_SERVER=http://127.0.0.1:7117 AVP_PERSON=mossab
MODEL=${BENCH_MODEL:-opus}
TASKS="customers invoices payments"
log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$OUT/log"; }

session() { # <task> <dir> <prompt> [extra claude args...]
  local task=$1 dir=$2 prompt=$3; shift 3
  ( cd "$dir" && date -u +%FT%TZ > "$OUT/$task.start" \
    && claude -p "$prompt" --model "$MODEL" --dangerously-skip-permissions --output-format json --max-turns 400 "$@" < /dev/null > "$OUT/$task.json" 2> "$OUT/$task.err"; echo $? > "$OUT/$task.exit"; date -u +%FT%TZ > "$OUT/$task.end" ) &
}

if [ "$ARM" = classic ]; then
  W=$OUT/work; git clone -q "$REPO" "$W/main" && cd "$W/main"
  git fetch -q origin tag bench-base && git reset -q --hard bench-base && git push -q --force origin main
  log "main reset to $(git rev-parse --short HEAD)"
  gh pr list --repo Mossab28/avp-bench --state open --json number --jq '.[].number' | xargs -I{} gh pr close {} --repo Mossab28/avp-bench --delete-branch >/dev/null 2>&1
  for t in $TASKS; do
    git worktree add -q "$W/$t" -b "bench/$t-$N" bench-base
    (cd "$W/$t" && CI=true pnpm install --frozen-lockfile --prefer-offline > /dev/null 2>&1)
  done
  log "start classic run $N"
  for t in $TASKS; do session "$t" "$W/$t" "$(cat $H/tasks/$t.md)$(cat $H/process-classic.md)"; done
  wait
  log "all sessions done"
  cd "$W/main" && git fetch -q origin && git reset -q --hard origin/main && git log --oneline bench-base..HEAD > "$OUT/merged.txt"
  RESULT="$W/main"
else
  TR=facturo-$N; SRC=$OUT/src; git clone -q "$REPO" "$SRC" && cd "$SRC" && git fetch -q origin tag bench-base && git checkout -q bench-base
  ~/.local/bin/avp init "$TR" > "$OUT/init.txt" 2>&1 || { cat "$OUT/init.txt"; exit 1; }
  for t in $TASKS; do
    V=$OUT/view-$t; mkdir -p "$V"; cd "$V"
    AVP_AGENT=warm ~/.local/bin/avp open --goal "warm $t" --repo "$TR" > /dev/null && CI=true pnpm install --frozen-lockfile --prefer-offline > /dev/null 2>&1; ~/.local/bin/avp abandon > /dev/null
    cat > .mcp.json <<JSON
{ "mcpServers": { "avp": { "command": "$HOME/.local/bin/avp", "args": ["mcp"], "env": { "AVP_SERVER": "http://127.0.0.1:7117", "AVP_AGENT": "claude-code", "AVP_PERSON": "mossab" } } } }
JSON
  done
  # warm the server-side setup once so the first check pays no install
  cd "$OUT/view-customers" && AVP_AGENT=warm ~/.local/bin/avp open --goal "warm setup" --repo "$TR" > /dev/null && ~/.local/bin/avp check > "$OUT/warm-check.txt" 2>&1; ~/.local/bin/avp abandon > /dev/null
  log "start avp run $N (repo $TR)"
  for t in $TASKS; do AVP_AGENT=claude-code session "$t" "$OUT/view-$t" "$(cat $H/tasks/$t.md)" --mcp-config .mcp.json --strict-mcp-config --append-system-prompt "$(cat $H/system-avp.md)" --disallowedTools "Bash(git:*),Bash(gh:*)"; done
  wait
  log "all sessions done"
  ~/.local/bin/avp -C "$OUT/view-customers" log --limit 10 > "$OUT/line.txt"
  X=$OUT/export; mkdir -p "$X" && cd "$X" && AVP_AGENT=export ~/.local/bin/avp open --goal export --repo "$TR" > /dev/null && ~/.local/bin/avp abandon > /dev/null
  RESULT="$X"
fi

log "evaluate"
cd "$RESULT" && CI=true pnpm install --frozen-lockfile --prefer-offline > /dev/null 2>&1
cp "$H/acceptance/acceptance.test.ts" test/acceptance.test.ts
(pnpm typecheck > "$OUT/eval-typecheck.txt" 2>&1; echo "typecheck exit $?" >> "$OUT/eval.txt")
node_modules/.bin/vitest run test/acceptance.test.ts --reporter=json --outputFile="$OUT/acceptance.json" > /dev/null 2>&1
python3.12 -I -c "
import json; j=json.load(open('$OUT/acceptance.json')); print('acceptance', j['numPassedTests'], '/', j['numTotalTests'])" | tee -a "$OUT/eval.txt"
rm -f test/acceptance.test.ts
log "done"
