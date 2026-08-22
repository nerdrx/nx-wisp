//! What is selected, and what the pointer currently does.
//!
//! One selection model covers every panel. The canvas, the bone tree, the
//! timeline and the graph all select into the same set, so "click a bone in
//! the tree, see its handle light up on the canvas" needs no synchronisation
//! code — there is only one place the answer lives.

use std::collections::BTreeSet;

/// Anything that can be selected. Ordered so a `BTreeSet` groups a
/// multi-selection by kind, which is what the properties panel wants when it
/// asks "are these all points of one shape?".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Shape(usize),
    /// One point of a shape's path, in the order the path lists them —
    /// control points included, exactly as `[[shape.weight]]` numbers them.
    Point { shape: usize, point: usize },
    Bone(usize),
    Gradient(usize),
    GradientStop { gradient: usize, stop: usize },
    Ik(usize),
    Chain(usize),
    Layer(usize),
    Clip(usize),
    Expression(usize),
    Key { clip: usize, track: usize, key: usize },
    /// A node in the state-machine view. Indexes the sidecar mood graph, not
    /// the skin.
    State(usize),
    Transition(usize),
}

impl Target {
    pub fn kind(&self) -> &'static str {
        match self {
            Target::Shape(_) => "shape",
            Target::Point { .. } => "point",
            Target::Bone(_) => "bone",
            Target::Gradient(_) => "gradient",
            Target::GradientStop { .. } => "gradient stop",
            Target::Ik(_) => "IK chain",
            Target::Chain(_) => "spring chain",
            Target::Layer(_) => "layer",
            Target::Clip(_) => "clip",
            Target::Expression(_) => "expression",
            Target::Key { .. } => "keyframe",
            Target::State(_) => "state",
            Target::Transition(_) => "transition",
        }
    }

    /// The shape a point belongs to, for the properties panel.
    pub fn owning_shape(&self) -> Option<usize> {
        match self {
            Target::Shape(i) => Some(*i),
            Target::Point { shape, .. } => Some(*shape),
            _ => None,
        }
    }
}

/// How a click combines with what is already selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectMode {
    /// Plain click: this and nothing else.
    #[default]
    Replace,
    /// Shift: add to the set.
    Add,
    /// Ctrl: remove from the set.
    Remove,
    /// Ctrl+Shift: flip.
    Toggle,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    items: BTreeSet<Target>,
    /// The most recently touched item. The properties panel shows this one
    /// when the set is mixed, and a shift-click range starts from it.
    anchor: Option<Target>,
}

impl Selection {
    pub fn new() -> Selection {
        Selection::default()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn contains(&self, t: Target) -> bool {
        self.items.contains(&t)
    }
    pub fn iter(&self) -> impl Iterator<Item = Target> + '_ {
        self.items.iter().copied()
    }
    pub fn anchor(&self) -> Option<Target> {
        self.anchor
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.anchor = None;
    }

    pub fn set(&mut self, t: Target) {
        self.items.clear();
        self.items.insert(t);
        self.anchor = Some(t);
    }

    pub fn set_many(&mut self, ts: impl IntoIterator<Item = Target>) {
        self.items.clear();
        self.items.extend(ts);
        self.anchor = self.items.iter().next_back().copied();
    }

    pub fn apply(&mut self, t: Target, mode: SelectMode) {
        match mode {
            SelectMode::Replace => self.set(t),
            SelectMode::Add => {
                self.items.insert(t);
                self.anchor = Some(t);
            }
            SelectMode::Remove => {
                self.items.remove(&t);
                if self.anchor == Some(t) {
                    self.anchor = self.items.iter().next_back().copied();
                }
            }
            SelectMode::Toggle => {
                if self.items.contains(&t) {
                    self.apply(t, SelectMode::Remove);
                } else {
                    self.apply(t, SelectMode::Add);
                }
            }
        }
    }

    /// Every selected point of one shape, in path order.
    pub fn points_of(&self, shape: usize) -> Vec<usize> {
        self.items
            .iter()
            .filter_map(|t| match t {
                Target::Point { shape: s, point } if *s == shape => Some(*point),
                _ => None,
            })
            .collect()
    }

    /// Every selected point, grouped by shape.
    pub fn points(&self) -> Vec<(usize, usize)> {
        self.items
            .iter()
            .filter_map(|t| match t {
                Target::Point { shape, point } => Some((*shape, *point)),
                _ => None,
            })
            .collect()
    }

    pub fn bones(&self) -> Vec<usize> {
        self.items
            .iter()
            .filter_map(|t| match t {
                Target::Bone(i) => Some(*i),
                _ => None,
            })
            .collect()
    }

    pub fn keys(&self) -> Vec<(usize, usize, usize)> {
        self.items
            .iter()
            .filter_map(|t| match t {
                Target::Key { clip, track, key } => Some((*clip, *track, *key)),
                _ => None,
            })
            .collect()
    }

    /// Drop anything addressing an index that no longer exists. Called after
    /// a delete or an undo, so the selection can never point into a hole.
    pub fn retain_valid(&mut self, valid: impl Fn(Target) -> bool) {
        self.items.retain(|t| valid(*t));
        if let Some(a) = self.anchor {
            if !valid(a) {
                self.anchor = self.items.iter().next_back().copied();
            }
        }
    }
}

/// What a press on the canvas means. One tool at a time, named for what the
/// operator is doing rather than for what the code does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    /// Click to select, drag to move what is selected.
    #[default]
    Select,
    /// Click on empty canvas to append a point to the selected shape's path;
    /// click on a segment to insert one there.
    Pen,
    /// Click to delete the point or shape under the pointer.
    Erase,
    /// Drag from a bone to create a child bone; drag a bone to move its rest
    /// position.
    Bone,
    /// Drag over points to bind them to the selected bone with a falloff.
    Weight,
    /// Click three bones in order to place a two-bone IK chain, or one bone
    /// for a look-at.
    Ik,
    /// Drag her limbs directly; the pose becomes keyframes.
    Puppet,
    /// Drag the canvas.
    Pan,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Pen => "pen",
            Tool::Erase => "erase",
            Tool::Bone => "bone",
            Tool::Weight => "weight",
            Tool::Ik => "IK",
            Tool::Puppet => "puppet",
            Tool::Pan => "pan",
        }
    }

    /// The status-strip hint shown while this tool is armed.
    pub fn hint(self) -> &'static str {
        match self {
            Tool::Select => "click to select, drag to move, shift to add",
            Tool::Pen => "click to add a point, click a segment to split it",
            Tool::Erase => "click a point or a shape to delete it",
            Tool::Bone => "drag from a bone to give it a child",
            Tool::Weight => "drag over points to bind them to the selected bone",
            Tool::Ik => "click root, mid and tip to place a two-bone chain",
            Tool::Puppet => "drag her to pose her, then press K to keyframe it",
            Tool::Pan => "drag to move the canvas, wheel to zoom",
        }
    }

    /// Every tool, in the order the toolbar shows them.
    pub const ALL: [Tool; 8] = [
        Tool::Select,
        Tool::Pen,
        Tool::Erase,
        Tool::Bone,
        Tool::Weight,
        Tool::Ik,
        Tool::Puppet,
        Tool::Pan,
    ];
}
