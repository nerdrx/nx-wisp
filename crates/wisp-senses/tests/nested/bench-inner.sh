#!/usr/bin/env bash
# Runs inside `dbus-run-session`. See bench.sh.
set -u
SOCKET="nxwisp-bench-$$"
export WAYLAND_DISPLAY="$SOCKET"
export QT_QPA_PLATFORM=wayland

kwin_wayland --virtual --width 1920 --height 1080 --no-global-shortcuts \
    --socket "$SOCKET" -- kwrite >/tmp/nx-wisp-bench-kwin.log 2>&1 &
KWIN=$!

# If anything kills this script — a `timeout` on the caller, Ctrl-C, an error —
# the nested compositor must go with it. Without this a killed run leaves a
# headless KWin and a kwrite running on the operator's machine indefinitely,
# which is precisely the sort of mess a test harness must not make.
cleanup() {
    trap - EXIT INT TERM HUP
    kill "$KWIN" 2>/dev/null
    wait "$KWIN" 2>/dev/null
    exit "${1:-0}"
}
trap 'cleanup 130' INT
trap 'cleanup 143' TERM HUP
trap 'cleanup 0' EXIT

for _ in $(seq 1 80); do
    busctl --user status org.kde.KWin >/dev/null 2>&1 && break
    sleep 0.25
done
sleep 2   # let the client map

scripting() { busctl --user call org.kde.KWin /Scripting org.kde.kwin.Scripting "$@" >/dev/null; }
scripting loadScript ss "$NX_BENCH_DRIVER" nxwisp-bench
scripting start

printf '%-10s %-14s %s\n' "flush_ms" "batches/s" "window updates"
for F in 0 4 8 16 33; do
    OUT=$(NX_WISP_CONFIG_DIR="$(mktemp -d)" \
        "$NX_BENCH_SMOKE" --seconds "$NX_BENCH_SECS" --flush-ms "$F" 2>/dev/null)
    RATE=$(echo "$OUT" | sed -n 's/.*achieved rate *\([0-9.]*\).*/\1/p')
    UPD=$(echo "$OUT" | sed -n 's/.*window updates *\([0-9]*\).*/\1/p')
    printf '%-10s %-14s %s\n' "$F" "${RATE:-n/a}" "${UPD:-0}"
done

scripting unloadScript s nxwisp-bench 2>/dev/null

# The EXIT trap tears KWin down. It also forces status 0: we kill KWin on
# purpose, so `wait` hands back 128+signal, and exiting on that would report a
# clean sweep as a failure — the kind of noise that trains you to ignore a
# harness.
cleanup 0
