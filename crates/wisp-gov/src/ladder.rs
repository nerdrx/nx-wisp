//! **F60 — the tier ladder.** SPEC §3.1.
//!
//! Two pieces, deliberately separated:
//!
//! * [`classify`] is a *pure function* — snapshot in, verdict out. No clock, no
//!   I/O, no memory. Given the same snapshot it always says the same thing.
//! * [`Ladder`] adds the only stateful thing the governor has: **asymmetric
//!   hysteresis**. Downgrades are applied the instant they are justified.
//!   Upgrades must be justified continuously for a dwell time first.
//!
//! That asymmetry is the whole design. Being wrong in the expensive direction
//! costs the operator a dropped frame in a headset; being wrong in the cheap
//! direction costs her a second of dumbness. Only one of those is acceptable.

use wisp_proto::{Tier, TierReason};

use crate::{
    config::GovConfig,
    reading::{Snapshot, VrRuntimeKind},
    Millis,
};

/// A tier, the SPEC-defined reason for it, and the sentence the operator reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub tier: Tier,
    pub reason: TierReason,
    /// Live human-readable readout, e.g. `"T3 because WiVRn is streaming"`.
    pub explanation: String,
}

impl Verdict {
    fn new(tier: Tier, reason: TierReason, because: impl AsRef<str>) -> Self {
        Verdict {
            explanation: format!("T{} because {}", tier as u8, because.as_ref()),
            tier,
            reason,
        }
    }
}

/// Emitted **only** when the tier actually changes. A change of *reason* at the
/// same tier updates the readout and is not an event — SPEC §3.2 events are
/// facts about the past, and "still T3, now for a different reason" is not one
/// the flight recorder needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierChange {
    pub from: Tier,
    pub to: Tier,
    pub reason: TierReason,
    pub explanation: String,
    pub at: Millis,
}

impl TierChange {
    /// True when this change sheds work, and therefore must be applied to every
    /// [`wisp_proto::Governed`] synchronously before we return (SPEC §3.1).
    pub fn is_downgrade(&self) -> bool {
        self.to > self.from
    }
    pub fn into_event_kind(self) -> wisp_proto::EventKind {
        wisp_proto::EventKind::TierChanged {
            from: self.from,
            to: self.to,
            reason: self.reason,
        }
    }
}

// ---------------------------------------------------------------------------
// The pure part
// ---------------------------------------------------------------------------

/// Decide the tier this snapshot justifies, ignoring history and pinning.
///
/// `prev` is passed only so the two-level [`crate::config::Band`] thresholds
/// know which side of the band they are on; the function is still pure and
/// still has no memory of its own.
///
/// Severity order, most severe first. The first rule that fires wins, so a
/// WiVRn session beats GPU pressure beats "the machine is quiet".
pub fn classify(s: &Snapshot, c: &GovConfig, prev: Tier) -> Verdict {
    // ---- T4 Dormant: emergencies only. -----------------------------------
    if let Some(t) = s.power.hottest_c() {
        if t >= c.temp_critical_c {
            return Verdict::new(
                Tier::Dormant,
                TierReason::PowerCritical,
                format!("the machine is at {t} \u{b0}C and that is an emergency"),
            );
        }
    }
    if s.power.battery_discharging {
        if let Some(pct) = s.power.battery_pct {
            if pct <= c.battery_critical_pct {
                return Verdict::new(
                    Tier::Dormant,
                    TierReason::PowerCritical,
                    format!("the battery is at {pct}% and still draining"),
                );
            }
        }
    }

    // ---- T3 Lobotomised: somebody else owns the GPU. ----------------------
    if let Some(vr) = &s.procs.vr {
        let verb = if vr.streaming {
            "is streaming"
        } else {
            "is running"
        };
        let name = match vr.kind {
            VrRuntimeKind::Other => vr.proc_name.clone(),
            k => k.display().to_string(),
        };
        return Verdict::new(
            Tier::Lobotomised,
            TierReason::VrSession,
            format!("{name} {verb}"),
        );
    }
    if let Some(g) = &s.procs.game {
        return Verdict::new(
            Tier::Lobotomised,
            TierReason::HeavyProcess {
                name: g.name.clone(),
            },
            format!("{} is running", g.name),
        );
    }
    if let Some(fs) = &s.surface.fullscreen {
        let app = if fs.app_id.is_empty() {
            fs.title.clone()
        } else {
            fs.app_id.clone()
        };
        let shown = if app.is_empty() {
            "something".to_string()
        } else {
            app.clone()
        };
        return Verdict::new(
            Tier::Lobotomised,
            TierReason::Fullscreen { app_id: app },
            format!("{shown} is fullscreen"),
        );
    }

    // ---- T2 Reduced: the machine is busy but nobody has claimed it. -------
    let pressured = prev >= Tier::Reduced;

    if let Some(gpu) = s.primary() {
        // VRAM first: running the discrete card out of memory is the failure
        // mode that actually stutters a game, and it is the one F13 exists for.
        let used_frac = if gpu.vram_total_mib == 0 {
            0.0
        } else {
            gpu.vram_used_mib as f32 / gpu.vram_total_mib as f32
        };
        let frac_tripped = c.vram_used_frac.tripped(used_frac, pressured);
        let headroom_tripped = gpu.vram_total_mib > 0 && gpu.vram_free_mib() < c.vram_headroom_mib;
        if frac_tripped || headroom_tripped {
            return Verdict::new(
                Tier::Reduced,
                TierReason::VramPressure {
                    used_mib: gpu.vram_used_mib,
                    total_mib: gpu.vram_total_mib,
                },
                format!(
                    "VRAM is nearly full ({} of {} MiB used)",
                    gpu.vram_used_mib, gpu.vram_total_mib
                ),
            );
        }

        if c.gpu_busy.tripped(gpu.busy_pct as f32, pressured) {
            return Verdict::new(
                Tier::Reduced,
                TierReason::GpuPressure {
                    busy_pct: gpu.busy_pct,
                },
                format!("the GPU is {}% busy", gpu.busy_pct),
            );
        }
    }

    // CPU and memory pressure. SPEC's `TierReason` has no variant for either,
    // so we name the process responsible instead of inventing a reason — see
    // the crate docs' note on the spec gap.
    let cpu_tripped = c
        .cpu_load_per_core
        .tripped(s.cpu.load_per_core(), pressured)
        || c.cpu_psi.tripped(s.cpu.psi_some_avg10, pressured);
    let mem_tripped = c.mem_psi.tripped(s.mem.psi_some_avg10, pressured);
    if cpu_tripped || mem_tripped {
        let what = if mem_tripped {
            "the machine is short of memory".to_string()
        } else {
            match &s.procs.top_cpu {
                Some(p) => format!("{} is eating the CPU", p.name),
                None => "the CPU is busy".to_string(),
            }
        };
        let name = s
            .procs
            .top_cpu
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| {
                if mem_tripped {
                    "memory pressure".to_string()
                } else {
                    "cpu pressure".to_string()
                }
            });
        return Verdict::new(Tier::Reduced, TierReason::HeavyProcess { name }, what);
    }

    // Running on a low battery is not an emergency, but it is not a licence to
    // hold a 20 GiB model either.
    if s.power.battery_discharging {
        if let Some(pct) = s.power.battery_pct {
            if pct <= c.battery_frugal_pct {
                return Verdict::new(
                    Tier::Reduced,
                    TierReason::PowerCritical,
                    format!("the battery is at {pct}% and she is being frugal"),
                );
            }
        }
    }

    // ---- T0 / T1: the machine is quiet. ----------------------------------
    let away = s.operator.locked || s.operator.idle_ms >= c.away_after_ms;
    if away {
        let why = if s.operator.locked {
            "the session is locked and the machine is idle".to_string()
        } else {
            format!(
                "you have been away {} and the machine is idle",
                human_duration(s.operator.idle_ms)
            )
        };
        return Verdict::new(Tier::Feral, TierReason::Idle, why);
    }
    Verdict::new(
        Tier::Full,
        TierReason::Idle,
        "you are at the desk and nothing heavy is running",
    )
}

fn human_duration(ms: Millis) -> String {
    let secs = ms / 1000;
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5400 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

// ---------------------------------------------------------------------------
// The stateful part
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    /// The *least capable* upgrade candidate seen since `since`. Taking the
    /// worst of the window is what stops a metric that dips for one sample
    /// from buying a full upgrade.
    verdict: Verdict,
    since: Millis,
}

/// The tier ladder with hysteresis, pinning and a live readout.
#[derive(Debug, Clone)]
pub struct Ladder {
    cfg: GovConfig,
    current: Verdict,
    /// Monotonic ms at which the current tier was entered.
    entered_at: Millis,
    pin: Option<Tier>,
    pending: Option<Pending>,
}

impl Ladder {
    /// Starts at T1 Full: the safe assumption at launch is that the operator is
    /// sitting there and has not started anything yet. The first [`step`] with
    /// a real snapshot corrects it, and because that correction is a downgrade
    /// it is instant.
    ///
    /// [`step`]: Ladder::step
    pub fn new(cfg: GovConfig) -> Self {
        Ladder {
            cfg,
            current: Verdict::new(
                Tier::Full,
                TierReason::Idle,
                "she just woke up and has not looked around yet",
            ),
            entered_at: 0,
            pin: None,
            pending: None,
        }
    }

    pub fn config(&self) -> &GovConfig {
        &self.cfg
    }
    pub fn tier(&self) -> Tier {
        self.current.tier
    }
    pub fn reason(&self) -> &TierReason {
        &self.current.reason
    }
    /// The sentence the cost meter shows: `"T3 because WiVRn is streaming"`.
    pub fn explanation(&self) -> &str {
        &self.current.explanation
    }
    pub fn verdict(&self) -> &Verdict {
        &self.current
    }
    pub fn pinned(&self) -> Option<Tier> {
        self.pin
    }
    /// Monotonic ms the current tier has been held, given `now`.
    pub fn held_for(&self, now: Millis) -> Millis {
        now.saturating_sub(self.entered_at)
    }
    /// The tier we are currently climbing towards, and since when. `None` when
    /// the machine agrees with where she already is.
    pub fn pending(&self) -> Option<(Tier, Millis)> {
        self.pending.as_ref().map(|p| (p.verdict.tier, p.since))
    }

    /// Pin the tier by hand. Applies **immediately in both directions** — the
    /// operator asked, so no dwell — and overrides every probe until cleared.
    pub fn pin(&mut self, tier: Tier, now: Millis) -> Option<TierChange> {
        self.pin = Some(tier);
        self.pending = None;
        let v = Verdict::new(tier, TierReason::Pinned, "you pinned her there");
        self.commit(v, now)
    }

    /// Release the pin. The next [`Ladder::step`] re-derives the tier from the
    /// machine; if that is a downgrade it lands instantly, if it is an upgrade
    /// it has to earn its dwell like any other.
    pub fn unpin(&mut self) {
        self.pin = None;
        self.pending = None;
    }

    /// Feed one snapshot. Returns `Some` **iff the tier actually changed**.
    pub fn step(&mut self, s: &Snapshot) -> Option<TierChange> {
        let now = s.at;

        if let Some(t) = self.pin {
            // Pinning overrides everything, every step, forever.
            if self.current.tier == t && matches!(self.current.reason, TierReason::Pinned) {
                return None;
            }
            let v = Verdict::new(t, TierReason::Pinned, "you pinned her there");
            return self.commit(v, now);
        }

        let cand = classify(s, &self.cfg, self.current.tier);

        // Equal tier: keep the readout honest (the reason may have changed from
        // "GPU is busy" to "a game is running") but emit nothing, and abandon
        // any climb, because the machine no longer justifies one.
        if cand.tier == self.current.tier {
            self.pending = None;
            self.current = cand;
            return None;
        }

        // Less capable than where we are: shed now. This is SPEC §3.1's
        // "synchronously and immediately".
        if cand.tier > self.current.tier {
            self.pending = None;
            return self.commit(cand, now);
        }

        // More capable: it has to hold.
        match &mut self.pending {
            Some(p) => {
                // Never get greedier mid-window: if this sample justifies less
                // than the window's running candidate, the window settles for
                // less but the clock keeps running. If it justifies more, we
                // ignore the improvement until the current climb lands.
                if cand.tier > p.verdict.tier {
                    p.verdict = cand;
                }
            }
            None => {
                self.pending = Some(Pending {
                    verdict: cand,
                    since: now,
                });
            }
        }

        let p = self.pending.as_ref().expect("just set");
        // Dwell is charged for the tier we would actually land on, which after
        // the settling above may be less capable than this sample's candidate.
        let dwell = self.cfg.dwell.for_tier(p.verdict.tier);
        if now.saturating_sub(p.since) >= dwell {
            let v = self.pending.take().expect("just checked").verdict;
            // The window may have settled on the tier we are already at.
            if v.tier == self.current.tier {
                self.current = v;
                return None;
            }
            return self.commit(v, now);
        }
        None
    }

    fn commit(&mut self, v: Verdict, now: Millis) -> Option<TierChange> {
        let from = self.current.tier;
        let to = v.tier;
        let change = TierChange {
            from,
            to,
            reason: v.reason.clone(),
            explanation: v.explanation.clone(),
            at: now,
        };
        self.current = v;
        if from == to {
            return None;
        }
        self.entered_at = now;
        Some(change)
    }
}
