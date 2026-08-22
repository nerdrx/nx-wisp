//! F8 — idle, from `ext_idle_notifier_v1`. Event driven: the compositor tells
//! us, we never poll. KWin 6.7 advertises this at version 2.
//!
//! Wayland is a blocking, non-`Send` world and tokio is not, so the Wayland
//! event loop lives on its own thread and the two talk over a channel. The
//! thread is idle-blocked in `poll()` the whole time it is not being told
//! something, which is the cheapest a sense can be (SPEC §0.1).

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wisp_proto::{Observation, SenseId};

use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

pub struct IdleSense {
    /// Thresholds, shortest first. The compositor gives us one notification per
    /// threshold, so "she notices at 30s, and again at 5min" costs nothing extra.
    pub thresholds_ms: Vec<u32>,
}

impl Default for IdleSense {
    fn default() -> Self {
        IdleSense { thresholds_ms: vec![30_000, 300_000, 1_800_000] }
    }
}

impl Sense for IdleSense {
    const ID: SenseId = SenseId::Idle;
    const LABEL: &'static str = crate::consent::label_of(SenseId::Idle);
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::Idle);
}

/// What the Wayland thread reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// A threshold elapsed with no input.
    Idled { threshold_ms: u32 },
    /// Input happened; the operator is back.
    Resumed { threshold_ms: u32 },
}

/// Turns raw compositor notifications into the `Observation::Idle` stream.
///
/// Several thresholds fire independently, so this collapses them: she is idle
/// once, for a duration that only grows, and comes back once. Pure, so the
/// collapsing rules are tested without a compositor.
#[derive(Debug, Default)]
pub struct IdleTracker {
    idle: bool,
    /// Longest threshold currently elapsed.
    deepest_ms: u32,
}

impl IdleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_idle(&self) -> bool {
        self.idle
    }

    pub fn apply(&mut self, ev: IdleEvent) -> Option<Observation> {
        match ev {
            IdleEvent::Idled { threshold_ms } => {
                if self.idle && threshold_ms <= self.deepest_ms {
                    return None;
                }
                self.idle = true;
                self.deepest_ms = threshold_ms;
                Some(Observation::Idle { idle: true, for_ms: threshold_ms as u64 })
            }
            IdleEvent::Resumed { .. } => {
                if !self.idle {
                    // Shorter thresholds resume too; only the first counts.
                    return None;
                }
                let was = self.deepest_ms;
                self.idle = false;
                self.deepest_ms = 0;
                Some(Observation::Idle { idle: false, for_ms: was as u64 })
            }
        }
    }
}

impl SensePlugin for IdleSense {
    fn spawn(self, handle: SenseHandle<Self>, mut ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // A tokio sender is usable from a plain thread, which lets the
            // Wayland side stay blocking without the async side ever polling.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IdleEvent>();
            let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
            let thresholds = self.thresholds_ms.clone();

            let worker = std::thread::Builder::new()
                .name("wisp-idle".into())
                .spawn(move || {
                    if let Err(e) = wayland_thread(&thresholds, tx, stop_rx) {
                        tracing::warn!(error = %e, "idle sense stopped");
                    }
                })
                .ok();

            let mut tracker = IdleTracker::new();
            loop {
                tokio::select! {
                    biased;
                    _ = ctx.shutdown.wait() => break,
                    ev = rx.recv() => match ev {
                        Some(ev) => {
                            if let Some(obs) = tracker.apply(ev) {
                                handle.emit(obs);
                            }
                        }
                        None => break,
                    },
                }
            }
            let _ = stop_tx.send(());
            if let Some(w) = worker {
                let _ = tokio::task::spawn_blocking(move || w.join()).await;
            }
        })
    }
}

// ---------------------------------------------------------------------------
// The Wayland side
// ---------------------------------------------------------------------------

struct IdleState {
    tx: tokio::sync::mpsc::UnboundedSender<IdleEvent>,
    /// notification proxy id -> threshold
    thresholds: Vec<(ExtIdleNotificationV1, u32)>,
}

impl Dispatch<ExtIdleNotificationV1, u32> for IdleState {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        &threshold_ms: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let ev = match event {
            ext_idle_notification_v1::Event::Idled => IdleEvent::Idled { threshold_ms },
            ext_idle_notification_v1::Event::Resumed => IdleEvent::Resumed { threshold_ms },
            _ => return,
        };
        let _ = state.tx.send(ev);
    }
}

wayland_client::delegate_noop!(IdleState: ignore ExtIdleNotifierV1);
wayland_client::delegate_noop!(IdleState: ignore WlSeat);

/// We bind what we need up front and never react to a global arriving or going
/// away, so the registry itself has nothing to tell us.
impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, GlobalListContents> for IdleState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn wayland_thread(
    thresholds_ms: &[u32],
    tx: tokio::sync::mpsc::UnboundedSender<IdleEvent>,
    stop: std_mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue): (GlobalList, _) = registry_queue_init::<IdleState>(&conn)?;
    let qh = queue.handle();

    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| anyhow::anyhow!("ext_idle_notifier_v1 is not available: {e}"))?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| anyhow::anyhow!("no wl_seat: {e}"))?;

    let mut state = IdleState { tx, thresholds: Vec::new() };
    for &ms in thresholds_ms {
        let n = notifier.get_idle_notification(ms, &seat, &qh, ms);
        state.thresholds.push((n, ms));
    }
    queue.roundtrip(&mut state)?;

    // Block on the Wayland fd, waking only for real events or the stop signal.
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        queue.flush()?;
        let guard = match queue.prepare_read() {
            Some(g) => g,
            None => {
                queue.dispatch_pending(&mut state)?;
                continue;
            }
        };
        let fd = {
            use std::os::fd::{AsFd, AsRawFd};
            conn.as_fd().as_raw_fd()
        };
        match poll_readable(fd, Duration::from_millis(500)) {
            Ok(true) => {
                let _ = guard.read();
                queue.dispatch_pending(&mut state)?;
            }
            Ok(false) => drop(guard),
            Err(e) => {
                drop(guard);
                return Err(e);
            }
        }
    }
    Ok(())
}

/// A bounded `poll(2)` so shutdown is honoured promptly without burning a core.
pub(crate) fn poll_readable(fd: std::os::fd::RawFd, timeout: Duration) -> anyhow::Result<bool> {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let n = unsafe { libc::poll(&mut pfd, 1, ms) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(err.into());
    }
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_threshold_produces_one_idle() {
        let mut t = IdleTracker::new();
        assert_eq!(
            t.apply(IdleEvent::Idled { threshold_ms: 30_000 }),
            Some(Observation::Idle { idle: true, for_ms: 30_000 })
        );
        assert!(t.is_idle());
    }

    #[test]
    fn deeper_thresholds_deepen_but_shallower_ones_are_silent() {
        let mut t = IdleTracker::new();
        t.apply(IdleEvent::Idled { threshold_ms: 30_000 });
        assert_eq!(
            t.apply(IdleEvent::Idled { threshold_ms: 300_000 }),
            Some(Observation::Idle { idle: true, for_ms: 300_000 })
        );
        // The compositor may re-deliver a shallower one; it says nothing new.
        assert_eq!(t.apply(IdleEvent::Idled { threshold_ms: 30_000 }), None);
    }

    #[test]
    fn resume_reports_how_long_she_was_away_and_only_once() {
        let mut t = IdleTracker::new();
        t.apply(IdleEvent::Idled { threshold_ms: 30_000 });
        t.apply(IdleEvent::Idled { threshold_ms: 300_000 });
        assert_eq!(
            t.apply(IdleEvent::Resumed { threshold_ms: 30_000 }),
            Some(Observation::Idle { idle: false, for_ms: 300_000 })
        );
        // Every other threshold resumes too. Silence.
        assert_eq!(t.apply(IdleEvent::Resumed { threshold_ms: 300_000 }), None);
        assert!(!t.is_idle());
    }

    #[test]
    fn resume_without_idle_is_silent() {
        let mut t = IdleTracker::new();
        assert_eq!(t.apply(IdleEvent::Resumed { threshold_ms: 30_000 }), None);
    }

    #[test]
    fn a_full_away_and_back_cycle() {
        let mut t = IdleTracker::new();
        let script = [
            IdleEvent::Idled { threshold_ms: 30_000 },
            IdleEvent::Idled { threshold_ms: 300_000 },
            IdleEvent::Idled { threshold_ms: 1_800_000 },
            IdleEvent::Resumed { threshold_ms: 1_800_000 },
            IdleEvent::Resumed { threshold_ms: 300_000 },
            IdleEvent::Resumed { threshold_ms: 30_000 },
            IdleEvent::Idled { threshold_ms: 30_000 },
        ];
        let out: Vec<_> = script.iter().filter_map(|&e| t.apply(e)).collect();
        assert_eq!(
            out,
            vec![
                Observation::Idle { idle: true, for_ms: 30_000 },
                Observation::Idle { idle: true, for_ms: 300_000 },
                Observation::Idle { idle: true, for_ms: 1_800_000 },
                Observation::Idle { idle: false, for_ms: 1_800_000 },
                Observation::Idle { idle: true, for_ms: 30_000 },
            ]
        );
    }

    #[test]
    fn tracker_only_ever_produces_idle_observations() {
        let mut t = IdleTracker::new();
        for e in [
            IdleEvent::Idled { threshold_ms: 1 },
            IdleEvent::Resumed { threshold_ms: 1 },
        ] {
            if let Some(o) = t.apply(e) {
                assert_eq!(o.sense(), SenseId::Idle);
            }
        }
    }
}
