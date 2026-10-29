#!/bin/sh
# Installs avp: the binary of the latest release for this machine, in ~/.local/bin (or $AVP_BIN).
#   curl -fsSL https://raw.githubusercontent.com/Mossab28/avp/main/install.sh | sh
# Without a release for this machine, builds it with cargo when cargo is there.
set -eu
repo=Mossab28/avp
bin=${AVP_BIN:-$HOME/.local/bin}
os=$(uname -s); arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) target= ;;
esac
mkdir -p "$bin"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
url="https://github.com/$repo/releases/latest/download/avp-$target.tar.gz"
if [ -n "$target" ] && curl -fsSL "$url" -o "$tmp/avp.tar.gz" 2>/dev/null; then
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
echo "then, in a git checkout: avp up"
