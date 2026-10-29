#!/bin/bash
# The same-house bench: N Codex agents (GPT-5.6 Luna) build the Facturo spec sized for N, through AVP or
# git, until every section's acceptance tests pass on the line, or BENCH_ROUNDS rounds. After each round
# the line is checked with the pristine tests, and the agents whose sections do not pass there yet go
# again, told which. Both arms get the same instructions, the same signal and the same machine.
# gitci is git with main protected by CI, as on GitHub: a push to main is refused when the typecheck or a
# test fails on the exact commit it would make main.
# ghmq is git on GitHub with a real merge queue (BENCH_GH_REPO, Murmell/avp-bench-mq by default): pull
# requests, CI on self-hosted runners of this machine, merged by the queue only when green.
#   BENCH_FLOW=… BENCH_TAG=… BENCH_ACCEPT=… BENCH_SPENT=… bash run-done.sh <avp|git|gitci|ghmq> <N> [port]
set -u
ARM=$1; N=$2; P=${3:-7361}
# BENCH_AVP_BIN: another avp (the Mistral series runs 0.5.2, whose MCP server takes calls side by side).
AVP=${BENCH_AVP_BIN:-$HOME/.local/bin/avp}; F=$BENCH_FLOW; TAG=$BENCH_TAG; S=$HOME/.ctx-bench/scale
# BENCH_HARNESS=opencode BENCH_MODEL=amazon-bedrock/<model>: OpenCode on Bedrock (the frontier bench) instead of
# Codex on GPT-5.6 Luna; BENCH_RUN names the run folder.
HARNESS=${BENCH_HARNESS:-codex}; OC_MODEL=${BENCH_MODEL:-}
# With Codex: BENCH_CODEX_MODEL and BENCH_CODEX_EFFORT (GPT-5.6 Luna, low, by default).
CODEX_MODEL=${BENCH_CODEX_MODEL:-gpt-5.6-luna}; CODEX_EFFORT=${BENCH_CODEX_EFFORT:-low}
# BENCH_AVP_MCP=1 (AVP arm, OpenCode): the agents reach AVP through its MCP tools, as `avp up` wires them,
# with the instructions of vcs-avpmcp.md.
VCS=$ARM; [ $ARM = avp ] && [ -n "${BENCH_AVP_MCP:-}" ] && VCS=avpmcp
# BENCH_REGIONS="us-east-1 us-west-2": the agents take turns over these Bedrock regions (a model's
# requests per minute are counted per region).
read -r -a REGIONS <<< "${BENCH_REGIONS:-us-east-1}"
# A model served outside Bedrock goes through its maker's API, with the key kept on this machine.
case $OC_MODEL in
  anthropic/*) export ANTHROPIC_API_KEY=$(cat $HOME/.ctx-bench/anthropic-key) ;;
  openai/*) export OPENAI_API_KEY=$(cat $HOME/.ctx-bench/openai-key) ;;
esac
OUT=$HOME/.ctx-bench/done-runs/${BENCH_RUN:-$ARM-n$N}; rm -rf "$OUT"; mkdir -p "$OUT"
MAX_ROUNDS=${BENCH_ROUNDS:-5}; LIMIT_MIN=${BENCH_LIMIT_MIN:-60}
# BENCH_TOTAL_MIN: one window of time for the whole run, rounds included (a round never outlasts it).
TOTAL_S=$(( ${BENCH_TOTAL_MIN:-100000} * 60 ))
# BENCH_CODEX_AUTH=sub: the ChatGPT subscription logged in under codex-sub instead of the API key. Its
# tokens are priced at the API's rates all the same (the chart's cost), and its rate limits are counted.
AUTH_DIR=$HOME/.ctx-bench/codex-home; [ "${BENCH_CODEX_AUTH:-api}" = sub ] && AUTH_DIR=$HOME/.ctx-bench/codex-sub
export CODEX_HOME=$OUT/codex-home; mkdir -p "$CODEX_HOME" && cp $AUTH_DIR/auth.json "$CODEX_HOME/" && echo "${BENCH_CODEX_AUTH:-api}" > "$OUT/auth.txt"
# vitest starts a worker per core by default: N agents testing at once on one machine ask N times the
# cores. Two workers each, in both arms (agents, proofs, the watcher, the checks).
export VITEST_MAX_FORKS=2 VITEST_MAX_THREADS=2
log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$OUT/log"; }

B=$OUT/base
git clone -q "$BENCH_BASE_REPO" "$B" && git -C "$B" fetch -q origin tag $TAG && git -C "$B" checkout -q $TAG || { log "no base $TAG"; exit 1; }
(cd "$B" && CI=true pnpm install --frozen-lockfile --prefer-offline > /dev/null 2>&1)
NM=$B/node_modules

if [ $ARM = avp ]; then
  mkdir -p "$OUT/bin" && ln -sf "$AVP" "$OUT/bin/avp" && export PATH="$OUT/bin:$PATH"
  printf '#!/bin/sh\necho "git is not used here: version control is AVP (avp open, avp land)" >&2\nexit 1\n' > "$OUT/agentbin-git"
  "$AVP" serve --listen 127.0.0.1:$P --data "$OUT/avp-data" > "$OUT/avp-server.log" 2>&1 & echo $! > "$OUT/avp.pid"; sleep 1
  # The server is this run's own, and goes with it on every exit.
  kill -0 "$(cat "$OUT/avp.pid")" 2>/dev/null || { log "avp server did not start: $(tail -1 "$OUT/avp-server.log")"; exit 1; }
  trap 'kill "$(cat "$OUT/avp.pid")" 2>/dev/null' EXIT
  export AVP_SERVER=http://127.0.0.1:$P AVP_REPO=facturo AVP_AGENT=claude-code
  (cd "$B" && AVP_PERSON=bench "$AVP" import-git facturo --history 1 > "$OUT/avp-import.txt" 2>&1) || { cat "$OUT/avp-import.txt"; exit 1; }
  for i in $(seq 1 $N); do
    mkdir -p "$OUT/a$i" && rsync -a --exclude .git --exclude .avp --exclude node_modules "$B/" "$OUT/a$i/" && ln -s "$NM" "$OUT/a$i/node_modules"
    (cd "$OUT/a$i" && "$AVP" attach --repo facturo > /dev/null 2>&1) || { log "attach a$i failed"; exit 1; }
    mkdir -p "$OUT/gitshim$i" && cp "$OUT/agentbin-git" "$OUT/gitshim$i/git" && chmod +x "$OUT/gitshim$i/git"
  done
  linehead() { "$AVP" log --repo facturo --limit 1 --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["position"])'; }
  layout() { rm -rf "$1" && mkdir -p "$1" && (cd "$1" && AVP_AGENT=watch "$AVP" open --repo facturo --goal watch > /dev/null 2>&1 && "$AVP" abandon > /dev/null 2>&1) && ln -s "$NM" "$1/node_modules"; }
  unlayout() { rm -rf "$1"; }
elif [ $ARM = ghmq ]; then
  # The repository back to this base with the queue's CI: the rules off, main forced, every pull request
  # and branch gone, the rules on again. origin.git mirrors GitHub's main for the watcher and the checks.
  GHR=${BENCH_GH_REPO:-Murmell/avp-bench-mq}; GHU=https://github.com/$GHR.git
  export GH_TOKEN=$(cat $HOME/.ctx-bench/gh-token) GH_REPO=$GHR
  RS=$(gh api repos/$GHR/rulesets --jq '.[].id')
  cp $S/ghmq-ci.yml "$B/.github/workflows/ci.yml" && git -C "$B" -c user.name=bench -c user.email=bench@bench commit -qam "CI on pull requests and the merge queue"
  for rs in $RS; do gh api -X PUT repos/$GHR/rulesets/$rs -f enforcement=disabled > /dev/null; done
  for pr in $(gh pr list --state open -L 1000 --json number --jq '.[].number'); do gh pr close $pr > /dev/null 2>&1; done
  git -C "$B" push -q -f $GHU HEAD:refs/heads/main
  git ls-remote --heads $GHU | awk '$2 != "refs/heads/main" {print ":" $2}' | xargs -r -n 50 git -C "$B" push -q $GHU
  for rs in $RS; do gh api -X PUT repos/$GHR/rulesets/$rs -f enforcement=active > /dev/null; done
  date -u +%FT%TZ > "$OUT/gh-start"
  git clone -q --bare $GHU "$OUT/origin.git"
  for i in $(seq 1 $N); do
    git clone -q $GHU "$OUT/a$i" && git -C "$OUT/a$i" config user.name "dev$i" && git -C "$OUT/a$i" config user.email "dev$i@bench"
    ln -s "$NM" "$OUT/a$i/node_modules"
  done
  linehead() { git -C "$OUT/origin.git" fetch -q $GHU +refs/heads/main:refs/heads/main 2>/dev/null; git -C "$OUT/origin.git" rev-parse main; }
  layout() { rm -rf "$1" && git -C "$OUT/origin.git" worktree add -q --detach "$1" "$2" > /dev/null 2>&1 && ln -s "$NM" "$1/node_modules"; }
  unlayout() { git -C "$OUT/origin.git" worktree remove --force "$1" > /dev/null 2>&1; rm -rf "$1"; }
else
  git clone -q --bare "$B" "$OUT/origin.git" && git -C "$OUT/origin.git" branch -q -f main $TAG 2>/dev/null
  git -C "$OUT/origin.git" symbolic-ref HEAD refs/heads/main
  for i in $(seq 1 $N); do
    git clone -q -b main "$OUT/origin.git" "$OUT/a$i" && git -C "$OUT/a$i" config user.name "dev$i" && git -C "$OUT/a$i" config user.email "dev$i@bench"
    ln -s "$NM" "$OUT/a$i/node_modules"
  done
  linehead() { git -C "$OUT/origin.git" rev-parse main; }
  if [ $ARM = gitci ]; then
    # The CI: the pushed commit, typecheck then every test; red, the push is refused with its output.
    # BENCH_CI=cold: as on a CI runner, each run starts from a fresh folder and installs the dependencies
    # (from the package store, as a CI cache would); warm links the installed ones.
    mkdir -p "$OUT/ci"
    cat > "$OUT/origin.git/hooks/pre-receive" <<HOOK
#!/bin/bash
while read old new ref; do
  [ "\$ref" = refs/heads/main ] && [ "\$new" != 0000000000000000000000000000000000000000 ] || continue
  d=\$(mktemp -d "$OUT/ci/run.XXXXXX"); start=\$(date +%s)
  git archive "\$new" | tar -x -C "\$d"
  if [ "${BENCH_CI:-warm}" = cold ]; then (cd "\$d" && CI=true pnpm install --frozen-lockfile --prefer-offline > /dev/null 2>&1); else ln -s "$NM" "\$d/node_modules"; fi
  if (cd "\$d" && node_modules/.bin/tsc --noEmit -p . && VITEST_MAX_FORKS=2 VITEST_MAX_THREADS=2 node_modules/.bin/vitest run) > "\$d.log" 2>&1; then v=green; else v=red; fi
  echo "\$start \$(( \$(date +%s) - start )) \$v \$new" >> "$OUT/ci/runs.txt"
  if [ \$v = red ]; then echo "CI failed on this push: main is protected, the push is refused. The checks said:" >&2; tail -40 "\$d.log" >&2; rm -rf "\$d" "\$d.log"; exit 1; fi
  rm -rf "\$d" "\$d.log"
done
HOOK
    chmod +x "$OUT/origin.git/hooks/pre-receive"
  fi
  layout() { rm -rf "$1" && git -C "$OUT/origin.git" worktree add -q --detach "$1" "$2" > /dev/null 2>&1 && ln -s "$NM" "$1/node_modules"; }
  unlayout() { git -C "$OUT/origin.git" worktree remove --force "$1" > /dev/null 2>&1; rm -rf "$1"; }
fi

# OpenCode's settings: AVP's MCP server when asked, and the same model for its small tasks (titles).
if [ $HARNESS = opencode ]; then
  python3 -c "import json,sys; c={'\$schema': 'https://opencode.ai/config.json', 'small_model': sys.argv[1]}; c.update({'mcp': {'avp': {'type': 'local', 'command': ['avp', 'mcp'], 'enabled': True}}} if sys.argv[2] == 'avpmcp' else {}); json.dump(c, open(sys.argv[3], 'w'), indent=1)" "$OC_MODEL" "$VCS" "$OUT/opencode.json"
fi

# Every new head of the line, green or red: typecheck and the line's own tests.
watch() {
  local last="" n=0
  while [ ! -f "$OUT/stop" ]; do
    local h; h=$(linehead 2>/dev/null)
    if [ -n "$h" ] && [ "$h" != "$last" ]; then
      last=$h; n=$((n + 1)); local d=$OUT/watch/$n
      if layout "$d" "$h"; then
        (cd "$d" && node_modules/.bin/tsc --noEmit -p . > /dev/null 2>&1 && node_modules/.bin/vitest run > /dev/null 2>&1) && v=green || v=red
        echo "$(date -u +%s) $h $v" >> "$OUT/states.txt"; unlayout "$d"
      fi
    fi
    sleep 3
  done
}

# One round: these agents, each with its queue (and, after the first, the sections still not done).
AGENT_S=0
round_go() {
  local r=$1; shift; local PIDS=() start; start=$(date +%s)
  [ $HARNESS = opencode ] && . $S/bedrock-env.sh
  for i in "$@"; do
    local PROMPT path
    PROMPT="$(cat "$F/common.md" "$S/vcs-$VCS.md" "$F/queue-$i.md")"
    [ $r -gt 1 ] && PROMPT="$PROMPT

This is not your first pass on this queue: you worked on it before, in this folder. On the line, these
sections of your queue are not done yet (their acceptance tests do not pass there):
$(grep "^$i:" "$OUT/todo-r$((r - 1)).txt" | cut -d: -f2-). Finish them as above and deliver them. If the
line itself is broken (its typecheck or its tests fail), fix what breaks it first."
    [ $ARM = avp ] && path="$OUT/gitshim$i:$PATH" || path=$PATH
    if [ $HARNESS = opencode ]; then
      # Its own data folder per agent (sessions), the models catalog shared; Bedrock through the role.
      ( cd "$OUT/a$i" && mkdir -p "$OUT/tmp$i" "$OUT/oc/$i" && PATH="$path" TMPDIR=$OUT/tmp$i AVP_PERSON=dev$i AWS_REGION=${REGIONS[$((i % ${#REGIONS[@]}))]} XDG_DATA_HOME=$OUT/oc/$i OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_CONFIG=$OUT/opencode.json opencode run --format json --auto -m "$OC_MODEL" "$PROMPT" < /dev/null > "$OUT/agent$i-r$r.jsonl" 2> "$OUT/agent$i-r$r.err"; echo $? > "$OUT/agent$i-r$r.exit" ) &
    else
      ( cd "$OUT/a$i" && mkdir -p "$OUT/tmp$i" && PATH="$path" TMPDIR=$OUT/tmp$i AVP_PERSON=dev$i codex exec --json --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -m $CODEX_MODEL -c model_reasoning_effort=$CODEX_EFFORT "$PROMPT" < /dev/null > "$OUT/agent$i-r$r.jsonl" 2> "$OUT/agent$i-r$r.err"; echo $? > "$OUT/agent$i-r$r.exit" ) &
    fi
    PIDS+=($!)
  done
  local left=$(( TOTAL_S - ($(date +%s) - RUN_START) )); local cap=$(( LIMIT_MIN * 60 < left ? LIMIT_MIN * 60 : left ))
  ( sleep $cap; for p in "${PIDS[@]}"; do pkill -P $p; done; echo "round $r stopped at its time limit (${cap} s)" >> "$OUT/log" ) & local WD=$!
  # The spend brake: past $BENCH_BRAKE in all (this run, live from the Codex sessions, plus $BENCH_SPENT
  # before it), the run stops. A safety net under the $50 cap, not a planned cut.
  ( while sleep 20; do if [ $HARNESS = opencode ]; then u=$(python3 -I $S/live-usd.py --opencode "$OUT"); else u=$(python3 -I $S/live-usd.py "$CODEX_HOME/sessions" "$CODEX_MODEL"); fi; echo "$u" > "$OUT/live-usd"
    python3 -c "import sys; sys.exit(0 if $u + ${BENCH_SPENT:-0} < ${BENCH_BRAKE:-47} else 1)" || { echo "stopped at the budget brake (\$$u this run)" >> "$OUT/log"; touch "$OUT/braked"; for p in "${PIDS[@]}"; do pkill -P $p; done; break; }; done ) & local BR=$!
  wait "${PIDS[@]}"; kill $WD $BR 2>/dev/null
  local took=$(( $(date +%s) - start )); AGENT_S=$((AGENT_S + took))
  log "round $r: $# agents done in $((took / 60)) min $((took % 60)) s"
}

# The pristine tests on the line's head; todo-r<round>.txt: "<queue>:<sections not done>" per line.
evaluate() {
  local r=$1 d=$OUT/eval-r$1
  layout "$d" "$(linehead)"
  cp "$BENCH_ACCEPT" "$d/test/zz-bench-acceptance.test.ts"
  (cd "$d" && node_modules/.bin/vitest run test/zz-bench-acceptance.test.ts --reporter=json --outputFile="$OUT/acceptance-r$r.json" > /dev/null 2>&1)
  cp "$OUT/acceptance-r$r.json" "$OUT/acceptance.json" 2>/dev/null
  unlayout "$d"
  python3 -I $S/todo.py "$OUT/acceptance-r$r.json" "$F/sections.json" > "$OUT/todo-r$r.txt"
}

[ $HARNESS = opencode ] && echo "$OC_MODEL" > "$OUT/model.txt" || echo "$CODEX_MODEL $CODEX_EFFORT" > "$OUT/model.txt"
mkdir -p "$OUT/watch"; watch & WATCH=$!
log "start: $ARM, $N agents, $(python3 -c "import json; print(len(json.load(open('$F/sections.json'))))") sections, up to $MAX_ROUNDS rounds"
RUN_START=$(date +%s); r=1; round_go 1 $(seq 1 $N)
while :; do
  evaluate $r
  log "after round $r: $(wc -l < "$OUT/todo-r$r.txt") queues with sections not done"
  if [ ! -s "$OUT/todo-r$r.txt" ]; then log "done: every section passes"; break; fi
  if [ $r -ge $MAX_ROUNDS ] || [ -f "$OUT/braked" ]; then log "not done after $r rounds"; break; fi
  if [ $(( $(date +%s) - RUN_START )) -ge $TOTAL_S ]; then log "not done at the end of the $((TOTAL_S / 60)) min window"; break; fi
  r=$((r + 1)); round_go $r $(cut -d: -f1 "$OUT/todo-r$((r - 1)).txt")
done
log "finished: $((AGENT_S / 60)) min $((AGENT_S % 60)) s of agent time over $r rounds"
sleep 8; touch "$OUT/stop"; wait $WATCH
if [ $ARM = avp ]; then "$AVP" log --repo facturo --limit 500 > "$OUT/line.txt" 2>&1; else linehead > /dev/null; git -C "$OUT/origin.git" log --oneline main > "$OUT/line.txt"; fi
# The queue's record: every CI run (pull request or merge group) and every pull request with its fate.
if [ $ARM = ghmq ]; then
  gh run list -L 1000 --created ">=$(cat "$OUT/gh-start")" --json databaseId,event,conclusion,createdAt,updatedAt > "$OUT/gh-runs.json" 2>/dev/null
  gh pr list --state all -L 1000 --search "created:>=$(cat "$OUT/gh-start")" --json number,state,createdAt,mergedAt,headRefName > "$OUT/gh-prs.json" 2>/dev/null
fi
# Rate limits met (a subscription has some): a run that met them is told apart in the results.
# (The models list refreshed by 64 agents at once gets 429s that block nothing: not counted.)
cat "$OUT"/agent*-r*.err 2>/dev/null | grep -i -E "usage limit|rate limit|too many requests" | grep -v -c "available models" > "$OUT/ratelimits"
# The subscription's login may have been refreshed during the run: the next run starts from that one.
[ "$AUTH_DIR" != "$HOME/.ctx-bench/codex-home" ] && cp "$CODEX_HOME/auth.json" "$AUTH_DIR/auth.json" 2>/dev/null
# The base and the folders are what weighs; the transcripts, the verdicts and the numbers stay.
rm -rf "$B" "$OUT/watch" "$OUT"/a[0-9]* "$OUT"/tmp[0-9]* "$OUT"/gitshim[0-9]* "$CODEX_HOME/auth.json" "$OUT/avp-data/work"
