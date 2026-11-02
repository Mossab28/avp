#!/bin/sh
# Installs avp in ~/.local/bin (or $AVP_BIN), then says the next command.
#   curl -fsSL <AVP Cloud>/install | sh
# An AVP server serves this script at /install with its own address below: the binary comes from it
# when it runs on this machine's platform, else from the latest release, else cargo builds it.
set -eu
cloud=${AVP_CLOUD_URL:-}
repo=Mossab28/avp
bin=${AVP_BIN:-$HOME/.local/bin}
os=$(uname -s); arch=$(uname -m)
# The release's name for this machine, and the platform an avp server says it runs (`/client`).
case "$os-$arch" in
  Darwin-arm64) target=aarch64-apple-darwin; platform=aarch64-macos ;;
  Darwin-x86_64) target=x86_64-apple-darwin; platform=x86_64-macos ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl; platform=x86_64-linux ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl; platform=aarch64-linux ;;
  *) target=; platform= ;;
esac
mkdir -p "$bin"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
url="https://github.com/$repo/releases/latest/download/avp-$target.tar.gz"
# The server's own binary, when it runs this platform and answers here (a glibc build may not).
from_server() {
  [ -n "$cloud" ] && [ -n "$platform" ] || return 1
  curl -fsSL "$cloud/client" -o "$tmp/client.json" 2>/dev/null || return 1
  grep -q "\"platform\":\"$platform" "$tmp/client.json" || return 1
  curl -fsSL "$cloud/client/avp" -o "$tmp/avp" 2>/dev/null || return 1
  chmod 755 "$tmp/avp"
  "$tmp/avp" --version >/dev/null 2>&1 || return 1
  install -m 755 "$tmp/avp" "$bin/avp"
}
if from_server; then
  :
elif [ -n "$target" ] && curl -fsSL "$url" -o "$tmp/avp.tar.gz" 2>/dev/null; then
  tar -xzf "$tmp/avp.tar.gz" -C "$tmp"
  install -m 755 "$tmp/avp" "$bin/avp"
elif command -v cargo >/dev/null 2>&1; then
  echo "no release for ${target:-$os-$arch}: building with cargo (a few minutes)"
  cargo install --quiet --git "https://github.com/$repo" --root "$tmp/root"
  install -m 755 "$tmp/root/bin/avp" "$bin/avp"
else
  echo "no release for ${target:-$os-$arch} and no cargo: install Rust (https://rustup.rs), then run this again" >&2
  exit 1
fi
echo "$("$bin/avp" --version) installed in $bin"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "add it to your PATH: export PATH=\"$bin:\$PATH\"" ;;
esac
if [ -n "$cloud" ]; then
  echo "next: avp login $cloud"
  echo "then, in a git checkout: avp up (your agents work on AVP Cloud)"
else
  echo "next, in a git checkout: avp up (or \`avp login\` first, for AVP Cloud)"
fi
