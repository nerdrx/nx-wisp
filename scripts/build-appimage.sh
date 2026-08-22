#!/usr/bin/env bash
# Build dist/NX-Wisp-<version>-linux-x86_64.AppImage from a cargo release build.
#
#   scripts/build-appimage.sh [--no-build] [--keep-appdir]
#
#   --no-build     use the binary already in target/release (for iterating on
#                  packaging without paying for a rebuild)
#   --keep-appdir  leave dist/AppDir in place so you can inspect it
#
# NX Wisp is a native Rust binary, not an Electron app, so there is no
# electron-builder here and nothing else in the NX family to copy. What there
# is instead is a bundling POLICY, which is the only interesting part of this
# file — see "the policy" below and docs/RELEASING.md.
set -euo pipefail
cd "$(dirname "$0")/.."

NO_BUILD=0
KEEP_APPDIR=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build)    NO_BUILD=1; shift ;;
        --keep-appdir) KEEP_APPDIR=1; shift ;;
        -h|--help)     sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 1 ;;
    esac
done

NX_TOOLS="${NX_TOOLS:-/run/media/nerdrx/Lex/claude/tools}"
APPIMAGETOOL="${APPIMAGETOOL:-$NX_TOOLS/appimagetool/AppRun}"
APPIMAGE_RUNTIME="${APPIMAGE_RUNTIME:-$NX_TOOLS/runtime-x86_64}"
BIN_NAME=nx-wisp
DIST=dist
APPDIR="$DIST/AppDir"

# ---------------------------------------------------------------- version

# Single source of truth: [workspace.package] version in the workspace
# Cargo.toml. Nothing downstream of here may hardcode a version.
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
[[ -n "$VERSION" ]] || { echo "could not read [workspace.package] version from Cargo.toml" >&2; exit 1; }
ASSET="NX-Wisp-$VERSION-linux-x86_64.AppImage"

echo "==> NX Wisp $VERSION"

# ---------------------------------------------------------------- toolchain

# Rust lives on the Lex drive, not in ~/.cargo — but only on the machines that
# have that drive. Sourcing env.sh anywhere else points CARGO_HOME at a path
# that does not exist, which breaks cargo more thoroughly than not sourcing it,
# so check first (CI runners land here and use their own rustup).
if [[ -f env.sh ]]; then
    _cargo_home=$(sed -n 's/^[[:space:]]*export CARGO_HOME=//p' env.sh | head -1)
    if [[ -z "$_cargo_home" || -d "$_cargo_home" ]]; then
        # shellcheck disable=SC1091
        . ./env.sh
    else
        echo "==> env.sh names a toolchain at $_cargo_home which is not here; using PATH cargo"
    fi
fi
command -v cargo >/dev/null || { echo "cargo not found" >&2; exit 1; }

# appimagetool ships as an AppImage. This box has no libfuse2 (the same reason
# NX Hub installs by extraction), so we keep it EXTRACTED and run its AppRun.
if [[ ! -x "$APPIMAGETOOL" ]]; then
    echo "==> appimagetool not found at $APPIMAGETOOL — fetching it"
    mkdir -p "$NX_TOOLS"
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/nx-wisp-ait-XXXXXX")
    curl -fsSL -o "$tmp/appimagetool" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$tmp/appimagetool"
    ( cd "$tmp" && ./appimagetool --appimage-extract >/dev/null )
    rm -rf "$NX_TOOLS/appimagetool"
    mv "$tmp/squashfs-root" "$NX_TOOLS/appimagetool"
    rm -rf "$tmp"
    APPIMAGETOOL="$NX_TOOLS/appimagetool/AppRun"
fi
if [[ ! -f "$APPIMAGE_RUNTIME" ]]; then
    # Pinned rather than let appimagetool fetch one per build: the runtime is
    # the first 900 KB of every artifact we ship and it should not change
    # under us between two releases.
    echo "==> AppImage runtime not found at $APPIMAGE_RUNTIME — fetching it"
    curl -fsSL -o "$APPIMAGE_RUNTIME" \
        https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64
    chmod +x "$APPIMAGE_RUNTIME"
fi

for t in ldd readelf; do
    command -v "$t" >/dev/null || { echo "$t not found (Arch: pacman -S binutils)" >&2; exit 1; }
done

# ---------------------------------------------------------------- build

if [[ $NO_BUILD -eq 0 ]]; then
    # The release ships the real thing: llama.cpp on Vulkan and Piper TTS.
    # Both are behind features because the default build (and CI) must work
    # with no GPU headers and no ort cache. WISP_FEATURES overrides — a laptop
    # without the staged Vulkan SDK can still cut a mock-brained build.
    FEATURES="${WISP_FEATURES:-full}"
    echo "==> cargo build --release -p wisp --features $FEATURES"
    # espeak-rs-sys copies its espeak-ng source tree into target/<profile>/
    # once, guarded by `if !exists` — so a build interrupted mid-copy leaves a
    # TRUNCATED tree that the guard then treats as complete forever, and every
    # later build fails with "Error processing file '…/phsource/intonation'".
    # Detect the truncation and clear it before building.
    for prof in release debug; do
        d="target/$prof/espeak-ng"
        if [[ -d "$d" && ! -d "$d/phsource" ]]; then
            echo "==> $d is a truncated espeak-ng copy (interrupted build); removing"
            rm -rf "$d"
            # The cmake configure cache in OUT_DIR remembers the truncated
            # source; it has to go with it.
            rm -rf target/$prof/build/espeak-rs-sys-*
        fi
    done
    cargo build --release -p wisp --features "$FEATURES"
fi

BIN="target/release/$BIN_NAME"
[[ -x "$BIN" ]] || { echo "no release binary at $BIN — drop --no-build" >&2; exit 1; }

# ---------------------------------------------------------------- AppDir

echo "==> assembling $APPDIR"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" \
         "$APPDIR/usr/share/applications" "$APPDIR/usr/share/metainfo" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "$BIN"              "$APPDIR/usr/bin/$BIN_NAME"
install -m 0755 packaging/AppRun    "$APPDIR/AppRun"

install -m 0644 packaging/nx-wisp.desktop "$APPDIR/usr/share/applications/nx-wisp.desktop"
# The AppImage spec wants the desktop file at the AppDir root too.
cp "$APPDIR/usr/share/applications/nx-wisp.desktop" "$APPDIR/nx-wisp.desktop"

sed -e "s/@VERSION@/$VERSION/g" -e "s/@DATE@/$(date -u +%Y-%m-%d)/g" \
    packaging/org.nx.Wisp.metainfo.xml > "$APPDIR/usr/share/metainfo/org.nx.Wisp.metainfo.xml"

echo "==> icons"
for png in packaging/icons/[0-9]*x[0-9]*.png; do
    size=$(basename "$png" .png)                     # e.g. 128x128
    dir="$APPDIR/usr/share/icons/hicolor/$size/apps"
    mkdir -p "$dir"
    install -m 0644 "$png" "$dir/$BIN_NAME.png"
done
install -m 0644 packaging/icons/icon.svg \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/$BIN_NAME.svg"

# Root-level icon named after the desktop file's Icon= key, as the AppImage
# spec requires. A REAL file, not a link: this is also the file NX Hub ends up
# showing on its launcher tile (src/main/install/desktop.js findIcon() takes
# .DirIcon first and resolves it if it is a symlink), so it wants to be the
# largest raster we have.
install -m 0644 packaging/icons/512x512.png "$APPDIR/$BIN_NAME.png"
ln -sf "$BIN_NAME.png" "$APPDIR/.DirIcon"

# ---------------------------------------------------------------- the policy

# Which shared libraries go in usr/lib.
#
# The rule is "bundle what the host cannot be assumed to have, and NOTHING
# that talks to the host's hardware, compositor or session bus", because those
# libraries are the machine's, not ours. Getting this backwards produces an
# AppImage that works on the machine that built it and dies on the laptop.
#
# It matters less than it looks like it should today, and that is not an
# accident. Everything NX Wisp needs from the graphics and Wayland stack is
# opened with dlopen() at runtime and is therefore invisible to ldd anyway:
#
#   libvulkan.so.1          wgpu loads the Vulkan loader through libloading
#   libwayland-client.so.0  wayland-backend, via smithay-client-toolkit's
#                           "system" feature (pinned in the workspace Cargo.toml)
#   libxkbcommon.so.0       xkbcommon-dl
#
# None of the three can be bundled even if we wanted to. A bundled Vulkan
# loader cannot see the host's ICD manifests; a bundled libwayland-client is
# not the one the host compositor's protocol extensions were built against.
# They are on the deny list below anyway, so that a future dependency which
# links one of them *directly* trips the policy instead of silently shipping.
#
# D-Bus is not here either: wisp-senses speaks it through zbus, which is pure
# Rust and does not link libdbus-1.
host_provided() {
    case "$1" in
        # glibc and the dynamic loader. Bundling libc without the matching
        # ld.so is the classic way to make an unrunnable AppImage.
        ld-linux*|libc.so.*|libm.so.*|libdl.so.*|libpthread.so.*|librt.so.*) return 0 ;;
        libutil.so.*|libnsl.so.*|libresolv.so.*|libanl.so.*|libmvec.so.*|libcrypt.so.*) return 0 ;;
        # compiler runtimes: ABI-stable and present anywhere a Rust binary runs
        libgcc_s.so.*|libstdc++.so.*|libgomp.so.*|libatomic.so.*) return 0 ;;
        # the graphics stack. NEVER ours.
        libvulkan.so.*|libGL.so.*|libEGL.so.*|libGLX*|libGLdispatch*|libOpenGL*|libGLESv2*) return 0 ;;
        libcuda.so.*|libnvidia-*|libnvcuvid*|libnvoptix*) return 0 ;;
        libdrm*|libgbm*|libepoxy.so.*) return 0 ;;
        # mesa internals and the hardware probes drivers drag in
        libLLVM*|libgallium*|libsensors.so.*|libpciaccess.so.*) return 0 ;;
        # the compositor stack. NEVER ours.
        libwayland-*|libxkbcommon*|libdecor-*|libffi.so.*) return 0 ;;
        # X11: SPEC §1 forbids it in the tree, but a transitive dep can still
        # drag it in. If it ever shows up it is the host's, and it is a bug.
        libX11*|libxcb*|libXau*|libXdmcp*|libXext*|libXrandr*|libXi*|libXcursor*|libXfixes*|libXrender*) return 0 ;;
        # the session: bus, logind, udev
        libdbus-1.so.*|libsystemd.so.*|libudev.so.*|libelf.so.*) return 0 ;;
        # audio. PipeWire in particular is a client of the host daemon.
        libpipewire-*|libspa-*|libasound.so.*|libpulse*|libjack*) return 0 ;;
        # ubiquitous compression/security libraries
        libz.so.*|libzstd.so.*|liblzma.so.*|libbz2.so.*|libcap.so.*|libseccomp*|libselinux*) return 0 ;;
    esac
    return 1
}

echo "==> resolving shared libraries"
BUNDLED=()
HOSTED=()
MISSING=()

while read -r soname arrow target _rest; do
    [[ -n "$soname" ]] || continue
    case "$soname" in
        linux-vdso.so*|linux-gate.so*) continue ;;
    esac
    if [[ "$arrow" != "=>" ]]; then
        # "/lib64/ld-linux-x86-64.so.2 (0x...)" — the loader, always the host's
        HOSTED+=("$(basename "$soname")")
        continue
    fi
    if [[ "$target" == "not" ]]; then
        MISSING+=("$soname")
        continue
    fi
    base=$(basename "$soname")
    if host_provided "$base"; then
        HOSTED+=("$base")
    else
        BUNDLED+=("$base|$target")
    fi
done < <(ldd "$BIN")

if [[ ${#MISSING[@]} -gt 0 ]]; then
    echo "!! unresolved shared libraries — the build machine is missing them," >&2
    echo "!! so we cannot know what to bundle. Refusing to package." >&2
    printf '!!   %s\n' "${MISSING[@]}" >&2
    exit 1
fi

for entry in ${BUNDLED[@]+"${BUNDLED[@]}"}; do
    base=${entry%%|*}
    src=${entry#*|}
    # -L: copy the real object, not the versioned symlink chain.
    cp -L "$src" "$APPDIR/usr/lib/$base"
    chmod 0644 "$APPDIR/usr/lib/$base"
    echo "    bundled  $base  ($src)"
done
if [[ ${#BUNDLED[@]} -eq 0 ]]; then
    echo "    nothing to bundle — the binary needs only host-provided libraries"
    rmdir "$APPDIR/usr/lib"
fi

# ---------------------------------------------------------------- glibc floor

# The one portability hazard left. glibc is deliberately NOT bundled, so the
# artifact runs on any machine whose glibc is at least as new as the build
# machine's — and silently fails with a symbol-version error on anything
# older. Record it, so a failure on the laptop is one `cat` away from an
# explanation instead of a mystery.
glibc_floor() {
    readelf -V "$@" 2>/dev/null \
        | grep -o 'GLIBC_[0-9][0-9.]*' \
        | sort -u -V \
        | tail -1
}
GLIBC_FLOOR=$(glibc_floor "$APPDIR/usr/bin/$BIN_NAME" \
    ${BUNDLED[@]+$(printf "$APPDIR/usr/lib/%s " "${BUNDLED[@]%%|*}")} )
GLIBC_FLOOR=${GLIBC_FLOOR:-unknown}
echo "    glibc floor: ${GLIBC_FLOOR}"

BUNDLE_DOC="$APPDIR/usr/share/$BIN_NAME/BUNDLE.txt"
mkdir -p "$(dirname "$BUNDLE_DOC")"
{
    echo "NX Wisp $VERSION — what is inside this bundle"
    echo
    echo "built:          $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "glibc required: $GLIBC_FLOOR or newer on the host"
    echo
    echo "Bundled shared libraries (usr/lib):"
    if [[ ${#BUNDLED[@]} -eq 0 ]]; then
        echo "  (none — the binary needs only host-provided libraries)"
    else
        printf '  %s\n' "${BUNDLED[@]%%|*}"
    fi
    echo
    echo "Deliberately NOT bundled; taken from the host at runtime:"
    printf '  %s\n' $(printf '%s\n' ${HOSTED[@]+"${HOSTED[@]}"} | sort -u)
    echo "  libvulkan.so.1          (dlopen: wgpu)"
    echo "  libwayland-client.so.0  (dlopen: wayland-backend, sctk 'system')"
    echo "  libxkbcommon.so.0       (dlopen: xkbcommon-dl)"
    echo
    echo "Why: those three talk to the machine's own driver stack, compositor"
    echo "and input layer. A bundled Vulkan loader cannot see the host's ICD"
    echo "manifests, and a bundled libwayland-client is not the one the host"
    echo "compositor's protocol extensions were built against."
    echo
    echo "There are no runtime data files. The default skin, the KWin script,"
    echo "the wgpu shaders and the fleet narration rules are all include_str!'d"
    echo "into the binary."
} > "$BUNDLE_DOC"

# ---------------------------------------------------------------- validate

if command -v desktop-file-validate >/dev/null; then
    desktop-file-validate "$APPDIR/nx-wisp.desktop" \
        || { echo "!! desktop entry is not valid" >&2; exit 1; }
fi

# ---------------------------------------------------------------- squash

mkdir -p "$DIST"
rm -f "$DIST/$ASSET"
echo "==> appimagetool"
# -n skips the AppStream check: appstreamcli is not installed everywhere and a
# missing linter must not be able to block a release.
ARCH=x86_64 "$APPIMAGETOOL" \
    --runtime-file "$APPIMAGE_RUNTIME" \
    -n \
    "$APPDIR" "$DIST/$ASSET" >/dev/null

[[ -f "$DIST/$ASSET" ]] || { echo "appimagetool produced nothing" >&2; exit 1; }
chmod 0755 "$DIST/$ASSET"

[[ $KEEP_APPDIR -eq 1 ]] || rm -rf "$APPDIR"

SIZE=$(wc -c < "$DIST/$ASSET")
printf '==> %s  (%s bytes, %s)\n' "$DIST/$ASSET" "$SIZE" \
    "$(numfmt --to=iec --suffix=B "$SIZE" 2>/dev/null || echo "${SIZE}B")"
