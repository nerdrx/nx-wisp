#!/usr/bin/env bash
# Verify a built artifact before it goes anywhere near a real machine.
#
#   scripts/smoke.sh [dist/NX-Wisp-<version>-linux-x86_64.AppImage]
#
#   --strict-version   fail if `--version` does not print the workspace version
#                      (default; set NX_WISP_SMOKE_LAX_VERSION=1 or pass
#                      --lax-version to downgrade it to a warning)
#
# This is the thing that stops a broken build landing on the laptop mid-trip,
# so it checks the artifact the way NX Hub will actually use it: it does NOT
# run the AppImage (CachyOS has no libfuse2 and neither does the hub's install
# path), it extracts it and runs the extracted tree.
#
# Every check is fatal. A warning here means "this cannot be checked yet",
# never "this failed but carry on".
set -euo pipefail
cd "$(dirname "$0")/.."

# The NX release public key, raw ed25519, hex. This is the same 32 bytes that
# nx-hub pins as PINNED_KEYS.nerdrx in src/main/provenance.js — pinning it
# here too means a swapped key file fails the smoke check instead of producing
# a release that the hub will later refuse.
PINNED_KEY_HEX=398bf09f463d78e3aa68ecb8995e69d286302b8b5ac470e5e199e91207d86653
PUBKEY="${NX_SIGNING_PUB:-/run/media/nerdrx/Lex/claude/tools/nx-signing/nx-release.pub}"

STRICT_VERSION=1
[[ ${NX_WISP_SMOKE_LAX_VERSION:-0} == 1 ]] && STRICT_VERSION=0
ARTIFACT=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --strict-version) STRICT_VERSION=1; shift ;;
        --lax-version)    STRICT_VERSION=0; shift ;;
        -h|--help)        sed -n '2,16p' "$0"; exit 0 ;;
        -*) echo "unknown option: $1" >&2; exit 1 ;;
        *)  ARTIFACT="$1"; shift ;;
    esac
done

FAILURES=0
pass()  { printf '  \033[32mok\033[0m    %s\n' "$*"; }
warn()  { printf '  \033[33mwarn\033[0m  %s\n' "$*"; }
fail()  { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAILURES=$((FAILURES + 1)); }
die()   { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; exit 1; }

workspace_version() {
    awk '
        /^\[/            { sec = ($0 ~ /^\[workspace\.package\]/) }
        sec && /^[[:space:]]*version[[:space:]]*=/ {
            line = $0
            sub(/^[^=]*=[[:space:]]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            exit
        }
    ' Cargo.toml
}
VERSION=$(workspace_version)

if [[ -z "$ARTIFACT" ]]; then
    ARTIFACT="dist/NX-Wisp-$VERSION-linux-x86_64.AppImage"
fi
[[ -f "$ARTIFACT" ]] || die "no artifact at $ARTIFACT"
ARTIFACT=$(readlink -f "$ARTIFACT")
DIR=$(dirname "$ARTIFACT")
BASE=$(basename "$ARTIFACT")

WORK=$(mktemp -d "${TMPDIR:-/tmp}/nx-wisp-smoke-XXXXXX")
trap 'rm -rf "$WORK"' EXIT

echo "==> $BASE  ($(wc -c < "$ARTIFACT") bytes)"

# ---------------------------------------------------------------- 1. the file

[[ -x "$ARTIFACT" ]] && pass "artifact is executable" || fail "artifact is not executable (chmod +x)"
if head -c 4 "$ARTIFACT" | grep -q $'\x7fELF'; then
    pass "artifact is an ELF (the AppImage runtime)"
else
    fail "artifact does not start with an ELF header — not an AppImage"
fi

# ---------------------------------------------------------------- 2. checksum

if [[ -f "$ARTIFACT.sha256" ]]; then
    # Run inside the directory: the .sha256 holds a bare filename so the pair
    # verifies from wherever it was downloaded to.
    if ( cd "$DIR" && sha256sum -c "$BASE.sha256" >/dev/null 2>&1 ); then
        pass "sha256 matches $BASE.sha256"
    else
        fail "sha256 does NOT match $BASE.sha256 — the artifact or the sidecar is stale"
    fi
else
    warn "no $BASE.sha256 sidecar (release.sh writes one; this is a bare build)"
fi

# ---------------------------------------------------------------- 3. signature

# Scheme (tools/nx-signing/README.md): sig = ed25519(privkey, sha256(asset)),
# lowercase hex on one line. The signature covers the DIGEST, not the bytes.
if [[ -f "$ARTIFACT.sig" ]]; then
    if [[ ! -f "$PUBKEY" ]]; then
        fail "found $BASE.sig but no public key at $PUBKEY — cannot verify"
    elif ! command -v node >/dev/null; then
        fail "found $BASE.sig but node is not installed — cannot verify"
    else
        if node -e '
const fs = require("fs"), c = require("crypto");
const [asset, sigFile, pubFile, pinned] = process.argv.slice(1);
const pub = c.createPublicKey(fs.readFileSync(pubFile, "utf8"));
const raw = pub.export({ format: "der", type: "spki" }).subarray(-32).toString("hex");
if (raw !== pinned) {
  console.error(`public key is ${raw}, not the pinned ${pinned}`);
  process.exit(2);
}
const digest = c.createHash("sha256").update(fs.readFileSync(asset)).digest();
const sig = Buffer.from(fs.readFileSync(sigFile, "utf8").trim(), "hex");
process.exit(c.verify(null, digest, pub, sig) ? 0 : 1);
' "$ARTIFACT" "$ARTIFACT.sig" "$PUBKEY" "$PINNED_KEY_HEX"; then
            pass "ed25519 signature verifies against the pinned NX release key"
        else
            case $? in
                2) fail "the key at $PUBKEY is NOT the key nx-hub pins — the hub would refuse this release" ;;
                *) fail "ed25519 signature does NOT verify" ;;
            esac
        fi
    fi
else
    warn "no $BASE.sig sidecar (release.sh writes one; this is a bare build)"
fi

# ---------------------------------------------------------------- 4. extract

# This IS how NX Hub installs: `--appimage-extract`, which the AppImage runtime
# handles without FUSE. If this step fails the artifact is uninstallable on
# CachyOS no matter how well it runs here.
ROOT="$WORK/squashfs-root"
if ( cd "$WORK" && "$ARTIFACT" --appimage-extract >/dev/null 2>"$WORK/extract.err" ) && [[ -d "$ROOT" ]]; then
    pass "--appimage-extract produced squashfs-root (works without libfuse2)"
else
    echo "--- appimagetool stderr ---" >&2
    tail -5 "$WORK/extract.err" >&2 || true
    die "--appimage-extract failed — NX Hub could not install this"
fi

# ---------------------------------------------------------------- 5. the tree

check_exec() {
    if [[ -f "$ROOT/$1" && -x "$ROOT/$1" ]]; then pass "$1 present and executable"
    else fail "$1 missing or not executable"; fi
}
check_file() {
    if [[ -f "$ROOT/$1" ]]; then pass "$1 present"
    else fail "$1 missing"; fi
}

check_exec AppRun
check_exec usr/bin/nx-wisp
check_file nx-wisp.desktop
check_file usr/share/applications/nx-wisp.desktop
check_file usr/share/metainfo/org.nx.Wisp.metainfo.xml
check_file usr/share/nx-wisp/BUNDLE.txt

# The launcher tile: nx-hub's findIcon() takes .DirIcon first and follows it if
# it is a symlink. A dangling one means every NX Hub tile falls back to the
# generic NX mark, which DESIGN.md §8 explicitly forbids for a real app.
if [[ -e "$ROOT/.DirIcon" ]]; then
    pass ".DirIcon resolves ($(readlink "$ROOT/.DirIcon" 2>/dev/null || echo 'regular file'))"
else
    fail ".DirIcon missing or dangling — NX Hub's launcher tile would have no icon"
fi
for size in 16x16 32x32 48x48 128x128 256x256 512x512; do
    [[ -f "$ROOT/usr/share/icons/hicolor/$size/apps/nx-wisp.png" ]] \
        || fail "icon missing: hicolor/$size/apps/nx-wisp.png"
done
pass "hicolor icon set present"

if command -v desktop-file-validate >/dev/null; then
    if desktop-file-validate "$ROOT/nx-wisp.desktop" >/dev/null 2>&1; then
        pass "desktop entry validates"
    else
        fail "desktop entry does not validate"
    fi
fi

# ---------------------------------------------------------------- 6. policy

# Belt and braces against a regression in build-appimage.sh's bundling policy.
# These are the libraries that make an AppImage work on the machine that built
# it and fail on the laptop.
if [[ -d "$ROOT/usr/lib" ]]; then
    BAD=$(find "$ROOT/usr/lib" -maxdepth 1 -type f -printf '%f\n' 2>/dev/null | grep -E \
        '^(libvulkan|libwayland-|libxkbcommon|libc\.so|libm\.so|libpthread|ld-linux|libGL|libEGL|libdrm|libgbm|libdbus-1|libX11)' || true)
    if [[ -n "$BAD" ]]; then
        fail "host-stack libraries were bundled — this AppImage is not portable:"
        printf '        %s\n' $BAD
    else
        pass "no host-stack library bundled ($(find "$ROOT/usr/lib" -maxdepth 1 -type f | wc -l) bundled libs)"
    fi
else
    pass "no bundled libraries at all"
fi

# ---------------------------------------------------------------- 7. it runs

# SPEC §4: never let a build or a test touch the operator's real state. The
# dev build and the installed copy otherwise share a config dir, and this
# script runs on the same machine she lives on.
SANDBOX="$WORK/home"
mkdir -p "$SANDBOX"/{config,data,state,cache,wisp}
RUNOUT="$WORK/version.out"
set +e
env -u WAYLAND_DISPLAY \
    HOME="$SANDBOX" \
    XDG_CONFIG_HOME="$SANDBOX/config" \
    XDG_DATA_HOME="$SANDBOX/data" \
    XDG_STATE_HOME="$SANDBOX/state" \
    XDG_CACHE_HOME="$SANDBOX/cache" \
    NX_WISP_CONFIG_DIR="$SANDBOX/wisp" \
    timeout 30 "$ROOT/AppRun" --version >"$RUNOUT" 2>&1
RC=$?
set -e

if [[ $RC -eq 0 ]]; then
    pass "AppRun --version exits 0"
elif [[ $RC -eq 124 ]]; then
    fail "AppRun --version hung (30s timeout) — it should not touch the compositor"
else
    fail "AppRun --version exited $RC"
    sed 's/^/        /' "$RUNOUT" | head -10
fi

OUT=$(tr -d '\r' < "$RUNOUT" | head -3)
if [[ -n "${OUT// /}" ]]; then
    pass "AppRun --version printed: ${OUT//$'\n'/ }"
else
    fail "AppRun --version printed nothing"
fi

if grep -qF "$VERSION" "$RUNOUT"; then
    pass "reported version contains $VERSION"
elif [[ $STRICT_VERSION -eq 1 ]]; then
    fail "--version does not report $VERSION (the workspace Cargo.toml version)"
else
    warn "--version does not report $VERSION — allowed by --lax-version"
fi

# The extracted tree must not have picked up a dependency the AppDir does not
# satisfy. `ldd` on the installed binary is the same question the dynamic
# loader will ask on the laptop.
if ldd "$ROOT/usr/bin/nx-wisp" 2>/dev/null | grep -q 'not found'; then
    fail "usr/bin/nx-wisp has unresolved libraries on THIS machine:"
    ldd "$ROOT/usr/bin/nx-wisp" | grep 'not found' | sed 's/^/        /'
else
    pass "every direct dependency resolves"
fi

GLIBC=$(readelf -V "$ROOT/usr/bin/nx-wisp" 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sort -u -V | tail -1)
[[ -n "$GLIBC" ]] && pass "needs $GLIBC or newer on the target host"

# ---------------------------------------------------------------- verdict

echo
if [[ $FAILURES -eq 0 ]]; then
    echo "==> smoke check passed"
else
    echo "==> smoke check FAILED ($FAILURES problem(s)) — do not release this"
    exit 1
fi
