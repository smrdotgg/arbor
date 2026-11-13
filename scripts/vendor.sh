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

# Rune's files exactly as committed (LF), whatever the staging machine's Git does with line
# endings: on Windows Rune loads its precompiled shader only while the WGSL it embeds hashes as
# committed (build.rs). An existing clone is reset to the committed bytes, since a checkout
# alone keeps files Git thinks unchanged.
run "set -e
  mkdir -p ~/arbor-deps && cd ~/arbor-deps
  [ -d rune ] || git clone -q -c core.autocrlf=false https://github.com/anomalyco/rune
  cd rune && git config core.autocrlf false && git config core.eol lf
  git fetch -q origin && git checkout -q $REV
  git rm -rq --cached . && git reset -q --hard
  git log -1 --format='rune %h %cd' >&2"
sha=$(run "git -C ~/arbor-deps/rune rev-parse HEAD") && echo "$sha" > RUNE_REV

mkdir -p vendor # a fresh clone has none, and rsync won't make two levels
rsync -az --delete --exclude .git --exclude demo "${R}arbor-deps/rune/" vendor/rune/
# The same check build.rs makes on Windows, here for every platform's vendored copy.
if crlf=$(grep -rlI --include='*.wgsl' $'\r' vendor/rune); then
  echo "vendor/rune has WGSL with CRLF line endings (Rune would compile its shader at every launch on Windows):" >&2
  echo "$crlf" >&2
  exit 1
fi
# Vendor in a copy without .cargo/, so cargo reads crates.io rather than vendor/crates. The
# exclude also shields the staging side from --delete, so remove a stale offline config there
# (only that file, only in the staging copy) before cargo runs.
rsync -az --delete --exclude target --exclude vendor/crates --exclude .cargo ./ "${R}arbor/"
run "set -e; export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/arbor && rm -f .cargo/config.toml && cargo update -w -q && cargo vendor -q --versioned-dirs vendor/crates >/dev/null"
rsync -az --delete "${R}arbor/vendor/crates/" vendor/crates/
rsync -az "${R}arbor/Cargo.lock" Cargo.lock
echo "vendored rune $(cut -c1-7 RUNE_REV); build with: cargo build --release"
