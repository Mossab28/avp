#!/bin/bash
# A proof runs what an agent wrote: none of the server's secrets, nor its own settings, reach a step,
# only what a build needs and what AVP_PROOF_ENV lists (a secret, never, even listed).
# Usage: AVP=target/release/avp bash tests/proof-env.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/proof-env.sh <scratch dir>}/pe.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7408 AVP_TOKEN=avp-s3cret-7408
env AWS_SECRET_ACCESS_KEY=aws-s3cret GH_TOKEN=gh-s3cret ANTHROPIC_API_KEY=anthropic-s3cret AVP_CHECK_SLOTS=1 \
  UNLISTED=private PROOF_SETTING=shared AVP_PROOF_ENV=PROOF_SETTING,GH_TOKEN,AVP_TOKEN \
  $T serve --listen 127.0.0.1:7408 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
mkdir src && printf 'a\n' > src/a.txt && printf '[check]\n[[check.steps]]\nname = "env"\nrun = "env > %s/step-env.txt"\n' "$W" > avp.toml && $T init pe >/dev/null
mkdir v && $T open -C v --repo pe --goal "a change to prove" >/dev/null && printf 'b\n' > v/src/a.txt
ok "the proof ran its step and landed" '[[ "$($T -C v land)" == "landed at position"* && -s step-env.txt ]]'
ok "the server's AVP_TOKEN is not visible, though listed" '! grep -q avp-s3cret step-env.txt'
ok "cloud credentials are not visible" '! grep -q aws-s3cret step-env.txt'
ok "GH_TOKEN is not visible, though listed" '! grep -q gh-s3cret step-env.txt'
ok "a model's key is not visible" '! grep -q anthropic-s3cret step-env.txt'
ok "the server's own settings are not visible" '! grep -q "^AVP_CHECK_SLOTS=" step-env.txt && ! grep -q "^AVP_PROOF_ENV=" step-env.txt'
ok "what AVP_PROOF_ENV does not list is not visible" '! grep -q "^UNLISTED=" step-env.txt'
ok "what AVP_PROOF_ENV lists is visible" 'grep -qx PROOF_SETTING=shared step-env.txt'
ok "what a build needs is visible" 'grep -q "^PATH=" step-env.txt && grep -q "^HOME=" step-env.txt && grep -qx CI=true step-env.txt'
