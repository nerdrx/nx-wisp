#!/usr/bin/env bash
# How fresh can the terrain be?
#
# SPEC §4: compositor tests run against a nested `kwin_wayland`, never the live
# session. This starts its own D-Bus and its own KWin, so the operator's
# compositor is never touched — no script is loaded into it and no window of
# theirs moves.
#
#   crates/wisp-senses/tests/nested/bench.sh [seconds] 2>/dev/null
#
# The table goes to stdout; the throwaway dbus-daemon and KWin chatter goes to
# stderr, so redirecting stderr gives a clean result and keeps it for debugging.
#
# It drives a window inside the nested compositor as fast as KWin will let it,
# then reports the rate at which window updates reach the bus for a range of
# `flush_ms` values. Measured on KWin 6.7.4 / Wayland, 2026-08-22:
#
#   flush=0ms   ~966 batches/s   one D-Bus call per KWin signal
#   flush=4ms   ~198 batches/s
#   flush=8ms   ~111 batches/s   the shipped default
#   flush=16ms   ~59 batches/s
#   flush=33ms   ~30 batches/s
#
# The uncoalesced number is the important one: the pipeline keeps up with
# everything KWin can emit, so the default is a budget, not a ceiling.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../../.." && pwd)"
SECS="${1:-5}"

# shellcheck disable=SC1091
source "$ROOT/env.sh"
cargo build -q -p wisp-senses --example smoke || exit 1

export NX_BENCH_SMOKE="$ROOT/target/debug/examples/smoke"
export NX_BENCH_SECS="$SECS"
export NX_BENCH_DRIVER="$HERE/driver.js"

exec dbus-run-session -- bash "$HERE/bench-inner.sh"
