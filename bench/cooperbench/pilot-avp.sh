#!/bin/bash
export PATH=/root/.cargo/bin:/root/.local/bin:$PATH
export CLAUDE_CODE_OAUTH_TOKEN="$(tr -d '\r\n' < /mnt/c/Users/mossa/claude-token.txt)"
export CB_AVP=1 CB_AVP_BIN=/root/bench/avp-musl CB_AVP_SERVER=http://host.docker.internal:7117
cd /root/bench/CooperBench
echo "$(date -u +%FT%TZ) start pilot"
uv run cooperbench run -n pilot-avp -r pallets_click_task -t 2068 -f 1,2 -a claude_code -m claude-sonnet-5-5 --setting coop --backend docker -c 1 --force 2>&1
echo "$(date -u +%FT%TZ) end pilot exit $?"
