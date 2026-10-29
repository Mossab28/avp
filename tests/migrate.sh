#!/bin/bash
# `avp migrate` on a checkout whose CI has a workflow, a service and a secret: it starts the server,
# imports the repo, writes avp.toml from the CI and proves it on this machine, red with what to
# change (a service this machine cannot run); run again after each fix it starts from where it is
# (the avp.toml there kept, the secret to set named with its command), green waits for a person's
# word, --yes is that word and lands it; the MCP tool returns the last report; main never moves.
# Usage: AVP=target/proof/avp bash tests/migrate.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/migrate.sh <scratch dir>}/mg.XXXX") && cd "$W" || exit 1
# Nothing of the caller's AVP_* (an agent's AVP_REPO) reaches the commands below.
for v in $(compgen -e | grep -E '^(AVP|TRAME)_'); do unset "$v"; done
# Nothing answers on its port yet: avp migrate starts the server itself, its data in AVP_HOME.
export AVP_SERVER=http://127.0.0.1:7472 AVP_HOME=$W/home AVP_PERSON=ada AVP_VIEWS=$W/views
# No coding agent nor container tool of this machine: docker and podman answer nothing.
mkdir -p nobin fresh && printf '#!/bin/sh\nexit 1\n' > nobin/docker && cp nobin/docker nobin/podman && chmod +x nobin/*
run(){ HOME=$W/fresh PATH=$W/nobin:/usr/bin:/bin "$T" -C "$W/shop" "$@"; }
trap 'run down >/dev/null 2>&1' EXIT
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
field(){ python3 -I -c 'import json, sys; r = json.load(open(sys.argv[1])); print(eval(sys.argv[2], {"r": r}))' "$@"; }

mkdir -p shop/.github/workflows shop/scripts && cd shop || exit 1
cat > .github/workflows/ci.yml <<'EOF'
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      db:
        image: postgres:16
        ports: ["5432:5432"]
        env:
          POSTGRES_PASSWORD: avp
    env:
      STRIPE_KEY: ${{ secrets.STRIPE_KEY }}
    steps:
      - uses: actions/checkout@v4
      - run: sh scripts/lint.sh
      - run: sh scripts/test.sh
EOF
printf 'echo lint-ok\n' > scripts/lint.sh && printf 'echo test-ok\n' > scripts/test.sh
git init -q -b main && git add -A && git -c user.name=ada -c user.email=ada@example.com commit -qm "the shop" && MAIN=$(git rev-parse main)
cd "$W" || exit 1

run migrate --json > one.json; one=$?
ok "it starts the server and imports the repo" '[[ "$(field one.json "r[\"setup\"][0][\"said\"]")" == *started* ]] && [[ "$(field one.json "r[\"setup\"][1][\"said\"]")" == "shop, imported"* ]]'
ok "avp.toml written from the CI, in the checkout" '[ "$(field one.json "r[\"checks\"][\"source\"]")" = ci ] && grep -q "check.services" shop/avp.toml'
ok "the mirror writes avp/line, never main" '[[ "$(field one.json "r[\"setup\"][3][\"said\"]")" == *avp/line* ]]'
ok "proven here: the service this machine cannot run is red, with what to change" '[ "$(field one.json "r[\"proof\"][\"verdict\"]")" = red ] && [[ "$(field one.json "[x[\"todo\"] for x in r[\"proof\"][\"runs\"] if x[\"verdict\"] == \"red\"][0]")" == *"neither docker nor podman"* ]]'
ok "the whole suite ran too" '[ "$(field one.json "len([x for x in r[\"proof\"][\"runs\"] if x[\"part\"] == \"full\"])")" -ge 1 ]'
ok "red: not landed, and the exit says so" '[ "$one" = 2 ] && [ "$(field one.json "r[\"checks\"][\"state\"]")" = red ]'
ok "the plan goes on as docs/MIGRATE.md does, with the commands" '[ "$(field one.json "\" \".join(dict.fromkeys(t[\"step\"] for t in r[\"plan\"]))")" = "2 3.1 3.2 3.3 4 5" ] && [[ "$(field one.json "r[\"plan\"][-1][\"command\"]")" == "GH_TOKEN=... avp stats --github "* ]]'

# The fix, by the person: a service without containers, the step that needs the secret named.
cat > shop/avp.toml <<'EOF'
[check]
[[check.services]]
name = "db"
run = "exec python3 -I -m http.server {port} --bind 127.0.0.1 --directory {data}"
env = { DB_URL = "http://127.0.0.1:{port}" }

[[check.steps]]
name = "tests"
run = "curl -sf \"$DB_URL/\" >/dev/null && test -n \"$STRIPE_KEY\" && sh scripts/test.sh"
secrets = ["STRIPE_KEY"]

[[check.full_shards]]
name = "test"
run = "curl -sf \"$DB_URL/\" >/dev/null && sh scripts/lint.sh && sh scripts/test.sh"
EOF
run migrate --json > two.json
ok "run again, the avp.toml there is kept, and what the CI says otherwise is listed" '[ "$(field one.json "r[\"repo_id\"]")" = "$(field two.json "r[\"repo_id\"]")" ] && [ "$(field two.json "r[\"checks\"][\"source\"]")" = checkout ] && [ "$(field two.json "len(r[\"checks\"][\"differs\"])")" -ge 1 ]'
ok "the secret not set: red, with the line that sets it" '[ "$(field two.json "[x[\"command_to_run\"] for x in r[\"proof\"][\"runs\"] if x[\"verdict\"] == \"red\"][0]")" = "avp secret set STRIPE_KEY < stripe_key.txt" ]'
ok "the service is up for the whole suite: green" '[ "$(field two.json "[x[\"verdict\"] for x in r[\"proof\"][\"runs\"] if x[\"part\"] == \"full\"][0]")" = green ]'

printf %s sk_test_51abcdef | run secret set STRIPE_KEY >/dev/null
run migrate --json > three.json; three=$?
ok "set, green on this machine with the server's secret" '[ "$three" = 0 ] && [ "$(field three.json "r[\"proof\"][\"verdict\"]")" = green ]'
ok "green, it waits for a person's word" '[ "$(field three.json "r[\"checks\"][\"state\"]")" = approval ] && [[ "$(field three.json "r[\"next\"]")" == *"avp migrate --yes"* ]]'

run migrate --yes > four.txt
ok "--yes is that word: landed, the proof of the same tree not run again" 'grep -q "landed at position 1, approved by ada" four.txt && [ "$(field "$AVP_HOME/migrate/$(ls "$AVP_HOME/migrate")" "r[\"proof\"][\"reused\"]")" = True ]'
ok "it is on the line, its task the one the run before opened" '[ "$(run log --json | python3 -I -c "import json, sys; print(json.load(sys.stdin)[0][\"task\"])")" = "$(field three.json "r[\"checks\"][\"task\"]")" ]'

run migrate --json > five.json
ok "once more, nothing new: landed, no task opened" '[ "$(field five.json "r[\"checks\"][\"state\"]")" = landed ] && [ "$(run log --json | python3 -I -c "import json, sys; print(len(json.load(sys.stdin)))")" = 2 ]'

mcp=$(cd shop && HOME=$W/fresh AVP_MCP_LOG= python3 -I -c '
import json, subprocess, sys
p = subprocess.Popen([sys.argv[1], "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def call(n, method, params):
    p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n"); p.stdin.flush()
    while True:
        m = json.loads(p.stdout.readline())
        if m.get("id") == n: return m
call(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})
print(call(2, "tools/call", {"name": "avp_migrate_report", "arguments": {}})["result"]["content"][0]["text"])
p.stdin.close(); p.wait(timeout=20)' "$T" 2>&1)
ok "avp_migrate_report gives an agent the last report" '[[ "$mcp" == "avp migrate: shop"* && "$mcp" == *"on the line since position 1"* && "$mcp" == *"3.1"* ]]'
ok "main never moved" '[ "$(git -C shop rev-parse main)" = "$MAIN" ]'
