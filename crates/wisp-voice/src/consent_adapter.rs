//! The bridge from [`crate::mic::MicPermit`] to the real consent ledger.
//!
//! Compiled only with the **non-default** `consent` feature. That is deliberate
//! and it is the one place this crate knowingly bends SPEC §2's crate map, so it
//! is worth being explicit about.
//!
//! §2 says `wisp-voice` may depend on `proto, gov`. The consent gate lives in
//! `wisp-senses`, and it is written so that a `SenseHandle` **has no
//! constructor** — `ConsentLedger::grant` is the only source of one, it refuses
//! unless the operator enabled the sense, it raises the visible tell of SPEC
//! §0.3 before it returns, and its `Drop` lowers the tell. That design is the
//! whole reason "a microphone running without consent" is unrepresentable, and
//! reimplementing any part of it here would throw the guarantee away.
//!
//! So there are exactly two honest options: hand the binary a trait and let it
//! write the ten lines, or write the ten lines here behind a feature that the
//! default build does not turn on. This file is the second, because:
//!
//! - the default `cargo build -p wisp-voice` and `cargo test -p wisp-voice`
//!   depend on `proto` and `gov` only, exactly as §2 requires;
//! - the adapter is *compiled and tested* against the real `SenseHandle` rather
//!   than being ten lines of hope in the binary that nobody type-checks until
//!   wiring day;
//! - and if §2 is meant to be absolute, deleting this file and moving it into
//!   `crates/wisp/` costs one `impl` block and changes nothing else.
//!
//! **Reported as a deliberate deviation, not slipped in.**
//!
//! ## What it does not do
//!
//! It does not decide anything. It does not cache "was I permitted", it does not
//! retry a refused publish, and it does not hold a second handle. Every question
//! is forwarded to the ledger on the spot, because the operator can revoke
//! consent at any instant and a cached answer is a microphone that stays open
//! for one more buffer than it was allowed to.

use wisp_proto::{Observation, SenseId};
use wisp_senses::consent::{
    description_of, label_of, ConsentError, ConsentLedger, Sense, SenseHandle,
};

use crate::mic::MicPermit;
use crate::{Result, VoiceError};

/// The microphone, as the consent ledger sees it.
///
/// `Sense::consent()` is derived from the `SenseId` and cannot be overridden, so
/// declaring this type is not a way to ask for a cheaper permission — it is
/// `Consent::Invasive` because [`SenseId::Microphone`] is, and that is that.
pub struct MicSense;

impl Sense for MicSense {
    const ID: SenseId = SenseId::Microphone;
    const LABEL: &'static str = label_of(SenseId::Microphone);
    const DESCRIPTION: &'static str = description_of(SenseId::Microphone);
}

/// A granted microphone. Holding one means the tell is up.
///
/// Not `Clone`, because [`SenseHandle`] is not: one live handle per sense is
/// what makes the ledger's 0→1 / 1→0 tell transitions exact. Move it into
/// [`crate::mic::Listener::open`] and let the `Listener` own it — when the
/// `Listener` is dropped (by `close()`, by a T3 downgrade, or by a panic
/// unwinding past it) the handle goes with it and the tell comes down.
pub struct GrantedMic {
    handle: SenseHandle<MicSense>,
}

impl std::fmt::Debug for GrantedMic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantedMic").field("sense", &SenseId::Microphone).finish()
    }
}

impl GrantedMic {
    /// Ask the ledger. Fails unless the operator has switched the microphone on
    /// in the consent panel — which, per SPEC §3.7, it is not by default.
    ///
    /// On success the ledger has **already** emitted
    /// `EventKind::InvasiveActive { active: true }`, so the tell is up before
    /// this function returns and therefore before any caller could possibly
    /// have opened a capture device.
    pub fn request(ledger: &ConsentLedger) -> std::result::Result<Self, ConsentError> {
        Ok(GrantedMic { handle: ledger.grant::<MicSense>()? })
    }
}

impl MicPermit for GrantedMic {
    fn publish(&self, obs: Observation) -> Result<()> {
        self.handle.publish(obs).map_err(|e| match e {
            wisp_senses::consent::PublishError::Revoked(_) => VoiceError::ConsentRevoked,
            // A sense publishing another sense's observation cannot happen
            // through honest code; the ledger says so too. Surface it loudly
            // rather than swallowing it.
            other => VoiceError::Stt(other.to_string()),
        })
    }

    fn still_permitted(&self) -> bool {
        self.handle.still_permitted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mic::{FakeMic, ListenConfig, Listener};
    use crate::stt::FakeStt;
    use tokio::sync::broadcast;
    use wisp_proto::{Event, EventKind};
    use wisp_senses::clock::Clock;

    fn ledger(dir: &std::path::Path) -> (ConsentLedger, broadcast::Receiver<Event>) {
        let (tx, rx) = broadcast::channel(64);
        (ConsentLedger::load_from(dir, tx, Clock::new()), rx)
    }

    #[test]
    fn the_microphone_ships_off_and_cannot_be_granted_until_the_operator_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let (l, _rx) = ledger(tmp.path());
        assert!(!l.is_enabled(SenseId::Microphone), "SPEC §3.7: invasive ships off");
        assert!(GrantedMic::request(&l).is_err());
        l.set_enabled(SenseId::Microphone, true).unwrap();
        assert!(GrantedMic::request(&l).is_ok());
    }

    #[test]
    fn the_tell_goes_up_before_the_permit_exists_and_down_when_it_is_dropped() {
        // SPEC §0.3, end to end through this crate's trait rather than through
        // the ledger's own tests.
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let (l, mut rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Microphone, true).unwrap();

        let permit = GrantedMic::request(&l).unwrap();
        assert_eq!(
            rx.try_recv().unwrap().kind,
            EventKind::InvasiveActive { sense: SenseId::Microphone, active: true }
        );
        assert!(l.rows().iter().find(|r| r.id == SenseId::Microphone).unwrap().live);

        drop(permit);
        assert_eq!(
            rx.try_recv().unwrap().kind,
            EventKind::InvasiveActive { sense: SenseId::Microphone, active: false }
        );
        assert!(!l.rows().iter().find(|r| r.id == SenseId::Microphone).unwrap().live);
    }

    /// The property the whole design exists for: a `Listener` is a thing you can
    /// only build out of a granted permit, and letting go of the `Listener`
    /// lowers the tell. No capture device is opened anywhere in this test — the
    /// source is a `FakeMic`.
    #[test]
    fn a_listener_cannot_outlive_the_tell_it_raised() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let (l, mut rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Microphone, true).unwrap();

        let listener = Listener::open(
            GrantedMic::request(&l).unwrap(),
            Box::new(FakeMic::new(16_000)),
            ListenConfig::default(),
        )
        .unwrap();
        let _ = rx.try_recv(); // the tell going up
        assert!(l.rows().iter().find(|r| r.id == SenseId::Microphone).unwrap().live);

        listener.close();
        assert_eq!(
            rx.try_recv().unwrap().kind,
            EventKind::InvasiveActive { sense: SenseId::Microphone, active: false },
            "closing the listener must lower the tell"
        );
    }

    #[test]
    fn transcribed_speech_reaches_the_bus_as_an_observation() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let (l, mut rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Microphone, true).unwrap();
        let permit = GrantedMic::request(&l).unwrap();
        let _ = rx.try_recv();

        permit
            .publish(Observation::Speech { text: "hello".into(), final_: true })
            .unwrap();
        match rx.try_recv().unwrap().kind {
            EventKind::Sensed(Observation::Speech { text, final_ }) => {
                assert_eq!(text, "hello");
                assert!(final_);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(l.uses_today(SenseId::Microphone), 1, "every use is recorded");
    }

    #[test]
    fn revoking_consent_stops_publication_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let (l, _rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Microphone, true).unwrap();
        let permit = GrantedMic::request(&l).unwrap();
        assert!(permit.still_permitted());

        l.set_enabled(SenseId::Microphone, false).unwrap();
        assert!(!permit.still_permitted());
        let err = permit
            .publish(Observation::Speech { text: "too late".into(), final_: false })
            .unwrap_err();
        assert!(matches!(err, VoiceError::ConsentRevoked), "{err:?}");
    }

    #[test]
    fn the_permit_cannot_be_used_to_speak_for_another_sense() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let (l, _rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Microphone, true).unwrap();
        let permit = GrantedMic::request(&l).unwrap();
        assert!(permit
            .publish(Observation::Clipboard { len: 4, kind: "text/plain".into() })
            .is_err());
    }

    #[test]
    fn a_whole_utterance_flows_from_a_fake_microphone_to_the_bus() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let (l, _rx) = ledger(tmp.path());
        l.set_enabled(SenseId::Microphone, true).unwrap();

        let mut listener = Listener::open(
            GrantedMic::request(&l).unwrap(),
            Box::new(FakeMic::new(16_000)),
            ListenConfig::default(),
        )
        .unwrap();
        let mut stt = FakeStt::saying("hello there wisp", 1_200);

        listener.ptt_down(0);
        let mut now = 0u64;
        let mut published = 0usize;
        for _ in 0..80 {
            now += 100;
            listener.feed(&vec![0.3f32; 1_600], now).unwrap();
            published += listener.pump(&mut stt, now).unwrap().len();
        }
        listener.ptt_up(now);
        published += listener.pump(&mut stt, now + 100).unwrap().len();
        assert!(published > 0, "nothing reached the bus");
        assert!(l.uses_today(SenseId::Microphone) > 0, "and nothing was recorded");
    }
}
