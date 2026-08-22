//! SPEC §3.1 — what she is allowed to be, per tier, expressed as one table.
//!
//! The whole ladder lives here as a pure function, [`policy`], so the governor's
//! verdict can be unit-tested without a voice, a model or a sound card, and so
//! that "what does T3 mean for speech" has exactly one answer in the tree rather
//! than one per module.
//!
//! ## The two decisions worth arguing about
//!
//! **T2 takes the GPU away, not her voice.** T2 is "something substantial
//! started". The right response is a cheaper voice, not a mute one — a companion
//! that goes silent the moment you open a compiler is a companion you turn off.
//! So T2 keeps speech, drops to a Piper pack, halves the lip-sync rate and cuts
//! the synthesis lookahead so a further downgrade sheds less finished work.
//!
//! **T3 turns the microphone off entirely.** This is the one place where the
//! tier ladder and the consent model meet, and it deliberately errs on the side
//! of the operator. T3 means a game or a VR session owns the machine — the
//! operator may well be wearing a headset, in a room, talking to somebody who
//! is not her. Keeping a transcriber running through that would be exactly the
//! kind of invisible listening SPEC §0.3 exists to forbid, even though consent
//! was technically granted for something else. She stops listening; if the
//! operator wants her to listen anyway they can press the push-to-talk key,
//! which is an explicit act.
//!
//! T4 is silence: no synthesis, no capture, and any duck released, because a
//! dormant companion that is still holding your music down is a bug you would
//! have to kill her to fix.

use wisp_proto::{Cost, Tier};

/// What speech may cost, and what it may do, at one tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoicePolicy {
    pub tier: Tier,
    /// May she synthesise at all?
    pub speak: bool,
    /// May the microphone be open?
    pub listen: bool,
    /// May the wake word run? (It implies a continuously open microphone, so it
    /// is strictly more invasive than push-to-talk and gets a stricter ceiling.)
    pub wake_word: bool,
    /// May any of this touch the discrete GPU?
    pub dgpu: bool,
    /// Lip-sync analysis rate, matched to the rig's target frame rate.
    pub drive_fps: u32,
    /// How far synthesis may run ahead of playback.
    pub lookahead_ms: u32,
    /// May she duck the operator's other audio?
    pub duck: bool,
    /// The weakest `allowed_until` a voice pack may declare and still be picked.
    /// A pack is usable when `tier <= pack.allowed_until`, so this is just the
    /// tier itself — kept explicit because it is the thing voice-pack selection
    /// actually compares against.
    pub voice_ceiling: Tier,
}

/// The ladder. Pure, total, and the only place that decides any of this.
pub const fn policy(tier: Tier) -> VoicePolicy {
    match tier {
        // T0: the machine is idle and the operator is away. She may be herself.
        Tier::Feral => VoicePolicy {
            tier,
            speak: true,
            listen: true,
            wake_word: true,
            dgpu: true,
            drive_fps: 60,
            lookahead_ms: 900,
            duck: true,
            voice_ceiling: Tier::Feral,
        },
        // T1: the operator is here and nothing heavy is running.
        Tier::Full => VoicePolicy {
            tier,
            speak: true,
            listen: true,
            wake_word: true,
            dgpu: true,
            drive_fps: 60,
            lookahead_ms: 700,
            duck: true,
            voice_ceiling: Tier::Full,
        },
        // T2: something substantial started. Cheaper, not quieter.
        Tier::Reduced => VoicePolicy {
            tier,
            speak: true,
            listen: true,
            // A continuously-open microphone is a background cost as well as a
            // privacy cost. Push-to-talk survives T2; the wake word does not.
            wake_word: false,
            // Whisper's Vulkan backend would be competing with whatever just
            // started for the same queues. CPU is slower and does not steal a
            // frame from the thing the operator is actually looking at.
            dgpu: false,
            drive_fps: 30,
            lookahead_ms: 400,
            duck: true,
            voice_ceiling: Tier::Reduced,
        },
        // T3: a game or a VR session owns the GPU. See the module docs.
        Tier::Lobotomised => VoicePolicy {
            tier,
            speak: true,
            listen: false,
            wake_word: false,
            dgpu: false,
            drive_fps: 15,
            lookahead_ms: 250,
            duck: true,
            voice_ceiling: Tier::Lobotomised,
        },
        // T4: silence.
        Tier::Dormant => VoicePolicy {
            tier,
            speak: false,
            listen: false,
            wake_word: false,
            dgpu: false,
            drive_fps: 0,
            lookahead_ms: 0,
            duck: false,
            voice_ceiling: Tier::Dormant,
        },
    }
}

/// Worst-case resident cost of the whole voice subsystem at a tier, for
/// `wisp-gov`'s accounting and the operator-facing cost meter.
///
/// These are honest estimates of *resident* cost, not peaks during synthesis:
/// a Piper VITS voice is a ~63 MB ONNX graph that ORT expands to roughly twice
/// that in arenas; `ggml-base.en` is ~142 MB and `tiny.en` ~75 MB. Nothing here
/// claims VRAM below T2 because nothing below T2 is allowed to touch the dGPU,
/// and at T0/T1 whisper's Vulkan context is the only thing that does.
pub const fn cost_at(tier: Tier) -> Cost {
    match tier {
        Tier::Feral | Tier::Full => Cost {
            // Kokoro graph + arenas, plus whisper base.en resident.
            ram_mib: 620,
            // whisper.cpp's Vulkan buffers for base.en.
            vram_mib: 260,
            cpu_centi_pct: 900,
        },
        Tier::Reduced => Cost {
            // Piper only, whisper tiny.en on CPU.
            ram_mib: 260,
            vram_mib: 0,
            cpu_centi_pct: 400,
        },
        Tier::Lobotomised => Cost {
            // Piper only, no capture at all.
            ram_mib: 150,
            vram_mib: 0,
            cpu_centi_pct: 150,
        },
        Tier::Dormant => Cost::FREE,
    }
}

impl VoicePolicy {
    /// Is anything at all allowed?
    pub const fn idle(&self) -> bool {
        !self.speak && !self.listen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LADDER: [Tier; 5] = [
        Tier::Feral,
        Tier::Full,
        Tier::Reduced,
        Tier::Lobotomised,
        Tier::Dormant,
    ];

    #[test]
    fn t3_switches_the_microphone_off_completely() {
        // The headline rule. She is not listening while the operator is in a
        // headset unless they explicitly ask.
        let p = policy(Tier::Lobotomised);
        assert!(!p.listen, "T3 must not keep the microphone open");
        assert!(!p.wake_word);
        assert!(p.speak, "…but canned speech survives T3 (SPEC §3.1)");
    }

    #[test]
    fn t3_and_below_never_touch_the_discrete_gpu() {
        for t in [Tier::Lobotomised, Tier::Dormant] {
            assert!(!policy(t).dgpu, "{t:?}");
        }
        // …and this agrees with the governor's own view of the same question.
        for t in LADDER {
            if !t.may_use_dgpu() {
                assert!(!policy(t).dgpu, "{t:?} disagrees with Tier::may_use_dgpu");
            }
        }
    }

    #[test]
    fn t4_is_silence_and_releases_everything() {
        let p = policy(Tier::Dormant);
        assert!(!p.speak && !p.listen && !p.duck && !p.dgpu);
        assert_eq!(p.drive_fps, 0);
        assert!(p.idle());
        assert_eq!(cost_at(Tier::Dormant), Cost::FREE);
    }

    #[test]
    fn t2_gets_cheaper_without_going_mute() {
        // A companion that goes silent when you open a compiler is one you
        // switch off.
        let full = policy(Tier::Full);
        let red = policy(Tier::Reduced);
        assert!(red.speak, "T2 must keep her voice");
        assert!(red.listen, "push-to-talk survives T2");
        assert!(!red.wake_word, "a continuously open mic does not");
        assert!(!red.dgpu);
        assert!(red.drive_fps < full.drive_fps);
        assert!(red.lookahead_ms < full.lookahead_ms);
    }

    #[test]
    fn cost_never_rises_as_the_tier_falls() {
        // The governor's accounting depends on this being monotonic.
        for w in LADDER.windows(2) {
            let (a, b) = (cost_at(w[0]), cost_at(w[1]));
            assert!(b.ram_mib <= a.ram_mib, "{:?} -> {:?}", w[0], w[1]);
            assert!(b.vram_mib <= a.vram_mib, "{:?} -> {:?}", w[0], w[1]);
            assert!(b.cpu_centi_pct <= a.cpu_centi_pct, "{:?} -> {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn capability_never_grows_as_the_tier_falls() {
        for w in LADDER.windows(2) {
            let (a, b) = (policy(w[0]), policy(w[1]));
            assert!(!b.speak || a.speak, "{:?} -> {:?}", w[0], w[1]);
            assert!(!b.listen || a.listen, "{:?} -> {:?}", w[0], w[1]);
            assert!(!b.wake_word || a.wake_word, "{:?} -> {:?}", w[0], w[1]);
            assert!(!b.dgpu || a.dgpu, "{:?} -> {:?}", w[0], w[1]);
            assert!(!b.duck || a.duck, "{:?} -> {:?}", w[0], w[1]);
            assert!(b.drive_fps <= a.drive_fps, "{:?} -> {:?}", w[0], w[1]);
            assert!(b.lookahead_ms <= a.lookahead_ms, "{:?} -> {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn the_lip_sync_rate_matches_the_rate_the_rig_is_actually_drawn_at() {
        // Driving a mouth at 60 Hz for a rig rendered at 15 is pure waste.
        for t in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            assert_eq!(policy(t).drive_fps, t.target_fps(), "{t:?}");
        }
    }

    #[test]
    fn a_pack_ceiling_lines_up_with_the_pack_selection_rule() {
        use crate::voices::VoiceRegistry;
        let reg = VoiceRegistry::builtin();
        for t in LADDER {
            let p = policy(t);
            match reg.for_tier(t) {
                Some(pack) => {
                    assert!(p.speak, "{t:?} produced a voice but forbids speech");
                    assert!(pack.usable_at(p.voice_ceiling), "{} at {t:?}", pack.id);
                }
                None => assert!(!p.speak, "{t:?} forbids every voice but permits speech"),
            }
        }
    }
}
