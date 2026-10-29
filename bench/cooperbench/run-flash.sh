#!/bin/bash
# run-flash.sh <solo|git|avp> : the flash_10 subset in one condition, logs in /root/bench/flash-<cond>.log
COND=$1
export PATH=/root/.cargo/bin:/root/.local/bin:$PATH
export CLAUDE_CODE_OAUTH_TOKEN="$(tr -d '\r\n' < /mnt/c/Users/mossa/claude-token.txt)"
cd /root/bench/CooperBench
COMMON="-s flash_10 -a claude_code -m claude-sonnet-5-5 --backend docker"
echo "$(date -u +%FT%TZ) start flash $COND"
case $COND in
  solo)  uv run cooperbench run -n flash-solo $COMMON --setting solo -c 3 2>&1 ;;
  git)   uv run cooperbench run -n flash-git $COMMON --setting coop --git -c 2 2>&1 ;;
  avp) CB_AVP=1 CB_AVP_BIN=/root/bench/avp-musl CB_AVP_SERVER=http://host.docker.internal:7117 CB_AVP_LOG=/root/bench/avp-adapter-flash.log \
         uv run cooperbench run -n flash-avp $COMMON --setting coop -c 2 2>&1 ;;
esac
echo "$(date -u +%FT%TZ) end flash $COND exit $?"
