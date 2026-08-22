//! Live puppet mode: drag her limbs, then keyframe the pose you liked.
//!
//! # How a drag becomes a pose
//!
//! Grabbing a bone means one of three things, and which one is decided by what
//! the *skin already says about that bone* rather than by a mode the operator
//! has to pick:
//!
//! * The bone is the tip of a `two_bone` IK chain → the whole chain solves,
//!   through `wisp_rig::ik::apply_two_bone`. That is the same solver the
//!   runtime uses for her neck and her tail, so the elbow bends the way it
//!   will bend when she is on the desktop.
//! * The bone has a length and a parent → it aims at the pointer
//!   (`aim_bone_at`). One joint, rotating.
//! * Otherwise → it translates.
//!
//! # Why the drag runs on a scratch pose
//!
//! `wisp_rig::Rig` exposes `pose()` but not `pose_mut()`, and `build_frame` is
//! private, so nothing outside the rig can push a pose through the geometry
//! pipeline. That is the right shape for a renderer and a reported gap for an
//! editor.
//!
//! The way around it is better than the thing it works around. The drag
//! mutates a **scratch copy** of the pose, which is enough to draw the bone
//! gizmos live and costs nothing; on release the pose becomes *keyframes* and
//! [`crate::preview::Preview`] re-poses from the document. So what the
//! operator ends up looking at came out of the shipping rig reading the
//! shipping file — the pose is never something only the editor can produce.
//!
//! # Radians in, degrees out
//!
//! `BoneOffsets::rot` is radians, because everything at runtime is. The file
//! authors degrees, because that is what a person types. [`Puppet::keys`] converts
//! exactly once, at the moment a pose becomes a document.

use wisp_rig::ik::{aim_bone_at, apply_two_bone};
use wisp_rig::math::{rad_to_deg, Vec2};
use wisp_rig::skeleton::{Channel, Pose};
use wisp_rig::skin::{IkKind, Skin};
use wisp_rig::skin::doc::SkinDoc;

use crate::cmd::Command;
use crate::error::EditError;

/// What grabbing this bone does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Grip {
    /// Solve a two-bone chain to put `end` on the pointer.
    TwoBone { root: usize, mid: usize, end: usize, bend_positive: bool },
    /// Rotate one bone to point at the pointer.
    Aim { bone: usize },
    /// Slide one bone.
    Translate { bone: usize },
}

impl Grip {
    /// Every bone this grip writes to — the set that gets keyframed.
    pub fn bones(&self) -> Vec<usize> {
        match *self {
            Grip::TwoBone { root, mid, .. } => vec![root, mid],
            Grip::Aim { bone } | Grip::Translate { bone } => vec![bone],
        }
    }

    /// Which channels this grip changes. Keying only these keeps a puppeted
    /// pose from writing six flat tracks per bone.
    pub fn channels(&self) -> &'static [Channel] {
        match self {
            Grip::TwoBone { .. } | Grip::Aim { .. } => &[Channel::Rot],
            Grip::Translate { .. } => &[Channel::Tx, Channel::Ty],
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Grip::TwoBone { .. } => "solving the IK chain",
            Grip::Aim { .. } => "aiming the bone",
            Grip::Translate { .. } => "sliding the bone",
        }
    }
}

/// Decide what grabbing `bone` should do, from what the skin already declares.
pub fn grip_for(skin: &Skin, bone: usize) -> Grip {
    for def in &skin.iks {
        if let IkKind::TwoBone { root, mid, end, bend_positive } = def.kind {
            if end == bone || mid == bone {
                return Grip::TwoBone { root, mid, end, bend_positive };
            }
        }
    }
    let b = skin.skeleton.bone(bone);
    if b.length > 1e-4 && b.parent.is_some() {
        Grip::Aim { bone }
    } else {
        Grip::Translate { bone }
    }
}

/// One in-flight drag.
#[derive(Debug, Clone)]
pub struct Drag {
    pub grip: Grip,
    /// Where the grabbed handle was when the drag started, in canvas units.
    pub from: Vec2,
    /// Offset between the pointer and the handle, so the handle does not jump
    /// to the cursor on the first pixel of movement.
    pub offset: Vec2,
}

/// Puppet mode's working state.
#[derive(Debug, Clone)]
pub struct Puppet {
    /// A resolved pose the drag mutates. Sourced from the preview, so it
    /// starts wherever the playhead left her.
    pose: Pose,
    drag: Option<Drag>,
    /// True once a drag has moved the pose away from what the clips say.
    dirty: bool,
}

impl Puppet {
    pub fn new(pose: Pose) -> Puppet {
        Puppet { pose, drag: None, dirty: false }
    }

    pub fn pose(&self) -> &Pose {
        &self.pose
    }
    pub fn drag(&self) -> Option<&Drag> {
        self.drag.as_ref()
    }
    /// Has the operator posed her away from what the timeline says?
    pub fn is_posed(&self) -> bool {
        self.dirty
    }

    /// Take the pose the preview is currently showing. Called whenever the
    /// playhead moves, so puppeting always starts from the frame on screen.
    pub fn sync(&mut self, pose: &Pose) {
        self.pose = pose.clone();
        self.dirty = false;
        self.drag = None;
    }

    /// Grab a bone. `at` is the pointer in canvas units.
    pub fn begin(&mut self, skin: &Skin, bone: usize, at: Vec2) -> Result<&Drag, EditError> {
        if bone >= skin.skeleton.len() {
            return Err(EditError::NoSuchIndex {
                kind: "bone",
                at: bone,
                len: skin.skeleton.len(),
            });
        }
        let grip = grip_for(skin, bone);
        let handle = match grip {
            Grip::TwoBone { end, .. } => self.pose.world_pos(end),
            Grip::Aim { bone } => self.pose.world_tip(&skin.skeleton, bone),
            Grip::Translate { bone } => self.pose.world_pos(bone),
        };
        self.drag = Some(Drag { grip, from: handle, offset: handle - at });
        Ok(self.drag.as_ref().expect("just set"))
    }

    /// Move the pointer. Returns the grip that was applied, or `None` when no
    /// drag is in flight.
    pub fn drag_to(&mut self, skin: &Skin, at: Vec2) -> Option<Grip> {
        let drag = self.drag.as_ref()?;
        let target = at + drag.offset;
        let grip = drag.grip;
        match grip {
            Grip::TwoBone { root, mid, end, bend_positive } => {
                apply_two_bone(
                    &skin.skeleton,
                    &mut self.pose,
                    root,
                    mid,
                    end,
                    target,
                    bend_positive,
                    1.0,
                );
            }
            Grip::Aim { bone } => {
                aim_bone_at(&skin.skeleton, &mut self.pose, bone, target);
            }
            Grip::Translate { bone } => {
                // Translation is authored in the bone's *parent* space, which
                // is where `tx`/`ty` live.
                let delta = target - self.pose.world_pos(bone);
                let local = match skin.skeleton.bone(bone).parent {
                    Some(p) => self.pose.world[p].inverse().apply_vec(delta),
                    None => delta,
                };
                self.pose.offsets[bone].tx += local.x;
                self.pose.offsets[bone].ty += local.y;
                self.pose.resolve_from(&skin.skeleton, bone);
            }
        }
        self.dirty = true;
        Some(grip)
    }

    /// Let go. The pose stays; [`Puppet::keys`] turns it into keyframes.
    pub fn end(&mut self) -> Option<Grip> {
        self.drag.take().map(|d| d.grip)
    }

    /// The keyframe commands that record the current pose at `t_ms` of `clip`.
    ///
    /// `grips` is what the operator actually touched since the last keyframe;
    /// pass the grips returned by [`Puppet::drag_to`]. Only their bones and
    /// only their channels are written, so pressing K does not carpet the clip
    /// with tracks that hold rest values.
    pub fn keys(
        &self,
        doc: &SkinDoc,
        skin: &Skin,
        clip: usize,
        t_ms: f32,
        grips: &[Grip],
    ) -> Result<Command, EditError> {
        let mut cmds = Vec::new();
        let mut done: Vec<(usize, Channel)> = Vec::new();
        for grip in grips {
            for bone in grip.bones() {
                for &channel in grip.channels() {
                    if done.contains(&(bone, channel)) {
                        continue;
                    }
                    done.push((bone, channel));
                    let name = skin.skeleton.bone(bone).name.to_string();
                    let raw = self.pose.offsets[bone].get(channel);
                    // The file authors degrees; everything else is 1:1.
                    let v = if channel == Channel::Rot { rad_to_deg(raw) } else { raw };
                    cmds.push(crate::timeline::set_key(
                        doc,
                        clip,
                        &name,
                        channel.name(),
                        t_ms,
                        v,
                        None,
                    )?);
                }
            }
        }
        Ok(Command::Batch { label: "keyframe the pose", cmds })
    }

    /// Keyframe **every** channel of the bones a grip touched, for the case
    /// where the operator wants a full pose key rather than a minimal one.
    pub fn keys_full(
        &self,
        doc: &SkinDoc,
        skin: &Skin,
        clip: usize,
        t_ms: f32,
        bones: &[usize],
    ) -> Result<Command, EditError> {
        let mut cmds = Vec::new();
        for &bone in bones {
            if bone >= skin.skeleton.len() {
                continue;
            }
            let name = skin.skeleton.bone(bone).name.to_string();
            let off = self.pose.offsets[bone];
            for channel in [
                Channel::Tx,
                Channel::Ty,
                Channel::Rot,
                Channel::Sx,
                Channel::Sy,
                Channel::Alpha,
            ] {
                let raw = off.get(channel);
                // Skip a channel that is still sitting on its identity: a key
                // that says "unchanged" is noise in the file and in the
                // timeline.
                if (raw - channel.identity()).abs() < 1e-5 {
                    continue;
                }
                let v = if channel == Channel::Rot { rad_to_deg(raw) } else { raw };
                cmds.push(crate::timeline::set_key(
                    doc,
                    clip,
                    &name,
                    channel.name(),
                    t_ms,
                    v,
                    None,
                )?);
            }
        }
        Ok(Command::Batch { label: "keyframe the pose", cmds })
    }

    /// The bone handle nearest a canvas point, within `radius`.
    ///
    /// Tips win over heads at equal distance: the tip is what puppet mode
    /// drags, and a bone whose length is short enough for the two to overlap
    /// is one the operator is trying to aim, not to slide.
    pub fn hit_bone(&self, skin: &Skin, at: Vec2, radius: f32) -> Option<usize> {
        let mut best = None;
        let mut best_d = radius;
        for i in 0..skin.skeleton.len() {
            let tip = self.pose.world_tip(&skin.skeleton, i);
            let d = tip.dist(at);
            if d <= best_d {
                best_d = d;
                best = Some(i);
            }
        }
        for i in 0..skin.skeleton.len() {
            let head = self.pose.world_pos(i);
            let d = head.dist(at);
            if d < best_d {
                best_d = d;
                best = Some(i);
            }
        }
        best
    }
}
