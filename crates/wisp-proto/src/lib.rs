//! Every type shared across crate boundaries. **No logic lives here** — this is
//! the contract of SPEC.md §3 expressed in Rust, and it is the only crate every
//! other crate is allowed to depend on.
//!
//! Amending a type here is a spec amendment. Do not add a variant because it
//! would be convenient in one crate.

pub mod attn;
pub mod event;
pub mod gov;
pub mod sense;

pub use attn::{Urgency, Utterance};
pub use event::{Event, EventKind};
pub use gov::{Cost, Governed, Tier, TierReason};
pub use sense::{Consent, Observation, SenseId};

/// Monotonic milliseconds since process start. We never use wall-clock time for
/// ordering — suspend/resume and clock steps would reorder the flight recorder.
pub type Millis = u64;
