#!/bin/bash
# People, tokens and roles from the CLI: the first `avp user add` turns accounts on and prints a
# token, shown once; a person's token says who they are, whatever AVP_PERSON says, and their role
# what they may do. A request with no token from the server's own machine keeps every right.
# Usage: AVP=target/release/avp bash tests/accounts.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/accounts.sh <scratch dir>}/ac.XXXX") && cd "$W" || exit 1
unset AVP_TOKEN AVP_USER
export AVP_SERVER=http://127.0.0.1:7482
$T serve --listen 127.0.0.1:7482 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
tok(){ grep -o 'avp_[A-Za-z0-9_-]\{40,\}' | head -1; }
mkdir -p p/src && printf 'a\n' > p/src/a.txt
(cd p && $T init shop >/dev/null 2>&1)
A=$($T user add ada --email ada@example.com 2>&1 | tok)
ok "the first person is added with a token shown once" '[ -n "$A" ]'
ok "and is an admin of the server" '[[ "$(AVP_TOKEN=$A $T whoami)" == "ada, admin of this server"* ]]'
B=$(AVP_TOKEN=$A $T user add bob 2>&1 | tok)
ok "an admin adds the next person" '[ -n "$B" ]'
ok "bob may not add people" '! AVP_TOKEN=$B $T user add eve'
AVP_TOKEN=$A $T user role bob read --repo shop >/dev/null 2>&1
mkdir v w
ok "read does not open a task" '! AVP_TOKEN=$B $T open -C w --repo shop --goal "no"'
AVP_TOKEN=$A $T user role bob write --repo shop >/dev/null 2>&1
AVP_TOKEN=$B AVP_PERSON=mallory $T open -C v --repo shop --goal "who" >/dev/null 2>&1
ok "write opens one, and it is the token's person's" '[[ "$(AVP_TOKEN=$B $T -C v status --json)" == *\"bob\"* ]] && [[ "$(AVP_TOKEN=$B $T -C v status --json)" != *mallory* ]]'
L=$(AVP_TOKEN=$B $T token create --name laptop --expires 30d 2>&1 | tok)
ok "a person makes a token of their own" '[[ "$(AVP_TOKEN=$L $T whoami)" == bob* ]]'
ID=$(AVP_TOKEN=$L $T --json token list | grep -o 'tok_[a-z0-9]*' | head -1)
ok "and revokes it" 'AVP_TOKEN=$B $T token revoke $ID'
ok "a revoked token is refused" '! AVP_TOKEN=$L $T -C v status'
ok "the log says who changed what" 'AVP_TOKEN=$A $T user log | grep -q "bob write on shop (was read), by ada"'
ok "the server's own machine keeps every right" '$T user list | grep -q "^bob .*shop write"'
