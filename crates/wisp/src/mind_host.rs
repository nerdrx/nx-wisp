//! Cognition, mounted.
//!
//! `wisp-mind` is a synchronous library: `observe`, `tick`, `take_outbox`.
//! Generation can take whole seconds, so none of it may run on the tokio
//! runtime — the mind gets a **dedicated OS thread** and talks to the rest of
//! the app through one inbox.
//!
//! Two invariants are enforced by the shape of this file rather than by
//! discipline:
//!
//! * **She may not speak directly** (SPEC §3.4). The only thing this host does
//!   with the mind's outbox is hand it to the [`Speaker`], which is the channel
//!   into `wisp-attn`. Nothing here touches the shell.
//! * **Downgrades outrank thought** (SPEC §3.1). The inbox is drained
//!   completely before any thinking happens, and tier messages are applied in
//!   arrival order within the drain — so a `Dormant` that arrives while she is
//!   mid-rumination is applied before the next token is ever sampled.

use std::sync::mpsc;
use std::thread::JoinHandle;

use wisp_mind::Mind;
use wisp_proto::{
    sense::Observation, Event, EventKind, Governed, Millis, Tier, TierReason,
};

use wisp_senses::Clock;

use crate::app::Speaker;
use crate::recorder::Recorder;

/// What the app sends the mind thread.
pub enum MindMsg {
    Obs(Observation, Millis),
    /// Tier plus the governor's device/VRAM verdicts, which always travel
    /// together — applying one without the other is how a model ends up on a
    /// GPU the governor just promised was untouched.
    Tier {
        tier: Tier,
        reason: TierReason,
        devices: wisp_gov::DeviceChoice,
        vram: wisp_gov::VramBudget,
        now: Millis,
    },
    /// The operator spoke to her (a final `Speech` observation).
    Heard(Millis),
    Tick(Millis),
    Shutdown,
}

pub struct MindHost {
    tx: mpsc::Sender<MindMsg>,
    thread: Option<JoinHandle<()>>,
}

/// A cloneable handle for the tasks that feed the mind. The host itself stays
/// owned by `run()`, which is what makes the join on shutdown possible.
#[derive(Clone)]
pub struct MindSender(mpsc::Sender<MindMsg>);

impl MindSender {
    pub fn send(&self, msg: MindMsg) {
        let _ = self.0.send(msg);
    }
}

impl MindHost {
    /// Build the mind and put it on its thread.
    ///
    /// The backend is feature-selected: with `mind-llama` this is llama.cpp on
    /// Vulkan; without it, the deterministic mock — which, unscripted, refuses
    /// to generate rather than inventing placeholder chatter, so a default
    /// build stays a creature without language instead of a creature that
    /// spouts mock strings.
    pub fn spawn(
        dir: &std::path::Path,
        speaker: Speaker,
        recorder: std::sync::Arc<Recorder>,
        bus: tokio::sync::broadcast::Sender<Event>,
        clock: Clock,
    ) -> anyhow::Result<MindHost> {
        #[cfg(feature = "mind-llama")]
        let backend: Box<dyn wisp_mind::backend::Backend> =
            Box::new(wisp_mind::backend::llama::LlamaBackend::new());
        #[cfg(not(feature = "mind-llama"))]
        let backend: Box<dyn wisp_mind::backend::Backend> =
            Box::new(wisp_mind::backend::mock::MockBackend::new());

        // The operator's config file has the same shape minus `embed` (three
        // roles, two named — a gap the mind's report flagged); map what exists
        // and let the registry's default cover the third.
        let cfg = crate::config::load_from(dir).config;
        let settings = wisp_mind::manager::ModelSettings {
            models_dir: cfg.model.models_dir.clone(),
            reflex: cfg.model.reflex.clone(),
            deliberate: cfg.model.deliberate.clone(),
            context_tokens: cfg.model.context_tokens,
            gpu_layers: cfg.model.gpu_layers,
            temperature: cfg.model.temperature,
            max_tokens: cfg.model.max_tokens,
            allow_downloads: cfg.model.allow_downloads,
            registry: cfg.model.registry.clone(),
            ..Default::default()
        };

        // The mind's own events (tool calls, model loads, deferrals) are facts,
        // so they go through the recorder onto the bus like everything else.
        // The closure runs ON the mind thread; the recorder and bus are both
        // fine with that.
        let sink_recorder = recorder.clone();
        let sink_bus = bus.clone();
        let sink_clock = clock.clone();
        let events = wisp_mind::events::EventSink::new(move |kind: EventKind| {
            let ev = Event { at: sink_clock.now(), kind };
            sink_recorder.record(&ev);
            let _ = sink_bus.send(ev);
        });

        let mut mind = Mind::builder(backend)
            .settings(settings)
            .events(events)
            .tool_state_file(dir.join("tools.json"))
            .build()?;

        // The nx CLI wrappers are already tools; register them rather than
        // rewriting them (SPEC amendment pending on where ToolDescriptor
        // lives, but the registration itself is uncontroversial).
        mind.add_tools(wisp_fleet::tools::NxTools::default().descriptors());

        let (tx, rx) = mpsc::channel::<MindMsg>();
        let thread = std::thread::Builder::new()
            .name("wisp-mind".into())
            .spawn(move || run(mind, rx, speaker))
            .expect("spawning the mind thread");

        Ok(MindHost { tx, thread: Some(thread) })
    }

    pub fn send(&self, msg: MindMsg) {
        let _ = self.tx.send(msg);
    }

    pub fn sender(&self) -> MindSender {
        MindSender(self.tx.clone())
    }

    pub fn shutdown(mut self) {
        let _ = self.tx.send(MindMsg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(mut mind: Mind, rx: mpsc::Receiver<MindMsg>, speaker: Speaker) {
    loop {
        // Block for the first message, then drain everything that is waiting.
        // The drain is what makes a downgrade outrank a backlog of chatter: a
        // Tier message deep in the queue is still applied before the tick that
        // follows the drain does any thinking.
        let first = match rx.recv_timeout(std::time::Duration::from_millis(1000)) {
            Ok(m) => Some(m),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let mut last_now: Millis = 0;
        let mut ticked = false;
        for msg in first.into_iter().chain(rx.try_iter()) {
            match msg {
                MindMsg::Obs(obs, now) => {
                    last_now = last_now.max(now);
                    mind.observe(&obs, now);
                }
                MindMsg::Tier { tier, reason, devices, vram, now } => {
                    last_now = last_now.max(now);
                    mind.set_tier(tier, &reason);
                    mind.apply_governor(devices, vram);
                }
                MindMsg::Heard(now) => {
                    last_now = last_now.max(now);
                    mind.heard(now);
                }
                MindMsg::Tick(now) => {
                    last_now = last_now.max(now);
                    ticked = true;
                }
                MindMsg::Shutdown => return,
            }
        }
        if ticked && last_now > 0 {
            // This is where generation happens, and where seconds can pass.
            // Everything above already landed, so a downgrade that arrived
            // with this batch has been applied before the first token.
            mind.tick(last_now);
        }
        // SPEC §3.4: the outbox goes to the attention budget and nowhere else.
        for u in mind.take_outbox() {
            speaker.propose(u);
        }
    }
}
