#!/usr/bin/env bash
# Refresh vendor/rune and vendor/crates on a machine with a fast connection, then pull them back.
# The Mac then builds fully offline (.cargo/config.toml).
#
#   scripts/vendor.sh                 # uses smr@tabot-dev, rune @ dev HEAD
#   HOST=me@box REV=<sha> scripts/vendor.sh
set -euo pipefail

HOST="${HOST:-smr@tabot-dev}"
REV="${REV:-}"
cd "$(dirname "$0")/.."

ssh "$HOST" "set -e
  export PATH=\$HOME/.cargo/bin:\$PATH
  mkdir -p ~/arbor-deps && cd ~/arbor-deps
  [ -d rune ] || git clone https://github.com/anomalyco/rune
  cd rune && git fetch -q origin && git checkout -q ${REV:-origin/dev} && git log -1 --format='rune %h %cd'"

rsync -az --delete --exclude .git --exclude demo "$HOST:arbor-deps/rune/" vendor/rune/
rsync -az --delete --exclude target --exclude vendor/crates --exclude .cargo ./ "$HOST:arbor/"
ssh "$HOST" "set -e; export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/arbor && cargo update -w -q && cargo vendor -q --versioned-dirs vendor/crates >/dev/null"
rsync -az --delete "$HOST:arbor/vendor/crates/" vendor/crates/
rsync -az "$HOST:arbor/Cargo.lock" Cargo.lock
echo "vendored; build with: cargo build --release"
