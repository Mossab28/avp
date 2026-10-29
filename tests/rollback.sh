#!/bin/bash
# `avp rollback <env>`: the environment goes back to what it served before, through its deploy, and
# the webhook of [notify] hears of it; its URL, in the server's environment, shows nowhere.
# Usage: AVP=target/release/avp bash tests/rollback.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/rollback.sh <scratch dir>}/rb.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7462 AVP_PERSON=ada
# A webhook on a free port: each post it gets, a line of $W/posts.
python3 -I -c '
import http.server, sys
out = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        with open(out, "ab") as f: f.write(body + b"\n")
        self.send_response(200); self.send_header("content-length", "0"); self.end_headers()
    def log_message(self, *a): pass
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(out + ".port", "w").write(str(s.server_port))
s.serve_forever()
' "$W/posts" & HP=$!
for _ in $(seq 50); do [ -s "$W/posts.port" ] && break; sleep 0.1; done
AVP_NOTIFY_HOOK="http://127.0.0.1:$(cat "$W/posts.port")/hooks/tok3n-s3cret" $T serve --listen 127.0.0.1:7462 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP $HP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
heard(){ for _ in $(seq 100); do grep -q "$1" "$W/posts" 2>/dev/null && return 0; sleep 0.1; done; return 1; }
mkdir app && cd app || exit 1
printf 'v0\n' > app.txt
cat > avp.toml <<EOF
[envs.staging]
deploy = { run = "cp app.txt '$W/served'" }
[notify]
webhooks = ["\$AVP_NOTIFY_HOOK"]
EOF
$T init rb >/dev/null
out=$($T rollback staging 2>&1)
ok "an environment that serves nothing has nothing to roll back" '[[ "$out" == *"staging serves nothing yet"* ]]'
$T promote staging --force >/dev/null
mkdir ../t && $T open -C ../t --repo rb --goal "v1" >/dev/null && printf 'v1\n' > ../t/app.txt && $T -C ../t land >/dev/null
$T promote staging 1 --force >/dev/null
out=$($T rollback staging 2>&1)
ok "rollback deploys what the environment served before" '[ "$out" = "staging rolled back from position 1: it serves position 0" ] && [ "$(cat "$W/served")" = v0 ]'
ok "the webhook hears of it" 'heard "staging is rolled back to position 0 (imported), from position 1."'
ok "--json answers the environment, rolled back again where it was" '[ "$(python3 -c "import json,sys; print(json.loads(sys.argv[1])[\"position\"])" "$($T --json rollback staging)")" = 1 ] && [ "$(cat "$W/served")" = v1 ]'
curl -s "$AVP_SERVER/repos/rb/events?limit=1000" > "$W/events.json"
rolled=$(python3 -I -c 'import json, sys; print(" ".join("%s:%s" % (e["kind"], e["data"]["from"]) for e in json.load(open(sys.argv[1])) if e["data"].get("rollback")))' "$W/events.json")
ok "the events record each rollback and where it came from" '[ "$rolled" = "env.promoted:1 env.moved:1 env.promoted:0 env.moved:0" ]'
ok "the webhook's URL is in no event and no log" '! grep -q tok3n "$W/events.json" "$W/server.log"'
