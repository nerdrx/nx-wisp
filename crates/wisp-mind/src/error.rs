//! Everything cognition can fail at.
//!
//! There is no `Other(String)` catch-all on purpose: every failure mode here is
//! one the operator could plausibly be told about in a sentence, and a variant
//! that cannot be phrased is a variant that has not been thought through.

use std::path::PathBuf;

use crate::backend::Role;

#[derive(Debug, thiserror::Error)]
pub enum MindError {
    // --- backend -----------------------------------------------------------
    #[error("no model is loaded for the {0:?} role")]
    NotLoaded(Role),
    #[error("model {name} is not on disk at {path}")]
    ModelMissing { name: String, path: PathBuf },
    #[error("the {backend} backend refused to load {name}: {why}")]
    LoadFailed {
        backend: &'static str,
        name: String,
        why: String,
    },
    #[error("inference failed: {0}")]
    Inference(String),
    #[error("this backend cannot {0}")]
    Unsupported(&'static str),

    // --- the governor ------------------------------------------------------
    /// SPEC §3.1: at T3/T4 [`wisp_proto::Tier::may_hold_model`] is false. This
    /// is not an error condition so much as the governor doing its job, and the
    /// caller is expected to defer the work (SPEC §3.5) rather than retry.
    #[error("she may not hold a model at {tier:?}")]
    NotAllowedAtTier { tier: wisp_proto::Tier },
    #[error("{want_mib} MiB does not fit in the {have_mib} MiB the governor allows")]
    OverBudget { want_mib: u64, have_mib: u64 },

    // --- the model registry and the fetcher --------------------------------
    #[error("no model named {0} in the registry")]
    UnknownModel(String),
    #[error("the model registry is malformed: {0}")]
    BadRegistry(String),
    #[error("model downloads are switched off (SPEC §0.2a); enable them to fetch {0}")]
    DownloadsDisabled(String),
    #[error("{name}: downloaded bytes hash to {got}, not the pinned {want}")]
    HashMismatch {
        name: String,
        want: String,
        got: String,
    },
    #[error("{name}: the server sent {got} bytes, the registry pins {want}")]
    SizeMismatch { name: String, want: u64, got: u64 },
    #[error("fetching {name}: {why}")]
    Fetch { name: String, why: String },

    // --- grammars ----------------------------------------------------------
    #[error("schema at {at}: {why}")]
    Schema { at: String, why: String },
    #[error("grammar: {0}")]
    Grammar(String),

    // --- tools -------------------------------------------------------------
    #[error("there is no tool called {0}")]
    NoSuchTool(String),
    /// SPEC §3.7. Refusing is the whole point, so it is a first-class outcome.
    #[error("{name} needs to be switched on first ({consent:?})")]
    ConsentRequired {
        name: String,
        consent: wisp_proto::Consent,
    },
    #[error("{name}: {why}")]
    BadArguments { name: String, why: String },

    // --- memory ------------------------------------------------------------
    #[error("memory: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("embeddings are {got} wide, this store holds {want}")]
    EmbeddingWidth { want: usize, got: usize },
    /// Consolidation is only allowed to run at T0 Feral (plan F18).
    #[error("consolidation only runs while the machine is at rest; she is at {tier:?}")]
    NotAtRest { tier: wisp_proto::Tier },

    // --- plumbing ----------------------------------------------------------
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

impl MindError {
    pub fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        MindError::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    /// Is this the governor saying "not now" rather than something being
    /// broken? Callers use it to decide between deferring (SPEC §3.5) and
    /// telling the operator.
    pub fn is_tier_refusal(&self) -> bool {
        matches!(
            self,
            MindError::NotAllowedAtTier { .. } | MindError::NotAtRest { .. }
        )
    }
}

pub type Result<T> = std::result::Result<T, MindError>;
