//! F22 — MPRIS. What is playing, and the handle she needs to skip a track later.

use std::collections::HashMap;

use futures_util::stream::{BoxStream, SelectAll};
use futures_util::StreamExt;
use wisp_proto::{Observation, SenseId};

use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

pub const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

pub struct MediaSense;

impl Sense for MediaSense {
    const ID: SenseId = SenseId::Media;
    const LABEL: &'static str = crate::consent::label_of(SenseId::Media);
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::Media);
}

// ---------------------------------------------------------------------------
// Parsing, kept away from D-Bus so it can be tested against captures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub title: String,
    pub artist: String,
    pub playing: bool,
}

/// Players disagree about how to say "nothing". Firefox reports
/// `xesam:artist = [""]` for a YouTube tab; others omit the key, or send an
/// empty list. All three mean the same thing and none of them should reach her
/// as the word "Unknown".
pub fn join_artists(artists: &[String]) -> String {
    let kept: Vec<&str> =
        artists.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
    kept.join(", ")
}

/// `PlaybackStatus` is `Playing` / `Paused` / `Stopped`.
pub fn is_playing(status: &str) -> bool {
    status.eq_ignore_ascii_case("Playing")
}

/// `org.mpris.MediaPlayer2.firefox.instance_1_5546` is not something to say out
/// loud. Prefer the player's own `Identity`, and otherwise make the bus name
/// presentable.
pub fn friendly_player_name(bus_name: &str, identity: Option<&str>) -> String {
    if let Some(id) = identity {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let tail = bus_name.strip_prefix(MPRIS_PREFIX).unwrap_or(bus_name);
    let tail = tail.split(".instance").next().unwrap_or(tail);
    if tail.is_empty() {
        bus_name.to_string()
    } else {
        tail.to_string()
    }
}

pub fn is_mpris_name(name: &str) -> bool {
    name.starts_with(MPRIS_PREFIX) && name.len() > MPRIS_PREFIX.len()
}

/// The subset of `org.mpris.MediaPlayer2.Player` we care about, extracted from a
/// `busctl --json=short` capture. Only used by the tests and the fixture
/// tooling; the live path reads the same fields off `zvariant` values.
pub fn snapshot_from_busctl(v: &serde_json::Value) -> Option<Snapshot> {
    let root = v.get("data")?.get(0)?;
    let status = root.get("PlaybackStatus")?.get("data")?.as_str().unwrap_or("Stopped");
    let meta = root.get("Metadata").and_then(|m| m.get("data"));
    let title = meta
        .and_then(|m| m.get("xesam:title"))
        .and_then(|t| t.get("data"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let artists: Vec<String> = meta
        .and_then(|m| m.get("xesam:artist"))
        .and_then(|a| a.get("data"))
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str()).map(|s| s.to_string()).collect())
        .unwrap_or_default();
    Some(Snapshot { title, artist: join_artists(&artists), playing: is_playing(status) })
}

/// Dedupe across players. MPRIS players emit `PropertiesChanged` for position,
/// volume and rate as well; none of that is an `Observation`.
#[derive(Debug, Default)]
pub struct MediaTracker {
    last: HashMap<String, Snapshot>,
}

impl MediaTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns an observation only when something she would comment on changed.
    pub fn update(&mut self, player: &str, snap: Snapshot) -> Option<Observation> {
        if self.last.get(player) == Some(&snap) {
            return None;
        }
        // A player with no track at all is not news; it is a player sitting idle.
        if snap.title.is_empty() && !snap.playing {
            self.last.insert(player.to_string(), snap);
            return None;
        }
        self.last.insert(player.to_string(), snap.clone());
        Some(Observation::Media {
            player: player.to_string(),
            title: snap.title,
            artist: snap.artist,
            playing: snap.playing,
        })
    }

    /// The player quit. Say it stopped, once, if it was ever playing.
    pub fn remove(&mut self, player: &str) -> Option<Observation> {
        let prev = self.last.remove(player)?;
        if !prev.playing {
            return None;
        }
        Some(Observation::Media {
            player: player.to_string(),
            title: prev.title,
            artist: prev.artist,
            playing: false,
        })
    }

    pub fn players(&self) -> usize {
        self.last.len()
    }
}

// ---------------------------------------------------------------------------
// D-Bus
// ---------------------------------------------------------------------------

#[zbus::proxy(interface = "org.mpris.MediaPlayer2", default_path = "/org/mpris/MediaPlayer2")]
pub trait MediaPlayer2 {
    #[zbus(property)]
    fn identity(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub trait Player {
    fn play_pause(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, zbus::zvariant::OwnedValue>>;
    #[zbus(property)]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_pause(&self) -> zbus::Result<bool>;
}

/// F22's "can skip tracks". Sensing and controlling are separate: this is a
/// tool the mind may call, and it never publishes an `Observation`.
pub struct MediaControl {
    conn: zbus::Connection,
}

impl MediaControl {
    pub async fn new() -> zbus::Result<Self> {
        Ok(MediaControl { conn: zbus::Connection::session().await? })
    }

    pub fn from_connection(conn: zbus::Connection) -> Self {
        MediaControl { conn }
    }

    async fn player(&self, bus_name: &str) -> zbus::Result<PlayerProxy<'_>> {
        PlayerProxy::builder(&self.conn).destination(bus_name.to_string())?.build().await
    }

    pub async fn play_pause(&self, bus_name: &str) -> zbus::Result<()> {
        self.player(bus_name).await?.play_pause().await
    }
    pub async fn next(&self, bus_name: &str) -> zbus::Result<()> {
        self.player(bus_name).await?.next().await
    }
    pub async fn previous(&self, bus_name: &str) -> zbus::Result<()> {
        self.player(bus_name).await?.previous().await
    }
    pub async fn stop(&self, bus_name: &str) -> zbus::Result<()> {
        self.player(bus_name).await?.stop().await
    }

    /// Every MPRIS player currently on the bus.
    pub async fn players(&self) -> zbus::Result<Vec<String>> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await?;
        Ok(dbus
            .list_names()
            .await?
            .into_iter()
            .map(|n| n.to_string())
            .filter(|n| is_mpris_name(n))
            .collect())
    }
}

fn snapshot_from_variants(
    status: &str,
    metadata: &HashMap<String, zbus::zvariant::OwnedValue>,
) -> Snapshot {
    use zbus::zvariant::Value;
    let title = metadata
        .get("xesam:title")
        .and_then(|v| <&str>::try_from(v as &Value).ok())
        .unwrap_or("")
        .to_string();
    let artists: Vec<String> = metadata
        .get("xesam:artist")
        .and_then(|v| <Vec<String>>::try_from(v.clone()).ok())
        .unwrap_or_default();
    Snapshot { title, artist: join_artists(&artists), playing: is_playing(status) }
}

impl SensePlugin for MediaSense {
    fn spawn(self, handle: SenseHandle<Self>, ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = run(handle, ctx).await {
                tracing::warn!(error = %e, "media sense stopped");
            }
        })
    }
}

type PlayerStream = BoxStream<'static, (String, String)>;

/// Subscribe to one player and turn it into a `(bus_name, "")` wake-up stream.
/// The payload is deliberately empty: on any change we re-read the properties,
/// which is one round trip and immune to partial `PropertiesChanged` dicts.
async fn watch_player(conn: &zbus::Connection, bus_name: &str) -> zbus::Result<PlayerStream> {
    let props = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(bus_name.to_string())?
        .path("/org/mpris/MediaPlayer2")?
        .build()
        .await?;
    let name = bus_name.to_string();
    let stream = props.receive_properties_changed().await?;
    Ok(stream.map(move |_| (name.clone(), String::new())).boxed())
}

async fn read_snapshot(conn: &zbus::Connection, bus_name: &str) -> Option<(String, Snapshot)> {
    let player = PlayerProxy::builder(conn).destination(bus_name.to_string()).ok()?.build().await.ok()?;
    let status = player.playback_status().await.unwrap_or_else(|_| "Stopped".into());
    let metadata = player.metadata().await.unwrap_or_default();
    let identity = MediaPlayer2Proxy::builder(conn)
        .destination(bus_name.to_string())
        .ok()?
        .build()
        .await
        .ok()?
        .identity()
        .await
        .ok();
    let name = friendly_player_name(bus_name, identity.as_deref());
    Some((name, snapshot_from_variants(&status, &metadata)))
}

async fn run(handle: SenseHandle<MediaSense>, mut ctx: SenseCtx) -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;

    let mut tracker = MediaTracker::new();
    let mut streams: SelectAll<PlayerStream> = SelectAll::new();
    // bus name -> the name she says out loud
    let mut friendly: HashMap<String, String> = HashMap::new();

    for name in dbus.list_names().await? {
        let name = name.to_string();
        if !is_mpris_name(&name) {
            continue;
        }
        if let Ok(s) = watch_player(&conn, &name).await {
            streams.push(s);
        }
        if let Some((f, snap)) = read_snapshot(&conn, &name).await {
            friendly.insert(name.clone(), f.clone());
            if let Some(obs) = tracker.update(&f, snap) {
                handle.emit(obs);
            }
        }
    }

    let mut owners = dbus.receive_name_owner_changed().await?;

    loop {
        tokio::select! {
            biased;
            _ = ctx.shutdown.wait() => break,

            Some(sig) = owners.next() => {
                let Ok(args) = sig.args() else { continue };
                let name = args.name().to_string();
                if !is_mpris_name(&name) { continue; }
                if args.new_owner().is_some() {
                    if let Ok(s) = watch_player(&conn, &name).await {
                        streams.push(s);
                    }
                    if let Some((f, snap)) = read_snapshot(&conn, &name).await {
                        friendly.insert(name.clone(), f.clone());
                        if let Some(obs) = tracker.update(&f, snap) {
                            handle.emit(obs);
                        }
                    }
                } else if let Some(f) = friendly.remove(&name) {
                    if let Some(obs) = tracker.remove(&f) {
                        handle.emit(obs);
                    }
                }
            }

            Some((name, _)) = streams.next() => {
                if let Some((f, snap)) = read_snapshot(&conn, &name).await {
                    friendly.insert(name.clone(), f.clone());
                    if let Some(obs) = tracker.update(&f, snap) {
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

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/dbus");

    fn fixture(name: &str) -> serde_json::Value {
        let raw = std::fs::read_to_string(format!("{FIXTURES}/{name}")).expect(name);
        serde_json::from_str(&raw).expect("fixture parses")
    }

    #[test]
    fn parses_a_captured_firefox_player() {
        let snap = snapshot_from_busctl(&fixture("mpris_firefox_playing.json")).unwrap();
        assert!(snap.playing);
        assert_eq!(snap.title, "(11) Memes That End Unexpected 37 - YouTube");
        // Firefox reports xesam:artist as [""] — that must not become "Unknown"
        // or a stray comma.
        assert_eq!(snap.artist, "");
    }

    #[test]
    fn artist_lists_are_joined_and_blanks_dropped() {
        assert_eq!(join_artists(&[]), "");
        assert_eq!(join_artists(&["".into()]), "");
        assert_eq!(join_artists(&["  ".into(), "Boards of Canada".into()]), "Boards of Canada");
        assert_eq!(
            join_artists(&["Autechre".into(), "".into(), "Aphex Twin".into()]),
            "Autechre, Aphex Twin"
        );
    }

    #[test]
    fn playback_status_words() {
        assert!(is_playing("Playing"));
        assert!(is_playing("playing"));
        assert!(!is_playing("Paused"));
        assert!(!is_playing("Stopped"));
        assert!(!is_playing(""));
    }

    #[test]
    fn player_names_are_made_presentable() {
        assert_eq!(
            friendly_player_name("org.mpris.MediaPlayer2.firefox.instance_1_5546", None),
            "firefox"
        );
        assert_eq!(friendly_player_name("org.mpris.MediaPlayer2.vlc", None), "vlc");
        assert_eq!(
            friendly_player_name("org.mpris.MediaPlayer2.firefox.instance_1_5546", Some("Firefox")),
            "Firefox"
        );
        // A player that reports a blank identity falls back rather than going mute.
        assert_eq!(friendly_player_name("org.mpris.MediaPlayer2.vlc", Some("  ")), "vlc");
    }

    #[test]
    fn only_mpris_names_are_watched() {
        assert!(is_mpris_name("org.mpris.MediaPlayer2.vlc"));
        assert!(!is_mpris_name("org.mpris.MediaPlayer2."));
        assert!(!is_mpris_name("org.kde.KWin"));
        assert!(!is_mpris_name(":1.42"));
    }

    #[test]
    fn a_track_is_reported_once_not_on_every_position_tick() {
        let mut t = MediaTracker::new();
        let snap = snapshot_from_busctl(&fixture("mpris_firefox_playing.json")).unwrap();
        let first = t.update("Firefox", snap.clone());
        assert!(matches!(first, Some(Observation::Media { playing: true, .. })));
        // MPRIS emits PropertiesChanged for Position and Volume constantly.
        assert_eq!(t.update("Firefox", snap.clone()), None);
        assert_eq!(t.update("Firefox", snap), None);
    }

    #[test]
    fn pausing_is_news() {
        let mut t = MediaTracker::new();
        let mut snap = snapshot_from_busctl(&fixture("mpris_firefox_playing.json")).unwrap();
        t.update("Firefox", snap.clone());
        snap.playing = false;
        assert!(matches!(
            t.update("Firefox", snap),
            Some(Observation::Media { playing: false, .. })
        ));
    }

    #[test]
    fn an_idle_player_with_no_track_says_nothing() {
        let mut t = MediaTracker::new();
        let empty = Snapshot { title: String::new(), artist: String::new(), playing: false };
        assert_eq!(t.update("Elisa", empty), None);
        assert_eq!(t.players(), 1, "still tracked, just not announced");
    }

    #[test]
    fn a_player_quitting_mid_track_reports_it_stopped() {
        let mut t = MediaTracker::new();
        let snap = snapshot_from_busctl(&fixture("mpris_firefox_playing.json")).unwrap();
        t.update("Firefox", snap);
        let obs = t.remove("Firefox").unwrap();
        assert!(matches!(obs, Observation::Media { playing: false, .. }));
        assert_eq!(t.players(), 0);
        assert_eq!(t.remove("Firefox"), None);
    }

    #[test]
    fn a_paused_player_quitting_is_silent() {
        let mut t = MediaTracker::new();
        t.update(
            "Elisa",
            Snapshot { title: "x".into(), artist: "y".into(), playing: false },
        );
        assert_eq!(t.remove("Elisa"), None);
    }

    #[test]
    fn several_players_are_tracked_independently() {
        let mut t = MediaTracker::new();
        let a = Snapshot { title: "A".into(), artist: "".into(), playing: true };
        let b = Snapshot { title: "B".into(), artist: "".into(), playing: true };
        assert!(t.update("Firefox", a.clone()).is_some());
        assert!(t.update("Elisa", b).is_some());
        assert_eq!(t.update("Firefox", a), None);
        assert_eq!(t.players(), 2);
    }

    #[test]
    fn only_media_observations_are_produced() {
        let mut t = MediaTracker::new();
        let snap = snapshot_from_busctl(&fixture("mpris_firefox_playing.json")).unwrap();
        assert_eq!(t.update("Firefox", snap).unwrap().sense(), SenseId::Media);
    }
}
