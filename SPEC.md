# NX Wisp — SPEC

**This file is the frozen contract.** Crates own disjoint files; everything they
share is defined here and implemented in `wisp-proto`. Change this file only by
deliberate amendment, never as a side effect of implementing something.

## 0. Charter

1. **She costs nothing when it matters.** Any feature that can make a game drop a
   frame must be reachable by the governor and must be sheddable. If a subsystem
   cannot be degraded, it does not ship.
2. **Local only.** No network egress except (a) model downloads from pinned URLs
   with pinned hashes, (b) the NX Connector / fleet bus on the LAN, (c) tools the
   operator explicitly enabled. No telemetry, ever.
3. **Nothing invisible.** Mic, clipboard and screen access have a visible tell on
   the character herself whenever they are live, and every use is recorded.
4. **She is honest.** The flight recorder holds the real trace. "Why did you say
   that?" is answerable from data, not from a plausible story she makes up.
5. **One operator, one machine.** She is not multi-user and never will be.

## 1. Hard target

Linux. Wayland. KDE Plasma 6 / KWin ≥ 6.0. Vulkan. **No X11, no GNOME, no
Windows, no macOS fallbacks anywhere in the tree** — do not add cfg branches or
abstraction layers for them. `zwlr_layer_shell_v1` and KWin's D-Bus scripting
interface are hard dependencies, and that is a deliberate, permanent choice.

## 2. Crate map and ownership

An agent owns its crate's files exclusively. It never edits another crate, never
edits this file, and never runs `git`.

| Crate | Owns | May depend on |
|---|---|---|
| `wisp-proto` | Every shared type in §3. No logic. | — |
| `wisp-theme` | DESIGN.md token port: palette, radii, easings, glass tiers, type ramp | proto |
| `wisp-paint` | wgpu device/surface, vello scene building, widget layer, text, sprite-atlas baker | proto, theme |
| `wisp-shell` | sctk, layer surface, input regions, outputs, seats, focus, cursor | proto, paint |
| `wisp-rig` | Skeleton, mesh deform, IK, clip playback, skin format parse/serialise | proto, paint |
| `wisp-editor` | The in-app rig editor (F76) | proto, rig, paint, theme |
| `wisp-gov` | Tier ladder, GPU/VRAM/CPU probes, process detection, cgroups, device selection | proto |
| `wisp-mind` | llama.cpp, GBNF grammars, memory + embeddings, tool registry, mood FSM | proto, gov |
| `wisp-senses` | KWin script + D-Bus, idle, MPRIS, PipeWire, notifications, clipboard, consent | proto |
| `wisp-voice` | whisper STT, Kokoro TTS, lip-sync envelope, ducking | proto, gov |
| `wisp-attn` | Interruption budget, flow detection, behaviour trees | proto |
| `wisp-fleet` | NX Connector client, fleet narration, `nx` CLI wrappers | proto |
| `wisp` | Binary: wiring, event loop, CLI, config, flight recorder | all |

## 3. Shared contracts (implemented in `wisp-proto`)

### 3.1 `Tier` — the governor's verdict

```
T0 Feral | T1 Full | T2 Reduced | T3 Lobotomised | T4 Dormant
```

Every subsystem implements `Governed`:

```rust
pub trait Governed {
    /// Called on every tier change. Must not block. Must not fail.
    fn set_tier(&mut self, tier: Tier, reason: &TierReason);
    /// Worst-case resident cost at a tier, for the governor's accounting.
    fn cost_at(tier: Tier) -> Cost;
}
```

Downgrades are applied **synchronously and immediately**; upgrades are lazy and
may be deferred. A subsystem that cannot honour a downgrade must shed the work,
not queue it — except `wisp-mind`, whose deferred queue is specified in §3.5.

### 3.2 `Event` — the internal bus

One broadcast channel. Every subsystem publishes; anything may subscribe. Every
event is recorded by the flight recorder before dispatch. Events are **facts
about the past**, never commands.

### 3.3 `Observation` — what the senses saw

A closed enum, deliberately, in the spirit of NX Orbit's `ObsKind`: adding a
kind is a spec amendment, not an implementation detail. Every `Observation`
carries the `SenseId` that produced it and the `Consent` level it required.

### 3.4 `Utterance` and the attention budget

Nothing reaches the operator except as an `Utterance` submitted to `wisp-attn`,
which holds the token bucket. `wisp-mind` may not speak directly. An `Utterance`
carries `urgency`, `cost`, `decay`, and a `defer_until` hint; `wisp-attn` decides
if and when it is said.

### 3.5 Deferred cognition

At T3/T4 `wisp-mind` accepts work into a bounded queue instead of running it.
On upgrade the queue is replayed **oldest-first with staleness filtering** — an
item whose `stale_after` has passed is dropped, recorded as dropped, and never
silently resurrected.

### 3.6 Skin format

Declarative and data-only. A skin can never contain executable code. Versioned;
`wisp-rig` owns parsing and is the only crate that may change the format.

### 3.7 Tools and consent

```
Consent: Ambient | Explicit | Invasive
```

`Ambient` tools and senses may run unprompted. `Explicit` require the operator to
have enabled them. `Invasive` (mic, clipboard, screen) additionally require the
visible tell of §0.3 while active. Defaults ship as: ambient on, explicit off,
invasive off.

## 4. Testing rules

- **`NX_WISP_CONFIG_DIR` must be set to a temp dir by every test.** The dev build
  and the installed copy otherwise share state, and test fixtures then write
  into the operator's real memory. This has bitten this suite before (NX Orbit,
  2026-08-20). No exceptions.
- Pure modules (rig math, IK, budget scheduler, grammar builder, tier ladder,
  memory decay, skin parse) are unit-tested with no GPU and no compositor.
- GPU tests render offscreen and assert on read-back pixels — the pattern proven
  by the M0 gate.
- Compositor tests run against a nested `kwin_wayland`, never the live session.
- A mock inference backend means the suite never needs a model or a GPU.

## 5. Milestones

M0 skeleton · M1 it lives · M2 it thinks · M2.5 it behaves · M3 it talks ·
M4 it pays attention · M5 it joins the fleet · M6 extras. Full detail in
`nx-wisp-plan.md` (F1–F76).

**Release early and often.** Every milestone ships a real release through NX Hub:
AppImage (extract-install; CachyOS has no libfuse2) + `.sha256` + ed25519 `.sig`
using the shared key at `tools/nx-signing`.
