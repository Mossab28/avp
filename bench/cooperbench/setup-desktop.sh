#!/bin/bash
# Prepares the desktop's WSL for CooperBench + AVP. Idempotent. Log: /root/bench-setup.log
set -u
export DEBIAN_FRONTEND=noninteractive
log() { echo "$(date -u +%FT%TZ) $*"; }
log "apt"
apt-get update -qq >/dev/null 2>&1; apt-get install -y -qq build-essential pkg-config git curl ca-certificates unzip >/dev/null 2>&1 && log "apt ok" || log "apt FAILED"
if [ ! -x /root/.cargo/bin/cargo ]; then log "rustup"; curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal >/dev/null 2>&1 && log "rust ok"; fi
export PATH=/root/.cargo/bin:/root/.local/bin:$PATH
if ! command -v uv >/dev/null; then log "uv"; curl -LsSf https://astral.sh/uv/install.sh | sh >/dev/null 2>&1 && log "uv ok"; fi
if ! command -v node >/dev/null; then log "node"; curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null 2>&1 && apt-get install -y -qq nodejs >/dev/null 2>&1 && log "node $(node -v)"; fi
mkdir -p /root/bench && cd /root/bench
log "avp build"
rm -rf avp && mkdir avp && tar -xzf /mnt/c/Users/mossa/avp-src.tgz -C avp && cd avp && cargo build --release > /root/avp-build.log 2>&1 && cp target/release/avp /usr/local/bin/avp && log "avp $(avp --version)" || log "avp build FAILED (see /root/avp-build.log)"
cd /root/bench
if [ ! -d CooperBench ]; then log "cooperbench clone"; git clone -q https://github.com/cooperbench/CooperBench.git; fi
cd CooperBench && uv python install 3.12 >/dev/null 2>&1; uv sync -q > /root/cb-sync.log 2>&1 && log "cooperbench synced" || log "cooperbench sync FAILED"
log "dataset prepare"; uv run cooperbench prepare > /root/cb-prepare.log 2>&1 && log "dataset ok: $(ls dataset | wc -l) repos" || log "prepare FAILED (see /root/cb-prepare.log)"
docker pull -q redis:7-alpine >/dev/null 2>&1 && log "redis image ok"
log "DONE"
