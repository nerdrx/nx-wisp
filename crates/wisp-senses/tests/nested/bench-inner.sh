#!/usr/bin/env bash
# Runs inside `dbus-run-session`. See bench.sh.
set -u
SOCKET="nxwisp-bench-$$"
export WAYLAND_DISPLAY="$SOCKET"
export QT_QPA_PLATFORM=wayland

kwin_wayland --virtual --width 1920 --height 1080 --no-global-shortcuts \
    --socket "$SOCKET" -- kwrite >/tmp/nx-wisp-bench-kwin.log 2>&1 &
KWIN=$!

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
kill "$KWIN" 2>/dev/null
wait "$KWIN" 2>/dev/null
