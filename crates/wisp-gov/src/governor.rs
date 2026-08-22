//! The control loop: probe, classify, apply, account.
//!
//! One [`Governor::step`] is the whole of the governor's behaviour. It is
//! deliberately synchronous and deliberately does no I/O of its own beyond the
//! probes, so the binary can call it from a timer on the main thread without
//! ever risking the frame budget.

use wisp_proto::{Event, EventKind, Tier};

use crate::{
    accounting::{self, CostReport, FreeThresholds},
    config::GovConfig,
    device::{self, DeviceChoice},
    ladder::{Ladder, TierChange},
    probe::{selfcost::SelfCostProbe, Probes, SnapshotSource},
    reading::Snapshot,
    registry::Registry,
    vram::{self, VramBudget},
};

/// The result of one poll. Everything the rest of the program needs to know.
#[derive(Debug, Clone)]
pub struct Step {
    pub snapshot: Snapshot,
    /// `Some` **iff the tier actually changed**. This is the only thing that
    /// becomes an [`EventKind::TierChanged`].
    pub change: Option<TierChange>,
    pub tier: Tier,
    /// `"T3 because WiVRn is streaming"`.
    pub explanation: String,
    pub devices: DeviceChoice,
    pub vram: VramBudget,
    pub cost: CostReport,
}

impl Step {
    /// The flight-recorder event, if there is one.
    pub fn event(&self) -> Option<Event> {
        self.change.as_ref().map(|c| Event {
            at: c.at,
            kind: EventKind::TierChanged {
                from: c.from,
                to: c.to,
                reason: c.reason.clone(),
            },
        })
    }
}

/// The governor. Owns the ladder, the registry and the probes.
pub struct Governor {
    ladder: Ladder,
    registry: Registry,
    source: Box<dyn SnapshotSource + Send>,
    selfcost: SelfCostProbe,
    free: FreeThresholds,
    last_vram: Option<VramBudget>,
}

impl Governor {
    /// Reading the real machine.
    pub fn real(cfg: GovConfig) -> Self {
        let probes = Probes::real(&cfg);
        Governor::with_source(cfg, Box::new(probes))
    }

    /// Reading whatever you hand it. This is how the ladder is tested.
    pub fn with_source(cfg: GovConfig, source: Box<dyn SnapshotSource + Send>) -> Self {
        Governor {
            ladder: Ladder::new(cfg),
            registry: Registry::new(),
            source,
            selfcost: SelfCostProbe::default(),
            free: FreeThresholds::default(),
            last_vram: None,
        }
    }

    /// Point the self-cost probe somewhere other than `/proc/self`. Tests only.
    pub fn with_selfcost(mut self, probe: SelfCostProbe) -> Self {
        self.selfcost = probe;
        self
    }

    pub fn registry(&mut self) -> &mut Registry {
        &mut self.registry
    }
    pub fn ladder(&self) -> &Ladder {
        &self.ladder
    }
    pub fn tier(&self) -> Tier {
        self.ladder.tier()
    }
    pub fn explanation(&self) -> &str {
        self.ladder.explanation()
    }

    /// How long the caller should wait before calling [`Governor::step`] again.
    ///
    /// Polling costs about 4 ms of CPU on the operator's desktop, so the
    /// governor deliberately slows down at the tiers where being a second late
    /// is harmless. It never delays a *downgrade*: a fullscreen surface and a
    /// VR session both arrive as pushed events on the hooks in
    /// [`crate::probe::surface`], and the binary should step immediately when
    /// one of those fires rather than waiting out this interval.
    pub fn poll_interval_ms(&self) -> crate::Millis {
        self.ladder.config().cadence.for_tier(self.ladder.tier())
    }

    /// Pin the tier by hand. Applies immediately and is broadcast immediately.
    pub fn pin(&mut self, tier: Tier, now: crate::Millis) -> Option<TierChange> {
        let change = self.ladder.pin(tier, now);
        if let Some(c) = &change {
            self.registry.apply(c.from, c.to, &c.reason);
        }
        change
    }

    pub fn unpin(&mut self) {
        self.ladder.unpin();
    }

    pub fn pinned(&self) -> Option<Tier> {
        self.ladder.pinned()
    }

    /// One turn of the loop.
    ///
    /// The ordering is not incidental. The tier change is broadcast to every
    /// [`wisp_proto::Governed`] **before** this function returns and before any
    /// accounting is done, so that by the time anything else observes the new
    /// tier the VRAM has already been freed (SPEC §3.1).
    pub fn step(&mut self) -> Step {
        let snapshot = self.source.snapshot();
        let change = self.ladder.step(&snapshot);

        if let Some(c) = &change {
            if c.is_downgrade() {
                // Synchronous, immediate, infallible. F13's eviction happens
                // inside this call.
                self.registry.downgrade(c.to, &c.reason);
                if let Some(why) = vram::eviction_reason(&c.reason) {
                    tracing::info!(from = ?c.from, to = ?c.to, why, "shedding now");
                }
            } else {
                self.registry.upgrade(c.to, &c.reason);
            }
        }

        let tier = self.ladder.tier();
        let cfg = self.ladder.config().clone();
        let devices = device::select_for(tier, &snapshot, &cfg);
        let measured = self.selfcost.read();

        let ours_dgpu = snapshot
            .discrete()
            .map(|g| measured.vram_on(&g.id.pci_slot))
            .unwrap_or(0);
        let vram = vram::budget(tier, &snapshot, &cfg, ours_dgpu, self.last_vram.as_ref());
        self.last_vram = Some(vram.clone());

        let cost = accounting::report(
            tier,
            self.ladder.explanation(),
            self.registry.estimate(tier),
            measured,
            &devices,
            &snapshot,
            self.free,
        );

        Step {
            explanation: self.ladder.explanation().to_string(),
            tier,
            change,
            devices,
            vram,
            cost,
            snapshot,
        }
    }
}
