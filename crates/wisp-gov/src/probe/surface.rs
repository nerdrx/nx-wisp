//! The hooks `wisp-senses` pushes into.
//!
//! `wisp-gov` may not depend on `wisp-senses` (SPEC §2), does not speak D-Bus
//! and never talks to KWin. What it does is **define the input types and accept
//! them**. `wisp-senses` owns the KWin script, the `ext_idle_notifier_v1`
//! binding and the NX Connector client; when any of them learns something it
//! calls `set` on the matching hook here and the next governor poll sees it.
//!
//! All three are cheap `Arc<Mutex<..>>` cells, cloneable, `Send + Sync`, and
//! deliberately last-write-wins: a stale fullscreen report is worse than none.

use std::sync::{Arc, Mutex};

use crate::{
    probe::{OperatorProbe, SurfaceProbe},
    reading::{FullscreenSurface, OperatorReading, SurfaceReading},
    Millis,
};

/// Fullscreen state, as seen by KWin. Cloning shares the cell.
#[derive(Debug, Clone, Default)]
pub struct FullscreenHook {
    cell: Arc<Mutex<Option<FullscreenSurface>>>,
}

impl FullscreenHook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by `wisp-senses` when a window goes fullscreen.
    pub fn set(&self, surface: FullscreenSurface) {
        *self.lock() = Some(surface);
    }

    /// Called by `wisp-senses` when nothing is fullscreen any more. This is the
    /// call that lets her come back after the game exits, so it must happen on
    /// window close as well as on un-fullscreen.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    pub fn get(&self) -> Option<FullscreenSurface> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<FullscreenSurface>> {
        // A poisoned lock here must not take the governor down: losing the
        // governor is the one failure the charter cannot tolerate.
        self.cell.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl SurfaceProbe for FullscreenHook {
    fn read(&mut self) -> SurfaceReading {
        SurfaceReading {
            fullscreen: self.get(),
        }
    }
}

/// Idle / lock state from `ext_idle_notifier_v1` and the session lock.
#[derive(Debug, Clone, Default)]
pub struct OperatorHook {
    cell: Arc<Mutex<OperatorReading>>,
}

impl OperatorHook {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_idle_ms(&self, idle_ms: Millis) {
        self.lock().idle_ms = idle_ms;
    }
    pub fn set_locked(&self, locked: bool) {
        self.lock().locked = locked;
    }
    pub fn get(&self) -> OperatorReading {
        *self.lock()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, OperatorReading> {
        self.cell.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl OperatorProbe for OperatorHook {
    fn read(&mut self) -> OperatorReading {
        self.get()
    }
}

/// An authoritative "a VR session is / is not live" answer, when something
/// knows better than the process probe's CPU heuristic — WiVRn's own state over
/// the NX Connector bus, for instance (F45).
///
/// `None` means "no opinion, use the heuristic".
#[derive(Debug, Clone, Default)]
pub struct VrSessionHint {
    cell: Arc<Mutex<Option<bool>>>,
}

impl VrSessionHint {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, streaming: bool) {
        *self.lock() = Some(streaming);
    }
    pub fn clear(&self) {
        *self.lock() = None;
    }
    pub fn get(&self) -> Option<bool> {
        *self.lock()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<bool>> {
        self.cell.lock().unwrap_or_else(|e| e.into_inner())
    }
}
