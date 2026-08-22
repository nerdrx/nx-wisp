//! `wisp-fleet` — she belongs to a suite of apps, and this is the part of her
//! that knows it. SPEC.md §2; plan items **F44–F47**.
//!
//! | | |
//! |---|---|
//! | **F44** | [`connector`] — the NX Connector bus client, and [`roster`], which is how she sees everyone else |
//! | **F45** | [`narrate`] — fleet events become [`Utterance`]s, from a rule *file*, not from `match` arms |
//! | **F46** | [`tools`] — descriptors wrapping the existing `nx` CLI, for `wisp-mind` to register |
//! | **F47** | [`hop`] — she walks off one machine's screen and appears on another |
//!
//! # The three things this crate refuses to get wrong
//!
//! 1. **She works alone.** No NX Hub is the normal case, not an error. Nothing
//!    here logs a warning, blocks startup, or reaches the operator because the
//!    hub is absent.
//! 2. **≤ 1 status per second, change-only.** The bus drops faster traffic
//!    *silently*; that is how an app ends up reported as connected forever.
//! 3. **Exactly one machine has her.** [`hop::Presence`] is a pure state
//!    machine so that invariant can be asserted after every step of a test.
//!
//! # What the binary still has to wire
//!
//! [`Fleet`] owns the bus client, the roster watcher and the narrator, and
//! hands the tool descriptors to `wisp-mind`. It deliberately does **not** own
//! the hop: [`hop::Hopper`] needs a node id (NX Hub's fleet id), a state file
//! under the config dir, and somebody to feed it [`BusEvent::Relay`] and to act
//! on its `Wake`/`Sleep` effects — all of which are the binary's business, and
//! none of which can do anything useful until NX Hub grows the relay verb
//! described in [`hop`].
//!
//! # Network
//!
//! SPEC §0.2 allows exactly three kinds of egress, and this crate uses one of
//! them: the NX Connector / fleet bus on the LAN. The connector socket is
//! loopback-only; the fleet hop travels over NX Hub's already-authenticated
//! session and never opens one of its own. No telemetry, ever.

pub mod connector;
pub mod error;
pub mod hop;
pub mod hub;
pub mod narrate;
pub mod roster;
pub mod status;
pub mod tools;
pub mod ws;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use wisp_proto::{Cost, Governed, Observation, Tier, TierReason, Urgency, Utterance};

pub use connector::{BusEvent, ConnectorConfig, ConnectorHandle, Dialer, TcpDialer};
pub use error::{FleetError, Result};
pub use hop::{Carry, HopMessage, HopTransport, Hopper, Presence};
pub use narrate::{Narrator, RuleSet};
pub use status::{Field, Fields, Throttle};
pub use tools::{NxTools, ToolDescriptor, ToolInvocation, ToolOutcome};

/// Everything that happens out in the fleet, as facts about the past. The
/// binary records these and turns them into `wisp_proto::Event`s; this crate
/// never speaks to the operator directly (SPEC §3.4).
#[derive(Debug, Clone, PartialEq)]
pub enum FleetEvent {
    /// Bus-level news about *us*.
    Bus(BusEvent),
    /// Another NX app's status changed.
    Observed(Observation),
    /// …and what she would like to say about it. Still only a proposal:
    /// `wisp-attn` holds the token bucket and decides.
    Says(Utterance),
}

#[derive(Debug, Clone)]
pub struct FleetConfig {
    pub connector: ConnectorConfig,
    /// Where the hub mirrors its client list. Polled, because the bus has no
    /// subscribe verb — see [`roster`].
    pub snapshot: std::path::PathBuf,
    /// How often to look. The hub debounces its own writes to ~1/s.
    pub roster_poll: Duration,
    /// Operator's rule override; falls back to the authored defaults.
    pub rules_path: Option<std::path::PathBuf>,
    /// `~/.local/bin/nx`.
    pub nx_binary: std::path::PathBuf,
}

impl Default for FleetConfig {
    fn default() -> Self {
        let data = hub::data_dir();
        Self {
            connector: ConnectorConfig::default(),
            snapshot: hub::snapshot_path(&data),
            roster_poll: Duration::from_secs(2),
            rules_path: config_dir().map(|d| d.join("fleet-rules.json")),
            nx_binary: hub::nx_binary(),
        }
    }
}

/// `$NX_WISP_CONFIG_DIR`, else `~/.config/nx-wisp`. Tests **must** set the
/// environment variable (SPEC §4) — the dev build and the installed copy
/// otherwise share state.
pub fn config_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("NX_WISP_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir));
        }
    }
    Some(hub::home().join(".config/nx-wisp"))
}

/// The running subsystem: bus client, roster watcher, narrator, tools.
pub struct Fleet {
    connector: ConnectorHandle,
    tools: NxTools,
    tier: Arc<AtomicU8>,
    inject: mpsc::UnboundedSender<Observation>,
}

impl Fleet {
    /// Start her fleet presence. Never fails and never blocks: if there is no
    /// hub, every part of this quietly does nothing until there is one.
    pub fn spawn(cfg: FleetConfig) -> (Self, mpsc::UnboundedReceiver<FleetEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let tier = Arc::new(AtomicU8::new(Tier::Full as u8));

        // The bus client.
        let (bus_tx, mut bus_rx) = mpsc::unbounded_channel();
        let dialer =
            TcpDialer { host: cfg.connector.host.clone(), port: cfg.connector.port };
        let connector = connector::spawn(cfg.connector.clone(), dialer, bus_tx);
        {
            let events = events.clone();
            tokio::spawn(async move {
                while let Some(e) = bus_rx.recv().await {
                    if events.send(FleetEvent::Bus(e)).is_err() {
                        break;
                    }
                }
            });
        }

        // The roster watcher and the narrator: one task, because the narrator's
        // state is only ever touched from here.
        let mut watcher = roster::RosterWatcher::new(cfg.snapshot.clone(), &cfg.connector.app);
        let mut narrator = match &cfg.rules_path {
            Some(path) => Narrator::load_or_default(path),
            None => Narrator::default(),
        };
        let poll = cfg.roster_poll;
        let tier_for_task = Arc::clone(&tier);
        let clock = connector::Clock::new();
        let (inject, mut inject_rx) = mpsc::unbounded_channel::<Observation>();
        tokio::spawn(async move {
            loop {
                let batch = tokio::select! {
                    _ = tokio::time::sleep(poll) => {
                        let tier = tier_from_u8(tier_for_task.load(Ordering::Relaxed));
                        // T4 is silence: stop reading the world at all. On the
                        // way back up, forget what we knew so the first poll
                        // re-reports reality instead of replaying a stale diff.
                        if tier == Tier::Dormant {
                            watcher.forget();
                            continue;
                        }
                        watcher.poll()
                    }
                    injected = inject_rx.recv() => match injected {
                        Some(obs) => vec![obs],
                        None => return,
                    },
                };
                let tier = tier_from_u8(tier_for_task.load(Ordering::Relaxed));
                narrator.set_min_urgency(min_urgency_for(tier));
                let now = clock.now_ms();
                for obs in batch {
                    if tier != Tier::Dormant {
                        for utterance in narrator.observe(&obs, now) {
                            if events.send(FleetEvent::Says(utterance)).is_err() {
                                return;
                            }
                        }
                    }
                    if events.send(FleetEvent::Observed(obs)).is_err() {
                        return;
                    }
                }
            }
        });

        (Fleet { connector, tools: NxTools::new(cfg.nx_binary), tier, inject }, rx)
    }

    /// Feed in a fleet fact that did not come from the bus.
    ///
    /// The bus is not the only place fleet news lives, and one case matters
    /// today: **NX Hub is the bus's server, never one of its clients**, so
    /// "there are updates waiting" can never arrive as a client status. Working
    /// out what has an update means the hub's discovery model, and this crate
    /// wraps the fleet rather than reimplementing it (F46) — nor will it poll
    /// GitHub in the background, which SPEC §0.2 does not allow it to do
    /// unasked. So whoever *does* know (the binary, from a hub notification or
    /// from an `nx list` the operator asked for) hands the fact in here, and it
    /// goes through exactly the same rules as everything else:
    ///
    /// ```no_run
    /// # use wisp_proto::Observation;
    /// # fn f(fleet: &wisp_fleet::Fleet) {
    /// fleet.observe(Observation::Fleet {
    ///     app: "nx-hub".into(),
    ///     field: "updates".into(),
    ///     value: "3".into(),
    /// });
    /// # }
    /// ```
    pub fn observe(&self, observation: Observation) {
        let _ = self.inject.send(observation);
    }

    /// Her own card on the hub's dashboard. Throttled and change-only.
    pub fn publish(&self, fields: Fields) {
        self.connector.publish(fields);
    }

    pub fn connector(&self) -> &ConnectorHandle {
        &self.connector
    }

    /// The `nx` CLI tools, for `wisp-mind` to register with the model.
    pub fn tools(&self) -> &NxTools {
        &self.tools
    }

    pub fn tier(&self) -> Tier {
        tier_from_u8(self.tier.load(Ordering::Relaxed))
    }

    /// Say goodbye to the hub and stop reconnecting.
    pub fn close(&self) {
        self.connector.close();
    }
}

/// At T3/T4 she is out of the way of a game or a headset. The bus connection
/// stays — it is one idle socket and a pong every 30 s — but she stops reading
/// the roster at T4 and stops saying anything short of an alarm at T3.
impl Governed for Fleet {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        // Must not block and must not fail: one atomic store.
        self.tier.store(tier as u8, Ordering::Relaxed);
        tracing::debug!(?tier, ?reason, "fleet tier");
    }

    fn cost_at(tier: Tier) -> Cost {
        match tier {
            Tier::Dormant => Cost { ram_mib: 1, vram_mib: 0, cpu_centi_pct: 0 },
            // One socket, one 2 s file poll, one small rule set.
            _ => Cost { ram_mib: 3, vram_mib: 0, cpu_centi_pct: 20 },
        }
    }
}

fn min_urgency_for(tier: Tier) -> Urgency {
    match tier {
        Tier::Feral | Tier::Full => Urgency::Whim,
        Tier::Reduced => Urgency::Notable,
        // A headset is on, or a game owns the GPU. Only an alarm is worth it.
        Tier::Lobotomised | Tier::Dormant => Urgency::Alarm,
    }
}

fn tier_from_u8(v: u8) -> Tier {
    match v {
        0 => Tier::Feral,
        1 => Tier::Full,
        2 => Tier::Reduced,
        3 => Tier::Lobotomised,
        _ => Tier::Dormant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_round_trip_and_gate_the_right_urgencies() {
        for tier in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            assert_eq!(tier_from_u8(tier as u8), tier);
        }
        assert_eq!(min_urgency_for(Tier::Full), Urgency::Whim);
        assert_eq!(min_urgency_for(Tier::Lobotomised), Urgency::Alarm);
    }

    #[test]
    fn she_is_cheap_at_every_tier() {
        for tier in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            let cost = <Fleet as Governed>::cost_at(tier);
            assert_eq!(cost.vram_mib, 0, "the fleet never touches the GPU");
            assert!(cost.ram_mib <= 4);
        }
    }
}
