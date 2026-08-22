//! Streaming a generation without pretending decoding is asynchronous.
//!
//! [`crate::backend::Backend::generate`] is a blocking loop, because that is
//! what it is. This is the one place in the crate that knows how to put it on a
//! blocking thread and turn the callback into a channel, so a speech bubble can
//! fill in as tokens arrive.
//!
//! Cancellation is the interesting part: dropping the receiver makes the next
//! `send` fail, the sink returns [`Flow::Stop`], and decoding ends at the next
//! token boundary. That is how a T2 downgrade or the operator starting to talk
//! stops a reply mid-sentence without a second cancellation mechanism.

use tokio::sync::mpsc;

use crate::backend::{Flow, GenRequest, Generated, ModelHandle};
use crate::error::Result;
use crate::manager::{lock_backend, SharedBackend};

/// One piece of a reply on its way to the surface.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub text: String,
    pub index: u32,
}

/// Start a generation on a blocking thread.
///
/// Returns the channel of pieces and a join handle carrying the final
/// [`Generated`]. Both must be consumed: dropping the receiver cancels, and
/// awaiting the handle is how errors surface.
pub fn spawn(
    backend: SharedBackend,
    handle: ModelHandle,
    req: GenRequest,
    buffer: usize,
) -> (
    mpsc::Receiver<Piece>,
    tokio::task::JoinHandle<Result<Generated>>,
) {
    let (tx, rx) = mpsc::channel(buffer.max(1));
    let join = tokio::task::spawn_blocking(move || {
        let mut b = lock_backend(&backend);
        b.generate(handle, &req, &mut |chunk| {
            // A closed receiver means nobody is listening any more. Stopping is
            // not an error; it is the whole cancellation story.
            match tx.blocking_send(Piece {
                text: chunk.text.to_string(),
                index: chunk.index,
            }) {
                Ok(()) => Flow::Continue,
                Err(_) => Flow::Stop,
            }
        })
    });
    (rx, join)
}

/// Collect a whole streamed reply. For callers that want the text and not the
/// theatre.
pub async fn collect(
    backend: SharedBackend,
    handle: ModelHandle,
    req: GenRequest,
) -> Result<Generated> {
    let (mut rx, join) = spawn(backend, handle, req, 64);
    while rx.recv().await.is_some() {}
    join.await.unwrap_or_else(|e| {
        Err(crate::error::MindError::Inference(format!(
            "the decoding thread went away: {e}"
        )))
    })
}
