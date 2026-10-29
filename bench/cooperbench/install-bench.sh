#!/bin/bash
export PATH=/root/.cargo/bin:/root/.local/bin:$PATH
log() { echo "$(date -u +%FT%TZ) $*"; }
while pgrep -x cargo >/dev/null; do sleep 10; done
log "rebuild avp (musl) with attach"
rm -rf /root/bench/avp-new && mkdir -p /root/bench/avp-new && tar -xzf /mnt/c/Users/mossa/avp-src.tgz -C /root/bench/avp-new
cd /root/bench/avp-new && rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1; (dpkg -l musl-tools >/dev/null 2>&1 || apt-get install -y -qq musl-tools >/dev/null 2>&1)
# The binary is bind-mounted into running containers and run by the server: never overwrite in
# place (text file busy); write beside, then rename, after stopping the server.
pkill -f 'avp serve' 2>/dev/null; sleep 1
if cargo build --release --target x86_64-unknown-linux-musl > /root/avp-musl.log 2>&1; then
  cp target/x86_64-unknown-linux-musl/release/avp /root/bench/avp-musl.new && mv -f /root/bench/avp-musl.new /root/bench/avp-musl && cp /root/bench/avp-musl /usr/local/bin/avp.new && mv -f /usr/local/bin/avp.new /usr/local/bin/avp && log "avp musl ok: $(/root/bench/avp-musl --version) $(md5sum /usr/local/bin/avp | cut -c1-8)"
else
  log "musl build FAILED, falling back to glibc build"; cargo build --release > /root/avp-build.log 2>&1 && cp target/release/avp /root/bench/avp-musl.new && mv -f /root/bench/avp-musl.new /root/bench/avp-musl && cp /root/bench/avp-musl /usr/local/bin/avp.new && mv -f /usr/local/bin/avp.new /usr/local/bin/avp && log "avp glibc ok"
fi
/root/bench/avp-musl attach --help >/dev/null 2>&1 && log "attach present" || log "attach MISSING"
log "install adapter"
cp /mnt/c/Users/mossa/avp_coop.py /root/bench/CooperBench/src/cooperbench/agents/claude_code/avp_coop.py
cp /mnt/c/Users/mossa/adapter.py /root/bench/CooperBench/src/cooperbench/agents/claude_code/adapter.py
cd /root/bench/CooperBench && uv run python -c "import cooperbench.agents.claude_code.adapter as a; print('adapter import ok')" 2>&1 | tail -1
log "avp server"
pkill -f 'avp serve' 2>/dev/null; sleep 1; mkdir -p /root/bench/avp-data
AVP_CHECK_SLOTS=2 nohup /usr/local/bin/avp serve --listen 0.0.0.0:7117 --data /root/bench/avp-data > /root/bench/avp-server.log 2>&1 &
sleep 2; curl -s http://127.0.0.1:7117/ && echo && log "server up"
log "DONE"
