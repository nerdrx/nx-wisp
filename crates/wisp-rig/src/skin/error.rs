//! Skin errors.
//!
//! Validation collects **every** problem before reporting, because a skin
//! author fixing one typo per build cycle is a bad afternoon. DESIGN.md §9
//! applies to these strings too: say what happened and what to do next, in
//! sentence case, with no exclamation marks.

use crate::paint::ColorError;
use crate::path::PathError;
use crate::skeleton::SkeletonError;

#[derive(Debug, thiserror::Error)]
pub enum SkinError {
    #[error("could not read the skin file: {0}")]
    Io(#[from] std::io::Error),
    #[error("the skin is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("the skin could not be written back to TOML: {0}")]
    Serialise(#[from] toml::ser::Error),
    #[error("{0}")]
    Invalid(#[from] Issues),
}

/// Every problem found in one pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Issues(pub Vec<Issue>);

impl Issues {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, Issue> {
        self.0.iter()
    }
}

impl std::fmt::Display for Issues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.len() {
            0 => write!(f, "the skin is valid"),
            1 => write!(f, "the skin is not usable: {}", self.0[0]),
            n => {
                writeln!(f, "the skin is not usable — {n} problems:")?;
                for (i, issue) in self.0.iter().enumerate() {
                    if i + 1 == n {
                        write!(f, "  - {issue}")?;
                    } else {
                        writeln!(f, "  - {issue}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Issues {}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Issue {
    #[error("this is not a skin — 'format' is {found:?}, expected {expected:?}")]
    NotASkin { found: String, expected: &'static str },

    #[error(
        "skin format version {found} is not supported by this build \
         (it reads {supported:?}) — update NX Wisp, or ask the skin's author for \
         a version {newest} pack"
    )]
    UnknownVersion { found: u32, supported: &'static [u32], newest: u32 },

    #[error("two {kind}s are both named {name:?} — names must be unique")]
    DuplicateName { kind: &'static str, name: String },

    #[error("{referenced_by} refers to a {kind} named {name:?}, which does not exist")]
    UnknownRef { kind: &'static str, name: String, referenced_by: String },

    #[error("the bone tree has a cycle: {}", .chain.join(" -> "))]
    BoneCycle { chain: Vec<String> },

    #[error("a skin needs at least one bone")]
    NoBones,

    #[error("shape {shape:?} has an unusable path: {source}")]
    BadPath { shape: String, source: PathError },

    #[error("{at}: {source}")]
    BadColor { at: String, source: ColorError },

    #[error(
        "shape {shape:?} weights point {point} at {weight} — a weight must be \
         between 0 and 1"
    )]
    WeightOutOfRange { shape: String, point: usize, weight: f32 },

    #[error(
        "shape {shape:?} weights point {point}, but the path only has {count} \
         points (they are numbered from 0, and control points count)"
    )]
    PointOutOfRange { shape: String, point: usize, count: usize },

    #[error(
        "shape {shape:?} lists {bones} bones and {weights} weights for point \
         {point} — the two arrays must be the same length"
    )]
    WeightArityMismatch { shape: String, point: usize, bones: usize, weights: usize },

    #[error("shape {shape:?} weights point {point} to nothing — at least one weight must be above zero")]
    WeightsAllZero { shape: String, point: usize },

    #[error("shape {shape:?} sets both a solid colour and a gradient for its {slot} — pick one")]
    PaintAmbiguous { shape: String, slot: &'static str },

    #[error("shape {shape:?} has a {slot} with neither a colour nor a gradient")]
    PaintEmpty { shape: String, slot: &'static str },

    #[error("gradient {name:?} has kind {kind:?} — expected \"linear\" or \"radial\"")]
    BadGradientKind { name: String, kind: String },

    #[error("gradient {name:?} is {kind} and needs {missing}")]
    GradientMissingGeometry { name: String, kind: &'static str, missing: &'static str },

    #[error(
        "gradient {name:?} has {at} stop positions and {colors} stop colours — \
         'stop_at' and 'stop_color' must be the same length"
    )]
    GradientArityMismatch { name: String, at: usize, colors: usize },

    #[error("gradient {name:?} has no stops")]
    GradientNoStops { name: String },

    #[error("gradient {name:?} has a stop at {at} — stop positions run from 0 to 1 and must not go backwards")]
    GradientBadStop { name: String, at: f32 },

    #[error("clip {clip:?} lasts {ms} ms — a clip must be longer than zero")]
    BadDuration { clip: String, ms: f32 },

    #[error(
        "track {channel:?} on bone {bone:?} in clip {clip:?} has {times} times and \
         {values} values — 't' and 'v' must be the same length"
    )]
    TrackArityMismatch { clip: String, bone: String, channel: String, times: usize, values: usize },

    #[error(
        "track {channel:?} on bone {bone:?} in clip {clip:?} has {eases} easings for \
         {keys} keys — give one easing for the whole track, or one per key"
    )]
    EaseArityMismatch { clip: String, bone: String, channel: String, eases: usize, keys: usize },

    #[error("track {channel:?} on bone {bone:?} in clip {clip:?} has no keys")]
    EmptyTrack { clip: String, bone: String, channel: String },

    #[error(
        "track {channel:?} on bone {bone:?} in clip {clip:?} has key {index} at {t} ms, \
         before the key ahead of it — keyframe times must not go backwards"
    )]
    KeysOutOfOrder { clip: String, bone: String, channel: String, index: usize, t: f32 },

    #[error("clip {clip:?} keys the channel {channel:?}, which does not exist — expected one of tx, ty, rot, sx, sy, alpha")]
    UnknownChannel { clip: String, channel: String },

    #[error("{at} uses the easing {name:?}, which does not exist — expected linear, soft, out, spring, step, or bezier(x1,y1,x2,y2)")]
    UnknownEase { at: String, name: String },

    #[error("{at} has a malformed bezier easing {name:?} — expected bezier(x1,y1,x2,y2) with x1 and x2 between 0 and 1")]
    BadBezier { at: String, name: String },

    #[error("ik {name:?} has kind {kind:?} — expected \"look_at\" or \"two_bone\"")]
    BadIkKind { name: String, kind: String },

    #[error("ik {name:?} targets {target:?} — expected \"cursor\", \"attention\" or \"none\"")]
    BadIkTarget { name: String, target: String },

    #[error(
        "the {kind} {name:?} lists {child:?} after {parent:?}, but {child:?} is not \
         a child of {parent:?} — a chain must follow the bone tree"
    )]
    ChainNotContiguous { kind: &'static str, name: String, parent: String, child: String },

    #[error("the {kind} {name:?} needs at least {need} bones, and has {have}")]
    ChainTooShort { kind: &'static str, name: String, need: usize, have: usize },

    #[error("the canvas is {w} by {h} — both must be above zero")]
    BadCanvas { w: f32, h: f32 },

    #[error(
        "the size range is {min}..{max} with a default of {default} — the \
         default must sit inside the range, and the range must not be inverted"
    )]
    BadSizeRange { min: f32, max: f32, default: f32 },

    #[error("{at} is {value}, which is not a finite number")]
    NotFinite { at: String, value: f32 },

    #[error("{at} is {value}, which is out of range — expected {expected}")]
    OutOfRange { at: String, value: f32, expected: &'static str },
}

impl From<SkeletonError> for Issue {
    fn from(e: SkeletonError) -> Issue {
        match e {
            SkeletonError::Cycle(chain) => Issue::BoneCycle { chain },
            SkeletonError::Empty => Issue::NoBones,
            SkeletonError::DuplicateName(name) => {
                Issue::DuplicateName { kind: "bone", name }
            }
            SkeletonError::UnknownParent(bone, parent) => Issue::UnknownRef {
                kind: "bone",
                name: parent,
                referenced_by: format!("bone {bone:?}'s parent"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_issue_reads_as_a_sentence() {
        let i = Issues(vec![Issue::NoBones]);
        assert_eq!(i.to_string(), "the skin is not usable: a skin needs at least one bone");
    }

    #[test]
    fn many_issues_are_listed_together() {
        let i = Issues(vec![
            Issue::NoBones,
            Issue::DuplicateName { kind: "clip", name: "idle".into() },
        ]);
        let s = i.to_string();
        assert!(s.contains("2 problems"), "{s}");
        assert!(s.contains("idle"), "{s}");
        assert_eq!(s.lines().count(), 3);
    }

    #[test]
    fn messages_say_what_to_do_next() {
        let v = Issue::UnknownVersion { found: 9, supported: &[1], newest: 1 };
        let s = v.to_string();
        assert!(s.contains("update NX Wisp"), "{s}");
        assert!(!s.contains('!'), "no exclamation marks (DESIGN.md §9): {s}");
    }

    #[test]
    fn skeleton_errors_map_onto_issues() {
        assert_eq!(
            Issue::from(SkeletonError::Cycle(vec!["a".into(), "b".into()])),
            Issue::BoneCycle { chain: vec!["a".into(), "b".into()] }
        );
        assert!(matches!(
            Issue::from(SkeletonError::UnknownParent("tail".into(), "ghost".into())),
            Issue::UnknownRef { kind: "bone", .. }
        ));
    }

    #[test]
    fn no_message_shouts_at_the_author() {
        let all = vec![
            Issue::NoBones,
            Issue::BadCanvas { w: 0.0, h: 0.0 },
            Issue::GradientNoStops { name: "core".into() },
            Issue::WeightsAllZero { shape: "shell".into(), point: 3 },
        ];
        for i in all {
            let s = i.to_string();
            assert!(!s.contains('!'), "{s}");
            assert!(!s.is_empty());
        }
    }
}
