//! Virtual desktops, from `org.kde.KWin.VirtualDesktopManager`.
//!
//! The Plasma Wayland protocol `org_kde_plasma_virtual_desktop_management` is
//! advertised too, but D-Bus is the cheaper route: the interface already carries
//! the name and position of every desktop as a property, and it emits change
//! signals. No Wayland thread, no seat, no roundtrip.

use std::collections::HashMap;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use wisp_proto::{Observation, SenseId};

use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

pub struct WorkspaceSense;

impl Sense for WorkspaceSense {
    const ID: SenseId = SenseId::Workspace;
    const LABEL: &'static str = crate::consent::label_of(SenseId::Workspace);
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::Workspace);
}

/// One entry of KWin's `desktops` property: `(position, id, name)`.
///
/// **`u32`, not `i32`.** KWin's introspection XML advertises `a(iss)` but the
/// value on the wire is `a(uss)`; a signature mismatch here does not error, it
/// just yields an empty desktop table and the sense goes permanently quiet.
/// Verified against the live session, not the XML.
pub type DesktopTuple = (u32, String, String);

/// KWin identifies the current desktop by UUID, but `Observation::Workspace`
/// wants an index and a name. This is the lookup, kept pure so the mapping is
/// tested against a captured `desktops` property rather than a live session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesktopTable {
    by_id: HashMap<String, (u32, String)>,
}

impl DesktopTable {
    pub fn from_tuples(tuples: &[DesktopTuple]) -> Self {
        let mut by_id = HashMap::new();
        for (pos, id, name) in tuples {
            by_id.insert(id.clone(), (*pos, name.clone()));
        }
        DesktopTable { by_id }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// `None` when KWin names a desktop we have not been told about — which
    /// happens for exactly as long as it takes the `desktopCreated` signal to
    /// arrive, so the caller refreshes rather than inventing an index.
    pub fn lookup(&self, id: &str) -> Option<Observation> {
        self.by_id
            .get(id)
            .map(|(index, name)| Observation::Workspace { index: *index, name: name.clone() })
    }
}

/// Dedupes: KWin re-announces the current desktop on any desktop-list change.
#[derive(Debug, Default)]
pub struct WorkspaceTracker {
    table: DesktopTable,
    last: Option<String>,
}

impl WorkspaceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_table(&mut self, tuples: &[DesktopTuple]) {
        self.table = DesktopTable::from_tuples(tuples);
    }

    pub fn table(&self) -> &DesktopTable {
        &self.table
    }

    /// Returns the observation to publish, if anything actually changed.
    pub fn current(&mut self, id: &str) -> Option<Observation> {
        if self.last.as_deref() == Some(id) {
            return None;
        }
        let obs = self.table.lookup(id)?;
        self.last = Some(id.to_string());
        Some(obs)
    }

    /// Replace the desktop list. Returns whether anything actually changed, and
    /// only then forgets the current desktop — a rename of the desktop she is
    /// already on is news, but zbus replays the property's cached value the
    /// moment we subscribe, and that is not.
    pub fn update_table(&mut self, tuples: &[DesktopTuple]) -> bool {
        let next = DesktopTable::from_tuples(tuples);
        if next == self.table {
            return false;
        }
        self.table = next;
        self.last = None;
        true
    }

    /// Force the next `current` to publish even if it names the same desktop.
    pub fn invalidate(&mut self) {
        self.last = None;
    }
}

#[zbus::proxy(
    interface = "org.kde.KWin.VirtualDesktopManager",
    default_service = "org.kde.KWin",
    default_path = "/VirtualDesktopManager"
)]
pub trait VirtualDesktopManager {
    // KWin's property names are lower camel case. zbus would otherwise ask for
    // `Current`, and KWin answers `UnknownProperty` — silently, at runtime.
    #[zbus(property, name = "current")]
    fn current(&self) -> zbus::Result<String>;
    #[zbus(property, name = "desktops")]
    fn desktops(&self) -> zbus::Result<Vec<DesktopTuple>>;
    #[zbus(property, name = "count")]
    fn count(&self) -> zbus::Result<u32>;
}

impl SensePlugin for WorkspaceSense {
    fn spawn(self, handle: SenseHandle<Self>, ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = run(handle, ctx).await {
                tracing::warn!(error = %e, "workspace sense stopped");
            }
        })
    }
}

async fn run(handle: SenseHandle<WorkspaceSense>, mut ctx: SenseCtx) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await?;
    let proxy = VirtualDesktopManagerProxy::new(&conn).await?;

    let mut tracker = WorkspaceTracker::new();
    tracker.update_table(&proxy.desktops().await.unwrap_or_default());

    // The desktop she is on right now is news to a subscriber that just started.
    if let Some(obs) = tracker.current(&proxy.current().await?) {
        handle.emit(obs);
    }

    let mut current_changes = proxy.receive_current_changed().await;
    let mut desktop_changes = proxy.receive_desktops_changed().await;

    loop {
        tokio::select! {
            biased;
            _ = ctx.shutdown.wait() => break,

            Some(c) = desktop_changes.next() => {
                if let Ok(v) = c.get().await {
                    // A desktop was added, removed or renamed; the name she
                    // would say may have changed under her. But zbus replays
                    // the cached value on subscribe, so only a real change counts.
                    if tracker.update_table(&v) {
                        if let Ok(id) = proxy.current().await {
                            if let Some(obs) = tracker.current(&id) {
                                handle.emit(obs);
                            }
                        }
                    }
                }
            }

            Some(c) = current_changes.next() => {
                if let Ok(id) = c.get().await {
                    if tracker.table().lookup(&id).is_none() {
                        // Switched to a desktop created a moment ago.
                        tracker.update_table(&proxy.desktops().await.unwrap_or_default());
                    }
                    if let Some(obs) = tracker.current(&id) {
                        handle.emit(obs);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dbus/virtual_desktops.json");

    fn captured() -> Vec<DesktopTuple> {
        let raw = std::fs::read_to_string(FIXTURE).expect("fixture");
        serde_json::from_str(&raw).expect("fixture parses")
    }

    #[test]
    fn captured_desktop_table_maps_uuids_to_index_and_name() {
        let t = DesktopTable::from_tuples(&captured());
        assert_eq!(t.len(), 4);
        assert_eq!(
            t.lookup("cb0671e1-6242-4335-99cd-cda50c2be49e"),
            Some(Observation::Workspace { index: 0, name: "Main".into() })
        );
        assert_eq!(
            t.lookup("a1f2e0f4-5c11-4a9e-9a5b-3f0c1d2e4b77"),
            Some(Observation::Workspace { index: 3, name: "VR".into() })
        );
        assert_eq!(t.lookup("not-a-desktop"), None);
    }

    #[test]
    fn switching_desktops_publishes_once_each() {
        let mut w = WorkspaceTracker::new();
        w.set_table(&captured());
        let a = "cb0671e1-6242-4335-99cd-cda50c2be49e";
        let b = "7c9a12b3-8d44-4a0e-b111-2f3e4d5c6a88";

        assert_eq!(w.current(a), Some(Observation::Workspace { index: 0, name: "Main".into() }));
        assert_eq!(w.current(a), None, "the same desktop twice is not news");
        assert_eq!(w.current(b), Some(Observation::Workspace { index: 1, name: "Code".into() }));
        assert_eq!(w.current(a), Some(Observation::Workspace { index: 0, name: "Main".into() }));
    }

    #[test]
    fn an_unknown_uuid_publishes_nothing_rather_than_a_guess() {
        let mut w = WorkspaceTracker::new();
        w.set_table(&captured());
        assert_eq!(w.current("brand-new-desktop"), None);
        // And having refused, it is not remembered as current.
        w.set_table(&[(4, "brand-new-desktop".into(), "Scratch".into())]);
        assert_eq!(
            w.current("brand-new-desktop"),
            Some(Observation::Workspace { index: 4, name: "Scratch".into() })
        );
    }

    #[test]
    fn renaming_the_current_desktop_is_republished() {
        let mut w = WorkspaceTracker::new();
        w.update_table(&captured());
        let a = "cb0671e1-6242-4335-99cd-cda50c2be49e";
        w.current(a);
        assert!(w.update_table(&[(0, a.into(), "Main — renamed".into())]));
        assert_eq!(
            w.current(a),
            Some(Observation::Workspace { index: 0, name: "Main — renamed".into() })
        );
    }

    /// zbus replays a property's cached value as soon as we subscribe to its
    /// change stream. That must not read as "the desktops changed", or she
    /// announces the desktop she is already on, twice, at every start-up.
    #[test]
    fn an_unchanged_desktop_list_is_not_a_change() {
        let mut w = WorkspaceTracker::new();
        assert!(w.update_table(&captured()), "the first table is a change");
        let a = "cb0671e1-6242-4335-99cd-cda50c2be49e";
        assert!(w.current(a).is_some());
        assert!(!w.update_table(&captured()), "the same list replayed is not");
        assert_eq!(w.current(a), None, "and she does not say it twice");
    }

    /// The property's signature is the whole ballgame: get it wrong and the
    /// sense is silent rather than broken, which is far harder to notice.
    #[test]
    fn the_captured_property_has_the_signature_kwin_actually_sends() {
        let raw = std::fs::read_to_string(FIXTURE).unwrap();
        let as_u32: Result<Vec<(u32, String, String)>, _> = serde_json::from_str(&raw);
        assert!(as_u32.is_ok(), "desktops is a(uss) on the wire, whatever the XML says");
    }

    #[test]
    fn only_workspace_observations_are_produced() {
        let mut w = WorkspaceTracker::new();
        w.set_table(&captured());
        let o = w.current("cb0671e1-6242-4335-99cd-cda50c2be49e").unwrap();
        assert_eq!(o.sense(), SenseId::Workspace);
    }
}
