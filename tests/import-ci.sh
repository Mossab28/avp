#!/bin/bash
# `avp import-ci` from the command line, no server: what it prints is an avp.toml as it stands,
# --json answers an agent, --write writes avp.toml then avp.imported.toml beside it, never over one.
# Usage: AVP=target/release/avp bash tests/import-ci.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
D=$(cd "$(dirname "$0")" && pwd)
W=$(mktemp -d "${1:?usage: tests/import-ci.sh <scratch dir>}/ic.XXXX") && cd "$W" || exit 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
cp -R "$D/import-ci/node-monorepo" repo && mkdir -p repo/packages/web/src

ok "it reads the CI from a folder below the repo's root" "$T import-ci -C repo/packages/web/src > out.toml"
ok "what it prints parses as TOML, the list in comments" "python3 -I -c 'import sys, tomllib; c = tomllib.load(open(sys.argv[1], \"rb\")); assert c[\"check\"][\"full_shards\"] and c[\"envs\"][\"staging\"]' out.toml && grep -q '^#   ci.yml, job test: .*matrix .node.' out.toml"
ok "a secret is named where it is read, and set once by a person" "grep -q '^secrets = \\[\"NPM_TOKEN\"\\]' out.toml && [ \"\$(grep -c '^#   avp secret set NPM_TOKEN\$' out.toml)\" = 1 ]"
ok "--json gives the file, the list and what to do next" "$T import-ci -C repo --json | python3 -I -c 'import json, sys; j = json.load(sys.stdin); assert j[\"toml\"] and j[\"not_translated\"] and j[\"secrets\"] == [\"NPM_TOKEN\"] and j[\"written\"] is None and \"--write\" in j[\"next\"]'"
ok "--write writes avp.toml" "$T import-ci -C repo --write | grep -q 'avp.toml written' && cmp -s <(cat repo/avp.toml; echo) <(sed '/^# Not translated/,\$d' out.toml)"
ok "then avp.imported.toml beside it" "$T import-ci -C repo --write | grep -q 'avp.imported.toml written' && [ -f repo/avp.imported.toml ]"
ok "and never over a file" "! $T import-ci -C repo --write 2>err && grep -q 'already there' err"
ok "a folder without a CI says where it looked" "mkdir -p bare && ! $T import-ci -C bare 2>err && grep -q '.github/workflows' err"
