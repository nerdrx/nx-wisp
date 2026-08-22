//! Skin parsing, validation, compilation and serialisation (F49, SPEC §3.6).
//!
//! [`SkinDoc`] is the file. [`Skin`] is the compiled form the rig runs: names
//! resolved to indices, degrees to radians, milliseconds to seconds, path
//! strings to point arrays, weights normalised. Compilation is where every
//! error lives, so the per-frame code has no error paths at all.
//!
//! `wisp-rig` is the only crate allowed to change this format (SPEC §3.6), and
//! the format is proven by use: the shipped default skin, "Wisp", is loaded
//! through exactly this parser, with no privileged path.

pub mod doc;
pub mod error;

use std::collections::HashMap;
use std::path::Path as FsPath;

use crate::clip::{Clip, Track, REQUIRED_CLIPS, REQUIRED_EXPRESSIONS};
use crate::deform::{auto_bind, AutoBind, Binding, Influence};
use crate::ease::{Ease, SpringParams};
use crate::ik::LookAt;
use crate::math::{clamp, deg_to_rad, Vec2};
use crate::motion::{ChainParams, SquashParams};
use crate::paint::{Cap, Extend, FillRule, GradientStop, Join, Rgba};
use crate::path::Path;
use crate::physics::PhysicsParams;
use crate::player::LayerSpec;
use crate::skeleton::{BoneRest, BoneSpec, Channel, Skeleton};

pub use doc::{SkinDoc, FORMAT_MAGIC, FORMAT_VERSION, SUPPORTED_VERSIONS};
pub use error::{Issue, Issues, SkinError};

use doc::{pt, Num};

/// The default skin, "Wisp" (F73), embedded in the binary.
///
/// She ships as an ordinary skin file loaded through the ordinary parser —
/// there is no privileged path for a first-party pack, which is what keeps the
/// format honest and makes the in-app editor (F76) able to open her.
pub const WISP_SKIN_TOML: &str = include_str!("../../skins/wisp.skin.toml");

/// Parse and compile the embedded default skin.
pub fn default_skin() -> Result<Skin, SkinError> {
    Skin::parse(WISP_SKIN_TOML)
}

// ---------------------------------------------------------------------------
// Compiled types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub name: Box<str>,
    pub author: Box<str>,
    pub license: Box<str>,
    pub summary: Box<str>,
    pub default_size_px: f32,
    pub min_size_px: f32,
    pub max_size_px: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Canvas {
    pub size: Vec2,
    /// The canvas point that sits at her screen position.
    pub anchor: Vec2,
}

/// Where a shape gets its colour. Gradients are indices so the rig can update
/// a following gradient's geometry once per frame instead of per shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaintRef {
    Solid(Rgba),
    Gradient { index: usize, alpha: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeRef {
    pub paint: PaintRef,
    pub width: f32,
    pub cap: Cap,
    pub join: Join,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GradientGeom {
    Linear { start: Vec2, end: Vec2 },
    Radial { center: Vec2, focus: Vec2, radius: f32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct GradientDef {
    pub name: Box<str>,
    pub geom: GradientGeom,
    pub extend: Extend,
    pub stops: Vec<GradientStop>,
    /// Displaces the gradient with a bone — the mechanism behind light that
    /// moves *inside* her (F73).
    pub follow_bone: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShapeDef {
    pub name: Box<str>,
    pub z: i32,
    pub opacity: f32,
    pub silhouette: bool,
    pub fill_rule: FillRule,
    /// Rest geometry in canvas units. Never written after compilation.
    pub path: Path,
    pub binding: Binding,
    pub fill: Option<PaintRef>,
    pub stroke: Option<StrokeRef>,
}

/// What drives an IK constraint at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IkTarget {
    /// The pointer.
    Cursor,
    /// Whatever currently has her attention (F69) — a notification, the active
    /// window, the operator.
    Attention,
    /// Driven by code, not by a standing target.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IkKind {
    LookAt { bone: usize, cfg: LookAt },
    TwoBone { root: usize, mid: usize, end: usize, bend_positive: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct IkDef {
    pub name: Box<str>,
    pub kind: IkKind,
    pub target: IkTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChainDef {
    pub name: Box<str>,
    /// Root first, contiguous down the bone tree.
    pub bones: Vec<usize>,
    pub params: ChainParams,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionDef {
    pub name: Box<str>,
    pub clip: usize,
    pub layer: usize,
    pub weight: f32,
    /// Seconds.
    pub fade: f32,
}

/// The procedural motion layer's wiring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionConfig {
    pub squash: SquashParams,
    pub squash_bone: Option<usize>,
    pub lean_bone: Option<usize>,
    pub lean: SpringParams,
    pub lean_gain: f32,
    pub light_bone: Option<usize>,
    pub light_gain: f32,
    pub light_range: f32,
}

impl Default for MotionConfig {
    fn default() -> Self {
        MotionConfig {
            squash: SquashParams::default(),
            squash_bone: None,
            lean_bone: None,
            lean: SpringParams::with_ratio(240.0, 1.0, 0.55),
            lean_gain: 0.35,
            light_bone: None,
            light_gain: 0.02,
            light_range: 14.0,
        }
    }
}

/// A validated, compiled skin.
#[derive(Debug, Clone)]
pub struct Skin {
    doc: SkinDoc,
    pub meta: Meta,
    pub canvas: Canvas,
    pub skeleton: Skeleton,
    pub gradients: Vec<GradientDef>,
    pub shapes: Vec<ShapeDef>,
    pub layers: Vec<LayerSpec>,
    pub clips: Vec<Clip>,
    pub expressions: Vec<ExpressionDef>,
    pub iks: Vec<IkDef>,
    pub chains: Vec<ChainDef>,
    pub physics: PhysicsParams,
    pub motion: MotionConfig,
    clip_index: HashMap<Box<str>, usize>,
    expr_index: HashMap<Box<str>, usize>,
    layer_index: HashMap<Box<str>, usize>,
}

impl Skin {
    /// Parse and compile from TOML source.
    pub fn parse(src: &str) -> Result<Skin, SkinError> {
        let doc: SkinDoc = toml::from_str(src)?;
        Skin::compile(doc)
    }

    /// Read and compile a skin file.
    pub fn load(path: &FsPath) -> Result<Skin, SkinError> {
        let src = std::fs::read_to_string(path)?;
        Skin::parse(&src)
    }

    /// Serialise back to TOML.
    ///
    /// Round-trips the document faithfully, but **does not preserve comments**
    /// — serde has nowhere to keep them. The in-app editor (F76) rewrites
    /// whole files, so this is the right trade; a tool that wants to preserve
    /// an author's comments should edit the TOML in place instead.
    pub fn to_toml(&self) -> Result<String, SkinError> {
        Ok(toml::to_string_pretty(&self.doc)?)
    }

    /// The document this was compiled from — the editor's working copy.
    pub fn doc(&self) -> &SkinDoc {
        &self.doc
    }

    pub fn clip_index(&self, name: &str) -> Option<usize> {
        self.clip_index.get(name).copied()
    }
    pub fn expression_index(&self, name: &str) -> Option<usize> {
        self.expr_index.get(name).copied()
    }
    pub fn layer_index(&self, name: &str) -> Option<usize> {
        self.layer_index.get(name).copied()
    }
    pub fn bone_index(&self, name: &str) -> Option<usize> {
        self.skeleton.index_of(name)
    }

    /// Which of the clips F67/F70/F72 name are missing. Not an error — a skin
    /// without a `hop` still loads and simply never hops — but `wispkit
    /// validate` (F51) turns it into a warning.
    pub fn missing_required_clips(&self) -> Vec<&'static str> {
        REQUIRED_CLIPS
            .iter()
            .copied()
            .filter(|n| !self.clip_index.contains_key(*n))
            .collect()
    }

    /// Which of F74's eight expressions are missing.
    pub fn missing_required_expressions(&self) -> Vec<&'static str> {
        REQUIRED_EXPRESSIONS
            .iter()
            .copied()
            .filter(|n| !self.expr_index.contains_key(*n))
            .collect()
    }

    /// Canvas units per surface pixel at a given rendered size.
    pub fn scale_for(&self, size_px: f32) -> f32 {
        let extent = self.canvas.size.x.max(self.canvas.size.y).max(1e-3);
        size_px.max(1.0) / extent
    }

    /// Compile a document. Reports every problem it finds, not just the first.
    pub fn compile(doc: SkinDoc) -> Result<Skin, SkinError> {
        let mut issues: Vec<Issue> = Vec::new();

        // --- header -------------------------------------------------------
        if doc.format != FORMAT_MAGIC {
            issues.push(Issue::NotASkin {
                found: doc.format.clone(),
                expected: FORMAT_MAGIC,
            });
        }
        if !SUPPORTED_VERSIONS.contains(&doc.version) {
            issues.push(Issue::UnknownVersion {
                found: doc.version,
                supported: SUPPORTED_VERSIONS,
                newest: FORMAT_VERSION,
            });
        }
        // A wrong magic or version means nothing below can be trusted.
        if !issues.is_empty() {
            return Err(SkinError::Invalid(Issues(issues)));
        }

        // --- meta and canvas ----------------------------------------------
        let canvas = Canvas { size: pt(doc.canvas.size), anchor: pt(doc.canvas.anchor) };
        if !(canvas.size.x > 0.0 && canvas.size.y > 0.0)
            || !canvas.size.is_finite()
            || !canvas.anchor.is_finite()
        {
            issues.push(Issue::BadCanvas { w: canvas.size.x, h: canvas.size.y });
        }
        let meta = Meta {
            name: doc.meta.name.clone().into_boxed_str(),
            author: doc.meta.author.clone().into_boxed_str(),
            license: doc.meta.license.clone().into_boxed_str(),
            summary: doc.meta.summary.clone().into_boxed_str(),
            default_size_px: doc.meta.default_size_px.0,
            min_size_px: doc.meta.min_size_px.0,
            max_size_px: doc.meta.max_size_px.0,
        };
        if !(meta.min_size_px > 0.0
            && meta.max_size_px >= meta.min_size_px
            && (meta.min_size_px..=meta.max_size_px).contains(&meta.default_size_px))
        {
            issues.push(Issue::BadSizeRange {
                min: meta.min_size_px,
                max: meta.max_size_px,
                default: meta.default_size_px,
            });
        }

        // --- skeleton -----------------------------------------------------
        let specs: Vec<BoneSpec> = doc
            .bones
            .iter()
            .map(|b| BoneSpec {
                name: b.name.clone(),
                parent: if b.parent.is_empty() { None } else { Some(b.parent.clone()) },
                rest: BoneRest {
                    pos: pt(b.pos),
                    rot: deg_to_rad(num(b.rot, 0.0)),
                    scale: b.scale.map(pt).unwrap_or(Vec2::ONE),
                },
                length: num(b.length, 0.0),
            })
            .collect();
        let skeleton = match Skeleton::build(&specs) {
            Ok(s) => s,
            Err(e) => {
                issues.push(Issue::from(e));
                // Nothing below can resolve a bone name; report what we have.
                return Err(SkinError::Invalid(Issues(issues)));
            }
        };
        let bone_of = |name: &str| skeleton.index_of(name);

        // --- colours ------------------------------------------------------
        let mut colors: HashMap<&str, Rgba> = HashMap::new();
        for c in &doc.colors {
            match Rgba::parse_hex(&c.value) {
                Ok(v) => {
                    if colors.insert(c.name.as_str(), v).is_some() {
                        issues.push(Issue::DuplicateName {
                            kind: "colour",
                            name: c.name.clone(),
                        });
                    }
                }
                Err(source) => issues.push(Issue::BadColor {
                    at: format!("colour {:?}", c.name),
                    source,
                }),
            }
        }

        // --- gradients ----------------------------------------------------
        let mut gradients: Vec<GradientDef> = Vec::with_capacity(doc.gradients.len());
        let mut gradient_index: HashMap<&str, usize> = HashMap::new();
        for g in &doc.gradients {
            if gradient_index.contains_key(g.name.as_str()) {
                issues.push(Issue::DuplicateName { kind: "gradient", name: g.name.clone() });
            }
            let geom = match g.kind.as_str() {
                "linear" => {
                    if g.start.is_none() || g.end.is_none() {
                        issues.push(Issue::GradientMissingGeometry {
                            name: g.name.clone(),
                            kind: "linear",
                            missing: "'start' and 'end'",
                        });
                    }
                    GradientGeom::Linear {
                        start: g.start.map(pt).unwrap_or(Vec2::ZERO),
                        end: g.end.map(pt).unwrap_or(Vec2::new(1.0, 0.0)),
                    }
                }
                "radial" => {
                    if g.center.is_none() || g.radius.is_none() {
                        issues.push(Issue::GradientMissingGeometry {
                            name: g.name.clone(),
                            kind: "radial",
                            missing: "'center' and 'radius'",
                        });
                    }
                    let center = g.center.map(pt).unwrap_or(Vec2::ZERO);
                    GradientGeom::Radial {
                        center,
                        focus: g.focus.map(pt).unwrap_or(center),
                        radius: num(g.radius, 1.0).max(1e-3),
                    }
                }
                other => {
                    issues.push(Issue::BadGradientKind {
                        name: g.name.clone(),
                        kind: other.to_string(),
                    });
                    GradientGeom::Linear { start: Vec2::ZERO, end: Vec2::new(1.0, 0.0) }
                }
            };

            if g.stop_at.len() != g.stop_color.len() {
                issues.push(Issue::GradientArityMismatch {
                    name: g.name.clone(),
                    at: g.stop_at.len(),
                    colors: g.stop_color.len(),
                });
            }
            if g.stop_at.is_empty() {
                issues.push(Issue::GradientNoStops { name: g.name.clone() });
            }
            let mut stops = Vec::with_capacity(g.stop_at.len());
            let mut last = f32::NEG_INFINITY;
            for (i, at) in g.stop_at.iter().enumerate() {
                if !(0.0..=1.0).contains(&at.0) || at.0 < last {
                    issues.push(Issue::GradientBadStop { name: g.name.clone(), at: at.0 });
                }
                last = at.0;
                let Some(text) = g.stop_color.get(i) else {
                    continue;
                };
                match resolve_color(text, &colors) {
                    Ok(c) => stops.push(GradientStop { at: clamp(at.0, 0.0, 1.0), color: c }),
                    Err(source) => issues.push(Issue::BadColor {
                        at: format!("gradient {:?} stop {i}", g.name),
                        source,
                    }),
                }
            }

            let follow_bone = if g.follow_bone.is_empty() {
                None
            } else {
                match bone_of(&g.follow_bone) {
                    Some(b) => Some(b),
                    None => {
                        issues.push(Issue::UnknownRef {
                            kind: "bone",
                            name: g.follow_bone.clone(),
                            referenced_by: format!("gradient {:?}'s follow_bone", g.name),
                        });
                        None
                    }
                }
            };

            gradient_index.insert(g.name.as_str(), gradients.len());
            gradients.push(GradientDef {
                name: g.name.clone().into_boxed_str(),
                geom,
                extend: parse_extend(&g.extend),
                stops,
                follow_bone,
            });
        }

        // --- shapes -------------------------------------------------------
        let mut shapes: Vec<ShapeDef> = Vec::with_capacity(doc.shapes.len());
        let mut seen_shapes: HashMap<&str, ()> = HashMap::new();
        for s in &doc.shapes {
            if seen_shapes.insert(s.name.as_str(), ()).is_some() {
                issues.push(Issue::DuplicateName { kind: "shape", name: s.name.clone() });
            }
            let path = match Path::parse(&s.path) {
                Ok(p) => p,
                Err(source) => {
                    issues.push(Issue::BadPath { shape: s.name.clone(), source });
                    Path::new()
                }
            };
            let n = path.point_count();

            let fill = s.fill.as_ref().and_then(|p| {
                compile_paint(
                    &s.name,
                    "fill",
                    &p.color,
                    &p.gradient,
                    p.alpha,
                    &colors,
                    &gradient_index,
                    &mut issues,
                )
            });
            let stroke = s.stroke.as_ref().and_then(|st| {
                let paint = compile_paint(
                    &s.name,
                    "stroke",
                    &st.color,
                    &st.gradient,
                    st.alpha,
                    &colors,
                    &gradient_index,
                    &mut issues,
                )?;
                Some(StrokeRef {
                    paint,
                    width: st.width.0.max(0.0),
                    cap: parse_cap(&st.cap),
                    join: parse_join(&st.join),
                })
            });

            // --- binding ---
            let mut base_lists: Vec<Vec<Influence>> = if let Some(ab) = &s.bind_auto {
                let mut bones = Vec::with_capacity(ab.bones.len());
                for name in &ab.bones {
                    match bone_of(name) {
                        Some(b) => bones.push(b),
                        None => issues.push(Issue::UnknownRef {
                            kind: "bone",
                            name: name.clone(),
                            referenced_by: format!("shape {:?}'s bind_auto", s.name),
                        }),
                    }
                }
                let cfg = AutoBind {
                    falloff: num(ab.falloff, 40.0),
                    power: num(ab.power, 2.0),
                };
                let b = auto_bind(&skeleton, &path.points, &bones, cfg);
                (0..n).map(|i| b.influences_of(i).to_vec()).collect()
            } else {
                let bone = if s.bind.is_empty() {
                    0
                } else {
                    match bone_of(&s.bind) {
                        Some(b) => b,
                        None => {
                            issues.push(Issue::UnknownRef {
                                kind: "bone",
                                name: s.bind.clone(),
                                referenced_by: format!("shape {:?}'s bind", s.name),
                            });
                            0
                        }
                    }
                };
                vec![vec![Influence { bone: bone as u32, weight: 1.0 }]; n]
            };

            for w in &s.weights {
                if w.point >= n {
                    issues.push(Issue::PointOutOfRange {
                        shape: s.name.clone(),
                        point: w.point,
                        count: n,
                    });
                    continue;
                }
                if w.bones.len() != w.weights.len() {
                    issues.push(Issue::WeightArityMismatch {
                        shape: s.name.clone(),
                        point: w.point,
                        bones: w.bones.len(),
                        weights: w.weights.len(),
                    });
                    continue;
                }
                let mut list = Vec::with_capacity(w.bones.len());
                let mut total = 0.0f32;
                for (bi, name) in w.bones.iter().enumerate() {
                    let weight = w.weights[bi].0;
                    if !(0.0..=1.0).contains(&weight) || !weight.is_finite() {
                        issues.push(Issue::WeightOutOfRange {
                            shape: s.name.clone(),
                            point: w.point,
                            weight,
                        });
                        continue;
                    }
                    match bone_of(name) {
                        Some(b) => {
                            total += weight;
                            list.push(Influence { bone: b as u32, weight });
                        }
                        None => issues.push(Issue::UnknownRef {
                            kind: "bone",
                            name: name.clone(),
                            referenced_by: format!(
                                "shape {:?}'s weight for point {}",
                                s.name, w.point
                            ),
                        }),
                    }
                }
                if total <= 1e-6 {
                    issues.push(Issue::WeightsAllZero {
                        shape: s.name.clone(),
                        point: w.point,
                    });
                    continue;
                }
                base_lists[w.point] = list;
            }

            shapes.push(ShapeDef {
                name: s.name.clone().into_boxed_str(),
                z: s.z,
                opacity: clamp(num(s.opacity, 1.0), 0.0, 1.0),
                silhouette: s.silhouette,
                fill_rule: if s.fill_rule == "evenodd" {
                    FillRule::EvenOdd
                } else {
                    FillRule::NonZero
                },
                binding: Binding::from_lists(&base_lists),
                path,
                fill,
                stroke,
            });
        }
        shapes.sort_by_key(|s| s.z);

        // --- layers -------------------------------------------------------
        let mut layer_docs = doc.layers.clone();
        if layer_docs.is_empty() {
            // A minimal skin still needs somewhere to play clips.
            layer_docs = vec![
                doc::LayerDoc {
                    name: "base".into(),
                    additive: false,
                    default_clip: String::new(),
                    weight: None,
                },
                doc::LayerDoc {
                    name: "face".into(),
                    additive: true,
                    default_clip: String::new(),
                    weight: None,
                },
            ];
        }
        let mut layer_index: HashMap<Box<str>, usize> = HashMap::new();
        for (i, l) in layer_docs.iter().enumerate() {
            if layer_index
                .insert(l.name.clone().into_boxed_str(), i)
                .is_some()
            {
                issues.push(Issue::DuplicateName { kind: "layer", name: l.name.clone() });
            }
        }

        // --- clips --------------------------------------------------------
        let mut clips: Vec<Clip> = Vec::with_capacity(doc.clips.len());
        let mut clip_index: HashMap<Box<str>, usize> = HashMap::new();
        for c in &doc.clips {
            if clip_index
                .insert(c.name.clone().into_boxed_str(), clips.len())
                .is_some()
            {
                issues.push(Issue::DuplicateName { kind: "clip", name: c.name.clone() });
            }
            // Written as a negated comparison on purpose: it rejects NaN,
            // which `<= 0.0` would quietly accept.
            #[allow(clippy::neg_cmp_op_on_partial_ord)]
            let bad_duration = !(c.duration_ms.0 > 0.0) || !c.duration_ms.0.is_finite();
            if bad_duration {
                issues.push(Issue::BadDuration { clip: c.name.clone(), ms: c.duration_ms.0 });
            }
            let mut clip = Clip {
                name: c.name.clone().into_boxed_str(),
                duration: (c.duration_ms.0 / 1000.0).max(1e-4),
                looping: c.looping,
                additive: c.additive,
                tracks: Vec::with_capacity(c.tracks.len()),
            };
            for t in &c.tracks {
                let Some(bone) = bone_of(&t.bone) else {
                    issues.push(Issue::UnknownRef {
                        kind: "bone",
                        name: t.bone.clone(),
                        referenced_by: format!("a track in clip {:?}", c.name),
                    });
                    continue;
                };
                let Some(channel) = Channel::from_name(&t.channel) else {
                    issues.push(Issue::UnknownChannel {
                        clip: c.name.clone(),
                        channel: t.channel.clone(),
                    });
                    continue;
                };
                if t.t.len() != t.v.len() {
                    issues.push(Issue::TrackArityMismatch {
                        clip: c.name.clone(),
                        bone: t.bone.clone(),
                        channel: t.channel.clone(),
                        times: t.t.len(),
                        values: t.v.len(),
                    });
                    continue;
                }
                if t.t.is_empty() {
                    issues.push(Issue::EmptyTrack {
                        clip: c.name.clone(),
                        bone: t.bone.clone(),
                        channel: t.channel.clone(),
                    });
                    continue;
                }
                if let Some(spec) = &t.ease {
                    if let Some(n_ease) = spec.len() {
                        if n_ease != t.t.len() {
                            issues.push(Issue::EaseArityMismatch {
                                clip: c.name.clone(),
                                bone: t.bone.clone(),
                                channel: t.channel.clone(),
                                eases: n_ease,
                                keys: t.t.len(),
                            });
                        }
                    }
                }

                let mut track = Track::new(bone, channel);
                let mut last_t = f32::NEG_INFINITY;
                for (i, time) in t.t.iter().enumerate() {
                    if time.0 < last_t {
                        issues.push(Issue::KeysOutOfOrder {
                            clip: c.name.clone(),
                            bone: t.bone.clone(),
                            channel: t.channel.clone(),
                            index: i,
                            t: time.0,
                        });
                    }
                    last_t = time.0;
                    let raw = t.v[i].0;
                    if !raw.is_finite() || !time.0.is_finite() {
                        issues.push(Issue::NotFinite {
                            at: format!(
                                "clip {:?}, track {:?} on bone {:?}, key {i}",
                                c.name, t.channel, t.bone
                            ),
                            value: raw,
                        });
                    }
                    // Rotation is authored in degrees.
                    let value = if channel == Channel::Rot { deg_to_rad(raw) } else { raw };
                    let ease = match t.ease.as_ref().and_then(|s| s.name_for(i, t.t.len())) {
                        Some(name) => parse_ease(
                            name,
                            &format!("clip {:?}, track {:?} on bone {:?}", c.name, t.channel, t.bone),
                            &mut issues,
                        ),
                        None => Ease::Soft,
                    };
                    track.times.push(time.0 / 1000.0);
                    track.values.push(value);
                    track.eases.push(ease);
                }
                clip.tracks.push(track);
            }
            clips.push(clip);
        }

        // --- layer default clips (now that clips exist) --------------------
        let mut layers: Vec<LayerSpec> = Vec::with_capacity(layer_docs.len());
        for l in &layer_docs {
            let default_clip = if l.default_clip.is_empty() {
                None
            } else {
                match clip_index.get(l.default_clip.as_str()) {
                    Some(&c) => Some(c),
                    None => {
                        issues.push(Issue::UnknownRef {
                            kind: "clip",
                            name: l.default_clip.clone(),
                            referenced_by: format!("layer {:?}'s default_clip", l.name),
                        });
                        None
                    }
                }
            };
            layers.push(LayerSpec {
                name: l.name.clone().into_boxed_str(),
                additive: l.additive,
                default_clip,
                weight: clamp(num(l.weight, 1.0), 0.0, 1.0),
            });
        }

        // --- expressions ---------------------------------------------------
        let default_expr_layer = layers
            .iter()
            .rposition(|l| l.additive)
            .unwrap_or(layers.len().saturating_sub(1));
        let mut expressions: Vec<ExpressionDef> = Vec::with_capacity(doc.expressions.len());
        let mut expr_index: HashMap<Box<str>, usize> = HashMap::new();
        for e in &doc.expressions {
            if expr_index
                .insert(e.name.clone().into_boxed_str(), expressions.len())
                .is_some()
            {
                issues.push(Issue::DuplicateName {
                    kind: "expression",
                    name: e.name.clone(),
                });
            }
            let Some(&clip) = clip_index.get(e.clip.as_str()) else {
                issues.push(Issue::UnknownRef {
                    kind: "clip",
                    name: e.clip.clone(),
                    referenced_by: format!("expression {:?}", e.name),
                });
                continue;
            };
            let layer = if e.layer.is_empty() {
                default_expr_layer
            } else {
                match layer_index.get(e.layer.as_str()) {
                    Some(&l) => l,
                    None => {
                        issues.push(Issue::UnknownRef {
                            kind: "layer",
                            name: e.layer.clone(),
                            referenced_by: format!("expression {:?}", e.name),
                        });
                        default_expr_layer
                    }
                }
            };
            expressions.push(ExpressionDef {
                name: e.name.clone().into_boxed_str(),
                clip,
                layer,
                weight: clamp(num(e.weight, 1.0), 0.0, 1.0),
                fade: num(e.fade_ms, 160.0).max(0.0) / 1000.0,
            });
        }

        // --- ik ------------------------------------------------------------
        let mut iks: Vec<IkDef> = Vec::with_capacity(doc.iks.len());
        for k in &doc.iks {
            let target = match k.target.as_str() {
                "" | "none" => IkTarget::None,
                "cursor" => IkTarget::Cursor,
                "attention" => IkTarget::Attention,
                other => {
                    issues.push(Issue::BadIkTarget {
                        name: k.name.clone(),
                        target: other.to_string(),
                    });
                    IkTarget::None
                }
            };
            let mut resolve = |name: &str, what: &str| match bone_of(name) {
                Some(b) => Some(b),
                None => {
                    issues.push(Issue::UnknownRef {
                        kind: "bone",
                        name: name.to_string(),
                        referenced_by: format!("ik {:?}'s {what}", k.name),
                    });
                    None
                }
            };
            let kind = match k.kind.as_str() {
                "look_at" => {
                    let Some(bone) = resolve(&k.bone, "bone") else {
                        continue;
                    };
                    IkKind::LookAt {
                        bone,
                        cfg: LookAt {
                            forward: k.forward.map(pt).unwrap_or(Vec2::new(0.0, -1.0)),
                            max_angle: deg_to_rad(num(k.max_deg, 26.0)).abs(),
                            weight: clamp(num(k.weight, 1.0), 0.0, 1.0),
                        },
                    }
                }
                "two_bone" => {
                    let (Some(root), Some(mid), Some(end)) = (
                        resolve(&k.root, "root"),
                        resolve(&k.mid, "mid"),
                        resolve(&k.end, "end"),
                    ) else {
                        continue;
                    };
                    if skeleton.bone(mid).parent != Some(root) {
                        issues.push(Issue::ChainNotContiguous {
                            kind: "ik chain",
                            name: k.name.clone(),
                            parent: k.root.clone(),
                            child: k.mid.clone(),
                        });
                    }
                    if skeleton.bone(end).parent != Some(mid) {
                        issues.push(Issue::ChainNotContiguous {
                            kind: "ik chain",
                            name: k.name.clone(),
                            parent: k.mid.clone(),
                            child: k.end.clone(),
                        });
                    }
                    IkKind::TwoBone {
                        root,
                        mid,
                        end,
                        bend_positive: k.bend_positive.unwrap_or(true),
                    }
                }
                other => {
                    issues.push(Issue::BadIkKind {
                        name: k.name.clone(),
                        kind: other.to_string(),
                    });
                    continue;
                }
            };
            iks.push(IkDef { name: k.name.clone().into_boxed_str(), kind, target });
        }

        // --- chains ---------------------------------------------------------
        let mut chains: Vec<ChainDef> = Vec::with_capacity(doc.chains.len());
        for c in &doc.chains {
            let mut bones = Vec::with_capacity(c.bones.len());
            let mut ok = true;
            for name in &c.bones {
                match bone_of(name) {
                    Some(b) => bones.push(b),
                    None => {
                        issues.push(Issue::UnknownRef {
                            kind: "bone",
                            name: name.clone(),
                            referenced_by: format!("chain {:?}", c.name),
                        });
                        ok = false;
                    }
                }
            }
            if !ok {
                continue;
            }
            if bones.len() < 2 {
                issues.push(Issue::ChainTooShort {
                    kind: "chain",
                    name: c.name.clone(),
                    need: 2,
                    have: bones.len(),
                });
                continue;
            }
            for w in 1..bones.len() {
                if skeleton.bone(bones[w]).parent != Some(bones[w - 1]) {
                    issues.push(Issue::ChainNotContiguous {
                        kind: "chain",
                        name: c.name.clone(),
                        parent: c.bones[w - 1].clone(),
                        child: c.bones[w].clone(),
                    });
                }
            }
            chains.push(ChainDef {
                name: c.name.clone().into_boxed_str(),
                bones,
                params: ChainParams {
                    stiffness: num(c.stiffness, 150.0).max(0.0),
                    damping: num(c.damping, 16.0).max(0.0),
                    mass: num(c.mass, 1.0).max(1e-3),
                    gravity: num(c.gravity, 60.0),
                    drag: num(c.drag, 1.6).max(0.0),
                    stiff_length: clamp(num(c.stiff_length, 1.0), 0.0, 1.0),
                },
            });
        }

        // --- physics and motion ---------------------------------------------
        let d = PhysicsParams::default();
        let physics = PhysicsParams {
            gravity: num(doc.physics.gravity, d.gravity),
            drag: num(doc.physics.drag, d.drag).max(0.0),
            restitution: clamp(num(doc.physics.restitution, d.restitution), 0.0, 1.0),
            friction: clamp(num(doc.physics.friction, d.friction), 0.0, 1.0),
            max_speed: num(doc.physics.max_speed, d.max_speed).max(1.0),
            rest_speed: num(doc.physics.rest_speed, d.rest_speed).max(0.0),
            hard_landing_speed: num(doc.physics.hard_landing_speed, d.hard_landing_speed),
            recovery_time: num(doc.physics.recovery_time_ms, d.recovery_time * 1000.0) / 1000.0,
            grab_transfer: clamp(num(doc.physics.grab_transfer, d.grab_transfer), 0.0, 1.0),
        };

        let md = MotionConfig::default();
        let mut resolve_motion_bone = |name: &str, field: &str| -> Option<usize> {
            if name.is_empty() {
                return None;
            }
            match bone_of(name) {
                Some(b) => Some(b),
                None => {
                    issues.push(Issue::UnknownRef {
                        kind: "bone",
                        name: name.to_string(),
                        referenced_by: format!("[motion].{field}"),
                    });
                    None
                }
            }
        };
        let motion = MotionConfig {
            squash: SquashParams {
                gain: num(doc.motion.squash_gain, md.squash.gain).max(0.0),
                max: clamp(num(doc.motion.squash_max, md.squash.max), 0.0, 0.9),
                deadzone: num(doc.motion.squash_deadzone, md.squash.deadzone).max(0.0),
            },
            squash_bone: resolve_motion_bone(&doc.motion.squash_bone, "squash_bone"),
            lean_bone: resolve_motion_bone(&doc.motion.lean_bone, "lean_bone"),
            lean: SpringParams::with_ratio(
                num(doc.motion.lean_stiffness, 240.0).max(1.0),
                1.0,
                clamp(num(doc.motion.lean_damping_ratio, 0.55), 0.05, 4.0),
            ),
            lean_gain: clamp(num(doc.motion.lean_gain, md.lean_gain), 0.0, 1.0),
            light_bone: resolve_motion_bone(&doc.motion.light_bone, "light_bone"),
            light_gain: num(doc.motion.light_gain, md.light_gain).max(0.0),
            light_range: num(doc.motion.light_range, md.light_range).max(0.0),
        };

        if !issues.is_empty() {
            return Err(SkinError::Invalid(Issues(issues)));
        }

        Ok(Skin {
            doc,
            meta,
            canvas,
            skeleton,
            gradients,
            shapes,
            layers,
            clips,
            expressions,
            iks,
            chains,
            physics,
            motion,
            clip_index,
            expr_index,
            layer_index,
        })
    }
}

#[inline]
fn num(v: Option<Num>, default: f32) -> f32 {
    v.map(|n| n.0).unwrap_or(default)
}

fn resolve_color(
    text: &str,
    named: &HashMap<&str, Rgba>,
) -> Result<Rgba, crate::paint::ColorError> {
    if let Some(c) = named.get(text) {
        return Ok(*c);
    }
    Rgba::parse_hex(text)
}

#[allow(clippy::too_many_arguments)]
fn compile_paint(
    shape: &str,
    slot: &'static str,
    color: &str,
    gradient: &str,
    alpha: Option<Num>,
    colors: &HashMap<&str, Rgba>,
    gradient_index: &HashMap<&str, usize>,
    issues: &mut Vec<Issue>,
) -> Option<PaintRef> {
    let a = clamp(num(alpha, 1.0), 0.0, 1.0);
    match (color.is_empty(), gradient.is_empty()) {
        (false, false) => {
            issues.push(Issue::PaintAmbiguous { shape: shape.to_string(), slot });
            None
        }
        (true, true) => {
            issues.push(Issue::PaintEmpty { shape: shape.to_string(), slot });
            None
        }
        (false, true) => match resolve_color(color, colors) {
            Ok(c) => Some(PaintRef::Solid(c.scale_alpha(a))),
            Err(source) => {
                issues.push(Issue::BadColor {
                    at: format!("shape {shape:?}'s {slot}"),
                    source,
                });
                None
            }
        },
        (true, false) => match gradient_index.get(gradient) {
            Some(&index) => Some(PaintRef::Gradient { index, alpha: a }),
            None => {
                issues.push(Issue::UnknownRef {
                    kind: "gradient",
                    name: gradient.to_string(),
                    referenced_by: format!("shape {shape:?}'s {slot}"),
                });
                None
            }
        },
    }
}

fn parse_ease(name: &str, at: &str, issues: &mut Vec<Issue>) -> Ease {
    if let Some(e) = Ease::from_name(name) {
        return e;
    }
    if let Some(args) = name
        .strip_prefix("bezier(")
        .and_then(|r| r.strip_suffix(')'))
    {
        let parts: Vec<f32> = args
            .split(',')
            .filter_map(|p| p.trim().parse::<f32>().ok())
            .collect();
        if parts.len() == 4
            && parts.iter().all(|v| v.is_finite())
            && (0.0..=1.0).contains(&parts[0])
            && (0.0..=1.0).contains(&parts[2])
        {
            return Ease::Bezier([parts[0], parts[1], parts[2], parts[3]]);
        }
        issues.push(Issue::BadBezier { at: at.to_string(), name: name.to_string() });
        return Ease::Soft;
    }
    issues.push(Issue::UnknownEase { at: at.to_string(), name: name.to_string() });
    Ease::Soft
}

fn parse_extend(s: &str) -> Extend {
    match s {
        "repeat" => Extend::Repeat,
        "reflect" => Extend::Reflect,
        _ => Extend::Pad,
    }
}

fn parse_cap(s: &str) -> Cap {
    match s {
        "round" => Cap::Round,
        "square" => Cap::Square,
        _ => Cap::Butt,
    }
}

fn parse_join(s: &str) -> Join {
    match s {
        "round" => Join::Round,
        "bevel" => Join::Bevel,
        _ => Join::Miter,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest document that compiles. Tests append to it.
    const MINIMAL: &str = r##"
format = "nx-wisp-skin"
version = 1
[meta]
name = "T"
[canvas]
size = [100.0, 100.0]
anchor = [50.0, 50.0]
[[bone]]
name = "root"
[[bone]]
name = "body"
parent = "root"
pos = [0.0, -20.0]
length = 20.0
"##;

    fn with(extra: &str) -> Result<Skin, SkinError> {
        Skin::parse(&format!("{MINIMAL}{extra}"))
    }

    fn issues(r: Result<Skin, SkinError>) -> Issues {
        match r {
            Ok(_) => panic!("expected the skin to be rejected"),
            Err(SkinError::Invalid(i)) => i,
            Err(e) => panic!("expected validation issues, got {e}"),
        }
    }

    fn has<F: Fn(&Issue) -> bool>(i: &Issues, f: F) -> bool {
        i.iter().any(f)
    }

    // -- header ------------------------------------------------------------

    #[test]
    fn the_minimal_document_compiles() {
        let s = Skin::parse(MINIMAL).unwrap();
        assert_eq!(&*s.meta.name, "T");
        assert_eq!(s.skeleton.len(), 2);
        assert_eq!(s.canvas.anchor, Vec2::new(50.0, 50.0));
    }

    #[test]
    fn a_file_that_is_not_a_skin_is_rejected_before_anything_else() {
        let src = r##"
format = "some-other-thing"
version = 1
[meta]
name = "T"
[canvas]
"##;
        let i = issues(Skin::parse(src));
        assert!(has(&i, |x| matches!(x, Issue::NotASkin { .. })));
        // The bone list is empty too, but that is not reported: a wrong magic
        // means nothing below can be trusted.
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn an_unknown_format_version_is_rejected_with_advice() {
        let src = MINIMAL.replace("version = 1", "version = 99");
        let i = issues(Skin::parse(&src));
        assert!(has(&i, |x| matches!(x, Issue::UnknownVersion { found: 99, .. })));
        assert!(i.to_string().contains("update NX Wisp"));
    }

    #[test]
    fn malformed_toml_is_a_parse_error_not_a_validation_error() {
        assert!(matches!(Skin::parse("format = ["), Err(SkinError::Toml(_))));
    }

    // -- skeleton ----------------------------------------------------------

    #[test]
    fn a_bone_cycle_is_rejected() {
        let src = r##"
format = "nx-wisp-skin"
version = 1
[meta]
name = "T"
[canvas]
[[bone]]
name = "a"
parent = "b"
[[bone]]
name = "b"
parent = "a"
"##;
        let i = issues(Skin::parse(src));
        assert!(has(&i, |x| matches!(x, Issue::BoneCycle { .. })), "{i}");
        assert!(i.to_string().contains("->"));
    }

    #[test]
    fn a_dangling_parent_is_reported_as_a_missing_bone_not_a_cycle() {
        let src = MINIMAL.replace(r##"parent = "root""##, r##"parent = "ghost""##);
        let i = issues(Skin::parse(&src));
        assert!(has(&i, |x| matches!(x, Issue::UnknownRef { kind: "bone", .. })), "{i}");
    }

    #[test]
    fn a_skin_with_no_bones_is_rejected() {
        let src = r##"
format = "nx-wisp-skin"
version = 1
[meta]
name = "T"
[canvas]
"##;
        assert!(has(&issues(Skin::parse(src)), |x| matches!(x, Issue::NoBones)));
    }

    #[test]
    fn bone_rotations_are_authored_in_degrees() {
        let src = format!("{MINIMAL}\n[[bone]]\nname = \"turned\"\nparent = \"root\"\nrot = 90.0\n");
        let s = Skin::parse(&src).unwrap();
        let b = s.skeleton.bone(s.bone_index("turned").unwrap());
        assert!((b.rest.rot - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }

    // -- canvas and sizes ---------------------------------------------------

    #[test]
    fn a_zero_sized_canvas_is_rejected() {
        let src = MINIMAL.replace("size = [100.0, 100.0]", "size = [0.0, 100.0]");
        assert!(has(&issues(Skin::parse(&src)), |x| matches!(x, Issue::BadCanvas { .. })));
    }

    #[test]
    fn an_inverted_size_range_is_rejected() {
        let src = MINIMAL.replace(
            r##"name = "T""##,
            "name = \"T\"\nmin_size_px = 300.0\nmax_size_px = 100.0\ndefault_size_px = 128.0",
        );
        assert!(has(&issues(Skin::parse(&src)), |x| matches!(x, Issue::BadSizeRange { .. })));
    }

    #[test]
    fn scale_maps_the_canvas_onto_the_requested_pixel_size() {
        let s = Skin::parse(MINIMAL).unwrap();
        assert!((s.scale_for(200.0) - 2.0).abs() < 1e-5);
        assert!((s.scale_for(50.0) - 0.5).abs() < 1e-5);
    }

    // -- gradients and colours ----------------------------------------------

    const GRAD: &str = r##"
[[color]]
name = "brand"
value = "#7700ff"

[[gradient]]
name = "g"
kind = "linear"
start = [0.0, 0.0]
end = [10.0, 10.0]
stop_at = [0.0, 1.0]
stop_color = ["brand", "#00e5ff"]
"##;

    #[test]
    fn a_gradient_stop_may_name_a_palette_colour_or_give_hex() {
        let s = with(GRAD).unwrap();
        assert_eq!(s.gradients.len(), 1);
        assert_eq!(s.gradients[0].stops[0].color, crate::paint::nx::VIOLET);
        assert_eq!(s.gradients[0].stops[1].color, crate::paint::nx::CYAN);
    }

    #[test]
    fn a_gradient_with_mismatched_stop_arrays_is_rejected() {
        let src = GRAD.replace(r##"stop_at = [0.0, 1.0]"##, "stop_at = [0.0, 0.5, 1.0]");
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::GradientArityMismatch { .. })
        ));
    }

    #[test]
    fn a_gradient_with_backwards_stops_is_rejected() {
        let src = GRAD.replace("stop_at = [0.0, 1.0]", "stop_at = [1.0, 0.0]");
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::GradientBadStop { .. })));
    }

    #[test]
    fn a_gradient_missing_its_geometry_is_rejected() {
        let src = r##"
[[gradient]]
name = "g"
kind = "radial"
stop_at = [0.0]
stop_color = ["#ffffff"]
"##;
        assert!(has(
            &issues(with(src)),
            |x| matches!(x, Issue::GradientMissingGeometry { kind: "radial", .. })
        ));
    }

    #[test]
    fn an_unknown_gradient_kind_is_rejected() {
        let src = GRAD.replace(r##"kind = "linear""##, r##"kind = "conic""##);
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::BadGradientKind { .. })));
    }

    #[test]
    fn a_bad_colour_is_reported_with_where_it_came_from() {
        let src = GRAD.replace(r##""brand", "#00e5ff""##, r##""brand", "notacolour""##);
        let i = issues(with(&src));
        assert!(has(&i, |x| matches!(x, Issue::BadColor { .. })));
        assert!(i.to_string().contains("stop 1"), "{i}");
    }

    #[test]
    fn a_gradient_may_follow_a_bone() {
        let src = format!("{GRAD}follow_bone = \"body\"\n");
        let s = with(&src).unwrap();
        assert_eq!(s.gradients[0].follow_bone, s.bone_index("body"));
    }

    #[test]
    fn a_gradient_following_a_missing_bone_is_rejected() {
        let src = format!("{GRAD}follow_bone = \"ghost\"\n");
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::UnknownRef { kind: "bone", .. })
        ));
    }

    // -- shapes -------------------------------------------------------------

    const SHAPE: &str = r##"
[[shape]]
name = "s"
z = 5
path = "M 0 0 L 10 0 L 10 10 Z"
bind = "body"
fill = { color = "#7700ff" }
"##;

    #[test]
    fn a_shape_compiles_to_a_path_and_a_rigid_binding() {
        let s = with(SHAPE).unwrap();
        assert_eq!(s.shapes.len(), 1);
        assert_eq!(s.shapes[0].path.point_count(), 3);
        let body = s.bone_index("body").unwrap() as u32;
        for i in 0..3 {
            assert_eq!(s.shapes[0].binding.influences_of(i)[0].bone, body);
        }
    }

    #[test]
    fn shapes_come_out_sorted_by_z() {
        let src = format!(
            "{SHAPE}\n[[shape]]\nname = \"under\"\nz = -1\npath = \"M 0 0 L 1 1\"\nfill = {{ color = \"#fff\" }}\n"
        );
        let s = with(&src).unwrap();
        assert_eq!(&*s.shapes[0].name, "under");
        assert_eq!(&*s.shapes[1].name, "s");
    }

    #[test]
    fn an_unusable_path_is_rejected_with_the_shape_name() {
        let src = SHAPE.replace(r##"path = "M 0 0 L 10 0 L 10 10 Z""##, r##"path = "A 1 2 3""##);
        let i = issues(with(&src));
        assert!(has(&i, |x| matches!(x, Issue::BadPath { .. })));
        assert!(i.to_string().contains(r##""s""##), "{i}");
    }

    #[test]
    fn a_paint_with_both_a_colour_and_a_gradient_is_rejected() {
        let src = format!(
            "{GRAD}{}",
            SHAPE.replace(
                r##"fill = { color = "#7700ff" }"##,
                r##"fill = { color = "#7700ff", gradient = "g" }"##
            )
        );
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::PaintAmbiguous { .. })));
    }

    #[test]
    fn a_paint_with_neither_is_rejected() {
        let src = SHAPE.replace(r##"fill = { color = "#7700ff" }"##, "fill = { alpha = 0.5 }");
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::PaintEmpty { .. })));
    }

    #[test]
    fn a_shape_naming_a_missing_gradient_is_rejected() {
        let src = SHAPE.replace(
            r##"fill = { color = "#7700ff" }"##,
            r##"fill = { gradient = "nope" }"##,
        );
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::UnknownRef { kind: "gradient", .. })
        ));
    }

    #[test]
    fn fill_alpha_multiplies_the_authored_colour() {
        let src = SHAPE.replace(
            r##"fill = { color = "#7700ff" }"##,
            r##"fill = { color = "#7700ff", alpha = 0.5 }"##,
        );
        let s = with(&src).unwrap();
        match s.shapes[0].fill.unwrap() {
            PaintRef::Solid(c) => assert!((c.a - 0.5).abs() < 1e-5),
            other => panic!("{other:?}"),
        }
    }

    // -- weights ------------------------------------------------------------

    #[test]
    fn an_out_of_range_weight_is_rejected() {
        let src = format!(
            "{SHAPE}\n[[shape.weight]]\npoint = 0\nbones = [\"body\"]\nweights = [1.5]\n"
        );
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::WeightOutOfRange { weight, .. } if *weight == 1.5)
        ));
    }

    #[test]
    fn a_negative_weight_is_rejected() {
        let src = format!(
            "{SHAPE}\n[[shape.weight]]\npoint = 0\nbones = [\"body\"]\nweights = [-0.2]\n"
        );
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::WeightOutOfRange { .. })));
    }

    #[test]
    fn a_weight_on_a_point_that_does_not_exist_is_rejected() {
        let src = format!(
            "{SHAPE}\n[[shape.weight]]\npoint = 99\nbones = [\"body\"]\nweights = [1.0]\n"
        );
        let i = issues(with(&src));
        assert!(has(&i, |x| matches!(x, Issue::PointOutOfRange { count: 3, .. })), "{i}");
        assert!(i.to_string().contains("control points count"), "{i}");
    }

    #[test]
    fn mismatched_bone_and_weight_arrays_are_rejected() {
        let src = format!(
            "{SHAPE}\n[[shape.weight]]\npoint = 0\nbones = [\"body\", \"root\"]\nweights = [1.0]\n"
        );
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::WeightArityMismatch { .. })));
    }

    #[test]
    fn a_weight_naming_a_missing_bone_is_rejected() {
        let src = format!(
            "{SHAPE}\n[[shape.weight]]\npoint = 0\nbones = [\"ghost\"]\nweights = [1.0]\n"
        );
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::UnknownRef { kind: "bone", .. })
        ));
    }

    #[test]
    fn weights_that_are_all_zero_are_rejected() {
        let src = format!(
            "{SHAPE}\n[[shape.weight]]\npoint = 0\nbones = [\"body\", \"root\"]\nweights = [0.0, 0.0]\n"
        );
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::WeightsAllZero { .. })));
    }

    #[test]
    fn explicit_weights_are_normalised_and_override_the_shape_binding() {
        let src = format!(
            "{SHAPE}\n[[shape.weight]]\npoint = 1\nbones = [\"body\", \"root\"]\nweights = [0.6, 0.2]\n"
        );
        let s = with(&src).unwrap();
        let infs = s.shapes[0].binding.influences_of(1);
        assert_eq!(infs.len(), 2);
        let sum: f32 = infs.iter().map(|i| i.weight).sum();
        assert!((sum - 1.0).abs() < 1e-5);
        assert!((infs[0].weight - 0.75).abs() < 1e-4, "{infs:?}");
        // The unweighted points still use the shape-level bind.
        assert_eq!(s.shapes[0].binding.influences_of(0).len(), 1);
    }

    #[test]
    fn auto_binding_produces_a_valid_normalised_binding() {
        let src = SHAPE.replace(
            r##"bind = "body""##,
            r##"bind_auto = { bones = ["root", "body"], falloff = 40.0, power = 2.0 }"##,
        );
        let s = with(&src).unwrap();
        assert!(s.shapes[0].binding.is_valid(s.skeleton.len()));
    }

    #[test]
    fn auto_binding_to_a_missing_bone_is_rejected() {
        let src = SHAPE.replace(
            r##"bind = "body""##,
            r##"bind_auto = { bones = ["ghost"] }"##,
        );
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::UnknownRef { kind: "bone", .. })
        ));
    }

    // -- clips --------------------------------------------------------------

    const CLIP: &str = r##"
[[clip]]
name = "idle"
duration_ms = 2000.0

[[clip.track]]
bone = "body"
channel = "ty"
t = [0.0, 1000.0, 2000.0]
v = [0.0, -5.0, 0.0]
ease = "soft"
"##;

    #[test]
    fn clip_times_are_authored_in_milliseconds_and_stored_in_seconds() {
        let s = with(CLIP).unwrap();
        assert!((s.clips[0].duration - 2.0).abs() < 1e-5);
        assert_eq!(s.clips[0].tracks[0].times, vec![0.0, 1.0, 2.0]);
    }

    #[test]
    fn rotation_tracks_are_authored_in_degrees() {
        let src = CLIP.replace(r##"channel = "ty""##, r##"channel = "rot""##);
        let s = with(&src).unwrap();
        assert!((s.clips[0].tracks[0].values[1] + std::f32::consts::PI / 36.0).abs() < 1e-4);
    }

    #[test]
    fn translation_tracks_are_left_in_canvas_units() {
        let s = with(CLIP).unwrap();
        assert_eq!(s.clips[0].tracks[0].values, vec![0.0, -5.0, 0.0]);
    }

    #[test]
    fn a_zero_length_clip_is_rejected() {
        let src = CLIP.replace("duration_ms = 2000.0", "duration_ms = 0.0");
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::BadDuration { .. })));
    }

    #[test]
    fn mismatched_time_and_value_arrays_are_rejected() {
        let src = CLIP.replace("v = [0.0, -5.0, 0.0]", "v = [0.0, -5.0]");
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::TrackArityMismatch { .. })));
    }

    #[test]
    fn a_per_key_easing_list_of_the_wrong_length_is_rejected() {
        let src = CLIP.replace(r##"ease = "soft""##, r##"ease = ["soft", "out"]"##);
        let i = issues(with(&src));
        assert!(has(&i, |x| matches!(x, Issue::EaseArityMismatch { eases: 2, keys: 3, .. })), "{i}");
    }

    #[test]
    fn a_per_key_easing_list_of_the_right_length_is_accepted() {
        let src = CLIP.replace(r##"ease = "soft""##, r##"ease = ["out", "spring", "soft"]"##);
        let s = with(&src).unwrap();
        assert_eq!(s.clips[0].tracks[0].eases, vec![Ease::Out, Ease::Spring, Ease::Soft]);
    }

    #[test]
    fn keyframes_that_go_backwards_in_time_are_rejected() {
        let src = CLIP.replace("t = [0.0, 1000.0, 2000.0]", "t = [0.0, 2000.0, 1000.0]");
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::KeysOutOfOrder { .. })));
    }

    #[test]
    fn an_unknown_channel_is_rejected_with_the_list_of_real_ones() {
        let src = CLIP.replace(r##"channel = "ty""##, r##"channel = "wobble""##);
        let i = issues(with(&src));
        assert!(has(&i, |x| matches!(x, Issue::UnknownChannel { .. })));
        assert!(i.to_string().contains("tx, ty, rot, sx, sy, alpha"), "{i}");
    }

    #[test]
    fn a_track_on_a_missing_bone_is_rejected() {
        let src = CLIP.replace(r##"bone = "body""##, r##"bone = "ghost""##);
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::UnknownRef { kind: "bone", .. })
        ));
    }

    #[test]
    fn an_empty_track_is_rejected() {
        let src = CLIP
            .replace("t = [0.0, 1000.0, 2000.0]", "t = []")
            .replace("v = [0.0, -5.0, 0.0]", "v = []");
        assert!(has(&issues(with(&src)), |x| matches!(x, Issue::EmptyTrack { .. })));
    }

    #[test]
    fn an_unknown_easing_is_rejected() {
        let src = CLIP.replace(r##"ease = "soft""##, r##"ease = "bouncy""##);
        let i = issues(with(&src));
        assert!(has(&i, |x| matches!(x, Issue::UnknownEase { .. })));
        assert!(i.to_string().contains("bezier(x1,y1,x2,y2)"), "{i}");
    }

    #[test]
    fn an_inline_bezier_easing_is_accepted() {
        let src = CLIP.replace(r##"ease = "soft""##, r##"ease = "bezier(0.3, 0.1, 0.2, 1)""##);
        let s = with(&src).unwrap();
        assert_eq!(s.clips[0].tracks[0].eases[0], Ease::Bezier([0.3, 0.1, 0.2, 1.0]));
    }

    #[test]
    fn a_malformed_bezier_easing_is_rejected() {
        for bad in ["bezier(1,2)", "bezier(2.0,0,0.5,1)", "bezier(a,b,c,d)"] {
            let src = CLIP.replace(r##"ease = "soft""##, &format!(r##"ease = "{bad}""##));
            let i = issues(with(&src));
            assert!(
                has(&i, |x| matches!(x, Issue::BadBezier { .. } | Issue::UnknownEase { .. })),
                "{bad} was accepted: {i}"
            );
        }
    }

    #[test]
    fn duplicate_clip_names_are_rejected() {
        let src = format!("{CLIP}{CLIP}");
        assert!(has(
            &issues(with(&src)),
            |x| matches!(x, Issue::DuplicateName { kind: "clip", .. })
        ));
    }

    // -- layers and expressions ---------------------------------------------

    #[test]
    fn a_skin_with_no_layers_gets_a_base_and_a_face_layer() {
        let s = Skin::parse(MINIMAL).unwrap();
        assert_eq!(s.layers.len(), 2);
        assert_eq!(s.layer_index("base"), Some(0));
        assert!(s.layers[1].additive);
    }

    #[test]
    fn a_layer_may_name_a_default_clip() {
        let src = format!("{CLIP}\n[[layer]]\nname = \"base\"\ndefault_clip = \"idle\"\n");
        let s = with(&src).unwrap();
        assert_eq!(s.layers[0].default_clip, Some(0));
    }

    #[test]
    fn a_layer_naming_a_missing_default_clip_is_rejected() {
        let src = "\n[[layer]]\nname = \"base\"\ndefault_clip = \"nope\"\n";
        assert!(has(
            &issues(with(src)),
            |x| matches!(x, Issue::UnknownRef { kind: "clip", .. })
        ));
    }

    #[test]
    fn an_expression_defaults_to_the_last_additive_layer() {
        let src = format!(
            "{CLIP}\n[[layer]]\nname = \"base\"\n[[layer]]\nname = \"face\"\nadditive = true\n\
             [[expression]]\nname = \"neutral\"\nclip = \"idle\"\n"
        );
        let s = with(&src).unwrap();
        assert_eq!(s.expressions[0].layer, 1);
    }

    #[test]
    fn an_expression_naming_a_missing_clip_is_rejected() {
        let src = "\n[[expression]]\nname = \"neutral\"\nclip = \"nope\"\n";
        assert!(has(
            &issues(with(src)),
            |x| matches!(x, Issue::UnknownRef { kind: "clip", .. })
        ));
    }

    #[test]
    fn expression_fades_are_authored_in_milliseconds() {
        let src = format!(
            "{CLIP}\n[[expression]]\nname = \"neutral\"\nclip = \"idle\"\nfade_ms = 250.0\n"
        );
        let s = with(&src).unwrap();
        assert!((s.expressions[0].fade - 0.25).abs() < 1e-5);
    }

    // -- ik and chains ------------------------------------------------------

    #[test]
    fn a_look_at_constraint_compiles_with_its_cone_in_radians() {
        let src = "\n[[ik]]\nname = \"gaze\"\nkind = \"look_at\"\nbone = \"body\"\ntarget = \"cursor\"\nmax_deg = 90.0\n";
        let s = with(src).unwrap();
        assert_eq!(s.iks[0].target, IkTarget::Cursor);
        match s.iks[0].kind {
            IkKind::LookAt { cfg, .. } => {
                assert!((cfg.max_angle - std::f32::consts::FRAC_PI_2).abs() < 1e-5)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unknown_ik_kind_or_target_is_rejected() {
        let bad_kind = "\n[[ik]]\nname = \"k\"\nkind = \"spline\"\nbone = \"body\"\n";
        assert!(has(&issues(with(bad_kind)), |x| matches!(x, Issue::BadIkKind { .. })));
        let bad_target =
            "\n[[ik]]\nname = \"k\"\nkind = \"look_at\"\nbone = \"body\"\ntarget = \"the moon\"\n";
        assert!(has(&issues(with(bad_target)), |x| matches!(x, Issue::BadIkTarget { .. })));
    }

    #[test]
    fn a_two_bone_ik_over_bones_that_are_not_a_chain_is_rejected() {
        let src = r##"
[[bone]]
name = "loose"
parent = "root"

[[ik]]
name = "k"
kind = "two_bone"
root = "root"
mid = "body"
end = "loose"
"##;
        let i = issues(with(src));
        assert!(has(&i, |x| matches!(x, Issue::ChainNotContiguous { kind: "ik chain", .. })), "{i}");
    }

    #[test]
    fn a_secondary_motion_chain_must_follow_the_bone_tree() {
        let src = r##"
[[bone]]
name = "loose"
parent = "root"

[[chain]]
name = "tail"
bones = ["body", "loose"]
"##;
        assert!(has(
            &issues(with(src)),
            |x| matches!(x, Issue::ChainNotContiguous { kind: "chain", .. })
        ));
    }

    #[test]
    fn a_one_bone_chain_is_rejected() {
        let src = "\n[[chain]]\nname = \"tail\"\nbones = [\"body\"]\n";
        assert!(has(&issues(with(src)), |x| matches!(x, Issue::ChainTooShort { .. })));
    }

    // -- reporting ----------------------------------------------------------

    #[test]
    fn every_problem_is_reported_in_one_pass() {
        let src = format!(
            "{}{}{}",
            SHAPE.replace(r##"bind = "body""##, r##"bind = "ghost""##),
            CLIP.replace(r##"channel = "ty""##, r##"channel = "wobble""##),
            "\n[[chain]]\nname = \"tail\"\nbones = [\"body\"]\n"
        );
        let i = issues(with(&src));
        assert!(i.len() >= 3, "expected several issues, got: {i}");
        assert!(has(&i, |x| matches!(x, Issue::UnknownRef { .. })));
        assert!(has(&i, |x| matches!(x, Issue::UnknownChannel { .. })));
        assert!(has(&i, |x| matches!(x, Issue::ChainTooShort { .. })));
    }

    // -- serialisation ------------------------------------------------------

    #[test]
    fn a_skin_round_trips_through_toml() {
        let src = format!("{GRAD}{SHAPE}{CLIP}");
        let a = with(&src).unwrap();
        let text = a.to_toml().unwrap();
        let b = Skin::parse(&text).unwrap();
        assert_eq!(a.doc(), b.doc(), "document changed across a round trip");
        assert_eq!(a.clips, b.clips);
        assert_eq!(a.shapes, b.shapes);
        assert_eq!(a.gradients, b.gradients);
    }

    #[test]
    fn a_blank_document_compiles_and_round_trips() {
        let doc = SkinDoc::blank("Empty");
        let s = Skin::compile(doc).unwrap();
        let back = Skin::parse(&s.to_toml().unwrap()).unwrap();
        assert_eq!(&*back.meta.name, "Empty");
    }

    // -- required sets ------------------------------------------------------

    #[test]
    fn a_skin_missing_the_required_clips_still_loads_but_reports_them() {
        let s = Skin::parse(MINIMAL).unwrap();
        assert_eq!(s.missing_required_clips().len(), 6);
        assert_eq!(s.missing_required_expressions().len(), 8);
    }

    // -- data only ----------------------------------------------------------

    #[test]
    fn there_is_nowhere_in_the_format_for_code() {
        // SPEC §3.6. Every value a skin can set is a number, a bool, a colour,
        // a name, or SVG path geometry — so an unknown key is simply refused
        // rather than becoming an extension point.
        let src = format!("{SHAPE}\non_click = \"rm -rf /\"\n");
        let e = Skin::parse(&format!("{MINIMAL}{src}")).unwrap_err();
        assert!(matches!(e, SkinError::Toml(_)), "unknown keys must be refused: {e}");
    }
}
