//! **`wisp`** — the host process. SPEC.md §2 gives this crate *"binary: wiring,
//! event loop, CLI, config, flight recorder"*, and deliberately nothing else.
//!
//! Every subsystem is finished elsewhere and is pure or self-contained. This
//! crate is the only place they meet, so it is the only place the two
//! whole-system guarantees can be enforced:
//!
//! * **SPEC §0.4 / §3.2 — she is honest.** Every event is written to the flight
//!   recorder *before* it is dispatched. That is structural here: the senses
//!   publish onto an inner channel that nothing else can subscribe to, and the
//!   only bridge from it to the bus everything else reads is
//!   [`app::run`]'s recorder relay. There is no path around it.
//! * **SPEC §3.1 — downgrades are synchronous.** [`wisp_gov::Governor::step`]
//!   calls every registered [`Governed`](wisp_proto::Governed) before it
//!   returns; this crate's job is to make sure everything that costs the
//!   machine anything is actually in that registry.
//!
//! # The shape of it
//!
//! ```text
//!    senses ─┐                                        ┌─▶ pump ─▶ wisp-attn
//!            ├─▶ inner bus ─▶ [flight recorder] ─▶ bus ┤          │
//!    fleet ──┘                       ▲                └─▶ shell   ├─▶ rig clips
//!                                    │                   seam    └─▶ shell seam
//!    governor ── step() ─────────────┘
//!         └── synchronous fan-out to every Governed, inside step()
//! ```
//!
//! # What is deliberately not here
//!
//! The layer surface, the wgpu device and the frame loop belong to
//! `wisp-shell`, which is a separate crate and a separate agent. The seam it
//! plugs into is [`shell::Shell`] and it is the only trait in this crate that a
//! compositor-aware component has to implement. Nothing in `wisp` opens a
//! window, creates a surface or touches a GPU.
//!
//! # Module map
//!
//! | Module | What it owns |
//! |---|---|
//! | [`config`] | `NX_WISP_CONFIG_DIR`, atomic load/save, fail-safe defaults |
//! | [`recorder`] | F20, the flight recorder: append-only JSONL, rotation, query, `explain` |
//! | [`app`] | The tokio event loop that wires the six subsystems together |
//! | [`cli`] | F53, the `wisp` CLI, driving exactly the same modules as the GUI |
//! | [`install`] | F57, the systemd user unit and the autostart entry |
//! | [`lock`] | F57, the single-instance lock — which respects the config-dir override |
//! | [`doctor`] | The environment check |
//! | [`mock`] | `--mock`: fake senses and a fake governor, for CI with no compositor |
//! | [`shell`] | The seam `wisp-shell` plugs into |
//! | [`fmt`] | Plain-text rendering, in DESIGN.md §9's voice |

pub mod app;
pub mod cli;
pub mod config;
pub mod doctor;
pub mod fmt;
pub mod install;
pub mod lock;
pub mod mock;
pub mod recorder;
pub mod shell;
pub mod shell_layer;
pub mod state;

#[cfg(test)]
pub(crate) mod testing;

pub use config::{Config, Loaded, Provenance};
pub use recorder::{Explanation, KindFilter, Recorder, Record};

/// The name the binary installs itself as, and her id on the Connector bus.
pub const APP_ID: &str = "nx-wisp";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Local wall-clock hour, 0..=23.
///
/// `wisp-attn` refuses to read a clock — that is what makes her judgement
/// replayable — so the host supplies the hour. This is the host.
pub fn local_hour() -> u8 {
    // SAFETY: `time(NULL)` and `localtime_r` into a zeroed `tm` we own. No
    // pointer outlives the call and `localtime_r` is the reentrant form.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 12;
        }
        tm.tm_hour.clamp(0, 23) as u8
    }
}

/// Wall-clock milliseconds since the Unix epoch.
///
/// Used **only** as an identity (which run produced this record) and as a
/// display origin. Ordering in the flight recorder is file order and `seq`,
/// never this — SPEC §3.2 is explicit that a clock step must not be able to
/// reorder the trace.
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
