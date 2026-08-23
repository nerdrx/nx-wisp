//! **The seam `wisp-shell` plugs into. This crate implements no part of it.**
//!
//! SPEC §2 gives `wisp-shell` sctk, the layer surface, input regions, outputs,
//! seats, focus and the cursor, and gives `wisp-paint` the wgpu device. None of
//! that is here, and none of it may be added here: `wisp` is a host process,
//! and a host process that quietly grew a swapchain would be two crates in one
//! file with two agents editing it.
//!
//! So the compositor half of the app is a trait object. [`app::run`] owns the
//! event loop, the governor and the rig, and pushes everything the body needs
//! through [`Shell`]; the implementation lives in `wisp-shell` and is handed in
//! at construction. There is exactly one direction of dependency and exactly
//! one place to look for it.
//!
//! [`app::run`]: crate::app::run
//!
//! # What an implementation gets
//!
//! * [`Shell::present`] — a [`RigFrame`] in surface pixels, already posed and
//!   deformed, plus the click-through [`Polygon`] for the Wayland input region.
//!   `wisp-rig` computed both; the shell uploads and draws them.
//! * [`Shell::say`] — an [`Utterance`] that survived the attention budget. It
//!   is the *only* thing that may become a speech bubble: SPEC §3.4 says
//!   nothing reaches the operator except through `wisp-attn`, and this is where
//!   that lands.
//! * [`Shell::invasive_tell`] — SPEC §0.3. While this is on for a sense, the
//!   character herself must show it. Not a tray icon, not a notification: her.
//! * [`Shell::set_tier`] — the governor's verdict. The shell sheds with
//!   everyone else (fewer frames, no blur, and at T4 a transparent frame rather
//!   than an unmapped surface, so she does not flicker back).
//!
//! # What it does not get
//!
//! A way to speak. A way to change tier. A way to publish an event. Those are
//! decisions, and they belong to `wisp-attn` and `wisp-gov`.

use wisp_attn::MoveTarget;
use wisp_proto::{sense::Observation, SenseId, Tier, Utterance};
use wisp_rig::{ContourOptions, Polygon, RigFrame};

/// What the body is told, once per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameCtx {
    /// Seconds since the previous frame.
    pub dt: f32,
    /// Rendered size in surface pixels (F75).
    pub size_px: f32,
    pub tier: Tier,
    /// The frame rate the governor is asking for.
    pub target_fps: u32,
}

/// The compositor half of the app.
///
/// Every method has a default that does nothing, so an implementation can grow
/// one feature at a time and so [`Headless`] is a one-line type. A shell that
/// implements none of it is a valid shell — it is what CI runs.
pub trait Shell: Send {
    /// A posed frame, ready to draw. `input_region` is the click-through
    /// silhouette; everywhere outside it, clicks must pass to the window below.
    fn present(&mut self, _frame: &RigFrame, _input_region: &Polygon, _ctx: &FrameCtx) {}

    /// She said something. This is the speech bubble.
    fn say(&mut self, _utterance: &Utterance) {}

    /// The rig switched expression, outside of a clip.
    fn set_expression(&mut self, _name: &str) {}

    /// A clip was requested by a behaviour tree.
    fn play_clip(&mut self, _name: &str, _looping: bool) {}

    /// SPEC §0.3: an invasive sense is live, or has stopped. The tell goes on
    /// the character herself and stays for the whole time it is active.
    fn invasive_tell(&mut self, _sense: SenseId, _active: bool) {}

    /// The governor moved her.
    fn set_tier(&mut self, _tier: Tier) {}

    /// A behaviour tree wants her somewhere.
    fn move_to(&mut self, _target: &MoveTarget) {}

    /// She poked a window (F40's focus warden, and her antics).
    fn poke(&mut self, _window: Option<u64>) {}

    /// Operator input the shell collected since the last call — today, lines
    /// typed into the summon palette. The app publishes each as
    /// `Observation::Speech { final_: true }`: typed words ARE speech, and the
    /// entire pipeline (mind → budget → bubble → voice) answers them with no
    /// separate path. Facts flow *into* the bus; the shell still cannot say
    /// anything on its own.
    fn take_input(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// Something was sensed.
    ///
    /// The shell needs this for one thing above all: `Observation::Window`
    /// carries the operator's real window rectangles, and F68 turns their top
    /// edges into ledges she can stand on. Without it her only floor is the
    /// bottom of the screen and she is a creature on top of the desktop rather
    /// than in it.
    fn observed(&mut self, _obs: &Observation) {}

    /// The operator changed her size in the config.
    fn set_size(&mut self, _size_px: f32) {}

    /// Where the pointer is, if the shell knows. The rig uses it for look-at.
    fn cursor(&self) -> Option<(f32, f32)> {
        None
    }

    /// Where she is standing, in surface pixels. The shell owns her position —
    /// it owns the outputs and the terrain — so the rig asks rather than tells.
    fn anchor(&self) -> Option<(f32, f32)> {
        None
    }

    /// How finely to flatten curves for the input region. A shell that draws
    /// nothing does not need a precise one.
    fn contour_options(&self) -> ContourOptions {
        ContourOptions::default()
    }

    /// The compositor is going away, or she is. Take the surface down before
    /// this returns.
    fn shutdown(&mut self) {}
}

/// No compositor, no GPU, no window. What `--mock` runs and what CI runs.
#[derive(Debug, Default)]
pub struct Headless {
    pub frames: u64,
    pub windows: Vec<(u64, bool)>,
    pub said: Vec<String>,
    pub clips: Vec<String>,
    pub tells: Vec<(SenseId, bool)>,
    pub tier: Option<Tier>,
}

impl Shell for Headless {
    fn present(&mut self, _frame: &RigFrame, _input_region: &Polygon, _ctx: &FrameCtx) {
        self.frames += 1;
    }
    fn observed(&mut self, obs: &Observation) {
        if let Observation::Window { id, gone, .. } = obs {
            self.windows.push((*id, *gone));
        }
    }
    fn say(&mut self, utterance: &Utterance) {
        self.said.push(utterance.text.clone());
    }
    fn play_clip(&mut self, name: &str, _looping: bool) {
        self.clips.push(name.to_string());
    }
    fn invasive_tell(&mut self, sense: SenseId, active: bool) {
        self.tells.push((sense, active));
    }
    fn set_tier(&mut self, tier: Tier) {
        self.tier = Some(tier);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_proto::Urgency;

    #[test]
    fn the_headless_shell_is_a_complete_shell() {
        let mut s = Headless::default();
        s.say(&Utterance::new("hello", Urgency::Notable));
        s.play_clip("wave", false);
        s.invasive_tell(SenseId::Clipboard, true);
        s.set_tier(Tier::Lobotomised);
        s.shutdown();
        assert_eq!(s.said, ["hello"]);
        assert_eq!(s.clips, ["wave"]);
        assert_eq!(s.tells, [(SenseId::Clipboard, true)]);
        assert_eq!(s.tier, Some(Tier::Lobotomised));
        assert_eq!(s.frames, 0);
        assert!(s.cursor().is_none() && s.anchor().is_none());
    }

    /// The seam has to be a trait object: the shell is handed in at
    /// construction and this crate never names its concrete type.
    #[test]
    fn a_shell_is_object_safe_and_sendable() {
        fn takes(_: Box<dyn Shell>) {}
        takes(Box::new(Headless::default()));
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn Shell>>();
    }
}
