//! The on-disk skin document (F49, SPEC §3.6).
//!
//! # Why TOML
//!
//! A skin is a *pack a person writes and a person reviews*. TOML wins on every
//! axis that matters for that:
//!
//! * **Comments.** A skin needs a licence header, an attribution, and lines
//!   like `# this bone drives the tail's second joint`. JSON has nowhere to put
//!   them, and a community format without comments is a format people fork
//!   instead of reading.
//! * **Diffable stanzas.** Array-of-tables gives one `[[bone]]` block per bone,
//!   so a pull request against a skin shows *which bone changed*, not a
//!   reflowed brace soup.
//! * **The plan already named it.** F49 specifies `wisp.toml`; making the rig
//!   payload a different format would split one artefact across two syntaxes.
//! * **Spanned errors.** `toml` reports line and column, which is what turns
//!   "invalid skin" into "line 84: bone 'tail2' names a parent that does not
//!   exist".
//!
//! The one place TOML is weak — long arrays of numbers — is handled by giving
//! keyframe tracks and gradient stops **parallel arrays** rather than arrays of
//! tables:
//!
//! ```toml
//! [[clip.track]]
//! bone = "body"
//! channel = "ty"
//! t = [0.0, 1600.0, 3200.0]
//! v = [0.0, -4.0, 0.0]
//! ease = "soft"
//! ```
//!
//! That is both the most compact and the most readable form: one track is one
//! stanza you can take in at a glance.
//!
//! # What a skin may contain
//!
//! Data. Only data. There is no expression language, no reference to a file
//! outside the document, no path with a `..` in it, and no field whose value is
//! interpreted as anything but a number, a colour, a name, or SVG path
//! geometry. That is SPEC §3.6's "a skin can never contain executable code",
//! enforced by the shape of the format rather than by a sandbox.
//!
//! # Units
//!
//! Coordinates are **canvas units** (see [`CanvasDoc`]), angles are **degrees**
//! and durations are **milliseconds** — all three are what a human authors.
//! Compilation converts to radians and seconds once.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Identifies the file as a skin. Anything else is rejected before parsing
/// goes any further, so a stray TOML file in the watched skins directory
/// produces a clear error rather than a hundred missing-field ones.
pub const FORMAT_MAGIC: &str = "nx-wisp-skin";

/// The version this build writes.
pub const FORMAT_VERSION: u32 = 1;

/// Versions this build can read.
pub const SUPPORTED_VERSIONS: &[u32] = &[1];

/// A number that accepts TOML integers as well as floats.
///
/// Authors write `0` and `0.0` interchangeably and should never see a type
/// error for it; TOML arrays may legally mix the two.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Num(pub f32);

impl From<Num> for f32 {
    fn from(n: Num) -> f32 {
        n.0
    }
}
impl From<f32> for Num {
    fn from(v: f32) -> Num {
        Num(v)
    }
}

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // Round-trip through f64 at six decimals so a re-serialised skin does
        // not gain `0.30000001192092896`-style noise.
        let v = (self.0 as f64 * 1e6).round() / 1e6;
        s.serialize_f64(v)
    }
}

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Num, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Num;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a number")
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Num, E> {
                Ok(Num(v as f32))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Num, E> {
                Ok(Num(v as f32))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Num, E> {
                Ok(Num(v as f32))
            }
        }
        d.deserialize_any(V)
    }
}

/// A point, as `[x, y]`.
pub type Pt = [Num; 2];

pub fn pt(p: Pt) -> crate::math::Vec2 {
    crate::math::Vec2::new(p[0].0, p[1].0)
}

pub fn to_pt(v: crate::math::Vec2) -> Pt {
    [Num(v.x), Num(v.y)]
}

/// The whole document.
///
/// Field order is also *serialisation* order, and TOML requires plain values
/// before tables: keep scalars first, then tables, then arrays of tables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkinDoc {
    /// Must equal [`FORMAT_MAGIC`].
    pub format: String,
    pub version: u32,

    pub meta: MetaDoc,
    pub canvas: CanvasDoc,
    #[serde(default)]
    pub physics: PhysicsDoc,
    #[serde(default)]
    pub motion: MotionDoc,

    /// Named colours, so a palette is written once.
    #[serde(default, rename = "color")]
    pub colors: Vec<ColorDoc>,
    #[serde(default, rename = "gradient")]
    pub gradients: Vec<GradientDoc>,
    #[serde(default, rename = "bone")]
    pub bones: Vec<BoneDoc>,
    #[serde(default, rename = "shape")]
    pub shapes: Vec<ShapeDoc>,
    #[serde(default, rename = "ik")]
    pub iks: Vec<IkDoc>,
    #[serde(default, rename = "chain")]
    pub chains: Vec<ChainDoc>,
    #[serde(default, rename = "layer")]
    pub layers: Vec<LayerDoc>,
    #[serde(default, rename = "clip")]
    pub clips: Vec<ClipDoc>,
    #[serde(default, rename = "expression")]
    pub expressions: Vec<ExpressionDoc>,
}

impl SkinDoc {
    /// An empty, well-formed document — the starting point for the in-app
    /// editor (F76) and for `wispkit scaffold` (F51).
    pub fn blank(name: &str) -> SkinDoc {
        SkinDoc {
            format: FORMAT_MAGIC.to_string(),
            version: FORMAT_VERSION,
            meta: MetaDoc { name: name.to_string(), ..Default::default() },
            canvas: CanvasDoc::default(),
            physics: PhysicsDoc::default(),
            motion: MotionDoc::default(),
            colors: Vec::new(),
            gradients: Vec::new(),
            bones: vec![BoneDoc { name: "root".into(), ..Default::default() }],
            shapes: Vec::new(),
            iks: Vec::new(),
            chains: Vec::new(),
            layers: Vec::new(),
            clips: Vec::new(),
            expressions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetaDoc {
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub summary: String,
    /// Size she is authored to look best at, in surface pixels.
    #[serde(default = "default_size")]
    pub default_size_px: Num,
    /// F75's slider range.
    #[serde(default = "min_size")]
    pub min_size_px: Num,
    #[serde(default = "max_size")]
    pub max_size_px: Num,
}

fn default_size() -> Num {
    Num(128.0)
}
fn min_size() -> Num {
    Num(48.0)
}
fn max_size() -> Num {
    Num(512.0)
}

impl Default for MetaDoc {
    fn default() -> Self {
        MetaDoc {
            name: String::new(),
            author: String::new(),
            license: String::new(),
            summary: String::new(),
            default_size_px: default_size(),
            min_size_px: min_size(),
            max_size_px: max_size(),
        }
    }
}

/// The coordinate space the artwork is authored in.
///
/// Everything scales from here, which is what makes her resolution
/// independent (F75): the canvas maps onto whatever pixel size the operator
/// picked, and no artwork carries a pixel measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanvasDoc {
    /// `[width, height]` in canvas units.
    #[serde(default = "default_canvas_size")]
    pub size: Pt,
    /// The point of the canvas that gets placed at her position on screen —
    /// normally where she touches the ground.
    #[serde(default = "default_anchor")]
    pub anchor: Pt,
}

fn default_canvas_size() -> Pt {
    [Num(256.0), Num(256.0)]
}
fn default_anchor() -> Pt {
    [Num(128.0), Num(128.0)]
}

impl Default for CanvasDoc {
    fn default() -> Self {
        CanvasDoc { size: default_canvas_size(), anchor: default_anchor() }
    }
}

/// Physical feel. Every field is optional; omitting the table gives the
/// defaults in [`crate::physics::PhysicsParams`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicsDoc {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gravity: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drag: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restitution: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub friction: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_speed: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rest_speed: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hard_landing_speed: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_time_ms: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grab_transfer: Option<Num>,
}

/// The procedural motion layer (F67): squash, lean, and the moving internal
/// light. Bones named here are driven by code, not by clips.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MotionDoc {
    /// Bone that receives velocity-driven squash and stretch.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub squash_bone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub squash_gain: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub squash_max: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub squash_deadzone: Option<Num>,

    /// Bone that lags and overshoots behind her actual position.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lean_bone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lean_stiffness: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lean_damping_ratio: Option<Num>,
    /// How much of the lag becomes visible offset, `0..=1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lean_gain: Option<Num>,

    /// Bone the internal light rides on. Displaced against her motion so the
    /// highlight slides *through* her rather than flashing (DESIGN.md §1,
    /// "light rides motion").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub light_bone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_gain: Option<Num>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub light_range: Option<Num>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorDoc {
    pub name: String,
    /// `#rgb`, `#rrggbb` or `#rrggbbaa`.
    pub value: String,
}

/// A gradient. `kind` selects which of the geometry fields are read.
///
/// Stops are parallel arrays: `stop_at` positions and `stop_color` colours,
/// which keeps a four-stop gradient to two readable lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GradientDoc {
    pub name: String,
    /// `"linear"` or `"radial"`.
    pub kind: String,
    /// Linear only: where the gradient starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Pt>,
    /// Linear only: where it ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Pt>,
    /// Radial only: the centre of the circle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<Pt>,
    /// Radial only: where the light appears to come from. Defaults to
    /// `center`; offsetting it towards the upper-left is what gives glass its
    /// off-axis highlight (DESIGN.md §1, one light source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<Pt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<Num>,
    /// `"pad"`, `"repeat"` or `"reflect"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extend: String,
    /// Move the whole gradient with this bone. The mechanism behind
    /// "cyan light moving *inside* her" (F73).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub follow_bone: String,
    pub stop_at: Vec<Num>,
    pub stop_color: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoneDoc {
    pub name: String,
    /// Empty or absent means this is a root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    /// Rest position in the parent's space.
    #[serde(default = "zero_pt")]
    pub pos: Pt,
    /// Rest rotation, **degrees**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rot: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<Pt>,
    /// Length along the bone's local +x. Used by IK and auto-binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<Num>,
}

fn zero_pt() -> Pt {
    [Num(0.0), Num(0.0)]
}

impl Default for BoneDoc {
    fn default() -> Self {
        BoneDoc {
            name: String::new(),
            parent: String::new(),
            pos: zero_pt(),
            rot: None,
            scale: None,
            length: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeDoc {
    pub name: String,
    /// Paint order. Higher draws later, on top.
    #[serde(default)]
    pub z: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<Num>,
    /// Does this shape count towards the click-through outline (F2)? Auras,
    /// glows and shadows say `false`.
    #[serde(default = "yes")]
    pub silhouette: bool,
    /// `"nonzero"` (default) or `"evenodd"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fill_rule: String,
    /// SVG path subset: `M L H V C Q Z`, absolute or relative. Arcs and smooth
    /// curves are rejected — see [`crate::path::parse_path`].
    pub path: String,
    /// Bind every point rigidly to this bone. The simple case.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bind: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<PaintDoc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<StrokeDoc>,
    /// Weight every point against a set of bones by distance. Overridden
    /// point by point by `[[shape.weight]]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_auto: Option<AutoBindDoc>,
    #[serde(default, rename = "weight", skip_serializing_if = "Vec::is_empty")]
    pub weights: Vec<WeightDoc>,
}

fn yes() -> bool {
    true
}

/// A fill. Exactly one of `color` and `gradient` must be set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaintDoc {
    /// A hex literal, or the name of a `[[color]]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    /// The name of a `[[gradient]]`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gradient: String,
    /// Multiplies the alpha of whatever was selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpha: Option<Num>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrokeDoc {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gradient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpha: Option<Num>,
    /// In canvas units, so a lit edge keeps its proportion at every size.
    pub width: Num,
    /// `"butt"`, `"round"` or `"square"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cap: String,
    /// `"miter"`, `"round"` or `"bevel"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub join: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoBindDoc {
    pub bones: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub falloff: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<Num>,
}

/// An explicit weight for one path point. `bones` and `weights` are parallel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightDoc {
    /// Index into the path's point array, in the order the path lists them —
    /// control points included.
    pub point: usize,
    pub bones: Vec<String>,
    pub weights: Vec<Num>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IkDoc {
    pub name: String,
    /// `"look_at"` or `"two_bone"`.
    pub kind: String,
    /// What drives it: `"cursor"`, `"attention"` or `"none"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<Num>,

    /// `look_at`: the bone to turn.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bone: String,
    /// `look_at`: the bone's forward axis in its own space. Defaults to
    /// `[0, -1]`, "up" in a y-down canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward: Option<Pt>,
    /// `look_at`: half-angle of the cone, degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_deg: Option<Num>,

    /// `two_bone`: the chain, root to tip. Must be a contiguous parent chain.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mid: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bend_positive: Option<bool>,
}

/// A secondary-motion chain: bones that lag the pose and keep moving after she
/// stops (F67).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainDoc {
    pub name: String,
    /// Root first. Must be a contiguous parent chain.
    pub bones: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stiffness: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub damping: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mass: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gravity: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drag: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stiff_length: Option<Num>,
}

/// An animation layer. Declaration order is evaluation order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerDoc {
    pub name: String,
    /// Additive layers stack on what came before; non-additive ones blend over
    /// it. The base layer is not additive; breathing and blinking are.
    #[serde(default)]
    pub additive: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_clip: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<Num>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipDoc {
    pub name: String,
    pub duration_ms: Num,
    #[serde(default = "yes")]
    pub looping: bool,
    #[serde(default)]
    pub additive: bool,
    #[serde(default, rename = "track", skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<TrackDoc>,
}

/// One bone-channel curve. `t` (milliseconds) and `v` are parallel arrays;
/// `ease` is either one name for the whole track or one per key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackDoc {
    pub bone: String,
    /// `tx`, `ty`, `rot` (degrees), `sx`, `sy`, `alpha`.
    pub channel: String,
    pub t: Vec<Num>,
    pub v: Vec<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ease: Option<EaseSpec>,
}

/// `ease = "soft"` or `ease = ["out", "spring", "soft"]`.
///
/// A name may also be an inline Bézier: `"bezier(0.3,0.1,0.2,1)"`. Still a
/// string, still data — there is nowhere in this format for an expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EaseSpec {
    All(String),
    Each(Vec<String>),
}

impl EaseSpec {
    pub fn name_for(&self, i: usize, len: usize) -> Option<&str> {
        match self {
            EaseSpec::All(s) => Some(s.as_str()),
            EaseSpec::Each(v) => {
                if v.len() != len {
                    None
                } else {
                    v.get(i).map(String::as_str)
                }
            }
        }
    }
    /// How many easings this spec names, or `None` for "one for every key".
    /// Not a collection length — there is no empty `EaseSpec`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> Option<usize> {
        match self {
            EaseSpec::All(_) => None,
            EaseSpec::Each(v) => Some(v.len()),
        }
    }
}

/// One of F74's eight expressions, mapped onto a clip and a layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpressionDoc {
    pub name: String,
    pub clip: String,
    /// Which layer it plays on. Defaults to the last additive layer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_ms: Option<Num>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_accepts_integers_and_floats_alike() {
        #[derive(Deserialize)]
        struct T {
            a: Num,
            b: Num,
            c: Vec<Num>,
        }
        let t: T = toml::from_str("a = 3\nb = -2.5\nc = [0, 1.5, -2]").unwrap();
        assert_eq!(t.a, Num(3.0));
        assert_eq!(t.b, Num(-2.5));
        assert_eq!(t.c, vec![Num(0.0), Num(1.5), Num(-2.0)]);
    }

    #[test]
    fn num_serialises_without_float_noise() {
        #[derive(Serialize)]
        struct T {
            v: Num,
        }
        let s = toml::to_string(&T { v: Num(0.3) }).unwrap();
        assert!(s.contains("0.3"), "{s}");
        assert!(!s.contains("0.30000"), "{s}");
    }

    #[test]
    fn ease_spec_accepts_one_name_or_a_list() {
        #[derive(Deserialize)]
        struct T {
            ease: EaseSpec,
        }
        let one: T = toml::from_str(r#"ease = "soft""#).unwrap();
        assert_eq!(one.ease, EaseSpec::All("soft".into()));
        let many: T = toml::from_str(r#"ease = ["soft", "spring"]"#).unwrap();
        assert_eq!(many.ease.len(), Some(2));
        assert_eq!(many.ease.name_for(1, 2), Some("spring"));
        // One name applies to every key, whatever the track length.
        assert_eq!(one.ease.name_for(7, 99), Some("soft"));
        // A per-key list of the wrong length refuses rather than guessing.
        assert_eq!(many.ease.name_for(0, 5), None);
    }

    #[test]
    fn a_blank_document_round_trips_through_toml() {
        let doc = SkinDoc::blank("Test");
        let s = toml::to_string(&doc).unwrap();
        let back: SkinDoc = toml::from_str(&s).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn optional_tables_may_be_omitted_entirely() {
        let src = r#"
format = "nx-wisp-skin"
version = 1
[meta]
name = "Minimal"
[canvas]
[[bone]]
name = "root"
"#;
        let doc: SkinDoc = toml::from_str(src).unwrap();
        assert_eq!(doc.meta.name, "Minimal");
        assert_eq!(doc.meta.default_size_px, Num(128.0));
        assert_eq!(doc.canvas.size, [Num(256.0), Num(256.0)]);
        assert_eq!(doc.bones.len(), 1);
        assert!(doc.clips.is_empty());
    }

    #[test]
    fn array_of_tables_reads_as_one_stanza_per_item() {
        let src = r#"
format = "nx-wisp-skin"
version = 1
[meta]
name = "T"
[canvas]

# The comment TOML lets us keep, and JSON would not.
[[bone]]
name = "root"
pos = [128, 200]

[[bone]]
name = "body"
parent = "root"
pos = [0, -40]
rot = 12
length = 40
"#;
        let doc: SkinDoc = toml::from_str(src).unwrap();
        assert_eq!(doc.bones.len(), 2);
        assert_eq!(doc.bones[1].parent, "root");
        assert_eq!(doc.bones[1].rot, Some(Num(12.0)));
        assert_eq!(pt(doc.bones[0].pos), crate::math::Vec2::new(128.0, 200.0));
    }

    #[test]
    fn a_track_is_two_parallel_arrays() {
        let src = r#"
bone = "body"
channel = "ty"
t = [0, 1600, 3200]
v = [0, -4.0, 0]
ease = "soft"
"#;
        let tr: TrackDoc = toml::from_str(src).unwrap();
        assert_eq!(tr.t.len(), 3);
        assert_eq!(tr.v[1], Num(-4.0));
        assert_eq!(tr.ease.unwrap().name_for(0, 3), Some("soft"));
    }

    #[test]
    fn a_stroke_may_be_written_inline() {
        #[derive(Deserialize)]
        struct T {
            stroke: StrokeDoc,
        }
        let t: T = toml::from_str(r#"stroke = { gradient = "edge", width = 1.5 }"#).unwrap();
        assert_eq!(t.stroke.gradient, "edge");
        assert_eq!(t.stroke.width, Num(1.5));
    }
}
