#!/bin/bash
# AVP Cloud: a repo lives under its space. Two people each bring `hello` and both work, through the
# CLI (`--repo eve/hello`, `avp clone eve/hello`) and git (`git clone <server>/eve/hello.git`); the
# same name twice in one organization is refused with a clear message.
# Usage: AVP=target/release/avp bash tests/cloud-spaces.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/cloud-spaces.sh <scratch dir>}/cs.XXXX") && cd "$W" || exit 1
unset AVP_TOKEN AVP_USER AVP_REPO
export AVP_SERVER=http://127.0.0.1:7611 GIT_TERMINAL_PROMPT=0
AVP_CLOUD=1 AVP_SIGNUP=open $T serve --listen 127.0.0.1:7611 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
for _ in $(seq 50); do curl -s $AVP_SERVER/ >/dev/null && break; sleep 0.2; done
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
signup(){ curl -s -X POST $AVP_SERVER/signup -H 'content-type: application/json' -H "x-forwarded-for: 203.0.113.$2" \
  -d "{\"name\":\"$1\",\"email\":\"$1@example.com\",\"password\":\"correct horse battery\"}" | grep -o 'avp_[A-Za-z0-9_-]\{40,\}' | head -1; }
E=$(signup eve 1); F=$(signup fay 2)
for who in eve fay; do
  tok=$([ $who = eve ] && echo $E || echo $F)
  mkdir -p "src-$who" && printf "$who's\n" > "src-$who/README.md"
  (cd "src-$who" && AVP_TOKEN=$tok $T init hello >"$W/init-$who.log" 2>&1)
done
ok "each person brings hello into their own space" 'grep -q "repo eve/hello" $W/init-eve.log && grep -q "repo fay/hello" $W/init-fay.log'
AVP_TOKEN=$E $T clone eve/hello "$W/c-eve" >/dev/null 2>&1
ok "avp clone eve/hello lays out eve's" 'grep -q "eve'"'"'s" $W/c-eve/README.md'
(cd "$W/c-eve" && AVP_TOKEN=$E $T open --goal "eve: more" >/dev/null 2>&1 && printf 'more\n' >> README.md && AVP_TOKEN=$E $T land >"$W/land.log" 2>&1)
ok "a task lands on eve/hello" 'grep -qi "landed" $W/land.log'
(mkdir -p "$W/v-fay" && cd "$W/v-fay" && AVP_TOKEN=$F $T open --repo fay/hello --goal "fay: hers" >"$W/open-fay.log" 2>&1)
ok "--repo fay/hello opens on fay's" 'grep -q "fay'"'"'s" $W/v-fay/README.md'
ok "the view remembers the id" 'grep -q "repo_" $W/v-fay/.avp/view.json'
# As from another machine: the server's own keeps every right.
far(){ git -c http.extraHeader="X-Forwarded-For: 10.0.0.9" "$@"; }
far clone -q http://eve:$E@127.0.0.1:7611/eve/hello.git "$W/g-eve" 2>"$W/git.err"
ok "git clone <server>/eve/hello.git gives eve's line" 'grep -q more $W/g-eve/README.md'
ok "and not fay's through eve's space" '! far clone -q http://fay:$F@127.0.0.1:7611/eve/hello.git $W/g-x'
AVP_TOKEN=$E $T org create eve-co >/dev/null 2>&1
mkdir -p "$W/o" && printf 'x\n' > "$W/o/x" && (cd "$W/o" && AVP_TOKEN=$E $T init eve-co/hello >/dev/null 2>&1)
(mkdir -p "$W/o2" && cd "$W/o2" && printf 'y\n' > y && AVP_TOKEN=$E $T init eve-co/hello >"$W/twice.log" 2>&1)
ok "the same name twice in one org is refused, clearly" 'grep -q "eve-co/hello already exists" $W/twice.log && ! grep -q UNIQUE $W/twice.log'
ok "two of hers: the name alone lists them" 'AVP_TOKEN=$E $T clone hello $W/c-two 2>&1 | grep -q "eve-co/hello"'
