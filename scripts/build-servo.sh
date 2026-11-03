#!/usr/bin/env bash
# Build serf with the real Servo engine.
#
# Run this on an aarch64 host with LOTS of RAM — the SpiderMonkey (mozjs_sys)
# compile is the bottleneck and its link step is the peak-RAM moment. An
# 8-16 GB ARM machine (Scaleway COPARM1 / a native ubuntu-24.04-arm runner)
# is the realistic target. On the Pinebook Pro this will swap for hours and
# very likely OOM.
#
# Usage: scripts/build-servo.sh [--check]   (--check = cargo check only)

set -euo pipefail

cd "$(dirname "$0")/.."

MODE="${1:-build}"

echo "== serf/Servo build =="
echo "host arch : $(uname -m)"
echo "cores     : $(nproc)"
echo "memory    : $(free -h 2>/dev/null | awk '/^Mem:/ {print $2 " total, " $7 " available"}' || echo unknown)"
echo

if [ "$(uname -m)" != "aarch64" ]; then
  echo "WARNING: target is aarch64 (Pinebook Pro). Building on $(uname -m) will" >&2
  echo "         not produce a binary that runs there without a cross toolchain." >&2
  echo "         mozjs_sys is actively hostile to cross builds — prefer native." >&2
  echo
fi

# SpiderMonkey needs a real toolchain, not just rustc.
echo "== toolchain check =="
for tool in clang++ cmake python3 make automake autoconf; do
  if command -v "$tool" >/dev/null; then
    printf '  %-10s %s\n' "$tool" "$(command -v "$tool")"
  else
    printf '  %-10s MISSING\n' "$tool"
    MISSING=1
  fi
done
if [ "${MISSING:-0}" = "1" ]; then
  echo
  echo "Install the missing build tools first, e.g. on Debian/Ubuntu:" >&2
  echo "  sudo apt-get install -y clang cmake python3 make automake autoconf \\" >&2
  echo "       libfontconfig1-dev libfreetype6-dev pkg-config" >&2
  exit 1
fi

# Keep parallelism sane: mozjs link is RAM-hungry, so cap jobs unless the
# caller asks otherwise.
JOBS="${CARGO_BUILD_JOBS:-$(nproc)}"
if [ -z "${CARGO_BUILD_JOBS:-}" ] && [ "$(nproc)" -gt 8 ]; then
  JOBS=8
  echo
  echo "capping jobs at 8 (SpiderMonkey links are memory-hungry); override with CARGO_BUILD_JOBS"
fi

echo
echo "== fetch (776 crates) =="
time cargo fetch

echo
if [ "$MODE" = "--check" ]; then
  echo "== cargo check --features servo =="
  echo "note: 'check' still RUNS build scripts, so this also builds SpiderMonkey."
  time CARGO_BUILD_JOBS="$JOBS" cargo check --features servo --all-targets
else
  echo "== cargo build --release --features servo =="
  time CARGO_BUILD_JOBS="$JOBS" cargo build --release --features servo
  echo
  echo "== artifact =="
  ls -lh target/release/serf target/release/serf-msg 2>/dev/null || true
  file target/release/serf 2>/dev/null || true
fi
