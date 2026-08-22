//! `wisp-mind` — cognition. SPEC.md §2, §3.4, §3.5, §3.7.
//!
//! > *She thinks with a local model, on the operator's own machine, and nothing
//! > leaves it.*
//!
//! # The shape of it
//!
//! ```text
//!                                  Mind
//!   ┌──────────────────────────────────────────────────────────────────┐
//!   │  observe(Observation) ──▶ mood::MoodFsm ──▶ prompt::State         │
//!   │                                                  │                │
//!   │  think(Ask) ──▶ escalate::triage ──▶ rung        ▼                │
//!   │       │                             prompt::PromptBuilder         │
//!   │       │  (T3/T4)                     ├─ persona core  ── cached   │
//!   │       ▼                              └─ state block   ── volatile │
//!   │  defer::DeferQueue                               │                │
//!   │       │  replay, oldest first,          kv::KvCache (F15)         │
//!   │       │  staleness filtered                      │                │
//!   │       ▼                                          ▼                │
//!   │  manager::ModelManager ─────────▶ backend::Backend                │
//!   │       ▲  load / warm / cold        (llama.cpp or mock)            │
//!   │       │                                          │                │
//!   │  wisp-gov: DeviceChoice + VramBudget    grammar::tool_grammar     │
//!   │                                                  │                │
//!   │  tools::ToolRegistry ◀── consent ─────── constrained decode       │
//!   │  memory::Memory ── recall, decay, consolidate                     │
//!   └──────────────────────────────────────────────────────────────────┘
//!                                    │
//!                     take_outbox() → Vec<Utterance> → wisp-attn
//! ```
//!
//! # Features (nx-wisp-plan.md §3)
//!
//! | Feature | Module |
//! |---|---|
//! | **F12** two-tier inference | [`backend`], [`manager`] |
//! | **F14** grammar-constrained tool calls | [`grammar`] |
//! | **F15** persistent KV cache | [`kv`], [`prompt`] |
//! | **F16** tool registry + consent | [`tools`] |
//! | **F17** escalation ladder | [`escalate`] |
//! | **F18** memory, recall, decay | [`memory`] |
//! | **F19** mood state machine | [`mood`] |
//! | **F55** model registry + fetch | [`models`], [`fetch`] |
//! | SPEC §3.5 deferred cognition | [`defer`] |
//!
//! F13 (the VRAM budget) is `wisp-gov`'s; this crate is the
//! [`wisp_proto::Governed`] that acts on it.
//!
//! # Two rules the whole crate is arranged around
//!
//! **SPEC §3.4 — she does not speak.** Everything she wants to say leaves as an
//! [`wisp_proto::Utterance`] from [`mind::Mind::take_outbox`], and `wisp-attn`
//! decides whether any of it is said. There is no path from here to a terminal,
//! a notification or a pixel, and `tests/no_speaking.rs` checks that
//! mechanically against the source rather than trusting it.
//!
//! **SPEC §4 — no GPU, no model, no exceptions.** `cargo test -p wisp-mind`
//! passes on a machine with no Vulkan loader, no GGUF and no llama.cpp build:
//! the real backend is behind an off-by-default cargo feature and everything
//! else runs against [`backend::mock::MockBackend`], which obeys grammars,
//! distinguishes warm from cold eviction, and is deterministic to the token.
//!
//! # Building the real backend
//!
//! ```sh
//! source env.sh
//! export VULKAN_SDK=/run/media/nerdrx/Lex/claude/tools/vulkan-sdk
//! export CMAKE_PREFIX_PATH=$VULKAN_SDK
//! cargo build -p wisp-mind --features vulkan
//! ```
//!
//! CachyOS ships the Vulkan *loader* and `glslc` but not the headers, and
//! `ggml-vulkan` needs both those and `SPIRV-Headers`' CMake package. Both are
//! staged under `tools/vulkan-sdk`, which is what `VULKAN_SDK` points at;
//! nothing is installed system-wide and nothing lands in the repository. Set
//! `NX_WISP_LLAMA_LOGS=1` to let llama.cpp's own logs through while debugging.
//!
//! The end-to-end check against a real model is `tests/real_llama.rs`, gated on
//! `NX_WISP_LLAMA_SMOKE` pointing at a GGUF. A 100 MB one is enough, and is in
//! the registry for exactly that reason.
//!
//! # Where this crate had to work around another crate's API
//!
//! Reported rather than patched around silently — none of these are this
//! crate's to fix (SPEC §2: an agent owns its crate's files exclusively).
//!
//! 1. **`ToolDescriptor` lives in `wisp-fleet`, and SPEC §2 gives `wisp-mind`
//!    only "proto, gov".** The type is the right one and cloning it would be
//!    worse, so `wisp-mind` depends on `wisp-fleet`. The amendment: move
//!    `ToolDescriptor` / `ToolOutcome` / `ToolFn` into `wisp-proto` beside
//!    §3.7's `Consent`, and have both crates use them from there.
//! 2. **`wisp-proto` has no `Mood`.** See [`mood`], and the recommendation
//!    below.
//! 3. **`wisp::config::ModelSettings` has no `embed` field.** There are three
//!    roles (reflex, deliberate, embedding); the config struct names two.
//!    [`manager::ModelSettings`] is otherwise field-for-field identical, so the
//!    operator's file does not have to change shape.
//! 4. **`wisp_gov::GpuTarget::index_in` cannot be used with llama.cpp.** It
//!    wants the `(vendor, device)` PCI pairs the backend enumerated; ggml
//!    exposes a name, a description, a device type and a memory size, and
//!    nothing that maps to PCI ids.
//!    [`backend::llama::LlamaBackend::device_index_for`] matches on
//!    kind-then-memory instead. A `GpuTarget::matches_description`, or an
//!    explicit "ggml device index" hint, would close it properly.
//! 5. **`TierReason` still has no CPU or memory pressure variant**, so a heavy
//!    compile arrives as `HeavyProcess`. Already on the backlog; noted again
//!    because [`mood::MoodFsm`] would react differently to the two.
//!
//! # The `Mood` amendment
//!
//! `wisp-attn` defines a nine-variant `Mood` with a comment saying it belongs
//! in `wisp-proto` once it becomes cross-crate. It has: `wisp-attn` reads it,
//! `wisp-mind` owns the FSM that produces it, `wisp-rig` consumes the
//! expression it maps to, and `wisp::app::expression_for` is the mapping. That
//! is four places and two copies of the same table.
//!
//! The recommended amendment, for whoever owns SPEC.md:
//!
//! * Add `Mood` to `wisp-proto` as **§3.8**, with the nine variants exactly as
//!   `wisp-attn` spells them and in that order, `Calm` as `Default`.
//! * Move `expression_for` onto it as `Mood::expression(self) -> &'static str`,
//!   returning the same eight names. `wisp-proto` cannot depend on `wisp-rig`,
//!   so the *names* live in proto and `wisp_rig::REQUIRED_EXPRESSIONS` keeps a
//!   test asserting it covers the range — the dependency points the right way
//!   and the coupling stays checkable.
//! * `wisp_attn::Mood` and [`mood::Mood`] become re-exports;
//!   `wisp::app::expression_for` becomes a one-line delegation.
//! * Keep the FSM itself in `wisp-mind`. SPEC §2 is right that the mood is
//!   cognition's; what belongs in proto is the *vocabulary*, not the machine.
//!
//! Until then [`mood::Mood`] is a third definition, deliberately
//! wire-compatible, with tests in that module standing guard over the variant
//! names, their order and the expression mapping.

pub mod backend;
pub mod defer;
pub mod dirs;
pub mod error;
pub mod escalate;
pub mod events;
pub mod fetch;
pub mod grammar;
pub mod kv;
pub mod manager;
pub mod memory;
pub mod mind;
pub mod models;
pub mod mood;
pub mod prompt;
pub mod stream;
pub mod testing;
pub mod tools;

pub use error::{MindError, Result};
pub use mind::{Mind, MindBuilder, Thought, TurnConfig};
pub use mood::Mood;

/// Monotonic milliseconds, from `wisp-proto`. Never wall-clock for *ordering*
/// (SPEC §3.2); the one place wall-clock is right is [`memory`]'s ageing, which
/// takes an injectable [`memory::WallClock`] and says so.
pub type Millis = wisp_proto::Millis;
