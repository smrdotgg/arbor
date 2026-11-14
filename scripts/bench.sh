#!/usr/bin/env bash
# Measures Satchel against NORTHSTAR's speed budgets: cold start, 10k-entry listing, key → frame.
# Builds the release binary, then runs examples/bench.rs (see it for what's measured).
#
#   scripts/bench.sh               # 10 cold starts × 10 rounds each
#   scripts/bench.sh 20 5          # 20 cold starts × 5 rounds
#   xvfb-run -a scripts/bench.sh   # Linux without a display (software rendering: not representative)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --quiet
cargo build --release --quiet --example bench
exec "target/release/examples/bench" "$@"
