//! Keyframes: insert, move, delete, and the scrub/loop/onion-skin state.
//!
//! # A track is two parallel arrays
//!
//! `t` and `v` are separate arrays in the file, which is what makes a track
//! read as one stanza rather than forty. It also means every edit has to keep
//! them in step, and that is the whole reason [`crate::cmd::Command`] has
//! key-level variants instead of letting callers rewrite `TrackDoc` by hand.
//!
//! # Milliseconds here, seconds in the engine
//!
//! The document authors milliseconds because that is what a person types
//! (`wisp-rig`'s skin module says so, and compilation converts once). This
//! module stays in **milliseconds** throughout so that what the timeline shows
//! and what the file holds are the same number. The conversion to seconds
//! happens exactly once, in [`compile_track`], at the boundary where the rig's
//! own sampler takes over.
//!
//! # The preview is the shipping player
//!
//! Scrubbing does not re-implement interpolation. [`compile_track`] builds the
//! rig's own [`wisp_rig::Track`] and asks *it* for the value, so a curve in the
//! editor and the same curve at runtime cannot disagree. Cross-fades go
//! through [`crate::preview`], which drives a real `ClipPlayer`.

use wisp_rig::clip::Track as RigTrack;
use wisp_rig::ease::Ease;
use wisp_rig::skeleton::Channel;
use wisp_rig::skin::doc::{ClipDoc, EaseSpec, ExpressionDoc, Num, SkinDoc, TrackDoc};

use crate::cmd::Command;
use crate::error::EditError;

/// How close two keyframe times have to be, in milliseconds, for a click to
/// count as landing on the existing key rather than making a new one. One
/// frame at 60fps, rounded up.
pub const SNAP_MS: f32 = 17.0;

/// The six channels, in the order the timeline lists them.
pub const CHANNELS: [&str; 6] = ["tx", "ty", "rot", "sx", "sy", "alpha"];

// ------------------------------------------------------------------ lookups

pub fn clip_index(doc: &SkinDoc, name: &str) -> Option<usize> {
    doc.clips.iter().position(|c| c.name == name)
}

pub fn track_index(doc: &SkinDoc, clip: usize, bone: &str, channel: &str) -> Option<usize> {
    doc.clips
        .get(clip)?
        .tracks
        .iter()
        .position(|t| t.bone == bone && t.channel == channel)
}

fn clip_of(doc: &SkinDoc, clip: usize) -> Result<&ClipDoc, EditError> {
    doc.clips
        .get(clip)
        .ok_or(EditError::NoSuchIndex { kind: "clip", at: clip, len: doc.clips.len() })
}

fn track_of(doc: &SkinDoc, clip: usize, track: usize) -> Result<&TrackDoc, EditError> {
    let c = clip_of(doc, clip)?;
    c.tracks
        .get(track)
        .ok_or(EditError::NoSuchIndex { kind: "track", at: track, len: c.tracks.len() })
}

/// Where a key at `t` belongs in a track, so keys stay sorted by time.
pub fn insertion_index(track: &TrackDoc, t: f32) -> usize {
    track.t.iter().position(|k| k.0 > t).unwrap_or(track.t.len())
}

/// The key at `t`, within `SNAP_MS`.
pub fn key_at(track: &TrackDoc, t: f32) -> Option<usize> {
    let mut best = None;
    let mut best_d = SNAP_MS;
    for (i, k) in track.t.iter().enumerate() {
        let d = (k.0 - t).abs();
        if d <= best_d {
            best_d = d;
            best = Some(i);
        }
    }
    best
}

// -------------------------------------------------------------------- edits

/// Put a key on `(bone, channel)` in `clip` at `t` ms, creating the track if
/// this is the first key on it.
///
/// If a key already sits within [`SNAP_MS`], its value is replaced rather than
/// a second key being stacked on top — two keys a third of a frame apart is
/// never what anyone meant, and the rig would sample only one of them.
pub fn set_key(
    doc: &SkinDoc,
    clip: usize,
    bone: &str,
    channel: &str,
    t: f32,
    v: f32,
    ease: Option<&str>,
) -> Result<Command, EditError> {
    if Channel::from_name(channel).is_none() {
        return Err(EditError::NoSuchName { kind: "channel", name: channel.to_string() });
    }
    if !doc.bones.iter().any(|b| b.name == bone) {
        return Err(EditError::NoSuchName { kind: "bone", name: bone.to_string() });
    }
    if !t.is_finite() {
        return Err(EditError::NotFinite { at: "the keyframe's time", value: t });
    }
    let _ = clip_of(doc, clip)?;

    match track_index(doc, clip, bone, channel) {
        None => {
            let value = TrackDoc {
                bone: bone.to_string(),
                channel: channel.to_string(),
                t: vec![Num(t)],
                v: vec![Num(v)],
                ease: ease.map(|e| EaseSpec::All(e.to_string())),
            };
            Ok(Command::InsertTrack {
                clip,
                at: doc.clips[clip].tracks.len(),
                value: Box::new(value),
            })
        }
        Some(track) => {
            let tr = track_of(doc, clip, track)?;
            match key_at(tr, t) {
                Some(at) => Ok(Command::SetKey { clip, track, at, t: tr.t[at].0, v }),
                None => Ok(Command::InsertKey {
                    clip,
                    track,
                    at: insertion_index(tr, t),
                    t,
                    v,
                    ease: ease.map(str::to_string),
                }),
            }
        }
    }
}

/// Move a key in time and/or value.
pub fn move_key(
    doc: &SkinDoc,
    clip: usize,
    track: usize,
    key: usize,
    t: f32,
    v: f32,
) -> Result<Command, EditError> {
    let tr = track_of(doc, clip, track)?;
    if key >= tr.t.len() {
        return Err(EditError::NoSuchIndex { kind: "keyframe", at: key, len: tr.t.len() });
    }
    // Clamp into the gap between the neighbours rather than refusing: a drag
    // that runs into the next key should stop there, not throw the gesture
    // away.
    let lo = if key == 0 { f32::NEG_INFINITY } else { tr.t[key - 1].0 };
    let hi = if key + 1 >= tr.t.len() { f32::INFINITY } else { tr.t[key + 1].0 };
    let t = t.clamp(lo, hi);
    Ok(Command::SetKey { clip, track, at: key, t, v })
}

/// Delete a key. Deleting the last key of a track takes the track with it —
/// an empty track is a validation error in the rig, so the editor never leaves
/// one behind.
pub fn delete_key(
    doc: &SkinDoc,
    clip: usize,
    track: usize,
    key: usize,
) -> Result<Command, EditError> {
    let tr = track_of(doc, clip, track)?;
    if key >= tr.t.len() {
        return Err(EditError::NoSuchIndex { kind: "keyframe", at: key, len: tr.t.len() });
    }
    if tr.t.len() == 1 {
        Ok(Command::RemoveTrack { clip, at: track })
    } else {
        Ok(Command::RemoveKey { clip, track, at: key })
    }
}

/// Give one key its own easing, converting the track from a single easing to
/// a per-key list if it has to.
pub fn set_key_ease(
    doc: &SkinDoc,
    clip: usize,
    track: usize,
    key: usize,
    ease: &str,
) -> Result<Command, EditError> {
    if Ease::from_name(ease).is_none() && !ease.starts_with("bezier(") {
        return Err(EditError::NoSuchName { kind: "easing", name: ease.to_string() });
    }
    let tr = track_of(doc, clip, track)?;
    if key >= tr.t.len() {
        return Err(EditError::NoSuchIndex { kind: "keyframe", at: key, len: tr.t.len() });
    }
    let mut list: Vec<String> = match &tr.ease {
        Some(EaseSpec::Each(v)) if v.len() == tr.t.len() => v.clone(),
        Some(EaseSpec::All(name)) => vec![name.clone(); tr.t.len()],
        _ => vec!["soft".to_string(); tr.t.len()],
    };
    list[key] = ease.to_string();
    // If every key ended up the same, write the compact form back.
    let value = if list.iter().all(|e| e == &list[0]) {
        EaseSpec::All(list[0].clone())
    } else {
        EaseSpec::Each(list)
    };
    Ok(Command::SetTrackEase { clip, track, value: Some(value) })
}

/// Set the easing of a whole track.
pub fn set_track_ease(
    doc: &SkinDoc,
    clip: usize,
    track: usize,
    ease: &str,
) -> Result<Command, EditError> {
    if Ease::from_name(ease).is_none() && !ease.starts_with("bezier(") {
        return Err(EditError::NoSuchName { kind: "easing", name: ease.to_string() });
    }
    let _ = track_of(doc, clip, track)?;
    Ok(Command::SetTrackEase { clip, track, value: Some(EaseSpec::All(ease.to_string())) })
}

/// A new, empty clip.
pub fn add_clip(doc: &SkinDoc, name: &str, duration_ms: f32) -> Result<Command, EditError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "clip", name: name.to_string() });
    }
    if doc.clips.iter().any(|c| c.name == trimmed) {
        return Err(EditError::DuplicateName { kind: "clip", name: trimmed.to_string() });
    }
    if !duration_ms.is_finite() || duration_ms <= 0.0 {
        return Err(EditError::NotFinite { at: "the clip's duration", value: duration_ms });
    }
    Ok(Command::InsertClip {
        at: doc.clips.len(),
        value: Box::new(ClipDoc {
            name: trimmed.to_string(),
            duration_ms: Num(duration_ms),
            looping: true,
            additive: false,
            tracks: Vec::new(),
        }),
    })
}

pub fn set_clip_duration(doc: &SkinDoc, clip: usize, ms: f32) -> Result<Command, EditError> {
    let c = clip_of(doc, clip)?;
    if !ms.is_finite() || ms <= 0.0 {
        return Err(EditError::NotFinite { at: "the clip's duration", value: ms });
    }
    Ok(Command::SetClip { at: clip, value: Box::new(ClipDoc { duration_ms: Num(ms), ..c.clone() }) })
}

pub fn set_clip_looping(doc: &SkinDoc, clip: usize, looping: bool) -> Result<Command, EditError> {
    let c = clip_of(doc, clip)?;
    Ok(Command::SetClip { at: clip, value: Box::new(ClipDoc { looping, ..c.clone() }) })
}

pub fn set_clip_additive(doc: &SkinDoc, clip: usize, additive: bool) -> Result<Command, EditError> {
    let c = clip_of(doc, clip)?;
    Ok(Command::SetClip { at: clip, value: Box::new(ClipDoc { additive, ..c.clone() }) })
}

/// Point one of F74's eight expressions at a clip.
pub fn set_expression_clip(
    doc: &SkinDoc,
    expression: usize,
    clip: &str,
) -> Result<Command, EditError> {
    let e = doc.expressions.get(expression).ok_or(EditError::NoSuchIndex {
        kind: "expression",
        at: expression,
        len: doc.expressions.len(),
    })?;
    if !doc.clips.iter().any(|c| c.name == clip) {
        return Err(EditError::NoSuchName { kind: "clip", name: clip.to_string() });
    }
    Ok(Command::SetExpression {
        at: expression,
        value: ExpressionDoc { clip: clip.to_string(), ..e.clone() },
    })
}

// ----------------------------------------------------------------- sampling

/// Build the rig's own track so the editor samples exactly what the runtime
/// samples. Times become **seconds**, degrees stay as the file wrote them —
/// the rig's compiler converts rotation at load, and a curve editor that
/// silently showed radians would be unusable.
pub fn compile_track(doc: &SkinDoc, clip: usize, track: usize) -> Result<RigTrack, EditError> {
    let tr = track_of(doc, clip, track)?;
    let channel = Channel::from_name(&tr.channel)
        .ok_or(EditError::NoSuchName { kind: "channel", name: tr.channel.clone() })?;
    let mut out = RigTrack::new(0, channel);
    let n = tr.t.len().min(tr.v.len());
    for i in 0..n {
        let name = tr.ease.as_ref().and_then(|e| e.name_for(i, n)).unwrap_or("soft");
        let ease = Ease::from_name(name).unwrap_or(Ease::Soft);
        out = out.key(tr.t[i].0 / 1000.0, tr.v[i].0, ease);
    }
    Ok(out)
}

/// The value of a track at `t` milliseconds.
pub fn sample(doc: &SkinDoc, clip: usize, track: usize, t_ms: f32) -> Result<f32, EditError> {
    Ok(compile_track(doc, clip, track)?.sample(t_ms / 1000.0))
}

// ------------------------------------------------------------------- state

/// Onion skin: how many frames either side of the playhead are ghosted, and
/// how far apart they are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Onion {
    pub enabled: bool,
    pub before: usize,
    pub after: usize,
    pub spacing_ms: f32,
    /// Alpha of the nearest ghost; further ones fade linearly from it.
    pub strength: f32,
}

impl Default for Onion {
    fn default() -> Self {
        Onion { enabled: false, before: 2, after: 1, spacing_ms: 80.0, strength: 0.35 }
    }
}

impl Onion {
    /// The times to draw ghosts at, with the alpha each one gets. Times are
    /// wrapped into `0..duration` for a looping clip and clamped for a
    /// one-shot, so a ghost never asks the player for a time it cannot show.
    pub fn ghosts(&self, playhead_ms: f32, duration_ms: f32, looping: bool) -> Vec<(f32, f32)> {
        if !self.enabled || duration_ms <= 0.0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.before + self.after);
        let mut push = |k: i32, n: usize| {
            let raw = playhead_ms + k as f32 * self.spacing_ms;
            let t = if looping {
                raw.rem_euclid(duration_ms)
            } else {
                raw.clamp(0.0, duration_ms)
            };
            let depth = k.unsigned_abs() as f32;
            let alpha = self.strength * (1.0 - (depth - 1.0) / (n.max(1) as f32)).clamp(0.15, 1.0);
            out.push((t, alpha));
        };
        for i in 1..=self.before {
            push(-(i as i32), self.before);
        }
        for i in 1..=self.after {
            push(i as i32, self.after);
        }
        out
    }
}

/// Everything the timeline panel needs that is not in the document.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineState {
    /// Which clip is being edited.
    pub clip: usize,
    /// Which layer the preview plays it on.
    pub layer: usize,
    /// Where the playhead is, in milliseconds.
    pub playhead_ms: f32,
    pub playing: bool,
    /// Override the clip's own `looping` for preview only.
    pub loop_preview: bool,
    pub onion: Onion,
    /// Pixels per millisecond across the timeline's ruler.
    pub scale_px_per_ms: f32,
    /// Leftmost visible time.
    pub scroll_ms: f32,
    /// Which bone's tracks are expanded, by bone index.
    pub expanded: Vec<usize>,
}

impl Default for TimelineState {
    fn default() -> Self {
        TimelineState {
            clip: 0,
            layer: 0,
            playhead_ms: 0.0,
            playing: false,
            loop_preview: true,
            onion: Onion::default(),
            scale_px_per_ms: 0.12,
            scroll_ms: 0.0,
            expanded: Vec::new(),
        }
    }
}

impl TimelineState {
    /// Advance the playhead. Pure: the host supplies the delta, so this module
    /// still owns no clock.
    pub fn tick(&mut self, dt_ms: f32, duration_ms: f32) {
        if !self.playing || duration_ms <= 0.0 {
            return;
        }
        let t = self.playhead_ms + dt_ms;
        self.playhead_ms = if self.loop_preview {
            t.rem_euclid(duration_ms)
        } else {
            t.min(duration_ms)
        };
        if !self.loop_preview && self.playhead_ms >= duration_ms {
            self.playing = false;
        }
    }

    /// Scrub to a pixel position on the ruler.
    pub fn scrub_to_px(&mut self, x_px: f32, ruler_x: f32, duration_ms: f32) {
        let ms = self.scroll_ms + (x_px - ruler_x) / self.scale_px_per_ms.max(1e-6);
        self.playhead_ms = ms.clamp(0.0, duration_ms.max(0.0));
    }

    pub fn time_to_px(&self, ms: f32, ruler_x: f32) -> f32 {
        ruler_x + (ms - self.scroll_ms) * self.scale_px_per_ms
    }

    pub fn px_to_time(&self, x: f32, ruler_x: f32) -> f32 {
        self.scroll_ms + (x - ruler_x) / self.scale_px_per_ms.max(1e-6)
    }

    pub fn toggle_expanded(&mut self, bone: usize) {
        match self.expanded.iter().position(|b| *b == bone) {
            Some(i) => {
                self.expanded.remove(i);
            }
            None => self.expanded.push(bone),
        }
    }

    pub fn is_expanded(&self, bone: usize) -> bool {
        self.expanded.contains(&bone)
    }
}

/// One row of the timeline: a bone, and the tracks under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub bone: String,
    pub tracks: Vec<usize>,
}

/// Group a clip's tracks by bone, in the document's bone order so the timeline
/// and the bone tree read top-to-bottom the same way.
pub fn rows(doc: &SkinDoc, clip: usize) -> Vec<Row> {
    let Some(c) = doc.clips.get(clip) else { return Vec::new() };
    let mut rows: Vec<Row> = Vec::new();
    for b in &doc.bones {
        let tracks: Vec<usize> = c
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.bone == b.name)
            .map(|(i, _)| i)
            .collect();
        if !tracks.is_empty() {
            rows.push(Row { bone: b.name.clone(), tracks });
        }
    }
    // Tracks naming a bone that is not in the document still get a row, so the
    // problem is visible where it can be fixed.
    for (i, t) in c.tracks.iter().enumerate() {
        if !doc.bones.iter().any(|b| b.name == t.bone) && !rows.iter().any(|r| r.bone == t.bone) {
            rows.push(Row { bone: t.bone.clone(), tracks: vec![i] });
        }
    }
    rows
}
