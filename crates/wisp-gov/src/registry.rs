//! The registry of [`Governed`] subsystems.
//!
//! SPEC §3.1: *downgrades are applied synchronously and immediately; upgrades
//! are lazy and may be deferred.* That is enforced here structurally —
//! [`Registry::downgrade`] calls every subsystem before it returns, and there is
//! no path that queues a downgrade.

use std::sync::{Arc, Mutex};

use wisp_proto::{Cost, Governed, Tier, TierReason};

/// A registered subsystem: the object, its name for the cost meter, and its
/// cost function.
///
/// `Governed::cost_at` is an associated function with a `Self: Sized` bound, so
/// it is not on the `dyn` vtable. We capture it as a function pointer at
/// registration time instead, which is why [`Registry::register`] is generic.
struct Entry {
    name: String,
    obj: Box<dyn Governed + Send>,
    cost_at: fn(Tier) -> Cost,
}

/// Wraps a subsystem that also needs to be used from elsewhere.
///
/// [`Governed`] takes `&mut self`, so the registry would otherwise have to own
/// every subsystem outright. `Shared` is a local newtype purely so that we are
/// allowed to implement the (foreign) `Governed` trait for an `Arc<Mutex<T>>`.
///
/// The lock is held only for the duration of `set_tier`, which SPEC §3.1 already
/// requires to be non-blocking and infallible.
pub struct Shared<T>(pub Arc<Mutex<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(Arc::clone(&self.0))
    }
}

impl<T> Shared<T> {
    pub fn new(inner: T) -> Self {
        Shared(Arc::new(Mutex::new(inner)))
    }
    pub fn handle(&self) -> Arc<Mutex<T>> {
        Arc::clone(&self.0)
    }
}

impl<T: Governed> Governed for Shared<T> {
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        // A panicking subsystem must not be able to wedge the governor: a
        // poisoned lock is recovered rather than propagated.
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        g.set_tier(tier, reason);
    }
    fn cost_at(tier: Tier) -> Cost {
        T::cost_at(tier)
    }
}

#[derive(Default)]
pub struct Registry {
    entries: Vec<Entry>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("subsystems", &self.names())
            .finish()
    }
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// Register a subsystem. Order matters: subsystems are told about a
    /// downgrade in registration order, so register the expensive ones first
    /// and the ones that merely draw last.
    pub fn register<T>(&mut self, name: impl Into<String>, subsystem: T)
    where
        T: Governed + Send + 'static,
    {
        self.entries.push(Entry {
            name: name.into(),
            obj: Box::new(subsystem),
            cost_at: T::cost_at,
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// Tell every subsystem, **now**, in registration order. Returns when the
    /// last one has returned. Used for downgrades, where SPEC §3.1 leaves no
    /// choice.
    pub fn downgrade(&mut self, tier: Tier, reason: &TierReason) {
        for e in self.entries.iter_mut() {
            e.obj.set_tier(tier, reason);
        }
        tracing::debug!(?tier, ?reason, "downgrade applied synchronously");
    }

    /// Tell every subsystem it may be more expensive now. Identical mechanics;
    /// the laziness lives in [`crate::ladder::Ladder`]'s dwell timers, so by the
    /// time this is called the upgrade has already earned its delay and there
    /// is nothing to gain by deferring it further.
    pub fn upgrade(&mut self, tier: Tier, reason: &TierReason) {
        for e in self.entries.iter_mut() {
            e.obj.set_tier(tier, reason);
        }
        tracing::debug!(?tier, ?reason, "upgrade applied");
    }

    /// Apply a tier change, choosing the right path automatically.
    pub fn apply(&mut self, from: Tier, to: Tier, reason: &TierReason) {
        if to > from {
            self.downgrade(to, reason);
        } else {
            self.upgrade(to, reason);
        }
    }

    /// Every subsystem's declared worst-case cost at `tier`.
    pub fn estimate(&self, tier: Tier) -> Vec<(String, Cost)> {
        self.entries
            .iter()
            .map(|e| (e.name.clone(), (e.cost_at)(tier)))
            .collect()
    }

    /// Sum of [`Registry::estimate`].
    pub fn estimate_total(&self, tier: Tier) -> Cost {
        self.entries
            .iter()
            .fold(Cost::FREE, |acc, e| acc + (e.cost_at)(tier))
    }
}
