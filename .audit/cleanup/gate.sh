#!/usr/bin/env bash
# Gate every cleanup unit must pass: format, lint, full tests, and the verify-m2k smoke run.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
cargo fmt --all --check
cargo clippy --all-targets -q -- -D warnings
cargo test -q 2>&1 | awk '/^test result:/ {p+=$4; f+=$6; i+=$8} /FAILED|panicked/ {bad=1} END {printf "tests passed=%d failed=%d ignored=%d\n", p, f, i; exit (f>0||bad)}'
.agents/skills/verify-m2k/scripts/smoke
