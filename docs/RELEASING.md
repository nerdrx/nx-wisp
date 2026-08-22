# Releasing NX Wisp

SPEC §5: **release early and often.** Every milestone ships a real release
through NX Hub — an AppImage plus a `.sha256` and an ed25519 `.sig`.

This is the runbook. It is short on purpose; the scripts carry the detail.

---

## The short version

```sh
# 1. bump [workspace.package] version in Cargo.toml — the ONLY place a version lives
# 2. commit and PUSH (release.sh refuses to build otherwise; see "the scar")
scripts/release.sh
# 3. run the `gh release create ...` line it prints
```

`release.sh` never publishes. It builds, checksums, signs, smoke-checks and
then hands you a command. Publishing is the main loop's job.

---

## What each script does

| script | what it does |
| --- | --- |
| `scripts/release.sh` | the whole pipeline; prints the `gh` command, runs nothing |
| `scripts/build-appimage.sh` | cargo release build → AppDir → `dist/NX-Wisp-<version>-linux-x86_64.AppImage` |
| `scripts/smoke.sh` | verifies a built artifact the way NX Hub will use it |
| `scripts/gen-icons.sh` | re-rasterises `packaging/icons/*.png` from the SVG masters |

`build-appimage.sh` and `smoke.sh` are useful on their own:

```sh
scripts/build-appimage.sh --no-build --keep-appdir   # iterate on packaging
scripts/smoke.sh dist/NX-Wisp-0.1.0-linux-x86_64.AppImage
```

## Versioning

One source of truth: `[workspace.package] version` in the workspace
`Cargo.toml`. `build-appimage.sh` and `release.sh` both read that same field
with the same `awk`, so the asset name (`NX-Wisp-<version>-…`) and the tag
(`v<version>`) cannot drift apart. Nothing else anywhere holds a version — the
AppStream metainfo has a `@VERSION@` placeholder that is substituted at build
time.

NX Hub derives the version it displays and compares against from the **tag**,
stripped of a leading `v`. So the tag is what actually decides whether an
installed copy shows "update available".

---

## The scar: `gh` tags the remote, not your HEAD

`gh release create <tag>` with no `--target` creates the tag at whatever the
**remote's default branch** currently points at. Build from an unpushed commit
and you publish assets built from code the tag does not contain, and nothing
tells you. NX Sentry shipped that once.

`release.sh` closes it from both ends:

1. it pins `--target` to the exact HEAD sha in the command it prints, and
2. it **refuses to build at all** unless

   - the working tree is clean (a dirty tree means the artifact matches no
     commit whatsoever), and
   - `git ls-remote origin` says HEAD is already on the remote — either as a
     ref tip or as an ancestor of the remote default branch head, and
   - the tag `v<version>` does not already exist on the remote.

It asks `git ls-remote` rather than reading `refs/remotes/*` on purpose: a
stale remote-tracking ref will happily tell you a commit is pushed when it is
not.

`--dry-run` skips all of that (and says so). It leaves `--target` unresolved
in the printed command so a dry-run output can never be pasted into a real
publish by accident.

---

## Signing

The shared NX release key lives at
`/run/media/nerdrx/Lex/claude/tools/nx-signing/`, **outside every repository**,
and must stay there. Scheme, from that directory's README:

```
<asset>.sig = ed25519(privkey, sha256(asset))    lowercase hex, one line
```

The signature covers the **digest**, not the file bytes — so signing costs one
hash pass and NX Hub can reuse the hash its download path already computed.

The hub pins **one key per GitHub owner**, not per repo
(`nx-hub/src/main/provenance.js`, `PINNED_KEYS.nerdrx`), so NX Wisp signs with
the same key as NX Hub, NX Sentry and PulseNX and **no hub change is needed**
as long as the owner stays `nerdrx`.

`smoke.sh` re-derives the raw 32-byte public key from
`nx-signing/nx-release.pub` and asserts it equals the hex the hub pins. A
swapped or regenerated key therefore fails the smoke check here rather than
producing a release the hub silently refuses later.

A machine without the private key still cuts a release; it is just unsigned.
That installs fine unless the operator has turned on `requireSignatures` in NX
Hub (default off).

---

## How NX Hub finds this repo

**Nothing to configure.** Confirmed against `nx-hub/SPEC.md`, not assumed:

- The hub auto-discovers from `settings.owners`, which defaults to
  `["nerdrx"]`. Every repo of that owner is scanned; a public repo needs no
  token. For each repo it fetches the **latest release** and classifies the
  assets.
- Asset classification: `*.AppImage` → platform `linux`, kind `appimage`.
  `*.sha256` and `*.sig` are on the ignore list as installable artifacts, but a
  sibling `<asset>.sha256` **is** used to verify the download, and a sibling
  `<asset>.sig` sets `artifact.hasSignature` and is verified against the pinned
  owner key.
- A repo with no classifiable release lands in the greyed-out "Unpublished"
  section. That is where `nerdrx/nx-wisp` sits until the first release.

So: a plain public release carrying one `.AppImage` plus its two sidecars shows
up in NX Hub on its own, with no registry edit and no hub release.

What we do *not* get for free is presentation. Without an entry in
`nx-hub/registry/overrides.json` the card shows the raw repo name (`nx-wisp`)
and the GitHub repo description as its tagline, ordered at the default 100.
There is no entry today. Adding one — display name "NX Wisp", a tagline, an
order hint — is a **change in the nx-hub repo**, not here, and is optional.
The overlay is also fetched live from nx-hub's main branch, so it can be added
later without shipping a new hub.

### How the hub installs it

Kind `appimage`, and this is why the packaging looks the way it does: the hub
**never runs the AppImage**. CachyOS has no libfuse2, so the hub downloads it,
`chmod +x`, runs it once with `--appimage-extract` (which the AppImage runtime
handles without FUSE), moves `squashfs-root` into
`~/Applications/nx/nx-wisp/<artifactId>/`, keeps the original single file
alongside for machines that do have FUSE, and writes a desktop entry whose
`Exec` is the extracted `AppRun`. Launching runs that `AppRun`.

`smoke.sh` therefore tests the extracted tree, never the mounted one.

The launcher tile's icon comes from the hub's `findIcon()`, which takes
`.DirIcon` first and follows it if it is a symlink. Ours points at the 512px
raster, so the tile shows *her* and not the generic NX mark — which DESIGN.md
§8 forbids substituting for a real app's identity.

---

## What is bundled, and why

The interesting half of `build-appimage.sh` is a policy, not a file list. It
runs `ldd` on the release binary and sorts every resolved library into
"bundle" or "the host's", and it **hard-fails** on anything unresolved rather
than shipping a bundle that is missing a library.

**Today it bundles nothing.** The binary needs `libc`, `libm` and `libgcc_s`,
and all three are the host's. That is not an oversight, it is the shape of the
dependency tree: everything NX Wisp needs from the graphics and Wayland stack
is `dlopen`'d at runtime and is invisible to `ldd` in the first place —

| library | opened by | why it can never be bundled |
| --- | --- | --- |
| `libvulkan.so.1` | wgpu, via `libloading` | a bundled loader cannot see the host's ICD manifests, so it finds no GPU |
| `libwayland-client.so.0` | `wayland-backend`, via smithay-client-toolkit's `system` feature (pinned in the workspace `Cargo.toml`) | not the library the host compositor's protocol extensions were built against |
| `libxkbcommon.so.0` | `xkbcommon-dl` | keymaps come from the host's session, not from us |

They are on the deny list anyway, so that a future dependency which links one
of them *directly* trips the policy instead of quietly shipping a copy. The
same list covers the rest of the machine's own stack: the glibc family and the
loader, the compiler runtimes, GL/EGL/GLX and the vendor driver libraries,
libdrm/libgbm/libepoxy and Mesa internals, X11 (which SPEC §1 forbids in the
tree but a transitive dep could still drag in), libdbus-1/libsystemd/libudev,
PipeWire/ALSA/PulseAudio, and the ubiquitous compression libraries.

Everything *else* that `ldd` resolves gets bundled into `usr/lib`. That is the
case this policy is really written for: when `wisp-mind` links llama.cpp or
`wisp-voice` links whisper, those `.so`s are ours, the host has never heard of
them, and they land in the bundle automatically with no edit to this script.

`AppRun` **appends** `usr/lib` to `LD_LIBRARY_PATH` rather than prepending it,
so a bundled library can never shadow a host one. `smoke.sh` independently
asserts that no host-stack library ended up in `usr/lib`, so a regression in
the policy fails the check instead of reaching the laptop.

There are **no runtime data files**. The default skin, the KWin script, the
wgpu shaders and the fleet narration rules are all `include_str!`'d into the
binary, so the AppDir is a binary, a desktop entry and icons.

### The one portability hazard left

glibc is deliberately not bundled — bundling it without the matching `ld.so`
is the classic way to make an unrunnable AppImage. The consequence is that the
artifact runs on any host whose glibc is **at least as new as the build
machine's**, and dies with a symbol-version error on anything older.

Both machines are CachyOS/Arch-family, so in practice this is fine, but "the
laptop is not identical" is exactly the assumption that bites. So the floor is
recorded rather than assumed: `build-appimage.sh` computes it with `readelf`,
prints it, writes it into `usr/share/nx-wisp/BUNDLE.txt` inside the bundle,
and `release.sh` puts it in the release notes. If she ever fails to start on
the laptop with a `GLIBC_2.xx not found` message, that file is the
explanation, and the fix is to build on the older machine.

The current floor is **GLIBC_2.34**.

---

## First-time install on a fresh machine

NX Hub handles the download, verification, extraction, desktop entry and icon.
What the *machine* still has to provide:

1. **KDE Plasma 6 on Wayland, KWin ≥ 6.0.** SPEC §1 makes this permanent:
   `zwlr_layer_shell_v1` and KWin's D-Bus scripting interface are hard
   dependencies and there is no X11 or GNOME fallback anywhere in the tree.
   She will not start on anything else, and that is by design.
2. **A Vulkan-capable GPU with a working ICD.** `vulkaninfo --summary` should
   list a device. This is a host-side check: the loader and the driver are
   never in the bundle.
3. **KWin's scripting D-Bus interface reachable** — `org.kde.KWin` /
   `/Scripting`. Nothing to install: the companion KWin script is embedded in
   the binary and `wisp-senses` writes it into the runtime dir and calls
   `loadScript` **fresh on every start** (see
   `crates/wisp-senses/src/kwin/script.rs`). There is no packaging step for
   it, no file to copy into `~/.local/share/kwin/scripts`, and nothing to
   uninstall — `loadScript` touches no KWin config and no KWin setting. If the
   session is not KWin, or the interface is not reachable, that is the failure
   to look at first.
4. **A `~/.local/share/applications` that the desktop actually reads.** The
   hub writes `nx-wisp-<artifactId>.desktop` there and runs
   `update-desktop-database` when it is available.

Nothing else. There are no models to fetch for a bare start, no system
packages, and no step that needs root.

---

## Icons

Three masters in `packaging/icons/`, per DESIGN.md §8 — one file cannot span
16→512px:

| file | used for | notes |
| --- | --- | --- |
| `icon.svg` | 48, 64, 128, 256, 512 | full facets, sheen, lit edge, aura |
| `icon-small.svg` | 16, 24, 32 | wider crystal, flat fills, no edge |
| `tray.svg` | tray, if one is ever added | flat violet, knocked-out holes |

The mark is *her*: the default skin's own `shell`, `core`, `spark`, facets,
eyes and lit edge, mapped out of `crates/wisp-rig/skins/wisp.skin.toml` onto a
512px canvas. Each SVG documents its transform and every deliberate departure.

The PNGs are **committed**, so cutting a release never needs a rasteriser
installed. `gen-icons.sh` regenerates them (needs `rsvg-convert`); run it by
hand after editing a master and commit the result.

> **Coupling worth knowing about.** The icon is derived from the shipped
> default skin. SPEC §3.5b says the geometry rule governs chrome, not the
> character — if the default skin is ever re-authored rounder, `icon.svg` and
> `icon-small.svg` have to be re-authored with it or the launcher tile stops
> looking like the creature on screen.

---

## The smoke check

`scripts/smoke.sh` is what protects a live-testing trip from a broken build.
`release.sh` runs it automatically. Every check is fatal; a `warn` means "this
cannot be checked yet", never "this failed but carry on". It:

- verifies the `.sha256` and the ed25519 `.sig`, and asserts the public key is
  the one nx-hub pins;
- runs `--appimage-extract` — **the hub's actual install path**, and the check
  that catches a bundle which only works where FUSE exists;
- asserts `AppRun`, `usr/bin/nx-wisp`, both desktop entries, the metainfo, the
  hicolor icon set and a `.DirIcon` that resolves;
- validates the desktop entry with `desktop-file-validate`;
- asserts no host-stack library was bundled;
- runs the extracted `AppRun --version` under a 30 s timeout **in a throwaway
  HOME with `NX_WISP_CONFIG_DIR` pointed at a temp dir**. SPEC §4: the dev
  build and the installed copy otherwise share state, and this script runs on
  the same machine she lives on. Never relax that.
- re-runs `ldd` on the installed binary and records the glibc floor.

### Known gap: `--version`

The version assertion is **strict by default** — it fails unless `--version`
prints the workspace version. The binary does not implement `--version` yet,
so until it does, pass `--lax-version` (or set
`NX_WISP_SMOKE_LAX_VERSION=1`) to downgrade that one check to a warning.

Delete this section, and stop passing the flag, the moment `wisp` implements
`--version`. It is the check that catches "you shipped last release's binary".

---

## CI

`.github/workflows/ci.yml` builds the workspace, packages an AppImage and runs
the smoke check on every push and PR. It is deliberately **not** a release
pipeline: there is no signing key on a runner and it never publishes.

It does not run the test suite. SPEC §4 puts the compositor tests against a
nested `kwin_wayland` and the GPU tests against a real device, neither of
which a stock runner has; running a partial suite there would be a false
"green". Tests are a local gate.

---

## Troubleshooting

**"could not read `[workspace.package]` version"** — you are not in the repo
root, or the `[workspace.package]` table moved. Both scripts parse it with the
same `awk`.

**appimagetool or the runtime is missing** — `build-appimage.sh` downloads
both into `/run/media/nerdrx/Lex/claude/tools/` on first use. No root, no
system packages. Override with `APPIMAGETOOL` / `APPIMAGE_RUNTIME` /
`NX_TOOLS`. appimagetool is kept **extracted** (as `tools/appimagetool/AppRun`)
for the same reason the hub extracts ours: no libfuse2 on this box.

**"unresolved shared libraries — refusing to package"** — `ldd` could not
resolve something on the *build* machine, so the policy cannot know whether it
is ours or the host's. Install the missing library and rebuild; do not work
around it.

**She starts from `cargo run` but not from the AppImage** — check
`usr/share/nx-wisp/BUNDLE.txt` inside the bundle first, then run the extracted
`AppRun` from a terminal. `NX_WISP_PACKAGED=appimage` is exported by `AppRun`,
so the flight recorder can tell the two apart.
