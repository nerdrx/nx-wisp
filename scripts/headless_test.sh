#!/usr/bin/env bash
# Run nx-wisp inside a nested compositor and photograph the result.
#
# NOTHING in this repo may open a window on the operator's desktop. They are
# using that machine. This is the only sanctioned way to look at her.
#
#   scripts/headless_test.sh -- run --for 8s      # her, on a layer surface
#   scripts/headless_test.sh --editor -- edit     # the rig editor, a window
#
# A nested `kwin_wayland --virtual` hosts the app. Two reasons for KWin
# specifically rather than gamescope or a wlroots compositor:
#
#   * she cannot exist without `zwlr_layer_shell_v1`, which gamescope does not
#     implement;
#   * KWin is what we actually ship for, including its D-Bus scripting, which
#     is where the terrain feed comes from.
#
# `--virtual` means the nested compositor has no window on the host at all, so
# nothing appears on the operator's screen.
#
# The screenshot comes from the APP, not the compositor: NX_WISP_DUMP_FRAME
# makes it read its own swapchain back to a PNG. That needs no capture portal,
# no ScreenShot2 file descriptor dance, and it photographs exactly the frame
# the compositor was about to show.
set -uo pipefail
cd "$(dirname "$0")/.."

OUT=/tmp/nx-wisp-headless.png
W=1920; H=1080; SETTLE=8
# Pin the tier by default. Otherwise the harness is at the mercy of whatever
# else is running: a machine busy compiling sends the GPU past its thermal
# limit, the governor correctly drops her to Dormant, and she draws nothing —
# a green test that photographs an empty screen.
PIN=full
BIN=""
ARGS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        -o) OUT="$2"; shift 2 ;;
        -W) W="$2"; shift 2 ;;
        -H) H="$2"; shift 2 ;;
        -s) SETTLE="$2"; shift 2 ;;
        -b) BIN="$2"; shift 2 ;;
        -p) PIN="$2"; shift 2 ;;
        --no-pin) PIN=""; shift ;;
        --) shift; ARGS=("$@"); break ;;
        *) ARGS+=("$1"); shift ;;
    esac
done

command -v kwin_wayland >/dev/null || { echo "kwin_wayland not installed"; exit 1; }
command -v dbus-run-session >/dev/null || { echo "dbus-run-session not installed"; exit 1; }

# shellcheck disable=SC1091
source ./env.sh
if [[ -z "$BIN" ]]; then
    cargo build -q -p wisp --bin nx-wisp || exit 1
    BIN="$(pwd)/target/debug/nx-wisp"
fi
[[ -x "$BIN" ]] || { echo "no binary at $BIN"; exit 1; }

# Never touch the operator's real state (SPEC §4).
CFG=$(mktemp -d /tmp/nx-wisp-headless-XXXXXX)
LOG=$(mktemp /tmp/nx-wisp-headless-XXXXXX.log)

rm -f "$OUT"
SOCK="nxwisp-headless-$$"

# KWin is started SEPARATELY from the app, not as `kwin_wayland -- app`.
#
# KWin's `--` child gets neither arguments nor environment: a probe of
# `kwin_wayland ... -- env FOO=bar ./script.sh some args` printed FOO unset and
# no args at all. Relying on it meant NX_WISP_CONFIG_DIR never reached the app,
# which then fell back to the operator's REAL config directory and wrote a
# flight recorder log there — precisely what SPEC §4 exists to prevent.
INNER=$(mktemp /tmp/nx-wisp-inner-XXXXXX.sh)
cat > "$INNER" <<INNEREOF
#!/usr/bin/env bash
set -u
kwin_wayland --virtual --width $W --height $H --no-global-shortcuts --socket $SOCK &
KWIN=\$!
trap 'kill "\$KWIN" 2>/dev/null; wait "\$KWIN" 2>/dev/null' EXIT INT TERM HUP

# Wait for the nested compositor to answer, not for a fixed sleep.
export WAYLAND_DISPLAY="$SOCK"
for _ in \$(seq 1 200); do
    busctl --user status org.kde.KWin >/dev/null 2>&1 && break
    sleep 0.05
done
sleep 1   # let it finish publishing its globals

export NX_WISP_CONFIG_DIR="$CFG"
export NX_WISP_DUMP_FRAME="$OUT"
export NX_WISP_DUMP_AFTER="\${NX_WISP_DUMP_AFTER:-90}"
export RUST_LOG="\${RUST_LOG:-wisp=info}"
"$BIN" ${ARGS[*]+${ARGS[*]}}
STATUS=\$?
kill "\$KWIN" 2>/dev/null
exit \$STATUS
INNEREOF
chmod +x "$INNER"

# Its own process group and its own D-Bus, so the nested KWin's scripting
# interface can never be mistaken for the operator's real one.
setsid --wait dbus-run-session -- "$INNER" >"$LOG" 2>&1 &
RUN=$!

cleanup() {
    if [[ -n "${RUN:-}" ]]; then
        kill -TERM "-$RUN" 2>/dev/null || kill -TERM "$RUN" 2>/dev/null
        for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "-$RUN" 2>/dev/null || break; sleep 0.1; done
        kill -KILL "-$RUN" 2>/dev/null
    fi
    rm -f "$INNER"
}
trap cleanup EXIT INT TERM HUP

for _ in $(seq 1 $((SETTLE * 10))); do
    [[ -s "$OUT" ]] && break
    kill -0 "$RUN" 2>/dev/null || break
    sleep 0.1
done
sleep 0.5

# The whole point of the isolation is that her real state is untouched.
if [[ -e "$HOME/.config/nx-wisp/wisp.lock" ]] && [[ "$CFG" != "$HOME/.config/nx-wisp" ]]; then
    if [[ "$HOME/.config/nx-wisp/wisp.lock" -nt "$LOG" ]]; then
        echo "!! the app touched the REAL config dir — isolation is broken, fix the harness"
    fi
fi

echo "=== nx-wisp ==="
grep -aiE "wisp|error|panic|summary|terrain" "$LOG" | head -30
echo "=== screenshot: $OUT ($(stat -c%s "$OUT" 2>/dev/null || echo 0) bytes) ==="
echo "=== config dir: $CFG ==="
echo "=== full log: $LOG ==="
