<p align="center">
  <img src="assets/wisp.png" width="220" alt="Wisp">
</p>

<h1 align="center">NX Wisp</h1>
<p align="center"><b>A creature that lives on your desktop.</b></p>

<p align="center">
She roams your screen, stands on your windows, watches what your machine is
doing, thinks with a language model that never leaves it, and talks — in a
bubble, and out loud. And the moment you start a game or put on a headset,
she gets out of the way <i>completely</i>.
</p>

---

## What she does

- **She has a real body.** A `zwlr_layer_shell_v1` surface with a Vulkan
  swapchain, click-through everywhere she isn't — clicks land on her actual
  pixels and pass through everything else. She roams the whole output, falls,
  gets thrown, and lands on the top edges of your real windows.
- **She thinks locally.** llama.cpp on Vulkan — no ROCm, no CUDA, no cloud.
  A small reflex model stays resident; a 30B-class MoE loads for real
  conversation and is the first thing evicted. Tool calls are
  grammar-constrained at the decoder, so even a tiny model cannot emit a
  malformed one. Nothing you do ever leaves the machine.
- **She speaks.** Local Piper synthesis, streamed so she starts talking before
  the sentence is finished, ducking your music instead of talking over it —
  and un-ducking even if she is killed mid-word.
- **She earns your attention instead of spending it.** Every thought passes an
  interruption budget with a token bucket, flow detection, and staleness. A
  thought that missed its moment is dropped and recorded, never nagged. The
  flight recorder can answer *"why did you say that?"* from data.
- **She costs nothing when it matters.** A five-tier governor watches the GPU,
  thermals, and your fullscreen state. A game or a WiVRn session drops her to
  a sprite on the integrated GPU with the model fully unloaded — and what she
  wanted to think about queues, so when you quit she has been waiting, not
  switched off.
- **Nothing invisible.** Mic, clipboard and screen senses ship *off*, and any
  invasive sense that is live shows a hard visual tell on her body for the
  whole time. A consent panel counts every use.
- **She is part of the NX fleet.** She appears on the NX Connector bus,
  narrates the other NX apps — an NX Sentry alarm interrupts, a WiVRn session
  makes her wave goodbye — and can drive them through the `nx` CLI.

## Install

Through [NX Hub](https://github.com/nerdrx/nx-hub), which discovers releases
automatically — or grab the AppImage from
[Releases](https://github.com/nerdrx/nx-wisp/releases) and extract-install it:

```
./NX-Wisp-*.AppImage --appimage-extract
./squashfs-root/AppRun
```

Give her language and a voice (once, ~1–19 GB depending on which models you
pick, from pinned URLs with pinned SHA-256 hashes):

```
nx-wisp models          # what exists, what is fetched
nx-wisp models fetch    # running this IS the consent; nothing downloads unasked
```

Useful things to try:

```
nx-wisp doctor          # is this machine able to run her, and why not
nx-wisp say "hello"     # the whole voice stack, once, on demand
nx-wisp status          # her tier, and what she is costing you right now
nx-wisp explain         # why she said the last thing she said
nx-wisp edit            # the rig editor — pose her against the live renderer
nx-wisp tier pin full   # pin the governor while you watch her
```

## Requirements

Linux, Wayland, **KDE Plasma 6** (KWin ≥ 6.0), a Vulkan-capable GPU, PipeWire,
glibc ≥ 2.43. That list is deliberate and permanent — there are no X11, GNOME
or Windows fallbacks anywhere in the tree, which is a large part of why the
tree is small.

## How it's built

Thirteen Rust crates with one shared contract ([SPEC.md](SPEC.md)) and a few
rules that are enforced by structure rather than review: nothing can reach you
except through the attention budget; an invasive sense literally cannot
publish without a consent handle, and the handle's drop turns the tell off;
the character is data (a commented TOML skin), so redesigning her costs
nothing but taste. The renderer is wgpu + lyon with the
[NX design language](https://github.com/nerdrx/nx-hub/blob/main/docs/DESIGN.md)
ported natively — no browser engine anywhere, which is why her idle footprint
is measured in tens of megabytes.

1,700+ tests, none of which need a GPU, a model, or a compositor. The ones
that draw, assert on pixels; the ones that think, run against a deterministic
mock; the one that ships, is the same code either way.

## License

MIT. The signing key for releases is pinned in NX Hub, so what the hub
installs is what this repo built.
