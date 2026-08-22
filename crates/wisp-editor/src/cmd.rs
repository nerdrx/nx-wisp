//! Every mutation the editor can make to a skin, as one reversible value.
//!
//! # The one rule
//!
//! [`Command::apply`] takes the document by `&mut` and **returns the command
//! that undoes it**. There is no separate `invert()` to keep in step with
//! `apply()`, and therefore no way for the two to drift: the inverse is
//! computed from the state that was actually overwritten, at the moment it was
//! overwritten. That is what makes `undo` byte-exact rather than approximately
//! right, and it is why the round-trip test in `tests/undo.rs` can assert on
//! the serialised bytes instead of on a hand-written list of fields.
//!
//! # Granularity
//!
//! Records that a person edits as a *unit* — a bone, a shape, a gradient — are
//! replaced whole ([`Command::SetShape`] and friends). Collections a person
//! edits *inside* — keyframes, gradient stops, per-point weights — get their
//! own insert/remove/set commands, because a 40-key track should not put 40
//! keys on the undo stack every time one of them moves.
//!
//! Higher-level gestures ("drag this point", "paint this fill violet") are
//! **builders** in the feature modules that return one of these, or a
//! [`Command::Batch`] of them. The editor core knows only this enum.
//!
//! # Indices, not names
//!
//! A command addresses its target by index. Within an undo stack that is
//! applied strictly last-in-first-out, an index is exact and a name is not — a
//! rename between two edits would silently retarget a name-addressed command.

use wisp_rig::skin::doc::{
    BoneDoc, CanvasDoc, ChainDoc, ClipDoc, ColorDoc, EaseSpec, ExpressionDoc, GradientDoc, IkDoc,
    LayerDoc, MetaDoc, MotionDoc, Num, PhysicsDoc, ShapeDoc, SkinDoc, TrackDoc, WeightDoc,
};

use crate::error::EditError;

/// One reversible edit.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Several edits that undo as one. An empty batch is a legal no-op.
    Batch { label: &'static str, cmds: Vec<Command> },

    // ---- document-level singletons -------------------------------------
    SetMeta(Box<MetaDoc>),
    SetCanvas(CanvasDoc),
    SetPhysics(Box<PhysicsDoc>),
    SetMotion(Box<MotionDoc>),

    // ---- colours --------------------------------------------------------
    InsertColor { at: usize, value: ColorDoc },
    RemoveColor { at: usize },
    SetColor { at: usize, value: ColorDoc },

    // ---- gradients ------------------------------------------------------
    InsertGradient { at: usize, value: Box<GradientDoc> },
    RemoveGradient { at: usize },
    SetGradient { at: usize, value: Box<GradientDoc> },
    /// A gradient stop is a `(position, colour)` pair held in two parallel
    /// arrays; this keeps them in step.
    InsertStop { gradient: usize, at: usize, position: f32, color: String },
    RemoveStop { gradient: usize, at: usize },
    SetStop { gradient: usize, at: usize, position: f32, color: String },

    // ---- bones ----------------------------------------------------------
    InsertBone { at: usize, value: BoneDoc },
    RemoveBone { at: usize },
    SetBone { at: usize, value: BoneDoc },

    // ---- shapes ---------------------------------------------------------
    InsertShape { at: usize, value: Box<ShapeDoc> },
    RemoveShape { at: usize },
    SetShape { at: usize, value: Box<ShapeDoc> },
    /// Set or clear the explicit weight list for one path point. `None`
    /// removes the override and lets `bind`/`bind_auto` decide again.
    SetWeight { shape: usize, point: usize, value: Option<WeightDoc> },

    // ---- constraints ----------------------------------------------------
    InsertIk { at: usize, value: Box<IkDoc> },
    RemoveIk { at: usize },
    SetIk { at: usize, value: Box<IkDoc> },

    InsertChain { at: usize, value: Box<ChainDoc> },
    RemoveChain { at: usize },
    SetChain { at: usize, value: Box<ChainDoc> },

    // ---- animation ------------------------------------------------------
    InsertLayer { at: usize, value: LayerDoc },
    RemoveLayer { at: usize },
    SetLayer { at: usize, value: LayerDoc },

    InsertClip { at: usize, value: Box<ClipDoc> },
    RemoveClip { at: usize },
    SetClip { at: usize, value: Box<ClipDoc> },

    InsertTrack { clip: usize, at: usize, value: Box<TrackDoc> },
    RemoveTrack { clip: usize, at: usize },
    SetTrack { clip: usize, at: usize, value: Box<TrackDoc> },

    /// Insert a keyframe. `ease` is only stored when the track carries one
    /// easing per key; on an `ease = "soft"` track it is ignored, because
    /// there is nowhere in the format to put a per-key value without
    /// rewriting the whole track's easing.
    InsertKey { clip: usize, track: usize, at: usize, t: f32, v: f32, ease: Option<String> },
    RemoveKey { clip: usize, track: usize, at: usize },
    SetKey { clip: usize, track: usize, at: usize, t: f32, v: f32 },
    SetTrackEase { clip: usize, track: usize, value: Option<EaseSpec> },

    InsertExpression { at: usize, value: ExpressionDoc },
    RemoveExpression { at: usize },
    SetExpression { at: usize, value: ExpressionDoc },
}

macro_rules! index_ck {
    ($vec:expr, $at:expr, $kind:literal) => {{
        let len = $vec.len();
        if $at >= len {
            return Err(EditError::NoSuchIndex { kind: $kind, at: $at, len });
        }
    }};
}

macro_rules! insert_ck {
    ($vec:expr, $at:expr, $kind:literal) => {{
        let len = $vec.len();
        if $at > len {
            return Err(EditError::NoSuchIndex { kind: $kind, at: $at, len });
        }
    }};
}

impl Command {
    /// Apply this command, returning the command that undoes it.
    ///
    /// On `Err` the document is left exactly as it was: every variant either
    /// validates first or performs a single infallible mutation. A
    /// [`Command::Batch`] that fails part-way rolls back what it already did.
    pub fn apply(self, doc: &mut SkinDoc) -> Result<Command, EditError> {
        match self {
            Command::Batch { label, cmds } => {
                let mut undo: Vec<Command> = Vec::with_capacity(cmds.len());
                for c in cmds {
                    match c.apply(doc) {
                        Ok(inv) => undo.push(inv),
                        Err(e) => {
                            // Roll back what already landed, newest first, so
                            // a refused batch is not a half-edit.
                            while let Some(back) = undo.pop() {
                                let _ = back.apply(doc);
                            }
                            return Err(e);
                        }
                    }
                }
                undo.reverse();
                Ok(Command::Batch { label, cmds: undo })
            }

            Command::SetMeta(v) => {
                let old = std::mem::replace(&mut doc.meta, *v);
                Ok(Command::SetMeta(Box::new(old)))
            }
            Command::SetCanvas(v) => {
                let old = std::mem::replace(&mut doc.canvas, v);
                Ok(Command::SetCanvas(old))
            }
            Command::SetPhysics(v) => {
                let old = std::mem::replace(&mut doc.physics, *v);
                Ok(Command::SetPhysics(Box::new(old)))
            }
            Command::SetMotion(v) => {
                let old = std::mem::replace(&mut doc.motion, *v);
                Ok(Command::SetMotion(Box::new(old)))
            }

            // ---- colours ----------------------------------------------
            Command::InsertColor { at, value } => {
                insert_ck!(doc.colors, at, "colour");
                doc.colors.insert(at, value);
                Ok(Command::RemoveColor { at })
            }
            Command::RemoveColor { at } => {
                index_ck!(doc.colors, at, "colour");
                let value = doc.colors.remove(at);
                Ok(Command::InsertColor { at, value })
            }
            Command::SetColor { at, value } => {
                index_ck!(doc.colors, at, "colour");
                let old = std::mem::replace(&mut doc.colors[at], value);
                Ok(Command::SetColor { at, value: old })
            }

            // ---- gradients --------------------------------------------
            Command::InsertGradient { at, value } => {
                insert_ck!(doc.gradients, at, "gradient");
                doc.gradients.insert(at, *value);
                Ok(Command::RemoveGradient { at })
            }
            Command::RemoveGradient { at } => {
                index_ck!(doc.gradients, at, "gradient");
                let value = doc.gradients.remove(at);
                Ok(Command::InsertGradient { at, value: Box::new(value) })
            }
            Command::SetGradient { at, value } => {
                index_ck!(doc.gradients, at, "gradient");
                let old = std::mem::replace(&mut doc.gradients[at], *value);
                Ok(Command::SetGradient { at, value: Box::new(old) })
            }
            Command::InsertStop { gradient, at, position, color } => {
                index_ck!(doc.gradients, gradient, "gradient");
                let g = &mut doc.gradients[gradient];
                insert_ck!(g.stop_at, at, "gradient stop");
                g.stop_at.insert(at, Num(position));
                g.stop_color.insert(at.min(g.stop_color.len()), color);
                Ok(Command::RemoveStop { gradient, at })
            }
            Command::RemoveStop { gradient, at } => {
                index_ck!(doc.gradients, gradient, "gradient");
                let g = &mut doc.gradients[gradient];
                index_ck!(g.stop_at, at, "gradient stop");
                let position = g.stop_at.remove(at).0;
                let color = if at < g.stop_color.len() {
                    g.stop_color.remove(at)
                } else {
                    String::new()
                };
                Ok(Command::InsertStop { gradient, at, position, color })
            }
            Command::SetStop { gradient, at, position, color } => {
                index_ck!(doc.gradients, gradient, "gradient");
                let g = &mut doc.gradients[gradient];
                index_ck!(g.stop_at, at, "gradient stop");
                let old_p = std::mem::replace(&mut g.stop_at[at], Num(position)).0;
                let old_c = if at < g.stop_color.len() {
                    std::mem::replace(&mut g.stop_color[at], color)
                } else {
                    g.stop_color.push(color);
                    String::new()
                };
                Ok(Command::SetStop { gradient, at, position: old_p, color: old_c })
            }

            // ---- bones ------------------------------------------------
            Command::InsertBone { at, value } => {
                insert_ck!(doc.bones, at, "bone");
                doc.bones.insert(at, value);
                Ok(Command::RemoveBone { at })
            }
            Command::RemoveBone { at } => {
                index_ck!(doc.bones, at, "bone");
                let value = doc.bones.remove(at);
                Ok(Command::InsertBone { at, value })
            }
            Command::SetBone { at, value } => {
                index_ck!(doc.bones, at, "bone");
                let old = std::mem::replace(&mut doc.bones[at], value);
                Ok(Command::SetBone { at, value: old })
            }

            // ---- shapes -----------------------------------------------
            Command::InsertShape { at, value } => {
                insert_ck!(doc.shapes, at, "shape");
                doc.shapes.insert(at, *value);
                Ok(Command::RemoveShape { at })
            }
            Command::RemoveShape { at } => {
                index_ck!(doc.shapes, at, "shape");
                let value = doc.shapes.remove(at);
                Ok(Command::InsertShape { at, value: Box::new(value) })
            }
            Command::SetShape { at, value } => {
                index_ck!(doc.shapes, at, "shape");
                let old = std::mem::replace(&mut doc.shapes[at], *value);
                Ok(Command::SetShape { at, value: Box::new(old) })
            }
            Command::SetWeight { shape, point, value } => {
                index_ck!(doc.shapes, shape, "shape");
                let s = &mut doc.shapes[shape];
                let existing = s.weights.iter().position(|w| w.point == point);
                match (existing, value) {
                    (Some(i), Some(v)) => {
                        let old = std::mem::replace(&mut s.weights[i], v);
                        Ok(Command::SetWeight { shape, point, value: Some(old) })
                    }
                    (Some(i), None) => {
                        let old = s.weights.remove(i);
                        Ok(Command::SetWeight { shape, point, value: Some(old) })
                    }
                    (None, Some(v)) => {
                        // Keep the list ordered by point so a re-saved file
                        // reads top to bottom like the path does.
                        let at = s.weights.iter().position(|w| w.point > point).unwrap_or(s.weights.len());
                        s.weights.insert(at, v);
                        Ok(Command::SetWeight { shape, point, value: None })
                    }
                    (None, None) => Ok(Command::SetWeight { shape, point, value: None }),
                }
            }

            // ---- constraints ------------------------------------------
            Command::InsertIk { at, value } => {
                insert_ck!(doc.iks, at, "IK chain");
                doc.iks.insert(at, *value);
                Ok(Command::RemoveIk { at })
            }
            Command::RemoveIk { at } => {
                index_ck!(doc.iks, at, "IK chain");
                let value = doc.iks.remove(at);
                Ok(Command::InsertIk { at, value: Box::new(value) })
            }
            Command::SetIk { at, value } => {
                index_ck!(doc.iks, at, "IK chain");
                let old = std::mem::replace(&mut doc.iks[at], *value);
                Ok(Command::SetIk { at, value: Box::new(old) })
            }

            Command::InsertChain { at, value } => {
                insert_ck!(doc.chains, at, "spring chain");
                doc.chains.insert(at, *value);
                Ok(Command::RemoveChain { at })
            }
            Command::RemoveChain { at } => {
                index_ck!(doc.chains, at, "spring chain");
                let value = doc.chains.remove(at);
                Ok(Command::InsertChain { at, value: Box::new(value) })
            }
            Command::SetChain { at, value } => {
                index_ck!(doc.chains, at, "spring chain");
                let old = std::mem::replace(&mut doc.chains[at], *value);
                Ok(Command::SetChain { at, value: Box::new(old) })
            }

            // ---- animation --------------------------------------------
            Command::InsertLayer { at, value } => {
                insert_ck!(doc.layers, at, "layer");
                doc.layers.insert(at, value);
                Ok(Command::RemoveLayer { at })
            }
            Command::RemoveLayer { at } => {
                index_ck!(doc.layers, at, "layer");
                let value = doc.layers.remove(at);
                Ok(Command::InsertLayer { at, value })
            }
            Command::SetLayer { at, value } => {
                index_ck!(doc.layers, at, "layer");
                let old = std::mem::replace(&mut doc.layers[at], value);
                Ok(Command::SetLayer { at, value: old })
            }

            Command::InsertClip { at, value } => {
                insert_ck!(doc.clips, at, "clip");
                doc.clips.insert(at, *value);
                Ok(Command::RemoveClip { at })
            }
            Command::RemoveClip { at } => {
                index_ck!(doc.clips, at, "clip");
                let value = doc.clips.remove(at);
                Ok(Command::InsertClip { at, value: Box::new(value) })
            }
            Command::SetClip { at, value } => {
                index_ck!(doc.clips, at, "clip");
                let old = std::mem::replace(&mut doc.clips[at], *value);
                Ok(Command::SetClip { at, value: Box::new(old) })
            }

            Command::InsertTrack { clip, at, value } => {
                index_ck!(doc.clips, clip, "clip");
                insert_ck!(doc.clips[clip].tracks, at, "track");
                doc.clips[clip].tracks.insert(at, *value);
                Ok(Command::RemoveTrack { clip, at })
            }
            Command::RemoveTrack { clip, at } => {
                index_ck!(doc.clips, clip, "clip");
                index_ck!(doc.clips[clip].tracks, at, "track");
                let value = doc.clips[clip].tracks.remove(at);
                Ok(Command::InsertTrack { clip, at, value: Box::new(value) })
            }
            Command::SetTrack { clip, at, value } => {
                index_ck!(doc.clips, clip, "clip");
                index_ck!(doc.clips[clip].tracks, at, "track");
                let old = std::mem::replace(&mut doc.clips[clip].tracks[at], *value);
                Ok(Command::SetTrack { clip, at, value: Box::new(old) })
            }

            Command::InsertKey { clip, track, at, t, v, ease } => {
                index_ck!(doc.clips, clip, "clip");
                index_ck!(doc.clips[clip].tracks, track, "track");
                let tr = &mut doc.clips[clip].tracks[track];
                insert_ck!(tr.t, at, "keyframe");
                if !t.is_finite() {
                    return Err(EditError::NotFinite { at: "the keyframe's time", value: t });
                }
                if !v.is_finite() {
                    return Err(EditError::NotFinite { at: "the keyframe's value", value: v });
                }
                let before_ok = at == 0 || tr.t[at - 1].0 <= t;
                let after_ok = at >= tr.t.len() || tr.t[at].0 >= t;
                if !(before_ok && after_ok) {
                    return Err(EditError::KeyOutOfOrder { t });
                }
                tr.t.insert(at, Num(t));
                tr.v.insert(at.min(tr.v.len()), Num(v));
                if let Some(EaseSpec::Each(list)) = &mut tr.ease {
                    let name = ease.unwrap_or_else(|| {
                        list.get(at.saturating_sub(1)).cloned().unwrap_or_else(|| "soft".into())
                    });
                    list.insert(at.min(list.len()), name);
                }
                Ok(Command::RemoveKey { clip, track, at })
            }
            Command::RemoveKey { clip, track, at } => {
                index_ck!(doc.clips, clip, "clip");
                index_ck!(doc.clips[clip].tracks, track, "track");
                let tr = &mut doc.clips[clip].tracks[track];
                index_ck!(tr.t, at, "keyframe");
                let t = tr.t.remove(at).0;
                let v = if at < tr.v.len() { tr.v.remove(at).0 } else { 0.0 };
                let mut ease = None;
                if let Some(EaseSpec::Each(list)) = &mut tr.ease {
                    if at < list.len() {
                        ease = Some(list.remove(at));
                    }
                }
                Ok(Command::InsertKey { clip, track, at, t, v, ease })
            }
            Command::SetKey { clip, track, at, t, v } => {
                index_ck!(doc.clips, clip, "clip");
                index_ck!(doc.clips[clip].tracks, track, "track");
                let tr = &mut doc.clips[clip].tracks[track];
                index_ck!(tr.t, at, "keyframe");
                if !t.is_finite() {
                    return Err(EditError::NotFinite { at: "the keyframe's time", value: t });
                }
                if !v.is_finite() {
                    return Err(EditError::NotFinite { at: "the keyframe's value", value: v });
                }
                let before_ok = at == 0 || tr.t[at - 1].0 <= t;
                let after_ok = at + 1 >= tr.t.len() || tr.t[at + 1].0 >= t;
                if !(before_ok && after_ok) {
                    return Err(EditError::KeyOutOfOrder { t });
                }
                let old_t = std::mem::replace(&mut tr.t[at], Num(t)).0;
                let old_v = if at < tr.v.len() {
                    std::mem::replace(&mut tr.v[at], Num(v)).0
                } else {
                    tr.v.push(Num(v));
                    0.0
                };
                Ok(Command::SetKey { clip, track, at, t: old_t, v: old_v })
            }
            Command::SetTrackEase { clip, track, value } => {
                index_ck!(doc.clips, clip, "clip");
                index_ck!(doc.clips[clip].tracks, track, "track");
                let old = std::mem::replace(&mut doc.clips[clip].tracks[track].ease, value);
                Ok(Command::SetTrackEase { clip, track, value: old })
            }

            Command::InsertExpression { at, value } => {
                insert_ck!(doc.expressions, at, "expression");
                doc.expressions.insert(at, value);
                Ok(Command::RemoveExpression { at })
            }
            Command::RemoveExpression { at } => {
                index_ck!(doc.expressions, at, "expression");
                let value = doc.expressions.remove(at);
                Ok(Command::InsertExpression { at, value })
            }
            Command::SetExpression { at, value } => {
                index_ck!(doc.expressions, at, "expression");
                let old = std::mem::replace(&mut doc.expressions[at], value);
                Ok(Command::SetExpression { at, value: old })
            }
        }
    }

    /// What the undo menu calls this edit. Short, sentence case, no full stop.
    pub fn label(&self) -> &'static str {
        match self {
            Command::Batch { label, .. } => label,
            Command::SetMeta(_) => "edit skin details",
            Command::SetCanvas(_) => "edit the canvas",
            Command::SetPhysics(_) => "edit physics",
            Command::SetMotion(_) => "edit procedural motion",
            Command::InsertColor { .. } => "add a colour",
            Command::RemoveColor { .. } => "delete a colour",
            Command::SetColor { .. } => "edit a colour",
            Command::InsertGradient { .. } => "add a gradient",
            Command::RemoveGradient { .. } => "delete a gradient",
            Command::SetGradient { .. } => "edit a gradient",
            Command::InsertStop { .. } => "add a gradient stop",
            Command::RemoveStop { .. } => "delete a gradient stop",
            Command::SetStop { .. } => "move a gradient stop",
            Command::InsertBone { .. } => "add a bone",
            Command::RemoveBone { .. } => "delete a bone",
            Command::SetBone { .. } => "edit a bone",
            Command::InsertShape { .. } => "add a shape",
            Command::RemoveShape { .. } => "delete a shape",
            Command::SetShape { .. } => "edit a shape",
            Command::SetWeight { .. } => "edit a weight",
            Command::InsertIk { .. } => "add an IK chain",
            Command::RemoveIk { .. } => "delete an IK chain",
            Command::SetIk { .. } => "edit an IK chain",
            Command::InsertChain { .. } => "add a spring chain",
            Command::RemoveChain { .. } => "delete a spring chain",
            Command::SetChain { .. } => "edit a spring chain",
            Command::InsertLayer { .. } => "add a layer",
            Command::RemoveLayer { .. } => "delete a layer",
            Command::SetLayer { .. } => "edit a layer",
            Command::InsertClip { .. } => "add a clip",
            Command::RemoveClip { .. } => "delete a clip",
            Command::SetClip { .. } => "edit a clip",
            Command::InsertTrack { .. } => "add a track",
            Command::RemoveTrack { .. } => "delete a track",
            Command::SetTrack { .. } => "edit a track",
            Command::InsertKey { .. } => "add a keyframe",
            Command::RemoveKey { .. } => "delete a keyframe",
            Command::SetKey { .. } => "move a keyframe",
            Command::SetTrackEase { .. } => "change an easing",
            Command::InsertExpression { .. } => "add an expression",
            Command::RemoveExpression { .. } => "delete an expression",
            Command::SetExpression { .. } => "edit an expression",
        }
    }

    /// True for commands that only move numbers around inside one record, so
    /// the editor may merge a drag's worth of them into a single undo step.
    pub fn is_continuous(&self) -> bool {
        matches!(
            self,
            Command::SetShape { .. }
                | Command::SetBone { .. }
                | Command::SetKey { .. }
                | Command::SetStop { .. }
                | Command::SetGradient { .. }
        )
    }

    /// Does this command target the same field as `other`? Used to coalesce a
    /// pointer drag into one undo entry.
    pub fn same_target(&self, other: &Command) -> bool {
        use Command::*;
        match (self, other) {
            (SetShape { at: a, .. }, SetShape { at: b, .. }) => a == b,
            (SetBone { at: a, .. }, SetBone { at: b, .. }) => a == b,
            (SetGradient { at: a, .. }, SetGradient { at: b, .. }) => a == b,
            (
                SetKey { clip: c1, track: t1, at: a1, .. },
                SetKey { clip: c2, track: t2, at: a2, .. },
            ) => c1 == c2 && t1 == t2 && a1 == a2,
            (
                SetStop { gradient: g1, at: a1, .. },
                SetStop { gradient: g2, at: a2, .. },
            ) => g1 == g2 && a1 == a2,
            _ => false,
        }
    }
}

impl crate::history::Reversible for Command {
    type Doc = SkinDoc;

    fn apply_to(self, doc: &mut SkinDoc) -> Result<Command, EditError> {
        self.apply(doc)
    }
    fn label(&self) -> &'static str {
        Command::label(self)
    }
    fn is_continuous(&self) -> bool {
        Command::is_continuous(self)
    }
    fn same_target(&self, other: &Command) -> bool {
        Command::same_target(self, other)
    }
}
