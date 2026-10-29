#!/bin/bash
# The secret goes in AVP_TOKEN, sent as a header: AVP_SERVER stays a plain address. An address that
# carries name:password still works.
# Usage: AVP=target/release/avp bash tests/token.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/token.sh <scratch dir>}/tk.XXXX") && cd "$W" || exit 1
AVP_TOKEN=test-token-7196 $T serve --listen 127.0.0.1:7196 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'a\n' > src/a.txt
ok "without the token the server refuses" '! env -u AVP_TOKEN AVP_SERVER=http://127.0.0.1:7196 $T init tk'
ok "with AVP_TOKEN and a plain address it answers" 'AVP_SERVER=http://127.0.0.1:7196 AVP_TOKEN=test-token-7196 $T init tk'
mkdir v && AVP_SERVER=http://127.0.0.1:7196 AVP_TOKEN=test-token-7196 AVP_USER=ada $T open -C v --repo tk --goal "who" >/dev/null 2>&1
ok "the task is the person's the token came with" '[[ "$(AVP_SERVER=http://127.0.0.1:7196 AVP_TOKEN=test-token-7196 $T -C v status --json)" == *ada* ]]'
ok "a wrong token is refused" '! AVP_SERVER=http://127.0.0.1:7196 AVP_TOKEN=nope $T -C v status'
ok "an address with name:password still works" 'env -u AVP_TOKEN AVP_SERVER=http://bob:test-token-7196@127.0.0.1:7196 $T -C v status'
ok "the token is never printed" '! AVP_SERVER=http://127.0.0.1:7196 AVP_TOKEN=test-token-7196 $T --help | grep -q s3cret'
