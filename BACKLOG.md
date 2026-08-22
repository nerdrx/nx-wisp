# NX Wisp — backlog

Work that is real, specified, and deliberately not being done yet. Nothing here
blocks a release; each item names what unblocks it.

## Needs a change in another repo

### 1. NX Hub: `relay` verb, for the fleet hop (F47) — *deferred, to do with Fable*

`wisp-fleet`'s hop is written and tested against a mock, and is **inert until
the hub side exists**. NX Hub v0.10's fleet session carries only hub-to-hub
verbs; there is no app→peer-app path, and wisp must not dial `:9023` herself
(she would collide with the hub's own session for that peer id).

Needed, all additive — no change to pairing, MAC or seq:

1. **Connector bus** — client→hub `{"type":"relay","peer":"<hub id|*>","app":"nx-wisp","body":{…}}`;
   hub→client `{"type":"relay","peer","app","body"}` delivered to the local
   client holding that app id, gated on a `relay` capability; plus
   `{"type":"relay-ack","ok",…}`. **Must be exempt from the 4/s status
   throttle.** Body cap 12 KB.
2. **Fleet session** — forward `{"type":"app-relay", app, from, body}` over the
   existing HMAC'd session.
3. *Nice to have* — `welcome` advertising hub capabilities (today support is
   detected by probing and reading back `unknown type: relay`), and read access
   to the local hub's fleet id and peer list for connector clients (otherwise we
   read `fleet.json`, 0600, same user).

### 2. NX Sentry and WiVRn NX don't join the Connector bus

Neither repo has connector code, so their field names in
`crates/wisp-fleet/src/rules.json` (`tripped`/`armed`, `session`) are
**proposals**, flagged in that file's `notes`. Only PulseNX's `{hr, connected}`
is real today. Wiring those two up is small — vendor `nx-connector.js` — and it
is what makes F45's narration actually fire.

### 3. DESIGN.md v1.5 — two amendments for nx-hub

- **§4 contradicts itself.** The table points Bar→`--glass-bar` (alpha
  0.62–0.72) and Sheet→`--glass-2` (0.66); the prose demands floating fills
  ≥ 0.85. `wisp-theme` resolved in favour of the prose and keeps the old values
  as `GLASS_BAR_LEGACY`/`GLASS_2_LEGACY` with a test asserting they fail the
  floor.
- **At ≥0.85, backdrop blur is invisible** but still costs a texture copy per
  floating layer — in NX Hub and every other NX app, not just here. Either drop
  the blur on floating layers, or lower the floor back toward 0.7 so it reads.
  **Operator's call; not being made unilaterally.**

## `wisp-proto` amendment batch (apply once the wave lands)

- `TierReason` has no CPU or memory pressure variant, so a heavy compile is
  reported as `HeavyProcess` naming the top consumer. Add `CpuPressure` /
  `MemoryPressure`.
- **No shared `Mood` type.** SPEC §2 gives the mood FSM to `wisp-mind`, but
  `wisp-attn`'s behaviour trees must *read* mood. Defined locally in `wisp-attn`
  for now; promote to proto and re-export.
- SPEC §2 says `wisp-paint` owns "vello scene building". It does not and cannot
  at wgpu 30 — reword to the tessellation path.
- SPEC §3.1 says downgrades are "synchronous and immediate". `Painter::set_tier`
  lands next frame, a frame being its unit of work. Reword, or state the
  exception.
- The plan's §12 dependency table says `wisp-attn` depends on `wisp-senses`;
  SPEC §2 says proto only. **SPEC is right** (it is what keeps the crate pure).
  Fix the plan.

## Deferred implementation

- **Drop shadows are tokens only.** `SHADOW`, `SHADOW_LIFT`, `SHADOW_BAR`,
  `SHADOW_SHEET` are ported faithfully but no Gaussian drop shadow is drawn yet;
  only `FOCUS_RING` is painted. Biggest remaining visual gap in `wisp-paint`.
- **No glyph atlas** — one cached texture per run. Fine for a companion; the rig
  editor's dense text will want a real one.
- **Nebula drift is not animated** — blob tokens exist; the 60–110 s transform
  loop belongs to whoever owns the clock.
- **"NX Hub has an update" has no automatic source.** Computing it means
  reimplementing hub discovery or polling GitHub, and unasked egress violates
  SPEC §0.2. `Fleet::observe()` lets the binary hand the fact in.
- **Roster latency**: the hub debounces its snapshot to ~1/s, so a Sentry trip
  reaches her 1–3 s late. A hub-side `observe` capability would make alarms
  immediate. Related to item 1.
