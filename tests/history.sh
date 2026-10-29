#!/bin/bash
# A file's history read from the line instead of GitHub: `avp log --path` lists the positions that
# changed it, `avp blame` gives each of its lines the position that last changed it, both with the
# person, through the server's /history routes that AVP Live reads too.
# Usage: AVP=target/release/avp bash tests/history.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/history.sh <scratch dir>}/hi.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7404
$T serve --listen 127.0.0.1:7404 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'one\ntwo\nthree\n' > src/a.txt && $T init hi >/dev/null
mkdir a && AVP_PERSON=ada $T open -C a --repo hi --goal "say TWO" >/dev/null
printf 'one\nTWO\nthree\n' > a/src/a.txt && $T -C a land >/dev/null
mkdir b && AVP_PERSON=bob $T open -C b --repo hi --goal "count to four" >/dev/null
mkdir b/docs && printf 'hello\n' > "b/docs/my notes & more.md"
printf 'one\nTWO\nthree\nfour\n' > b/src/a.txt && $T -C b land >/dev/null

log=$($T -C b log --path src/a.txt)
ok "log --path lists the positions that changed the file, newest first" '[ "$(awk "{print \$1}" <<<"$log" | tr "\n" " ")" = "2 1 0 " ]'
ok "each with its time, person, change and goal" '[[ "$(sed -n 1p <<<"$log")" == *" UTC  bob "*modified*"count to four" ]] && [[ "$(sed -n 2p <<<"$log")" == *ada*"say TWO" ]] && [[ "$(sed -n 3p <<<"$log")" == *imported*added* ]]'
ok "--limit and --from page through it" '[ "$($T -C b log --path src/a.txt --limit 1 | head -1 | awk "{print \$1}")" = 2 ] && [ "$($T -C b log --path src/a.txt --from 1 | awk "{print \$1}" | tr "\n" " ")" = "1 0 " ]'
ok "log without --path is still the line" '[[ "$($T -C b log)" == *"count to four"* ]]'

blame=$($T -C b blame src/a.txt)
ok "blame gives each line the position that last changed it and its person" '[ "$(awk "{print \$1\":\"\$2\":\"\$3\":\"\$4}" <<<"$blame" | tr "\n" " ")" = "0:imported:1:one 1:ada:2:TWO 0:imported:3:three 2:bob:4:four " ]'
ok "blame --at reads the file as it was" '[ "$($T -C b blame src/a.txt --at 1 | wc -l | tr -d " ")" = 3 ]'
ok "a path with spaces and & reaches the server whole" '[[ "$($T -C b blame "docs/my notes & more.md")" == *"2  bob  1  hello"* ]] && [[ "$($T -C b log --path "docs/my notes & more.md")" == *added*"count to four"* ]]'
ok "a file that is not there is said so" '[[ "$($T -C b blame src/nope.txt 2>&1)" == *"no src/nope.txt at position 2"* ]]'
json=$($T -C b blame src/a.txt --json)
ok "--json gives the lines and the positions they name" '[ "$(python3 -c "import json,sys; b=json.loads(sys.argv[1]); print(b[\"lines\"][3][\"person\"], [c[\"position\"] for c in b[\"changes\"]])" "$json")" = "bob [2, 1, 0]" ]'
page=$(curl -s "$AVP_SERVER/repos/hi/history/log?path=src%2Fa.txt&limit=1")
ok "the log route answers a page and where the next one starts" '[ "$(python3 -c "import json,sys; l=json.loads(sys.argv[1]); print(len(l[\"changes\"]), l[\"changes\"][0][\"person\"], l[\"more\"])" "$page")" = "1 bob 1" ]'
ok "the blame route answers 404 for a file the head does not have" '[ "$(curl -s -o /dev/null -w "%{http_code}" "$AVP_SERVER/repos/hi/history/blame?path=src/nope.txt")" = 404 ]'
