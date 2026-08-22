//! Undo and redo.
//!
//! The stack holds **inverses**, not the commands the operator issued: a
//! [`Reversible`] command hands back its own inverse when it is applied, so
//! the entry pushed here is already the thing that
//! puts the document back. Undo applies it and keeps the inverse *of that*,
//! which is the redo entry. The two stacks are therefore the same shape and
//! neither one can go stale.
//!
//! # Gestures
//!
//! Dragging a path point emits one command per pointer move. Thirty of those
//! on the undo stack would make undo useless, so a drag runs inside a
//! **gesture**: [`History::begin`] opens one, [`History::end`] closes it, and
//! while it is open a push that targets the same field as the entry on top is
//! folded into it. The *oldest* inverse survives, which is the state from
//! before the drag started — exactly what one press of undo should restore.
//!
//! There is no clock in here. Coalescing is driven by the caller's gesture
//! boundaries, so the module stays pure and its behaviour is reproducible.

use crate::error::EditError;

/// A command that knows how to undo itself.
///
/// Implemented by [`crate::cmd::Command`] over the skin document and by
/// [`crate::graph::GraphCommand`] over the mood graph, so both get the same
/// undo stack rather than a second, subtly different one.
pub trait Reversible: Sized {
    /// What this command edits.
    type Doc;
    /// Apply, returning the command that undoes this one.
    fn apply_to(self, doc: &mut Self::Doc) -> Result<Self, EditError>;
    /// What the undo menu calls it.
    fn label(&self) -> &'static str;
    /// May a run of these fold into one undo step inside a gesture?
    fn is_continuous(&self) -> bool {
        false
    }
    /// Do two of them edit the same field?
    fn same_target(&self, _other: &Self) -> bool {
        false
    }
}

/// How many undo steps are kept. A rig session is long and the entries are
/// small; 512 is far past what anyone reaches for and still bounded.
pub const DEFAULT_LIMIT: usize = 512;

#[derive(Debug, Clone)]
struct Entry<C> {
    inverse: C,
    label: &'static str,
    gesture: Option<u64>,
}

/// The undo/redo stacks and the save watermark.
#[derive(Debug, Clone)]
pub struct History<C: Reversible> {
    undo: Vec<Entry<C>>,
    redo: Vec<Entry<C>>,
    limit: usize,
    gesture: Option<u64>,
    next_gesture: u64,
    /// Total edits applied, ever. Compared against `saved_at` for the dirty
    /// flag, and monotonic so undo past the save point still reads as dirty.
    revision: u64,
    saved_at: Option<u64>,
}

impl<C: Reversible> Default for History<C> {
    fn default() -> Self {
        History::new(DEFAULT_LIMIT)
    }
}

impl<C: Reversible> History<C> {
    pub fn new(limit: usize) -> History<C> {
        History {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: limit.max(1),
            gesture: None,
            next_gesture: 1,
            revision: 0,
            saved_at: Some(0),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// What one press of undo would put back, for the menu item's label.
    pub fn undo_label(&self) -> Option<&'static str> {
        self.undo.last().map(|e| e.label)
    }
    pub fn redo_label(&self) -> Option<&'static str> {
        self.redo.last().map(|e| e.label)
    }

    /// Has the document changed since the last [`History::mark_saved`]?
    pub fn dirty(&self) -> bool {
        self.saved_at != Some(self.revision)
    }

    pub fn mark_saved(&mut self) {
        self.saved_at = Some(self.revision);
    }

    /// Open a gesture. Pushes inside it that hit the same field fold together.
    pub fn begin(&mut self) -> u64 {
        let g = self.next_gesture;
        self.next_gesture += 1;
        self.gesture = Some(g);
        g
    }

    /// Close the gesture. The next push starts a fresh undo entry.
    pub fn end(&mut self) {
        self.gesture = None;
    }

    pub fn in_gesture(&self) -> bool {
        self.gesture.is_some()
    }

    /// Apply a command and record how to undo it.
    pub fn apply(&mut self, doc: &mut C::Doc, cmd: C) -> Result<(), EditError> {
        let label = cmd.label();
        let continuous = cmd.is_continuous();
        let inverse = cmd.apply_to(doc)?;
        self.redo.clear();
        self.revision += 1;

        // Fold into the entry on top when this is the same drag touching the
        // same field: keep the older inverse, drop the new one.
        if continuous {
            if let (Some(g), Some(top)) = (self.gesture, self.undo.last()) {
                if top.gesture == Some(g) && top.inverse.same_target(&inverse) {
                    return Ok(());
                }
            }
        }

        self.undo.push(Entry { inverse, label, gesture: self.gesture });
        if self.undo.len() > self.limit {
            self.undo.remove(0);
        }
        Ok(())
    }

    /// Apply a command **without** recording it. For anything that is not a
    /// document edit — there is currently nothing, and the method exists so
    /// that a future caller has to say out loud that it is skipping undo.
    pub fn apply_untracked(doc: &mut C::Doc, cmd: C) -> Result<C, EditError> {
        cmd.apply_to(doc)
    }

    pub fn undo(&mut self, doc: &mut C::Doc) -> Result<&'static str, EditError> {
        let Some(entry) = self.undo.pop() else {
            return Err(EditError::NothingToUndo);
        };
        let label = entry.label;
        let back = entry.inverse.apply_to(doc)?;
        self.revision += 1;
        self.redo.push(Entry { inverse: back, label, gesture: entry.gesture });
        Ok(label)
    }

    pub fn redo(&mut self, doc: &mut C::Doc) -> Result<&'static str, EditError> {
        let Some(entry) = self.redo.pop() else {
            return Err(EditError::NothingToRedo);
        };
        let label = entry.label;
        let back = entry.inverse.apply_to(doc)?;
        self.revision += 1;
        self.undo.push(Entry { inverse: back, label, gesture: entry.gesture });
        Ok(label)
    }

    /// Throw both stacks away — used when a different skin is opened.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.gesture = None;
        self.revision = 0;
        self.saved_at = Some(0);
    }
}
