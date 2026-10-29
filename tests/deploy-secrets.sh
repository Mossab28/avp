#!/bin/bash
# A deploy's credentials (AVP_DEPLOY_ENV) reach the deploy AVP runs and no proof's step, AVP_TOKEN
# never, and what the deploy prints of them is masked in what AVP keeps and shows.
# Usage: AVP=target/release/avp bash tests/deploy-secrets.sh <scratch dir>
set -u
T=${AVP:-$HOME/Projects/avp/target/release/avp}
W=$(mktemp -d "${1:?usage: tests/deploy-secrets.sh <scratch dir>}/ds.XXXX") && cd "$W" || exit 1
export AVP_SERVER=http://127.0.0.1:7421 AVP_PERSON=ada AVP_TOKEN=avp-s3cret-7421
# REGISTRY_TOKEN still in AVP_PROOF_ENV too, as before AVP_DEPLOY_ENV: the deploy's alone all the same.
env REGISTRY_TOKEN=reg-s3cret-7421 AWS_PROFILE=deployer-7421 GH_TOKEN=gh-s3cret-7421 \
  AVP_DEPLOY_ENV=REGISTRY_TOKEN,AWS_PROFILE,AVP_TOKEN AVP_PROOF_ENV=REGISTRY_TOKEN \
  $T serve --listen 127.0.0.1:7421 --data "$W/data" >"$W/server.log" 2>&1 & SP=$!
trap 'kill $SP 2>/dev/null' EXIT
sleep 1
ok(){ if eval "$2" >/dev/null 2>&1; then echo "PASS $1"; else echo "FAIL $1"; fi; }
events(){ curl -s -u "ada:$AVP_TOKEN" "$AVP_SERVER/repos/ds/events?limit=100"; }
mkdir app && cd app || exit 1
printf 'v0\n' > app.txt
cat > avp.toml <<EOF
[check]
[[check.steps]]
name = "env"
run = "env > '$W/step-env.txt'"

[envs.staging]
deploy = { run = "env > '$W/deploy-env.txt' && echo pushing with \$REGISTRY_TOKEN as \$AWS_PROFILE" }
[envs.broken]
deploy = { run = "echo login with \$REGISTRY_TOKEN refused; exit 3" }
EOF
$T init ds >/dev/null
$T open -C "$W/v" --repo ds --goal "a change to deploy" >/dev/null && printf 'v1\n' > "$W/v/app.txt"
cd "$W" || exit 1
ok "the proof ran its step and landed" '[[ "$($T -C v land)" == "landed at position 1"* && -s step-env.txt ]]'
ok "a proof's step does not see the deploy's credential, though AVP_PROOF_ENV lists it" '! grep -q reg-s3cret step-env.txt && ! grep -q "^REGISTRY_TOKEN=" step-env.txt'
ok "nor what else AVP_DEPLOY_ENV lists, nor the list" '! grep -q deployer-7421 step-env.txt && ! grep -q "^AVP_DEPLOY_ENV=" step-env.txt'
out=$($T -C app promote staging --force 2>&1)
ok "the deploy ran and moved staging" '[[ "$out" == "staging now serves position 1" ]]'
ok "the deploy sees what AVP_DEPLOY_ENV lists" 'grep -qx REGISTRY_TOKEN=reg-s3cret-7421 deploy-env.txt'
ok "a cloud's credential reaches the deploy when listed" 'grep -qx AWS_PROFILE=deployer-7421 deploy-env.txt'
ok "a forge's token not listed does not" '! grep -q gh-s3cret deploy-env.txt'
ok "AVP_TOKEN never reaches a deploy, though listed" '! grep -q avp-s3cret deploy-env.txt'
id=$(events | grep -o '"env.moved"[^}]*"proof":"proof_[a-z0-9]*"' | grep -o 'proof_[a-z0-9]*' | head -1)
ok "the output kept masks what the deploy printed of them" '[ -n "$id" ] && p=$($T -C app proof "$id") && [[ "$p" == *"pushing with *** as ***"* && "$p" != *reg-s3cret* && "$p" != *deployer-7421* ]]'
out=$($T -C app promote broken --force 2>&1)
ok "a failed deploy's answer masks it" '[[ "$out" == *"failed (exit 3)"* && "$out" == *"login with *** refused"* && "$out" != *reg-s3cret* ]]'
ok "its progress in avp envs masks it" 'e=$($T -C app envs) && [[ "$e" == *"failed (exit 3): login with *** refused"* && "$e" != *reg-s3cret* ]]'
ok "its event masks it" 'e=$(events) && [[ "$e" == *"\"env.failed\""* && "$e" == *"login with *** refused"* && "$e" != *reg-s3cret* ]]'
ok "nothing the server stores or logs holds it" '! grep -rq reg-s3cret data server.log && ! grep -rq deployer-7421 data server.log'
