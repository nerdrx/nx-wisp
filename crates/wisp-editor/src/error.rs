//! What the editor refuses to do, and why.
//!
//! DESIGN.md §9 applies to every string in here: say what happened **and what
//! to do next**, in sentence case, with no exclamation marks. These messages
//! are shown to the operator in the editor's status strip, so they are the
//! whole of the UI's explanation.

use wisp_rig::skin::SkinError;

/// A refused edit. Nothing in this enum is a panic and nothing is a silent
/// no-op: an edit either happens or it says why it did not.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EditError {
    #[error("there is no {kind} at index {at} — the document has {len}")]
    NoSuchIndex { kind: &'static str, at: usize, len: usize },

    #[error("there is no {kind} named {name:?} in this skin")]
    NoSuchName { kind: &'static str, name: String },

    #[error(
        "making {child:?} a child of {parent:?} would close a loop in the bone \
         tree ({}) — pick a parent that is not below {child:?}",
        .chain.join(" -> ")
    )]
    BoneCycle { child: String, parent: String, chain: Vec<String> },

    #[error("a bone cannot be its own parent — {name:?} already is where it is")]
    SelfParent { name: String },

    #[error("two {kind}s would both be named {name:?} — names must be unique")]
    DuplicateName { kind: &'static str, name: String },

    #[error("{name:?} is not a usable name — give it at least one character that is not a space")]
    EmptyName { kind: &'static str, name: String },

    #[error("{at} would leave the path unusable: {reason}")]
    BadPath { at: String, reason: String },

    #[error("a keyframe at {t} ms would land before the key ahead of it — keyframe times must not go backwards")]
    KeyOutOfOrder { t: f32 },

    #[error("{at} is {value}, which is not a finite number")]
    NotFinite { at: &'static str, value: f32 },

    #[error("there is nothing to undo")]
    NothingToUndo,

    #[error("there is nothing to redo")]
    NothingToRedo,

    #[error("removing bone {name:?} would leave {referenced_by} pointing at nothing — repoint it first")]
    BoneStillUsed { name: String, referenced_by: String },

    #[error("the skin could not be written: {0}")]
    Write(String),

    #[error("the skin could not be read: {0}")]
    Read(String),
}

impl EditError {
    /// The refusal as the editor's status strip shows it.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// A validation result: the compile either succeeded or produced a list of
/// problems, each already phrased for a human by `wisp-rig`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Validation {
    pub problems: Vec<String>,
}

impl Validation {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn from_result(e: Option<&SkinError>) -> Validation {
        let Some(e) = e else { return Validation::default() };
        match e {
            SkinError::Invalid(issues) => {
                Validation { problems: issues.iter().map(|i| i.to_string()).collect() }
            }
            other => Validation { problems: vec![other.to_string()] },
        }
    }

    /// One line for the status strip: the first problem, plus a count.
    pub fn summary(&self) -> String {
        match self.problems.len() {
            0 => "the skin is valid".to_string(),
            1 => self.problems[0].clone(),
            n => format!("{} — and {} more", self.problems[0], n - 1),
        }
    }
}
