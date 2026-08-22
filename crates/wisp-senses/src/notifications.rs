//! F24 — the notification bus, watched. **Read only.**
//!
//! We become a D-Bus monitor (`org.freedesktop.DBus.Monitoring.BecomeMonitor`)
//! with a match rule narrowed to `Notify` calls. A monitor connection cannot
//! send anything, which is exactly the guarantee we want: this sense is
//! physically incapable of posting, closing or answering a notification.
//!
//! It also means we never register as the notification server and never race
//! Plasma for the name.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use wisp_proto::{Observation, SenseId};

use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

pub struct NotificationSense;

impl Sense for NotificationSense {
    const ID: SenseId = SenseId::Notifications;
    const LABEL: &'static str = crate::consent::label_of(SenseId::Notifications);
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::Notifications);
}

/// The only rule we ask the bus for. Narrow on purpose: a monitor with a wide
/// rule would see every message on the session bus, which is not what the
/// operator agreed to when they left "Notifications" switched on.
pub const MATCH_RULE: &str = "type='method_call',interface='org.freedesktop.Notifications',member='Notify'";

/// `Notify(susssasa{sv}i)` — app_name, replaces_id, app_icon, summary, body,
/// actions, hints, expire_timeout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotifyCall {
    pub app_name: String,
    pub replaces_id: u32,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub expire_timeout: i32,
    /// `urgency` out of the hints dict: 0 low, 1 normal, 2 critical.
    #[serde(default)]
    pub urgency: Option<u8>,
    /// Some senders mark a notification as transient; those are progress
    /// popups and volume OSDs, not news.
    #[serde(default)]
    pub transient: bool,
}

/// What the sense will not repeat back to the operator.
#[derive(Debug, Clone)]
pub struct NotificationFilter {
    /// Applications whose notifications are ignored entirely. Ours is in here
    /// by default — she must not react to her own speech bubbles.
    pub ignored_apps: Vec<String>,
    /// Drop notifications with an empty summary *and* empty body.
    pub drop_empty: bool,
    /// Drop `transient` hints (volume OSD, progress).
    pub drop_transient: bool,
}

impl Default for NotificationFilter {
    fn default() -> Self {
        NotificationFilter {
            ignored_apps: vec!["nx-wisp".into(), "NX Wisp".into()],
            drop_empty: true,
            drop_transient: true,
        }
    }
}

impl NotificationFilter {
    pub fn allows(&self, n: &NotifyCall) -> bool {
        if self.drop_transient && n.transient {
            return false;
        }
        if self.drop_empty && n.summary.trim().is_empty() && n.body.trim().is_empty() {
            return false;
        }
        !self
            .ignored_apps
            .iter()
            .any(|a| a.eq_ignore_ascii_case(n.app_name.trim()))
    }
}

/// Collapses the update storms that progress notifications produce: an app that
/// keeps calling `Notify` with the same non-zero `replaces_id` is replacing one
/// popup, not raising a new one.
#[derive(Debug, Default)]
pub struct NotificationTracker {
    filter: NotificationFilter,
    last_replaced: std::collections::HashMap<u32, (String, String)>,
}

impl NotificationTracker {
    pub fn new(filter: NotificationFilter) -> Self {
        NotificationTracker { filter, last_replaced: Default::default() }
    }

    pub fn apply(&mut self, n: &NotifyCall) -> Option<Observation> {
        if !self.filter.allows(n) {
            return None;
        }
        if n.replaces_id != 0 {
            let key = (n.summary.clone(), n.body.clone());
            if self.last_replaced.get(&n.replaces_id) == Some(&key) {
                return None;
            }
            self.last_replaced.insert(n.replaces_id, key);
        }
        Some(Observation::Notification {
            app: n.app_name.clone(),
            summary: n.summary.clone(),
            body: n.body.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// D-Bus
// ---------------------------------------------------------------------------

/// Pull a `Notify` call out of a raw monitored message.
pub fn parse_notify(msg: &zbus::Message) -> Option<NotifyCall> {
    let header = msg.header();
    if header.interface()?.as_str() != "org.freedesktop.Notifications" {
        return None;
    }
    if header.member()?.as_str() != "Notify" {
        return None;
    }
    let body = msg.body();
    type Raw<'a> = (
        String,
        u32,
        String,
        String,
        String,
        Vec<String>,
        std::collections::HashMap<String, zbus::zvariant::Value<'a>>,
        i32,
    );
    let (app_name, replaces_id, app_icon, summary, body_text, actions, hints, expire_timeout) =
        body.deserialize::<Raw>().ok()?;

    let urgency = hints.get("urgency").and_then(|v| u8::try_from(v.clone()).ok());
    let transient = hints
        .get("transient")
        .and_then(|v| bool::try_from(v.clone()).ok())
        .unwrap_or(false);

    Some(NotifyCall {
        app_name,
        replaces_id,
        app_icon,
        summary,
        body: body_text,
        actions,
        expire_timeout,
        urgency,
        transient,
    })
}

impl SensePlugin for NotificationSense {
    fn spawn(self, handle: SenseHandle<Self>, ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = run(handle, ctx).await {
                tracing::warn!(error = %e, "notification sense stopped");
            }
        })
    }
}

async fn run(handle: SenseHandle<NotificationSense>, mut ctx: SenseCtx) -> anyhow::Result<()> {
    // A dedicated connection: once it is a monitor it can do nothing else, and
    // we do not want that restriction on the connection other senses share.
    let conn = zbus::Connection::session().await?;
    let monitoring = zbus::fdo::MonitoringProxy::builder(&conn)
        .destination("org.freedesktop.DBus")?
        .path("/org/freedesktop/DBus")?
        .build()
        .await?;
    let rule = zbus::MatchRule::try_from(MATCH_RULE)?;
    monitoring.become_monitor(&[rule], 0).await?;

    let mut messages = zbus::MessageStream::from(&conn);
    let mut tracker = NotificationTracker::new(NotificationFilter::default());

    loop {
        tokio::select! {
            biased;
            _ = ctx.shutdown.wait() => break,
            msg = messages.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Some(call) = parse_notify(&msg) else { continue };
                if let Some(obs) = tracker.apply(&call) {
                    handle.emit(obs);
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
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dbus/notify_calls.json");

    fn captured() -> Vec<NotifyCall> {
        let raw = std::fs::read_to_string(FIXTURE).expect("fixture");
        serde_json::from_str(&raw).expect("fixture parses")
    }

    #[test]
    fn the_match_rule_is_narrow() {
        assert!(MATCH_RULE.contains("member='Notify'"));
        assert!(MATCH_RULE.contains("interface='org.freedesktop.Notifications'"));
        assert!(MATCH_RULE.contains("type='method_call'"));
        assert!(!MATCH_RULE.contains("eavesdrop"), "eavesdrop is the old, wide way");
    }

    #[test]
    fn a_captured_burst_becomes_the_right_observations() {
        let mut t = NotificationTracker::new(NotificationFilter::default());
        let out: Vec<_> = captured().iter().filter_map(|n| t.apply(n)).collect();
        assert_eq!(
            out,
            vec![
                Observation::Notification {
                    app: "Thunderbird".into(),
                    summary: "New message".into(),
                    body: "nerdrx — NX Hub release build failed".into(),
                },
                Observation::Notification {
                    app: "Discord".into(),
                    summary: "#nx-dev".into(),
                    body: "someone: did the wisp land yet".into(),
                },
                Observation::Notification {
                    app: "Dolphin".into(),
                    summary: "Copying".into(),
                    body: "42% — 3 of 7 files".into(),
                },
            ]
        );
    }

    #[test]
    fn progress_updates_on_the_same_id_are_collapsed() {
        let mut t = NotificationTracker::new(NotificationFilter::default());
        let n = NotifyCall {
            app_name: "Dolphin".into(),
            replaces_id: 91,
            app_icon: String::new(),
            summary: "Copying".into(),
            body: "42% — 3 of 7 files".into(),
            actions: vec![],
            expire_timeout: -1,
            urgency: Some(1),
            transient: false,
        };
        assert!(t.apply(&n).is_some());
        assert!(t.apply(&n).is_none(), "identical replacement is not news");
        let mut n2 = n.clone();
        n2.body = "80% — 6 of 7 files".into();
        assert!(t.apply(&n2).is_some(), "actual progress is");
    }

    #[test]
    fn transient_popups_are_dropped() {
        let f = NotificationFilter::default();
        let volume = NotifyCall {
            app_name: "plasmashell".into(),
            replaces_id: 0,
            app_icon: String::new(),
            summary: "Volume".into(),
            body: "60%".into(),
            actions: vec![],
            expire_timeout: 2000,
            urgency: Some(0),
            transient: true,
        };
        assert!(!f.allows(&volume));
    }

    #[test]
    fn she_does_not_hear_herself() {
        let f = NotificationFilter::default();
        let own = NotifyCall {
            app_name: "nx-wisp".into(),
            replaces_id: 0,
            app_icon: String::new(),
            summary: "hello".into(),
            body: String::new(),
            actions: vec![],
            expire_timeout: -1,
            urgency: None,
            transient: false,
        };
        assert!(!f.allows(&own));
        let mut cased = own.clone();
        cased.app_name = "NX WISP".into();
        assert!(!f.allows(&cased), "app name matching must not be case sensitive");
    }

    #[test]
    fn wholly_empty_notifications_are_dropped() {
        let f = NotificationFilter::default();
        let empty = NotifyCall {
            app_name: "someapp".into(),
            replaces_id: 0,
            app_icon: String::new(),
            summary: "   ".into(),
            body: String::new(),
            actions: vec![],
            expire_timeout: -1,
            urgency: None,
            transient: false,
        };
        assert!(!f.allows(&empty));
    }

    #[test]
    fn only_notification_observations_are_produced() {
        let mut t = NotificationTracker::new(NotificationFilter::default());
        for n in captured() {
            if let Some(o) = t.apply(&n) {
                assert_eq!(o.sense(), SenseId::Notifications);
            }
        }
    }
}
