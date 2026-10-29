#!/bin/bash
# The end-to-end tests CI runs (deploy/Dockerfile, stage test): each prints PASS/FAIL per check;
# any FAIL, or a test that printed no PASS, fails the build. Usage: AVP=<binary> bash tests/ci.sh
# They run AVP Cloud's features too (workers, approvals, environments...): a free binary, built
# without `--features cloud`, is built again with it from this checkout.
set -u
T=${AVP:?AVP=<avp binary>}
D=$(cd "$(dirname "$0")" && pwd)
if ! "$T" --version 2>/dev/null | grep -q "AVP Cloud"; then
  (cd "$D/.." && cargo build --profile proof --locked --features cloud) >&2 || exit 1
  T=${CARGO_TARGET_DIR:-$D/../target}/proof/avp
fi
# Each test sets the server it talks to; nothing of the caller's AVP_* (an agent's AVP_REPO) may
# make a test pass that fails on the server.
for v in $(compgen -e | grep -E '^(AVP|TRAME)_'); do unset "$v"; done
W=$(mktemp -d)
# A run leaves about 80 MB here: dozens filled a server's disk in a day (28/10).
trap 'rm -rf "$W"' EXIT
# What AVP keeps on the machine (the ready copies new views are cloned from) stays in the scratch dir.
export AVP_HOME=$W/home
bad=0
run() {
  local name=$1; shift
  local out; out=$("$@" 2>&1)
  local pass fail
  pass=$(grep -c '^PASS' <<<"$out"); fail=$(grep -c '^FAIL' <<<"$out")
  echo "$name: $pass PASS, $fail FAIL"
  if [ "$fail" != 0 ] || [ "$pass" = 0 ]; then echo "$out" | tail -30; bad=1; fi
}
run open-in-checkout env AVP="$T" bash "$D/open-in-checkout.sh" "$W"
run git-bridge env AVP="$T" bash "$D/git-bridge-roundtrip.sh" "$W"
run git-inbound env AVP="$T" bash "$D/git-inbound.sh" "$W"
run views python3 -I "$D/views-e2e.py" "$T" "$W"
run carry python3 -I "$D/open-carry.py" "$T" "$W"
run wait env AVP="$T" bash "$D/wait.sh" "$W"
run open-hints python3 -I "$D/open-hints.py" "$T" "$W"
run land-queue env AVP="$T" bash "$D/land-queue.sh" "$W"
run catch-up env AVP="$T" bash "$D/catch-up.sh" "$W"
run stacked env AVP="$T" bash "$D/stacked.sh" "$W"
run land-together env AVP="$T" bash "$D/land-together.sh" "$W"
run token env AVP="$T" bash "$D/token.sh" "$W"
run accounts env AVP="$T" bash "$D/accounts.sh" "$W"
run review env AVP="$T" bash "$D/review.sh" "$W"
run line-checks env AVP="$T" bash "$D/line-checks.sh" "$W"
run proof-env env AVP="$T" bash "$D/proof-env.sh" "$W"
run history env AVP="$T" bash "$D/history.sh" "$W"
run context env AVP="$T" bash "$D/context.sh" "$W"
run deploy-run env AVP="$T" bash "$D/deploy-run.sh" "$W"
run cli-ergonomics env AVP="$T" bash "$D/cli-ergonomics.sh" "$W"
run deploy-secrets env AVP="$T" bash "$D/deploy-secrets.sh" "$W"
run flaky-full env AVP="$T" bash "$D/flaky-full.sh" "$W"
run auto-revert env AVP="$T" bash "$D/auto-revert.sh" "$W"
run revert-toml env AVP="$T" bash "$D/revert-toml.sh" "$W"
run deploy-restart env AVP="$T" bash "$D/deploy-restart.sh" "$W"
run repo-scope env AVP="$T" bash "$D/repo-scope.sh" "$W"
run drain env AVP="$T" bash "$D/drain.sh" "$W"
run stats env AVP="$T" bash "$D/stats.sh" "$W"
run services env AVP="$T" bash "$D/services.sh" "$W"
run secrets env AVP="$T" bash "$D/secrets.sh" "$W"
run step-logs env AVP="$T" bash "$D/step-logs.sh" "$W"
run rollback env AVP="$T" bash "$D/rollback.sh" "$W"
run up-checks env AVP="$T" bash "$D/up-checks.sh" "$W"
run instant-views env AVP="$T" bash "$D/instant-views.sh" "$W"
run import-ci env AVP="$T" bash "$D/import-ci.sh" "$W"
run offline env AVP="$T" bash "$D/offline.sh" "$W"
run outbox env AVP="$T" bash "$D/outbox.sh" "$W"
run everyday env AVP="$T" bash "$D/everyday.sh" "$W"
run migrate env AVP="$T" bash "$D/migrate.sh" "$W"
run history-verbs env AVP="$T" bash "$D/history-verbs.sh" "$W"
run subagents python3 -I "$D/subagents.py" "$T" "$W"
exit $bad
