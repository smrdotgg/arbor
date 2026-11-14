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
  [ "$PWD" != "$HOME/satchel" ] || { echo "~/satchel is the staging copy; run from another checkout" >&2; exit 1; }
  run() { bash -c "$1"; }; R="$HOME/"
else
  run() { ssh "$HOST" "$1"; }; R="$HOST:"
fi

# Rune's files exactly as committed (LF), whatever the staging machine's Git does with line
# endings. (Rune's own .gitattributes and build.rs now keep its precompiled shaders matching a
# CRLF checkout too; this keeps the vendored bytes identical to the commit anyway.) An existing
# clone is reset to the committed bytes, since a checkout alone keeps files Git thinks unchanged.
# Rune's patched crates (vendor/rune/vendor/: winit, wgpu-hal, parley_engine, accesskit_windows)
# come with it; Satchel's Cargo.toml patches crates.io with them.
run "set -e
  mkdir -p ~/satchel-deps && cd ~/satchel-deps
  [ -d rune ] || git clone -q -c core.autocrlf=false https://github.com/anomalyco/rune
  cd rune && git config core.autocrlf false && git config core.eol lf
  git fetch -q origin && git checkout -q $REV
  git rm -rq --cached . && git reset -q --hard
  git log -1 --format='rune %h %cd' >&2"
sha=$(run "git -C ~/satchel-deps/rune rev-parse HEAD") && echo "$sha" > RUNE_REV

mkdir -p vendor # a fresh clone has none, and rsync won't make two levels
rsync -az --delete --exclude .git --exclude demo "${R}satchel-deps/rune/" vendor/rune/
# Vendor in a copy without .cargo/, so cargo reads crates.io rather than vendor/crates. The
# exclude also shields the staging side from --delete, so remove a stale offline config there
# (only that file, only in the staging copy) before cargo runs.
rsync -az --delete --exclude target --exclude vendor/crates --exclude .cargo ./ "${R}satchel/"
run "set -e; export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/satchel && rm -f .cargo/config.toml && cargo update -w -q && cargo vendor -q --versioned-dirs vendor/crates >/dev/null"
rsync -az --delete "${R}satchel/vendor/crates/" vendor/crates/
rsync -az "${R}satchel/Cargo.lock" Cargo.lock
echo "vendored rune $(cut -c1-7 RUNE_REV); build with: cargo build --release"
