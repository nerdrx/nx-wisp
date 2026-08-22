//! How this crate says what it did.
//!
//! SPEC §3.2: *every event is recorded by the flight recorder before dispatch*,
//! and §0.4: *"why did you say that?" is answerable from data.* `wisp-mind`
//! therefore narrates itself — every model load, every eviction, every tool
//! call, every deferral — into an [`EventSink`] the binary owns.
//!
//! The sink takes an [`EventKind`] rather than a whole [`wisp_proto::Event`] on
//! purpose. An `Event` carries `at`, a monotonic millisecond count from process
//! start, and this crate has no clock: everything time-shaped is passed in by
//! its caller so that the memory-decay and staleness tests can move six weeks in
//! a microsecond. Stamping is the binary's job, next to the recorder that owns
//! the clock.
//!
//! Nothing here prints. SPEC §3.4 gives speech to `wisp-attn`, and a crate that
//! could `println!` would eventually do it at four in the morning.

use std::sync::Arc;

use wisp_proto::EventKind;

/// A place to put facts about the past.
#[derive(Clone, Default)]
pub struct EventSink(Option<Arc<dyn Fn(EventKind) + Send + Sync>>);

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "EventSink(wired)"
        } else {
            "EventSink(silent)"
        })
    }
}

impl EventSink {
    /// Goes nowhere. The default, so every type in this crate can be
    /// constructed in a test without wiring a recorder.
    pub fn silent() -> Self {
        EventSink(None)
    }

    pub fn new(f: impl Fn(EventKind) + Send + Sync + 'static) -> Self {
        EventSink(Some(Arc::new(f)))
    }

    /// Collect into a shared vector. For tests, and for `wisp status`.
    pub fn collector() -> (Self, Collected) {
        let log: Collected = Collected::default();
        let inner = log.clone();
        (EventSink::new(move |k| inner.push(k)), log)
    }

    pub fn emit(&self, kind: EventKind) {
        if let Some(f) = &self.0 {
            f(kind);
        }
    }

    pub fn is_wired(&self) -> bool {
        self.0.is_some()
    }
}

/// The other end of [`EventSink::collector`].
#[derive(Clone, Default)]
pub struct Collected(Arc<std::sync::Mutex<Vec<EventKind>>>);

impl std::fmt::Debug for Collected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.all()).finish()
    }
}

impl Collected {
    fn push(&self, k: EventKind) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push(k);
    }
    pub fn all(&self) -> Vec<EventKind> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    /// Every `Model` event, as `(name, loaded, vram_mib)`.
    pub fn models(&self) -> Vec<(String, bool, u64)> {
        self.all()
            .into_iter()
            .filter_map(|k| match k {
                EventKind::Model {
                    name,
                    loaded,
                    vram_mib,
                } => Some((name, loaded, vram_mib)),
                _ => None,
            })
            .collect()
    }
    /// Every `ToolCall`, as `(name, ok)`.
    pub fn tool_calls(&self) -> Vec<(String, bool)> {
        self.all()
            .into_iter()
            .filter_map(|k| match k {
                EventKind::ToolCall { name, ok, .. } => Some((name, ok)),
                _ => None,
            })
            .collect()
    }
    pub fn deferred(&self) -> Vec<(String, usize)> {
        self.all()
            .into_iter()
            .filter_map(|k| match k {
                EventKind::Deferred { what, queued } => Some((what, queued)),
                _ => None,
            })
            .collect()
    }
    pub fn replayed(&self) -> Vec<(String, bool)> {
        self.all()
            .into_iter()
            .filter_map(|k| match k {
                EventKind::Replayed { what, dropped } => Some((what, dropped)),
                _ => None,
            })
            .collect()
    }
}
