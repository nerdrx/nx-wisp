<h1 align="center">NX Wisp</h1>
<p align="center">A creature that lives on your desktop.</p>

NX Wisp is a desktop companion for KDE Plasma on Wayland. She walks around your
screen, sits on your windows, watches what your machine is doing, thinks with a
local language model, and talks to you — and she gets out of the way completely
the moment you need your GPU back.

- **She has a body.** A `zwlr_layer_shell_v1` surface rendered with wgpu and
  vello, click-through everywhere she isn't, so she can go anywhere on screen
  and sit on top of your actual windows.
- **She thinks locally.** llama.cpp on Vulkan. Nothing leaves the machine.
- **She costs you nothing when it matters.** A five-tier resource governor
  unloads her brain and moves her rendering to the iGPU the moment a game or a
  VR session starts, and gives it all back when you're done.
- **She's part of the fleet.** Speaks the NX Connector bus, so she narrates and
  drives every other NX app.

Part of the NX suite. Install through [NX Hub](https://github.com/nerdrx/nx-hub).

## Status

Early. See [SPEC.md](SPEC.md) for the frozen contract and the milestone plan.

## Requirements

Linux, Wayland, KDE Plasma 6. A Vulkan-capable GPU.
