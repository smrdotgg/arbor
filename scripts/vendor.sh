#!/usr/bin/env bash
# Refresh vendor/rune and vendor/crates on a machine with a fast connection, then pull them back.
# The Mac then builds fully offline (.cargo/config.toml). Rune's pinned commit lives in RUNE_REV
# (CI reads it too).
#
#   scripts/vendor.sh                     # smr@tabot-dev, rune @ RUNE_REV
#   REV=origin/dev scripts/vendor.sh      # move the pin to rune's dev HEAD (rewrites RUNE_REV)
#   HOST=me@box scripts/vendor.sh         # another machine; HOST=local (or running on it) skips ssh
set -euo pipefail

cd "$(dirname "$0")/.."
HOST="${HOST:-smr@tabot-dev}"
REV="${REV:-$(cat RUNE_REV)}"
[ "${HOST#*@}" = "$(hostname -s)" ] && HOST=local
if [ "$HOST" = local ]; then
  [ "$PWD" != "$HOME/arbor" ] || { echo "~/arbor is the staging copy; run from another checkout" >&2; exit 1; }
  run() { bash -c "$1"; }; R="$HOME/"
else
  run() { ssh "$HOST" "$1"; }; R="$HOST:"
fi

run "set -e
  mkdir -p ~/arbor-deps && cd ~/arbor-deps
  [ -d rune ] || git clone -q https://github.com/anomalyco/rune
  cd rune && git fetch -q origin && git checkout -q $REV && git log -1 --format='rune %h %cd' >&2"
sha=$(run "git -C ~/arbor-deps/rune rev-parse HEAD") && echo "$sha" > RUNE_REV

mkdir -p vendor # a fresh clone has none, and rsync won't make two levels
rsync -az --delete --exclude .git --exclude demo "${R}arbor-deps/rune/" vendor/rune/
# Vendor in a copy without .cargo/, so cargo reads crates.io rather than vendor/crates. The
# exclude also shields the staging side from --delete, so remove a stale offline config there
# (only that file, only in the staging copy) before cargo runs.
rsync -az --delete --exclude target --exclude vendor/crates --exclude .cargo ./ "${R}arbor/"
run "set -e; export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/arbor && rm -f .cargo/config.toml && cargo update -w -q && cargo vendor -q --versioned-dirs vendor/crates >/dev/null"
rsync -az --delete "${R}arbor/vendor/crates/" vendor/crates/
rsync -az "${R}arbor/Cargo.lock" Cargo.lock
echo "vendored rune $(cut -c1-7 RUNE_REV); build with: cargo build --release"
