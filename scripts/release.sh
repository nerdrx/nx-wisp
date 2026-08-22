#!/usr/bin/env bash
# Build, checksum, sign and smoke-check a release, then PRINT the publish
# command. It never publishes: the main loop runs the `gh` line itself.
#
#   scripts/release.sh [--dry-run] [--skip-smoke] [--notes FILE]
#
#   --dry-run     build and sign, but skip every check that talks to the git
#                 remote. Use it to iterate on packaging. The printed publish
#                 command is deliberately left unresolved in this mode.
#   --skip-smoke  do not run scripts/smoke.sh. Only for debugging the build.
#                 (To relax only the --version assertion, which `wisp` does not
#                 implement yet, set NX_WISP_SMOKE_LAX_VERSION=1 instead.)
#   --notes FILE  release notes body; one is generated if you do not pass one.
#
# Assets carry two sidecars, matching the rest of the NX family:
#   <asset>.sha256  the digest NX Hub verifies every download against
#   <asset>.sig     ed25519 over that digest, from the per-owner NX release key
# The key lives OUTSIDE every repository and is never copied into one.
set -euo pipefail
cd "$(dirname "$0")/.."

REPO=nerdrx/nx-wisp
KEY="${NX_SIGNING_KEY:-/run/media/nerdrx/Lex/claude/tools/nx-signing/nx-release.key}"
DRY_RUN=0
SKIP_SMOKE=0
NOTES_FILE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run|-n) DRY_RUN=1; shift ;;
        --skip-smoke) SKIP_SMOKE=1; shift ;;
        --notes)      NOTES_FILE="${2:-}"; shift 2 ;;
        -h|--help)    sed -n '2,18p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 1 ;;
    esac
done

# ---------------------------------------------------------------- version

# Single source of truth: [workspace.package] version in the workspace
# Cargo.toml. Bump it there and nowhere else. build-appimage.sh reads the same
# field the same way, so the asset name and the tag cannot drift apart.
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
TAG="v$VERSION"
ASSET="NX-Wisp-$VERSION-linux-x86_64.AppImage"

echo "==> NX Wisp $VERSION  (tag $TAG, repo $REPO)"

# ---------------------------------------------------------------- vcs preflight

# NX Sentry's scar, and the reason this block exists:
#
#   `gh release create <tag>` with no --target creates the tag at whatever the
#   REMOTE's default branch currently points at. Build locally from an unpushed
#   commit and you publish assets built from code the tag does not contain —
#   and nothing anywhere tells you. The artifact and the source disagree
#   forever.
#
# Two things fix it and we do both:
#   1. the printed publish command pins --target to the exact HEAD sha, and
#   2. that sha has to already exist on the remote, or the tag cannot resolve.
#
# So: refuse unless the tree is clean AND HEAD is on the remote. Asking
# `git ls-remote` rather than reading refs/remotes/* is deliberate — a stale
# remote-tracking ref will happily tell you a commit is pushed when it is not.
HEAD_SHA=""
if [[ $DRY_RUN -eq 1 ]]; then
    echo "==> dry run: NOT checking that HEAD is pushed"
else
    git rev-parse --git-dir >/dev/null 2>&1 || { echo "not a git repository" >&2; exit 1; }

    if [[ -n "$(git status --porcelain)" ]]; then
        echo "!! the working tree is dirty. The artifact would match no commit at all." >&2
        echo "!! Commit or stash, then re-run." >&2
        git status --short | sed 's/^/!!   /' >&2
        exit 1
    fi

    HEAD_SHA=$(git rev-parse HEAD)

    REMOTE_TIPS=$(git ls-remote origin 2>/dev/null || true)
    if [[ -z "$REMOTE_TIPS" ]]; then
        echo "!! could not reach the git remote 'origin'." >&2
        echo "!! Refusing to build a release whose tag cannot be verified." >&2
        exit 1
    fi

    REMOTE_HEAD=$(printf '%s\n' "$REMOTE_TIPS" | awk '$2 == "HEAD" { print $1; exit }')

    on_remote=0
    # HEAD is itself a remote ref tip (default branch, or a pushed branch)...
    printf '%s\n' "$REMOTE_TIPS" | awk '{print $1}' | grep -qx "$HEAD_SHA" && on_remote=1
    # ...or it is an ancestor of the remote's default branch head.
    if [[ $on_remote -eq 0 && -n "$REMOTE_HEAD" ]] \
        && git merge-base --is-ancestor "$HEAD_SHA" "$REMOTE_HEAD" 2>/dev/null; then
        on_remote=1
    fi

    if [[ $on_remote -eq 0 ]]; then
        echo "!! HEAD ($HEAD_SHA) is not on the remote." >&2
        echo "!! gh would tag the remote default branch instead, and the release" >&2
        echo "!! would ship assets built from code the tag does not contain." >&2
        echo "!! Push first, then re-run." >&2
        exit 1
    fi
    echo "    HEAD $HEAD_SHA is on the remote"

    if printf '%s\n' "$REMOTE_TIPS" | awk '{print $2}' | grep -qx "refs/tags/$TAG"; then
        echo "!! tag $TAG already exists on $REPO — bump [workspace.package] version" >&2
        exit 1
    fi
    echo "    tag $TAG is free"
fi

# ---------------------------------------------------------------- build

echo "==> building"
scripts/build-appimage.sh

[[ -f "dist/$ASSET" ]] || { echo "expected dist/$ASSET, not found" >&2; exit 1; }

# ---------------------------------------------------------------- checksum

echo "==> checksum"
# Run inside dist/ so the .sha256 holds a bare filename and `sha256sum -c`
# works from wherever the pair was downloaded to.
( cd dist && sha256sum "$ASSET" > "$ASSET.sha256" )
echo "    $ASSET.sha256  $(cut -d' ' -f1 < "dist/$ASSET.sha256")"

# ---------------------------------------------------------------- signature

# tools/nx-signing/README.md: sig = ed25519(privkey, sha256(asset)), lowercase
# hex on one line. The signature covers the DIGEST, not the file bytes, so
# signing costs one hash pass and NX Hub can reuse the hash its download path
# already computed. The hub pins one public key per GitHub owner, so this is
# the same key every other NX repo signs with.
SIGNED=0
if [[ -r "$KEY" ]]; then
    echo "==> signing (ed25519)"
    node -e 'const fs=require("fs"),c=require("crypto");
      const d=c.createHash("sha256").update(fs.readFileSync(process.argv[1])).digest();
      const k=c.createPrivateKey(fs.readFileSync(process.argv[2],"utf8"));
      process.stdout.write(c.sign(null,d,k).toString("hex")+"\n");' \
      "dist/$ASSET" "$KEY" > "dist/$ASSET.sig"
    SIGNED=1
    echo "    $ASSET.sig  $(cut -c1-24 < "dist/$ASSET.sig")..."
else
    # An unsigned release is still installable; it just will not verify for
    # anyone who has turned on "require signatures" in NX Hub.
    echo "!! signing key not readable at $KEY — this release would be UNSIGNED"
    rm -f "dist/$ASSET.sig"
fi

# ---------------------------------------------------------------- smoke

if [[ $SKIP_SMOKE -eq 0 ]]; then
    echo "==> smoke check"
    scripts/smoke.sh "dist/$ASSET"
else
    echo "==> smoke check SKIPPED (--skip-smoke) — you are on your own"
fi

# ---------------------------------------------------------------- notes

# The one portability fact a downloader actually needs: glibc is not bundled,
# so the artifact runs on any host at least as new as the build machine.
GLIBC_FLOOR=$(readelf -V target/release/nx-wisp 2>/dev/null \
    | grep -o 'GLIBC_[0-9][0-9.]*' | sort -u -V | tail -1)
GLIBC_FLOOR=${GLIBC_FLOOR:-GLIBC (unknown)}

# Notes land in dist/, NOT in a temp file. This script prints a `gh` command
# for someone else to run later, and an exit-trap'd tempfile would be gone by
# the time they ran it.
if [[ -z "$NOTES_FILE" ]]; then
    NOTES_FILE="dist/RELEASE-NOTES-$VERSION.md"
    {
        echo "NX Wisp $VERSION — a creature that lives on your desktop."
        echo
        echo "Install through [NX Hub](https://github.com/nerdrx/nx-hub), which"
        echo "picks this release up automatically."
        echo
        echo "### Requirements"
        echo
        echo "- KDE Plasma 6 on Wayland (KWin >= 6.0). No X11, no GNOME."
        echo "- A Vulkan-capable GPU."
        echo "- $GLIBC_FLOOR or newer. glibc is deliberately not bundled; see"
        echo "  \`usr/share/nx-wisp/BUNDLE.txt\` inside the AppImage for the full list."
        echo
        echo "### Verify"
        echo
        echo '```'
        echo "sha256sum -c $ASSET.sha256"
        echo '```'
        echo
        if [[ $SIGNED -eq 1 ]]; then
            echo "The \`.sig\` sibling is an ed25519 signature over the asset's sha256,"
            echo "which NX Hub checks against a pinned key before it installs anything."
            echo "Nothing to do by hand — it verifies itself."
            echo
        fi
        echo "### Running it by hand"
        echo
        echo "CachyOS and most modern distributions have no libfuse2, so the single"
        echo "file will not mount. Extract it instead — which is exactly what NX Hub"
        echo "does:"
        echo
        echo '```'
        echo "chmod +x $ASSET"
        echo "./$ASSET --appimage-extract"
        echo "./squashfs-root/AppRun"
        echo '```'
    } > "$NOTES_FILE"
fi

# ---------------------------------------------------------------- hand off

ASSETS=("dist/$ASSET" "dist/$ASSET.sha256")
[[ $SIGNED -eq 1 ]] && ASSETS+=("dist/$ASSET.sig")

echo
echo "==> artifacts"
for a in "${ASSETS[@]}"; do printf '    %-52s %s\n' "$a" "$(wc -c < "$a") bytes"; done
echo
echo "==> release notes ($NOTES_FILE)"
sed 's/^/    /' "$NOTES_FILE"
echo
echo "==> NOT publishing. Run this to publish:"
echo
if [[ $DRY_RUN -eq 1 ]]; then
    TARGET='"$(git rev-parse HEAD)"'
    echo "    # dry run: --target left unresolved, and HEAD was NOT checked"
    echo "    # against the remote. Re-run without --dry-run before publishing."
else
    TARGET="$HEAD_SHA"
fi
printf '    gh release create %s \\\n' "$TAG"
printf '        --repo %s \\\n' "$REPO"
printf '        --target %s \\\n' "$TARGET"
printf '        --title "NX Wisp %s" \\\n' "$VERSION"
printf '        --notes-file %s \\\n' "$NOTES_FILE"
printf '        %s\n' "${ASSETS[*]}"
echo
