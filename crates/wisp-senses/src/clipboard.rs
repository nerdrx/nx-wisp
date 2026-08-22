//! F25 — clipboard sensing over `ext_data_control_manager_v1`.
//!
//! **This is the invasive one.** It ships disabled (SPEC §3.7), and while it is
//! live `EventKind::InvasiveActive` is on the bus so the character can show a
//! visible tell (SPEC §0.3). Both of those are enforced by the consent layer,
//! not by this module remembering to be careful — obtaining the `SenseHandle`
//! at all is what raises the tell, and dropping it is what lowers it.
//!
//! What it may report is fixed by `wisp-proto`:
//!
//! ```ignore
//! Observation::Clipboard { len: usize, kind: String }
//! ```
//!
//! A length and a MIME type. **There is no field for the contents and there
//! must never be one.** That is not an oversight in the enum, it is the design:
//! she can notice that you copied a 4 kB block of Rust, and she cannot know
//! what it said. This module therefore never keeps the bytes it reads — it
//! counts them as they stream past and drops the buffer.
//!
//! `ext_data_control_manager_v1` is chosen over the clipboard managers'
//! interfaces because it does not require focus and does not disturb the
//! selection: reading an offer with it is invisible to the application that
//! owns the clipboard.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};
use wisp_proto::{Observation, SenseId};

use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

pub struct ClipboardSense;

impl Sense for ClipboardSense {
    const ID: SenseId = SenseId::Clipboard;
    const LABEL: &'static str = crate::consent::label_of(SenseId::Clipboard);
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::Clipboard);
}

/// The most we will ever read from an offer. A clipboard can hold a whole
/// image; we only need a length, and we stop counting at this point rather than
/// pulling megabytes through the process for a number nobody will use precisely.
pub const MAX_COUNT_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Choosing what to measure
// ---------------------------------------------------------------------------

/// MIME types we will read a length from, best first.
///
/// Text is preferred because "you copied 40 lines" is the useful fact. Binary
/// types are reported by their type alone.
const PREFERRED: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "text/uri-list",
    "text/html",
];

/// Pick one MIME type out of everything the offer advertises.
pub fn choose_mime(offered: &[String]) -> Option<String> {
    for want in PREFERRED {
        if let Some(m) = offered.iter().find(|m| m.eq_ignore_ascii_case(want)) {
            return Some(m.clone());
        }
    }
    // Images and everything else: take the first thing that looks like a real
    // MIME type rather than an X11 atom name.
    offered.iter().find(|m| m.contains('/')).cloned().or_else(|| offered.first().cloned())
}

/// Normalise for the operator's eyes. `text/plain;charset=utf-8` is noise.
pub fn tidy_kind(mime: &str) -> String {
    let base = mime.split(';').next().unwrap_or(mime).trim();
    match base {
        "UTF8_STRING" | "STRING" | "TEXT" => "text/plain".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Suppresses the duplicate offers Wayland hands out. KWin re-advertises the
/// selection on focus changes, and a clipboard manager re-offers what it just
/// stored; neither is the operator copying something.
#[derive(Debug, Default)]
pub struct ClipboardTracker {
    last: Option<(usize, String)>,
}

impl ClipboardTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note the deliberate shape of the argument: a length and a type. There is
    /// no path into this function that carries the clipboard's contents,
    /// because there is nothing here that could hold them.
    pub fn apply(&mut self, len: usize, mime: &str) -> Option<Observation> {
        let kind = tidy_kind(mime);
        let key = (len, kind.clone());
        if self.last.as_ref() == Some(&key) {
            return None;
        }
        self.last = Some(key);
        if len == 0 {
            return None;
        }
        Some(Observation::Clipboard { len, kind })
    }

    /// The selection was cleared.
    pub fn clear(&mut self) {
        self.last = None;
    }
}

// ---------------------------------------------------------------------------
// The Wayland side
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Selection {
    len: usize,
    mime: String,
}

struct ClipState {
    tx: tokio::sync::mpsc::UnboundedSender<Selection>,
    /// MIME types advertised by the offer currently being announced.
    offered: std::collections::HashMap<u32, Vec<String>>,
}

impl Dispatch<ExtDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offered.entry(id_of(proxy)).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::Selection { id: Some(offer) } => {
                let mimes = state.offered.remove(&id_of(&offer)).unwrap_or_default();
                let Some(mime) = choose_mime(&mimes) else {
                    offer.destroy();
                    return;
                };
                match measure_offer(&offer, &mime) {
                    Ok(len) => {
                        let _ = state.tx.send(Selection { len, mime });
                    }
                    Err(e) => tracing::debug!(error = %e, "could not measure the selection"),
                }
                offer.destroy();
            }
            ext_data_control_device_v1::Event::Selection { id: None } => {
                let _ = state.tx.send(Selection { len: 0, mime: String::new() });
            }
            ext_data_control_device_v1::Event::Finished => {}
            _ => {}
        }
    }
}

fn id_of<T: wayland_client::Proxy>(p: &T) -> u32 {
    p.id().protocol_id()
}

wayland_client::delegate_noop!(ClipState: ignore ExtDataControlManagerV1);
wayland_client::delegate_noop!(ClipState: ignore WlSeat);

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, GlobalListContents> for ClipState {
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

/// Read an offer purely to count it.
///
/// The buffer never leaves this function and is not returned, logged or
/// hashed. That is the whole contract of this module.
fn measure_offer(offer: &ExtDataControlOfferV1, mime: &str) -> anyhow::Result<usize> {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    offer.receive(mime.to_string(), write_end.as_fd_borrowed());
    drop(write_end);

    let mut file = std::fs::File::from(read_end);
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0usize;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if total >= MAX_COUNT_BYTES {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    // The bytes go out of scope here, unexamined.
    buf.fill(0);
    Ok(total)
}

trait AsFdBorrowed {
    fn as_fd_borrowed(&self) -> std::os::fd::BorrowedFd<'_>;
}

impl AsFdBorrowed for std::os::fd::OwnedFd {
    fn as_fd_borrowed(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.as_fd()
    }
}

impl SensePlugin for ClipboardSense {
    fn spawn(self, handle: SenseHandle<Self>, mut ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Reaching this point means consent was granted, which means the
            // tell is already up. It comes down when `handle` drops, at the end
            // of this task — including if the task panics.
            tracing::warn!("clipboard sense is live; the invasive tell is showing");

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Selection>();
            let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
            let worker = std::thread::Builder::new()
                .name("wisp-clipboard".into())
                .spawn(move || {
                    if let Err(e) = wayland_thread(tx, stop_rx) {
                        tracing::warn!(error = %e, "clipboard sense stopped");
                    }
                })
                .ok();

            let mut tracker = ClipboardTracker::new();
            loop {
                tokio::select! {
                    biased;
                    _ = ctx.shutdown.wait() => break,
                    sel = rx.recv() => match sel {
                        Some(sel) => {
                            if sel.mime.is_empty() {
                                tracker.clear();
                                continue;
                            }
                            if let Some(obs) = tracker.apply(sel.len, &sel.mime) {
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
            tracing::info!("clipboard sense stopped; the tell comes down");
        })
    }
}

fn wayland_thread(
    tx: tokio::sync::mpsc::UnboundedSender<Selection>,
    stop: std_mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue): (GlobalList, _) = registry_queue_init::<ClipState>(&conn)?;
    let qh = queue.handle();

    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| anyhow::anyhow!("ext_data_control_manager_v1 is not available: {e}"))?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=8, ())
        .map_err(|e| anyhow::anyhow!("no wl_seat: {e}"))?;

    let _device = manager.get_data_device(&seat, &qh, ());
    let mut state = ClipState { tx, offered: Default::default() };
    queue.roundtrip(&mut state)?;

    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        queue.flush()?;
        let Some(guard) = queue.prepare_read() else {
            queue.dispatch_pending(&mut state)?;
            continue;
        };
        let fd = {
            use std::os::fd::{AsFd, AsRawFd};
            conn.as_fd().as_raw_fd()
        };
        match crate::idle::poll_readable(fd, Duration::from_millis(500)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_preferred_over_the_rest() {
        let offered = vec![
            "image/png".into(),
            "text/html".into(),
            "text/plain;charset=utf-8".into(),
            "text/plain".into(),
        ];
        assert_eq!(choose_mime(&offered).as_deref(), Some("text/plain;charset=utf-8"));
    }

    #[test]
    fn a_copied_image_is_still_measured() {
        let offered = vec!["image/png".into(), "image/bmp".into()];
        assert_eq!(choose_mime(&offered).as_deref(), Some("image/png"));
    }

    #[test]
    fn x11_atom_names_are_handled() {
        assert_eq!(choose_mime(&["UTF8_STRING".into()]).as_deref(), Some("UTF8_STRING"));
        assert_eq!(tidy_kind("UTF8_STRING"), "text/plain");
        assert_eq!(tidy_kind("STRING"), "text/plain");
    }

    #[test]
    fn an_offer_with_nothing_in_it_chooses_nothing() {
        assert_eq!(choose_mime(&[]), None);
    }

    #[test]
    fn kinds_are_tidied_for_the_operator() {
        assert_eq!(tidy_kind("text/plain;charset=utf-8"), "text/plain");
        assert_eq!(tidy_kind("TEXT/HTML"), "text/html");
        assert_eq!(tidy_kind("image/png"), "image/png");
        assert_eq!(tidy_kind(" text/uri-list "), "text/uri-list");
    }

    #[test]
    fn a_copy_is_reported_once_however_many_times_it_is_re_offered() {
        let mut t = ClipboardTracker::new();
        assert_eq!(
            t.apply(4096, "text/plain;charset=utf-8"),
            Some(Observation::Clipboard { len: 4096, kind: "text/plain".into() })
        );
        // KWin re-announces the selection on every focus change.
        assert_eq!(t.apply(4096, "text/plain;charset=utf-8"), None);
        assert_eq!(t.apply(4096, "text/plain"), None, "same thing, tidier name");
        assert_eq!(
            t.apply(4097, "text/plain"),
            Some(Observation::Clipboard { len: 4097, kind: "text/plain".into() })
        );
    }

    #[test]
    fn an_empty_selection_is_not_an_observation() {
        let mut t = ClipboardTracker::new();
        assert_eq!(t.apply(0, "text/plain"), None);
    }

    #[test]
    fn clearing_lets_the_same_thing_be_copied_again() {
        let mut t = ClipboardTracker::new();
        t.apply(10, "text/plain");
        assert_eq!(t.apply(10, "text/plain"), None);
        t.clear();
        assert!(t.apply(10, "text/plain").is_some());
    }

    /// The point of the whole module.
    #[test]
    fn the_observation_can_carry_a_length_and_a_type_and_nothing_else() {
        let mut t = ClipboardTracker::new();
        let obs = t.apply(31, "text/plain").unwrap();
        let json = serde_json::to_string(&obs).unwrap();
        assert_eq!(obs.sense(), SenseId::Clipboard);
        assert!(json.contains("31"));
        assert!(json.contains("text/plain"));
        // Whatever was actually on the clipboard, none of it is in here.
        match obs {
            Observation::Clipboard { len, kind } => {
                assert_eq!(len, 31);
                assert_eq!(kind, "text/plain");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn clipboard_is_invasive_and_ships_off() {
        assert_eq!(SenseId::Clipboard.consent(), wisp_proto::Consent::Invasive);
        assert!(!crate::consent::ships_enabled(SenseId::Clipboard));
    }

    #[test]
    fn the_read_cap_is_bounded() {
        // A clipboard can hold a whole framebuffer. We must never pull an
        // unbounded amount through the process for a number.
        const { assert!(MAX_COUNT_BYTES <= 8 * 1024 * 1024) };
    }
}
