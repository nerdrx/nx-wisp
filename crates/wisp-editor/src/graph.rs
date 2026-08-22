//! The state-machine view: moods and behaviours wired to clips, as a drawn
//! graph.
//!
//! # This is a sidecar, and that is a reported gap
//!
//! F76 wants "bored → wander" to be data rather than code, drawn rather than
//! typed. The skin format cannot hold it. `SkinDoc` has
//! `#[serde(deny_unknown_fields)]` on every struct, so a `[[state]]` table in
//! a `.skin.toml` is a hard parse error, and SPEC §2 and §3.6 put the format
//! under `wisp-rig`'s exclusive ownership — this crate may not extend it.
//!
//! What the skin *does* model is `[[expression]]`: mood name → clip. That is
//! one edge of the graph and only one. There is nowhere for a transition, a
//! trigger, a behaviour, or a node position.
//!
//! So the graph lives beside the skin, in `<name>.moods.toml`, in a document
//! this crate owns. It is still **data only** — SPEC §3.6's rule that a skin
//! can never contain executable code is honoured by shape here too: a state
//! names a clip and a behaviour, a transition names an event and a delay, and
//! there is no field anywhere whose value is interpreted as an expression.
//!
//! The proper home for this is the skin format itself, next to F50's
//! declarative behaviour trees. That is a `wisp-rig` change and a SPEC
//! amendment, and it is written up in this crate's report rather than done
//! quietly here.
//!
//! # Positions are part of the document
//!
//! A node's `x`/`y` are saved. A graph that re-laid itself out on every open
//! would make the operator re-learn the picture every session, and the picture
//! *is* the point of a state-machine view.

use serde::{Deserialize, Serialize};
use wisp_rig::skin::doc::SkinDoc;

use crate::error::EditError;
use crate::history::Reversible;

pub const MOODS_MAGIC: &str = "nx-wisp-moods";
pub const MOODS_VERSION: u32 = 1;

/// One node: a mood or a behaviour she can be in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateNode {
    /// What the mood FSM calls this state.
    pub name: String,
    /// The clip that plays while she is in it. Empty means "whatever was
    /// already playing" — a mood that only changes her face.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub clip: String,
    /// One of F74's eight expressions, played on its own layer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expression: String,
    /// The behaviour that runs in this state: `idle`, `wander`, `follow`,
    /// `sleep`, `watch`. A name, not a script.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub behaviour: String,
    /// Where the node sits in the graph view.
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    /// Exactly one state may be the entry point.
    #[serde(default, skip_serializing_if = "is_false")]
    pub initial: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One edge: what moves her from one state to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    pub from: String,
    pub to: String,
    /// The event that fires it — an `Observation` kind or a mood change.
    /// Empty plus `after_ms` is a plain timeout.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub on: String,
    /// Fire after this long in `from`, with no event at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_ms: Option<f32>,
    /// Cross-fade length when the clip changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fade_ms: Option<f32>,
}

/// The whole sidecar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoodGraph {
    pub format: String,
    pub version: u32,
    /// The skin this graph belongs to, by `meta.name`. Checked on load so a
    /// graph cannot be silently applied to the wrong character.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub skin: String,
    #[serde(default, rename = "state")]
    pub states: Vec<StateNode>,
    #[serde(default, rename = "transition")]
    pub transitions: Vec<Transition>,
}

impl Default for MoodGraph {
    fn default() -> Self {
        MoodGraph {
            format: MOODS_MAGIC.to_string(),
            version: MOODS_VERSION,
            skin: String::new(),
            states: Vec::new(),
            transitions: Vec::new(),
        }
    }
}

/// The behaviours a state may name. A closed list, in the spirit of SPEC
/// §3.3's closed `Observation`: adding one is a decision, not a typo.
pub const BEHAVIOURS: &[&str] =
    &["idle", "wander", "follow", "watch", "sleep", "hide", "play", "none"];

impl MoodGraph {
    /// A starting graph for a skin that has none: one node per expression the
    /// skin declares, laid out in a ring, with the two edges F76 names by
    /// example already drawn.
    pub fn from_skin(doc: &SkinDoc) -> MoodGraph {
        let n = doc.expressions.len().max(1);
        let mut states: Vec<StateNode> = Vec::with_capacity(n);
        for (i, e) in doc.expressions.iter().enumerate() {
            let a = std::f32::consts::TAU * (i as f32 / n as f32) - std::f32::consts::FRAC_PI_2;
            states.push(StateNode {
                name: e.name.clone(),
                clip: String::new(),
                expression: e.name.clone(),
                behaviour: default_behaviour(&e.name).to_string(),
                x: (RING_R * a.cos() * 100.0).round() / 100.0,
                y: (RING_R * a.sin() * 100.0).round() / 100.0,
                initial: e.name == "neutral",
            });
        }
        let mut transitions = Vec::new();
        // The example from the plan, drawn rather than coded.
        if states.iter().any(|s| s.name == "bored") && states.iter().any(|s| s.name == "neutral") {
            transitions.push(Transition {
                from: "bored".into(),
                to: "neutral".into(),
                on: String::new(),
                after_ms: Some(20_000.0),
                fade_ms: Some(320.0),
            });
            transitions.push(Transition {
                from: "neutral".into(),
                to: "bored".into(),
                on: String::new(),
                after_ms: Some(120_000.0),
                fade_ms: Some(320.0),
            });
        }
        MoodGraph {
            skin: doc.meta.name.clone(),
            states,
            transitions,
            ..Default::default()
        }
    }

    pub fn state_index(&self, name: &str) -> Option<usize> {
        self.states.iter().position(|s| s.name == name)
    }

    /// Everything wrong with this graph, phrased for the operator. A graph is
    /// never *invalid* enough to refuse to open — an editor that will not show
    /// you the broken thing is no help — so this is a report, not an error.
    pub fn problems(&self, doc: &SkinDoc) -> Vec<String> {
        let mut out = Vec::new();
        if self.format != MOODS_MAGIC {
            out.push(format!(
                "this is not a mood graph — 'format' is {:?}, expected {MOODS_MAGIC:?}",
                self.format
            ));
        }
        if self.version != MOODS_VERSION {
            out.push(format!(
                "mood graph version {} is not supported by this build (it reads {MOODS_VERSION})",
                self.version
            ));
        }
        if !self.skin.is_empty() && self.skin != doc.meta.name {
            out.push(format!(
                "this graph was written for the skin {:?}, and the open skin is {:?}",
                self.skin, doc.meta.name
            ));
        }
        for (i, s) in self.states.iter().enumerate() {
            if self.states.iter().take(i).any(|o| o.name == s.name) {
                out.push(format!("two states are both named {:?} — names must be unique", s.name));
            }
            if !s.clip.is_empty() && !doc.clips.iter().any(|c| c.name == s.clip) {
                out.push(format!(
                    "state {:?} plays the clip {:?}, which this skin does not have",
                    s.name, s.clip
                ));
            }
            if !s.expression.is_empty()
                && !doc.expressions.iter().any(|e| e.name == s.expression)
            {
                out.push(format!(
                    "state {:?} uses the expression {:?}, which this skin does not have",
                    s.name, s.expression
                ));
            }
            if !s.behaviour.is_empty() && !BEHAVIOURS.contains(&s.behaviour.as_str()) {
                out.push(format!(
                    "state {:?} names the behaviour {:?} — expected one of {}",
                    s.name,
                    s.behaviour,
                    BEHAVIOURS.join(", ")
                ));
            }
        }
        for t in &self.transitions {
            if self.state_index(&t.from).is_none() {
                out.push(format!("a transition starts at {:?}, which is not a state", t.from));
            }
            if self.state_index(&t.to).is_none() {
                out.push(format!("a transition ends at {:?}, which is not a state", t.to));
            }
            if t.on.is_empty() && t.after_ms.is_none() {
                out.push(format!(
                    "the transition {:?} -> {:?} has neither an event nor a delay, so nothing \
                     can ever fire it",
                    t.from, t.to
                ));
            }
        }
        let initials = self.states.iter().filter(|s| s.initial).count();
        if initials > 1 {
            out.push(format!("{initials} states are marked as the entry point — pick one"));
        }
        if initials == 0 && !self.states.is_empty() {
            out.push("no state is marked as the entry point".to_string());
        }
        // A state nothing can reach is a drawing mistake worth pointing at.
        for s in &self.states {
            if s.initial {
                continue;
            }
            if !self.transitions.iter().any(|t| t.to == s.name) {
                out.push(format!("nothing leads to {:?}", s.name));
            }
        }
        out
    }

    pub fn to_toml(&self) -> Result<String, EditError> {
        toml::to_string_pretty(self).map_err(|e| EditError::Write(e.to_string()))
    }

    pub fn parse(src: &str) -> Result<MoodGraph, EditError> {
        toml::from_str(src).map_err(|e| EditError::Read(e.to_string()))
    }

    /// Where the graph for a skin file lives: the same stem, `.moods.toml`.
    pub fn path_for(skin: &std::path::Path) -> std::path::PathBuf {
        let stem = skin
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.trim_end_matches(".toml").trim_end_matches(".skin").to_string())
            .unwrap_or_else(|| "skin".to_string());
        skin.with_file_name(format!("{stem}.moods.toml"))
    }
}

/// Radius of the default ring layout, in graph units.
const RING_R: f32 = 220.0;

fn default_behaviour(mood: &str) -> &'static str {
    match mood {
        "bored" => "wander",
        "sleepy" => "sleep",
        "curious" | "alarmed" => "watch",
        "delighted" | "smug" => "play",
        "worried" => "hide",
        _ => "idle",
    }
}

// ------------------------------------------------------------------ commands

/// A reversible edit to the mood graph, with the same apply-returns-inverse
/// contract as [`crate::cmd::Command`].
#[derive(Debug, Clone, PartialEq)]
pub enum GraphCommand {
    Batch { label: &'static str, cmds: Vec<GraphCommand> },
    InsertState { at: usize, value: Box<StateNode> },
    RemoveState { at: usize },
    SetState { at: usize, value: Box<StateNode> },
    /// Drag a node. Separate from `SetState` so a drag coalesces into one undo
    /// step without swallowing an edit to the node's clip.
    MoveState { at: usize, x: f32, y: f32 },
    InsertTransition { at: usize, value: Box<Transition> },
    RemoveTransition { at: usize },
    SetTransition { at: usize, value: Box<Transition> },
}

impl GraphCommand {
    pub fn apply(self, g: &mut MoodGraph) -> Result<GraphCommand, EditError> {
        match self {
            GraphCommand::Batch { label, cmds } => {
                let mut undo = Vec::with_capacity(cmds.len());
                for c in cmds {
                    match c.apply(g) {
                        Ok(inv) => undo.push(inv),
                        Err(e) => {
                            while let Some(back) = undo.pop() {
                                let _ = back.apply(g);
                            }
                            return Err(e);
                        }
                    }
                }
                undo.reverse();
                Ok(GraphCommand::Batch { label, cmds: undo })
            }
            GraphCommand::InsertState { at, value } => {
                if at > g.states.len() {
                    return Err(EditError::NoSuchIndex {
                        kind: "state",
                        at,
                        len: g.states.len(),
                    });
                }
                g.states.insert(at, *value);
                Ok(GraphCommand::RemoveState { at })
            }
            GraphCommand::RemoveState { at } => {
                if at >= g.states.len() {
                    return Err(EditError::NoSuchIndex {
                        kind: "state",
                        at,
                        len: g.states.len(),
                    });
                }
                let value = g.states.remove(at);
                Ok(GraphCommand::InsertState { at, value: Box::new(value) })
            }
            GraphCommand::SetState { at, value } => {
                if at >= g.states.len() {
                    return Err(EditError::NoSuchIndex {
                        kind: "state",
                        at,
                        len: g.states.len(),
                    });
                }
                let old = std::mem::replace(&mut g.states[at], *value);
                Ok(GraphCommand::SetState { at, value: Box::new(old) })
            }
            GraphCommand::MoveState { at, x, y } => {
                if at >= g.states.len() {
                    return Err(EditError::NoSuchIndex {
                        kind: "state",
                        at,
                        len: g.states.len(),
                    });
                }
                let s = &mut g.states[at];
                let (ox, oy) = (s.x, s.y);
                s.x = x;
                s.y = y;
                Ok(GraphCommand::MoveState { at, x: ox, y: oy })
            }
            GraphCommand::InsertTransition { at, value } => {
                if at > g.transitions.len() {
                    return Err(EditError::NoSuchIndex {
                        kind: "transition",
                        at,
                        len: g.transitions.len(),
                    });
                }
                g.transitions.insert(at, *value);
                Ok(GraphCommand::RemoveTransition { at })
            }
            GraphCommand::RemoveTransition { at } => {
                if at >= g.transitions.len() {
                    return Err(EditError::NoSuchIndex {
                        kind: "transition",
                        at,
                        len: g.transitions.len(),
                    });
                }
                let value = g.transitions.remove(at);
                Ok(GraphCommand::InsertTransition { at, value: Box::new(value) })
            }
            GraphCommand::SetTransition { at, value } => {
                if at >= g.transitions.len() {
                    return Err(EditError::NoSuchIndex {
                        kind: "transition",
                        at,
                        len: g.transitions.len(),
                    });
                }
                let old = std::mem::replace(&mut g.transitions[at], *value);
                Ok(GraphCommand::SetTransition { at, value: Box::new(old) })
            }
        }
    }
}

impl Reversible for GraphCommand {
    type Doc = MoodGraph;

    fn apply_to(self, g: &mut MoodGraph) -> Result<GraphCommand, EditError> {
        self.apply(g)
    }

    fn label(&self) -> &'static str {
        match self {
            GraphCommand::Batch { label, .. } => label,
            GraphCommand::InsertState { .. } => "add a state",
            GraphCommand::RemoveState { .. } => "delete a state",
            GraphCommand::SetState { .. } => "edit a state",
            GraphCommand::MoveState { .. } => "move a state",
            GraphCommand::InsertTransition { .. } => "add a transition",
            GraphCommand::RemoveTransition { .. } => "delete a transition",
            GraphCommand::SetTransition { .. } => "edit a transition",
        }
    }

    fn is_continuous(&self) -> bool {
        matches!(self, GraphCommand::MoveState { .. })
    }

    fn same_target(&self, other: &GraphCommand) -> bool {
        match (self, other) {
            (GraphCommand::MoveState { at: a, .. }, GraphCommand::MoveState { at: b, .. }) => {
                a == b
            }
            _ => false,
        }
    }
}

// -------------------------------------------------------------- edit helpers

/// Add a state, refusing a duplicate name.
pub fn add_state(
    g: &MoodGraph,
    name: &str,
    behaviour: &str,
    at: (f32, f32),
) -> Result<GraphCommand, EditError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(EditError::EmptyName { kind: "state", name: name.to_string() });
    }
    if g.state_index(trimmed).is_some() {
        return Err(EditError::DuplicateName { kind: "state", name: trimmed.to_string() });
    }
    Ok(GraphCommand::InsertState {
        at: g.states.len(),
        value: Box::new(StateNode {
            name: trimmed.to_string(),
            clip: String::new(),
            expression: String::new(),
            behaviour: behaviour.to_string(),
            x: at.0,
            y: at.1,
            initial: g.states.is_empty(),
        }),
    })
}

/// Delete a state, and every transition that touched it — a dangling edge is
/// never what the operator meant, and leaving one would only show up later as
/// a validation warning.
pub fn delete_state(g: &MoodGraph, state: usize) -> Result<GraphCommand, EditError> {
    let s = g
        .states
        .get(state)
        .ok_or(EditError::NoSuchIndex { kind: "state", at: state, len: g.states.len() })?;
    let mut cmds = Vec::new();
    // Highest index first, so earlier removals do not shift later ones.
    let mut edges: Vec<usize> = g
        .transitions
        .iter()
        .enumerate()
        .filter(|(_, t)| t.from == s.name || t.to == s.name)
        .map(|(i, _)| i)
        .collect();
    edges.reverse();
    for i in edges {
        cmds.push(GraphCommand::RemoveTransition { at: i });
    }
    cmds.push(GraphCommand::RemoveState { at: state });
    Ok(GraphCommand::Batch { label: "delete a state", cmds })
}

/// Draw an edge between two states.
pub fn connect(
    g: &MoodGraph,
    from: &str,
    to: &str,
    on: &str,
    after_ms: Option<f32>,
) -> Result<GraphCommand, EditError> {
    if g.state_index(from).is_none() {
        return Err(EditError::NoSuchName { kind: "state", name: from.to_string() });
    }
    if g.state_index(to).is_none() {
        return Err(EditError::NoSuchName { kind: "state", name: to.to_string() });
    }
    if g.transitions.iter().any(|t| t.from == from && t.to == to && t.on == on) {
        return Err(EditError::DuplicateName {
            kind: "transition",
            name: format!("{from} -> {to}"),
        });
    }
    Ok(GraphCommand::InsertTransition {
        at: g.transitions.len(),
        value: Box::new(Transition {
            from: from.to_string(),
            to: to.to_string(),
            on: on.to_string(),
            after_ms,
            fade_ms: Some(320.0),
        }),
    })
}

/// Point a state at a clip, checking the clip exists in the skin.
pub fn set_state_clip(
    g: &MoodGraph,
    doc: &SkinDoc,
    state: usize,
    clip: &str,
) -> Result<GraphCommand, EditError> {
    let s = g
        .states
        .get(state)
        .ok_or(EditError::NoSuchIndex { kind: "state", at: state, len: g.states.len() })?;
    if !clip.is_empty() && !doc.clips.iter().any(|c| c.name == clip) {
        return Err(EditError::NoSuchName { kind: "clip", name: clip.to_string() });
    }
    Ok(GraphCommand::SetState {
        at: state,
        value: Box::new(StateNode { clip: clip.to_string(), ..s.clone() }),
    })
}

/// Set a state's behaviour, refusing a name that is not in the closed list.
pub fn set_state_behaviour(
    g: &MoodGraph,
    state: usize,
    behaviour: &str,
) -> Result<GraphCommand, EditError> {
    let s = g
        .states
        .get(state)
        .ok_or(EditError::NoSuchIndex { kind: "state", at: state, len: g.states.len() })?;
    if !BEHAVIOURS.contains(&behaviour) {
        return Err(EditError::NoSuchName { kind: "behaviour", name: behaviour.to_string() });
    }
    Ok(GraphCommand::SetState {
        at: state,
        value: Box::new(StateNode { behaviour: behaviour.to_string(), ..s.clone() }),
    })
}

/// Make one state the entry point, clearing the flag everywhere else.
pub fn set_initial(g: &MoodGraph, state: usize) -> Result<GraphCommand, EditError> {
    if state >= g.states.len() {
        return Err(EditError::NoSuchIndex { kind: "state", at: state, len: g.states.len() });
    }
    let cmds = g
        .states
        .iter()
        .enumerate()
        .filter(|(i, s)| s.initial != (*i == state))
        .map(|(i, s)| GraphCommand::SetState {
            at: i,
            value: Box::new(StateNode { initial: i == state, ..s.clone() }),
        })
        .collect();
    Ok(GraphCommand::Batch { label: "set the entry state", cmds })
}

// --------------------------------------------------------------------- layout

/// A node's box in graph units. Angular, because the graph is chrome.
pub const NODE_W: f32 = 148.0;
pub const NODE_H: f32 = 56.0;

/// The node under a point in graph units, topmost first.
pub fn hit_state(g: &MoodGraph, x: f32, y: f32) -> Option<usize> {
    (0..g.states.len()).rev().find(|&i| {
        let s = &g.states[i];
        x >= s.x - NODE_W * 0.5
            && x <= s.x + NODE_W * 0.5
            && y >= s.y - NODE_H * 0.5
            && y <= s.y + NODE_H * 0.5
    })
}

/// Where an edge starts and ends: the two node centres, pulled back to the
/// edge of each box so the arrow touches the node rather than vanishing under
/// it.
pub fn edge_ends(g: &MoodGraph, t: &Transition) -> Option<((f32, f32), (f32, f32))> {
    let a = &g.states[g.state_index(&t.from)?];
    let b = &g.states[g.state_index(&t.to)?];
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-3 {
        return None;
    }
    let clip = |sx: f32, sy: f32, dx: f32, dy: f32| {
        // Distance from the centre to the box edge along (dx, dy).
        let tx = if dx.abs() > 1e-6 { (NODE_W * 0.5) / dx.abs() } else { f32::INFINITY };
        let ty = if dy.abs() > 1e-6 { (NODE_H * 0.5) / dy.abs() } else { f32::INFINITY };
        let k = tx.min(ty);
        (sx + dx * k, sy + dy * k)
    };
    let start = clip(a.x, a.y, dx / len, dy / len);
    let end = clip(b.x, b.y, -dx / len, -dy / len);
    Some((start, end))
}

/// Re-lay the graph out in a ring, for when it has become spaghetti. Returns a
/// batch so one undo puts it back exactly as it was.
pub fn relayout(g: &MoodGraph) -> GraphCommand {
    let n = g.states.len().max(1);
    let cmds = g
        .states
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let a = std::f32::consts::TAU * (i as f32 / n as f32) - std::f32::consts::FRAC_PI_2;
            GraphCommand::MoveState {
                at: i,
                x: (RING_R * a.cos() * 100.0).round() / 100.0,
                y: (RING_R * a.sin() * 100.0).round() / 100.0,
            }
        })
        .collect();
    GraphCommand::Batch { label: "lay the graph out again", cmds }
}
