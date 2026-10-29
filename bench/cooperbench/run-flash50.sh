#!/bin/bash
# flash (50 pairs), one pass, three conditions in a row: avp, git, solo (reverse order of the
# flash_10 run so time of night does not always favor the same arm).
export PATH=/root/.cargo/bin:/root/.local/bin:$PATH
export CLAUDE_CODE_OAUTH_TOKEN="$(tr -d '\r\n' < /mnt/c/Users/mossa/claude-token.txt)"
cd /root/bench/CooperBench
COMMON="-s flash -a claude_code -m claude-sonnet-5-5 --backend docker"
echo "$(date -u +%FT%TZ) start flash50 avp"
CB_AVP=1 CB_AVP_BIN=/root/bench/avp-musl CB_AVP_SERVER=http://host.docker.internal:7117 CB_AVP_LOG=/root/bench/avp-adapter-flash50.log \
  uv run cooperbench run -n flash50-avp $COMMON --setting coop -c 3 2>&1 | grep -vE 'DEBUG|run -d|with ID|Started container' 
echo "$(date -u +%FT%TZ) start flash50 git"
uv run cooperbench run -n flash50-git $COMMON --setting coop --git -c 3 2>&1 | grep -vE 'DEBUG|run -d|with ID|Started container'
echo "$(date -u +%FT%TZ) start flash50 solo"
uv run cooperbench run -n flash50-solo $COMMON --setting solo -c 4 2>&1 | grep -vE 'DEBUG|run -d|with ID|Started container'
echo "$(date -u +%FT%TZ) end flash50"
