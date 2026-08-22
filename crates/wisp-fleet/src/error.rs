//! One error type for the whole crate. Nothing in `wisp-fleet` is allowed to
//! panic on a hub that is missing, stale, hostile or simply old.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FleetError {
    /// No `connector.token` — NX Hub has never run here, or is not installed.
    /// This is the *normal* case on a machine without the hub and must never be
    /// surfaced to the operator (PROTOCOL.md §8: "be silent about it").
    #[error("no connector token at {0}")]
    NoToken(String),

    #[error("websocket: {0}")]
    Ws(#[from] crate::ws::WsError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad json: {0}")]
    Json(#[from] serde_json::Error),

    /// The bus rejected us — bad token, missing app id, protocol violation.
    #[error("hub refused us: {0}")]
    Refused(String),

    /// A status update was over the bus's 2 KB / 64 key cap and was dropped
    /// rather than sent, because sending it earns a disconnect (§6).
    #[error("status too large: {0}")]
    StatusTooLarge(String),

    /// The client task is gone.
    #[error("fleet client stopped")]
    Stopped,
}

pub type Result<T> = std::result::Result<T, FleetError>;
