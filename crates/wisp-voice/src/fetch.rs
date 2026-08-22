//! The one place this crate is allowed to touch the network, and the trait that
//! keeps that fact out of everything else.
//!
//! SPEC §0.2a permits exactly one kind of egress: model downloads from pinned
//! URLs with pinned hashes. [`models`](crate::models) owns the pinning; this
//! module owns the socket, and it owns it behind [`Fetcher`] so that
//! [`crate::models::ModelStore::ensure`] — the code with all the interesting
//! logic in it — never mentions HTTP at all. The entire download story
//! (resume, verify-then-move, refusal on mismatch) is therefore testable with
//! no network, which is why `cargo test -p wisp-voice` passes on the default
//! feature set where `ureq` is not even compiled.
//!
//! ## Why the trait is "give me a `Read` from byte N", and nothing more
//!
//! A richer trait would be a worse one. Retry policy, backoff, progress
//! reporting and the `.part` file all belong to the store, because the store is
//! the thing that knows the pinned length and the pinned hash; a fetcher that
//! also had opinions about those would have two places that can disagree about
//! whether a download finished. So [`Fetcher::get`] answers one question —
//! *give me the bytes of this URL starting at offset N* — and reports one fact
//! the store cannot work out for itself: whether the server actually honoured
//! the offset.
//!
//! ## The resume bug this is shaped to prevent
//!
//! A server that ignores `Range:` answers `200 OK` with the *whole* resource
//! while the client believes it is receiving the tail. Appending that to a
//! partial file produces a file of the right length made of the wrong bytes —
//! at which point "resume" has turned a slow download into a corrupt one, and
//! the only thing standing between the operator and a broken model is the
//! sha256. That is too close to the edge. [`Fetched::resumed`] is therefore
//! `true` only on a real `206 Partial Content`, and the store restarts from
//! zero whenever it is `false`, wasting bandwidth rather than risking bytes.

use std::collections::HashMap;
use std::io::Read;

use crate::{Result, VoiceError};

/// Where a fetch should start. Byte offsets only — this is a file transfer, not
/// a media player, so there is never a need for a suffix or an end bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Range {
    pub from: u64,
}

impl Range {
    pub fn from(from: u64) -> Self {
        Range { from }
    }

    /// The header value a byte-range request needs. Open-ended on purpose: the
    /// store already knows how long the resource is meant to be and stops on
    /// its own, so asking for an end bound only adds a way to disagree.
    pub fn header(&self) -> String {
        format!("bytes={}-", self.from)
    }
}

/// An open byte stream, and what the server said about it.
pub struct Fetched {
    /// The body. Reading it to EOF yields the bytes from the requested offset
    /// onwards — *unless* `resumed` is false, in which case it yields the whole
    /// resource from zero regardless of what was asked for.
    pub body: Box<dyn Read + Send>,
    /// Length of the **whole** resource, not of this response. `None` when the
    /// server declined to say; the store falls back to the pinned length.
    pub total: Option<u64>,
    /// The offset was honoured. False for a fresh `from = 0` fetch and false
    /// for a server that answered `200` to a `Range:` request.
    pub resumed: bool,
}

impl std::fmt::Debug for Fetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetched")
            .field("total", &self.total)
            .field("resumed", &self.resumed)
            .finish_non_exhaustive()
    }
}

/// Something that can open a byte stream for a URL.
///
/// Blocking, like every other engine trait in this crate, and for the same
/// reason: the caller owns the thread so the governor can take it away.
pub trait Fetcher: Send {
    /// Open a byte stream for `url`, starting at byte `from`.
    ///
    /// An implementation that cannot honour `from` must say so via
    /// [`Fetched::resumed`] rather than fail — a server without range support
    /// is a slow path, not an error.
    fn get(&mut self, url: &str, from: u64) -> Result<Fetched>;
}

// ---------------------------------------------------------------------------
// The real one
// ---------------------------------------------------------------------------

/// HTTPS through `ureq`, rustls, no compression.
///
/// Compression is off in `Cargo.toml` rather than here, but the reason lives
/// with the code that depends on it: a transparently decompressed body makes
/// both the `Content-Range` arithmetic and the pinned sha256 describe different
/// things than the bytes we are about to write to disk.
///
/// Every URL in [`crate::models::MANIFEST`] is a `huggingface.co/.../resolve/`
/// address that answers `302` to a CDN host, so a fetch is always at least two
/// round trips and the `Range:` header has to survive a cross-host redirect.
/// `ureq` re-sends it (only *auth* headers are dropped across hosts), and the
/// CDN answers `206` with a `Content-Range` — but this code does not depend on
/// either fact: if the header were lost the CDN would answer `200`, `resumed`
/// would come back false, and the store would restart from zero. Slow, not
/// broken, which is the only failure mode worth having here.
#[cfg(feature = "net")]
pub struct HttpFetcher {
    agent: ureq::Agent,
    /// A second agent pinned to IPv4. See [`HttpFetcher::call`].
    v4: ureq::Agent,
    /// Once IPv6 has failed on this host, stop paying the timeout for it.
    v6_is_dead: std::cell::Cell<bool>,
}

#[cfg(feature = "net")]
impl Default for HttpFetcher {
    fn default() -> Self {
        HttpFetcher::new()
    }
}

#[cfg(feature = "net")]
impl HttpFetcher {
    pub fn new() -> Self {
        // `http_status_as_error(false)` because a 206 and a 200 are both
        // successes we have to tell apart, and a 416 is a recoverable "your
        // resume offset is stale" rather than a failure. Getting statuses as
        // errors would mean parsing them back out of an error type.
        let base = || {
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .user_agent(concat!("nx-wisp/", env!("CARGO_PKG_VERSION")))
        };
        HttpFetcher {
            agent: ureq::Agent::new_with_config(base().build()),
            v4: ureq::Agent::new_with_config(
                base().ip_family(ureq::config::IpFamily::Ipv4Only).build(),
            ),
            v6_is_dead: std::cell::Cell::new(false),
        }
    }

    /// Does this transport error mean "there is no path to that address"?
    ///
    /// `ENETUNREACH` / `EHOSTUNREACH` are the two the broken-IPv6 case produces.
    /// Matched on the message because `ureq` flattens the `io::Error` into its
    /// own `Transport` variant by the time we see it, and the alternative —
    /// treating *every* transport failure as a reason to retry — would double
    /// the wait on a genuinely offline machine.
    fn is_unreachable(why: &str) -> bool {
        let w = why.to_ascii_lowercase();
        w.contains("network is unreachable")
            || w.contains("no route to host")
            || w.contains("host is unreachable")
            || w.contains("address family not supported")
    }

    fn header(res: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
        res.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    /// `bytes 200-1023/1024` → `Some(1024)`. `bytes 200-1023/*` → `None`,
    /// because an unknown total is not the same as a total of zero.
    fn total_from_content_range(v: &str) -> Option<u64> {
        v.rsplit('/').next()?.trim().parse().ok()
    }

    /// One request, with an IPv4 fallback.
    ///
    /// ## Why this is here
    ///
    /// The machine this was built on has an IPv6 default route learned from a
    /// router advertisement and **no working IPv6 path**. That is not exotic —
    /// it is what a great many consumer connections look like, and it is why
    /// `curl` grew happy-eyeballs. `ureq` has no such fallback: its resolver
    /// hands back whatever the system returns first, and `getent` on this host
    /// returns the AAAA record, so every download failed with `ENETUNREACH`
    /// while `curl` to the same URL worked fine.
    ///
    /// Discovered the honest way: the model fetch for a real Piper voice failed
    /// on this machine. Without this, "download on first use" is a feature that
    /// does not work for the operator it was written for.
    ///
    /// The fallback is sticky per fetcher, so a 200 MB download does not pay the
    /// failed IPv6 connect once per resumed chunk.
    fn call(&self, url: &str, from: Option<u64>) -> Result<ureq::http::Response<ureq::Body>> {
        let send = |agent: &ureq::Agent| {
            let mut req = agent.get(url);
            if let Some(from) = from {
                req = req.header("Range", Range::from(from).header());
            }
            req.call()
        };

        if self.v6_is_dead.get() {
            return send(&self.v4).map_err(|e| VoiceError::Fetch {
                url: url.to_string(),
                why: e.to_string(),
            });
        }

        match send(&self.agent) {
            Ok(r) => Ok(r),
            Err(e) => {
                let why = e.to_string();
                if !Self::is_unreachable(&why) {
                    return Err(VoiceError::Fetch { url: url.to_string(), why });
                }
                tracing::info!(url, %why, "no route on the preferred family; retrying over IPv4");
                self.v6_is_dead.set(true);
                send(&self.v4).map_err(|e2| VoiceError::Fetch {
                    url: url.to_string(),
                    // Both failures, so "it is broken" does not turn into a
                    // support thread about which half was broken.
                    why: format!("{why}; and over IPv4: {e2}"),
                })
            }
        }
    }
}

#[cfg(feature = "net")]
impl Fetcher for HttpFetcher {
    fn get(&mut self, url: &str, from: u64) -> Result<Fetched> {
        let want_range = from > 0;
        let mut res = self.call(url, want_range.then_some(from))?;

        // 416 means the partial file is at least as long as the resource — a
        // stale `.part` from a manifest bump, most likely. Drop the offset and
        // take the whole thing; the store truncates when `resumed` is false.
        if res.status().as_u16() == 416 && want_range {
            tracing::debug!(url, from, "server rejected the resume offset; restarting");
            res = self.call(url, None)?;
        }

        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(VoiceError::Fetch {
                url: url.to_string(),
                why: format!("HTTP {status}"),
            });
        }

        // The one line that decides whether resume is safe. A 200 here is a
        // server that took the `Range` header and threw it away.
        let resumed = want_range && status == 206;
        if want_range && !resumed {
            tracing::info!(url, from, "server ignored Range; restarting from zero");
        }

        let total = if resumed {
            Self::header(&res, "content-range")
                .as_deref()
                .and_then(Self::total_from_content_range)
        } else {
            Self::header(&res, "content-length")
                .and_then(|v| v.trim().parse::<u64>().ok())
        };

        Ok(Fetched {
            body: Box::new(res.into_body().into_reader()),
            total,
            resumed,
        })
    }
}

// ---------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------

/// One request a [`FakeNet`] was asked to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub url: String,
    pub from: u64,
}

/// How a [`FakeNet`] misbehaves for one URL.
///
/// These are not hypothetical failure modes. Every one of them is something a
/// HuggingFace CDN edge, a captive portal or a corporate proxy does in the
/// field, and each maps to a test the download path has to pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serve {
    /// Everything that was asked for, then a clean EOF.
    Whole,
    /// Send `after` bytes of this response and then die, as a dropped
    /// connection does: the reader yields those bytes and then an
    /// `UnexpectedEof`. Applies to the next `times` responses, after which the
    /// URL behaves like [`Serve::Whole`] — so a test can say "fail once, then
    /// let the resume through".
    Truncated { after: u64, times: usize },
    /// Accept the `Range:` header and ignore it: `200`, whole body, from zero.
    IgnoresRange,
}

#[derive(Debug, Clone)]
struct Blob {
    bytes: Vec<u8>,
    serve: Serve,
}

/// An in-memory fetcher. Always compiled, `pub`, and the only fetcher the test
/// suite ever uses.
///
/// It is `pub` rather than `cfg(test)` for the same reason [`crate::tts::FakeTts`]
/// is: the binary has to be able to run her with nothing installed, and a
/// developer has to be able to reproduce a download bug without a network.
#[derive(Debug, Clone, Default)]
pub struct FakeNet {
    blobs: HashMap<String, Blob>,
    log: Vec<Asked>,
}

impl FakeNet {
    pub fn new() -> Self {
        FakeNet::default()
    }

    /// Serve `bytes` at `url`, normally.
    pub fn serve(&mut self, url: impl Into<String>, bytes: impl Into<Vec<u8>>) -> &mut Self {
        self.put(url, bytes, Serve::Whole)
    }

    /// Serve `bytes` at `url`, dropping the connection after `after` bytes of
    /// each of the next `times` responses.
    pub fn serve_truncated(
        &mut self,
        url: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
        after: u64,
        times: usize,
    ) -> &mut Self {
        self.put(url, bytes, Serve::Truncated { after, times })
    }

    /// Serve `bytes` at `url` from a server that pretends not to understand
    /// `Range:`.
    pub fn serve_ignoring_range(
        &mut self,
        url: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
    ) -> &mut Self {
        self.put(url, bytes, Serve::IgnoresRange)
    }

    fn put(&mut self, url: impl Into<String>, bytes: impl Into<Vec<u8>>, serve: Serve) -> &mut Self {
        self.blobs.insert(
            url.into(),
            Blob {
                bytes: bytes.into(),
                serve,
            },
        );
        self
    }

    /// Stop serving a URL at all, so the next request 404s. There is no
    /// `Serve::Missing`: absence is how a missing thing is spelt.
    pub fn forget(&mut self, url: &str) -> &mut Self {
        self.blobs.remove(url);
        self
    }

    /// Every request, in order, across all URLs.
    pub fn log(&self) -> &[Asked] {
        &self.log
    }

    /// How many requests this URL received.
    pub fn hits(&self, url: &str) -> usize {
        self.log.iter().filter(|a| a.url == url).count()
    }

    /// The offsets this URL was asked for, in order. This is what the resume
    /// tests assert on: a working resume asks for `[0, 4096]`, a broken one
    /// asks for `[0, 0]`.
    pub fn offsets(&self, url: &str) -> Vec<u64> {
        self.log
            .iter()
            .filter(|a| a.url == url)
            .map(|a| a.from)
            .collect()
    }

    pub fn requests(&self) -> usize {
        self.log.len()
    }

    pub fn forget_log(&mut self) {
        self.log.clear();
    }
}

impl Fetcher for FakeNet {
    fn get(&mut self, url: &str, from: u64) -> Result<Fetched> {
        self.log.push(Asked {
            url: url.to_string(),
            from,
        });

        let blob = self.blobs.get_mut(url).ok_or_else(|| VoiceError::Fetch {
            url: url.to_string(),
            why: "HTTP 404".to_string(),
        })?;
        let total = blob.bytes.len() as u64;

        // Decide the response first, then mutate the remaining fault budget, so
        // that a URL set to fail twice really does fail exactly twice.
        let (resumed, cut) = match &mut blob.serve {
            Serve::Whole => (from > 0, None),
            Serve::IgnoresRange => (false, None),
            Serve::Truncated { after, times } if *times > 0 => {
                *times -= 1;
                (from > 0, Some(*after))
            }
            Serve::Truncated { .. } => (from > 0, None),
        };

        let start = if resumed { from.min(total) as usize } else { 0 };
        let mut chunk = blob.bytes[start..].to_vec();
        let dropped = match cut {
            Some(after) if (after as usize) < chunk.len() => {
                chunk.truncate(after as usize);
                true
            }
            // A cut at or past the end of the body is not a dropped
            // connection, it is a complete response.
            _ => false,
        };

        Ok(Fetched {
            body: Box::new(Wire {
                rest: std::io::Cursor::new(chunk),
                dropped,
            }),
            total: Some(total),
            resumed,
        })
    }
}

/// A body that may end in a dead socket rather than an EOF.
struct Wire {
    rest: std::io::Cursor<Vec<u8>>,
    dropped: bool,
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.rest.read(buf)?;
        if n == 0 && self.dropped {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "FakeNet dropped the connection",
            ));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(f: Fetched) -> (Vec<u8>, Option<std::io::Error>) {
        let mut body = f.body;
        let mut out = Vec::new();
        let mut buf = [0u8; 7]; // deliberately not a power of two
        loop {
            match body.read(&mut buf) {
                Ok(0) => return (out, None),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) => return (out, Some(e)),
            }
        }
    }

    #[test]
    fn a_whole_serve_yields_every_byte_and_a_clean_eof() {
        let mut net = FakeNet::new();
        net.serve("https://x/a", b"hello world".to_vec());
        let f = net.get("https://x/a", 0).unwrap();
        assert_eq!(f.total, Some(11));
        assert!(!f.resumed);
        let (bytes, err) = drain(f);
        assert_eq!(bytes, b"hello world");
        assert!(err.is_none());
    }

    #[test]
    fn an_offset_request_yields_only_the_tail_and_still_reports_the_whole_length() {
        let mut net = FakeNet::new();
        net.serve("https://x/a", b"hello world".to_vec());
        let f = net.get("https://x/a", 6).unwrap();
        assert!(f.resumed);
        assert_eq!(f.total, Some(11), "total is the resource, not the response");
        assert_eq!(drain(f).0, b"world");
    }

    #[test]
    fn a_truncated_body_ends_in_an_error_rather_than_a_quiet_eof() {
        let mut net = FakeNet::new();
        net.serve_truncated("https://x/a", b"hello world".to_vec(), 5, 1);
        let (bytes, err) = drain(net.get("https://x/a", 0).unwrap());
        assert_eq!(bytes, b"hello");
        assert_eq!(err.unwrap().kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_truncation_budget_is_spent_and_then_the_url_behaves() {
        let mut net = FakeNet::new();
        net.serve_truncated("https://x/a", b"hello world".to_vec(), 5, 1);
        assert_eq!(drain(net.get("https://x/a", 0).unwrap()).0, b"hello");
        let (bytes, err) = drain(net.get("https://x/a", 5).unwrap());
        assert_eq!(bytes, b" world");
        assert!(err.is_none());
    }

    #[test]
    fn a_server_that_ignores_range_says_so_rather_than_lying_about_the_offset() {
        let mut net = FakeNet::new();
        net.serve_ignoring_range("https://x/a", b"hello world".to_vec());
        let f = net.get("https://x/a", 6).unwrap();
        assert!(!f.resumed, "this is the whole point of the flag");
        assert_eq!(drain(f).0, b"hello world");
    }

    #[test]
    fn a_url_nobody_serves_is_a_fetch_error_naming_the_url() {
        let mut net = FakeNet::new();
        match net.get("https://x/missing", 0) {
            Err(VoiceError::Fetch { url, why }) => {
                assert_eq!(url, "https://x/missing");
                assert!(why.contains("404"), "{why}");
            }
            other => panic!("expected a Fetch error, got {other:?}"),
        }
    }

    #[test]
    fn requests_are_counted_per_url_and_remember_their_offsets() {
        let mut net = FakeNet::new();
        net.serve("https://x/a", vec![0u8; 16]);
        net.serve("https://x/b", vec![0u8; 16]);
        let _ = net.get("https://x/a", 0);
        let _ = net.get("https://x/a", 4);
        let _ = net.get("https://x/b", 0);
        assert_eq!(net.hits("https://x/a"), 2);
        assert_eq!(net.hits("https://x/b"), 1);
        assert_eq!(net.offsets("https://x/a"), vec![0, 4]);
        assert_eq!(net.requests(), 3);
    }

    #[test]
    fn a_forgotten_url_stops_being_served_without_forgetting_it_was_asked_for() {
        let mut net = FakeNet::new();
        net.serve("https://x/a", b"abc".to_vec());
        assert!(net.get("https://x/a", 0).is_ok());
        net.forget("https://x/a");
        assert!(net.get("https://x/a", 0).is_err());
        assert_eq!(net.hits("https://x/a"), 2);
    }

    #[test]
    fn an_offset_past_the_end_yields_nothing_rather_than_panicking() {
        let mut net = FakeNet::new();
        net.serve("https://x/a", b"abc".to_vec());
        assert_eq!(drain(net.get("https://x/a", 99).unwrap()).0, b"");
    }

    #[test]
    fn a_range_header_is_open_ended_because_the_pin_already_bounds_it() {
        assert_eq!(Range::from(0).header(), "bytes=0-");
        assert_eq!(Range::from(4096).header(), "bytes=4096-");
    }

    #[cfg(feature = "net")]
    #[test]
    fn a_content_range_footer_gives_the_whole_length_and_a_star_gives_nothing() {
        assert_eq!(
            HttpFetcher::total_from_content_range("bytes 200-1023/1024"),
            Some(1024)
        );
        assert_eq!(HttpFetcher::total_from_content_range("bytes 200-1023/*"), None);
    }
}
