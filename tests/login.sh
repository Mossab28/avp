#!/bin/bash
# `avp login`, end to end: the CLI shows a code, a signed-in person approves it on the server, the
# CLI keeps the token in ~/.config/avp/credentials (0600) and every command then talks to that
# server as them, with no AVP_SERVER nor AVP_TOKEN; `avp up` creates the repo there, in their
# organization, and `avp clone <org>/<repo>` lays it out elsewhere; `avp logout`
# revokes the token. The server serves install.sh with its own address.
# Usage: AVP=target/release/avp bash tests/login.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/login.sh <scratch dir>}/lg.XXXX") && cd "$W" || exit 1
for v in $(compgen -e | grep -E '^AVP_'); do unset "$v"; done
S=http://127.0.0.1:7603
export XDG_CONFIG_HOME=$W/config AVP_HOME=$W/home AVP_VIEWS=$W/views
AVP_CLOUD=1 $T serve --listen 127.0.0.1:7603 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null; kill $LP 2>/dev/null' EXIT
LP=
for _ in $(seq 50); do curl -s $S/ >/dev/null && break; sleep 0.1; done
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
tok(){ grep -o 'avp_[A-Za-z0-9_-]\{40,\}' | head -1; }
A=$($T --server $S user add ada 2>&1 | tok)

$T login $S --no-browser >"$W/login.out" 2>"$W/login.err" & LP=$!
for _ in $(seq 50); do grep -q 'code [A-Z]\{4\}-[A-Z]\{4\}' "$W/login.err" && break; sleep 0.1; done
CODE=$(grep -o '[BCDFGHJKLMNPQRSTVWXZ]\{4\}-[BCDFGHJKLMNPQRSTVWXZ]\{4\}' "$W/login.err" | head -1)
ok "the CLI shows a code and the page to approve it on" '[ -n "$CODE" ] && grep -q "$S/login/device?code=$CODE" "$W/login.err"'
ok "the page is served without a token" '[ "$(curl -s -o /dev/null -w "%{http_code}" $S/login/device)" = 200 ]'
ok "approving takes a signed-in person" '[ "$(curl -s -o /dev/null -w "%{http_code}" -H "x-forwarded-for: 10.0.0.1" -H "content-type: application/json" -d "{\"code\":\"$CODE\"}" $S/login/device/approve)" = 401 ]'
curl -s -H "authorization: Bearer $A" -H "content-type: application/json" -d "{\"code\":\"$CODE\"}" $S/login/device/approve >/dev/null
wait $LP; LP=
ok "approved, the CLI is logged in" 'grep -q "logged in to $S as ada" "$W/login.out"'
ok "the token is kept for its owner alone" '[ "$(stat -c %a "$XDG_CONFIG_HOME/avp/credentials" 2>/dev/null || stat -f %Lp "$XDG_CONFIG_HOME/avp/credentials")" = 600 ]'
ok "every command talks to the server logged in to, as its person" '[[ "$($T whoami)" == "ada, admin of this server on $S (logged in"* ]]'
ok "a code is used once" '! curl -sf -H "authorization: Bearer $A" -H "content-type: application/json" -d "{\"code\":\"$CODE\"}" $S/login/device/approve'

$T org create acme >/dev/null 2>&1
git init -q -b main shop && (cd shop && printf 'a\n' > a.txt && git add a.txt && git -c user.email=a@b -c user.name=a commit -qm one)
(cd shop && HOME=$W $T up --no-checks --no-push >"$W/up.out" 2>&1)
ok "avp up creates the repo on that server, as the person logged in, in their organization" 'grep -q "as ada" "$W/up.out" && grep -q "shop, imported: .*, in acme" "$W/up.out" && $T --json tasks --repo shop >/dev/null'
(cd shop && HOME=$W $T down >/dev/null 2>&1)
(HOME=$W $T clone acme/shop "$W/elsewhere" >"$W/clone.out" 2>&1)
ok "avp clone <org>/<repo> lays the head out from the server logged in to" '[ -f "$W/elsewhere/a.txt" ] && ! HOME=$W $T clone other/shop "$W/nowhere"'

DT=$(grep -o 'avp_[A-Za-z0-9_-]\{40,\}' "$XDG_CONFIG_HOME/avp/credentials" | head -1)
ok "avp logout forgets the login and revokes its token" '$T logout | grep -q "logged out of $S (ada), its token revoked" && ! grep -q "$S" "$XDG_CONFIG_HOME/avp/credentials" && ! AVP_TOKEN=$DT $T --server $S whoami'
ok "/install carries the server it was asked of" 'curl -s $S/install | grep -q "^cloud=\${AVP_CLOUD_URL:-$S}"'
ok "--token logs in with no browser, checked first" '! $T login $S --token avp_nothing && echo "$A" | $T login $S --token - | grep -q "as ada"'
$T login $S --no-browser >/dev/null 2>"$W/again.err" & LP=$!
for _ in $(seq 50); do grep -q 'code [A-Z]' "$W/again.err" && break; sleep 0.1; done
C2=$(grep -o '[BCDFGHJKLMNPQRSTVWXZ]\{4\}-[BCDFGHJKLMNPQRSTVWXZ]\{4\}' "$W/again.err" | head -1)
curl -s -H "authorization: Bearer $A" -H "content-type: application/json" -d "{\"code\":\"$C2\",\"deny\":true}" $S/login/device/approve >/dev/null
wait $LP; R=$?; LP=
ok "a denied login fails and says who said no" '[ $R != 0 ] && grep -q "ada said no" "$W/again.err"'
