//! Holds the printer's camera connection for the life of the process and
//! records every frame, which all camera views read from. The printer serves
//! one camera client, so Bambu Studio's view won't work while this runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::clock::{self, Clock};
use crate::history::Store;
use crate::p1s::{self, Link};

pub const MIN_RETRY: Duration = Duration::from_secs(1);
pub const MAX_RETRY: Duration = Duration::from_secs(30);

/// A recorded frame: when it was taken (unix seconds) and the JPEG.
pub type Frame = Arc<(i64, Vec<u8>)>;

#[derive(Clone)]
pub struct Hub {
    link: Link,
    store: Store,
    clock: Clock,
    frames: broadcast::Sender<Frame>,
}

impl Hub {
    pub fn new(link: Link, store: Store, clock: Clock) -> Hub {
        Hub {
            link,
            store,
            clock,
            frames: broadcast::Sender::new(4),
        }
    }

    /// Every frame as it is recorded. A slow receiver skips ahead rather than
    /// holding anything up.
    pub fn frames(&self) -> broadcast::Receiver<Frame> {
        self.frames.subscribe()
    }

    /// Keeps the camera connected — retrying with backoff, and redialling
    /// whenever the printer is reconfigured — forever. Spawn once.
    pub async fn run(self) {
        let mut config = self.link.watch_config();
        let mut delay = MIN_RETRY;
        loop {
            let conf = config.borrow_and_update().clone();
            if !conf.complete() {
                // Nothing to dial until a printer is set up.
                if config.changed().await.is_err() {
                    return;
                }
                continue;
            }
            let got_frame = AtomicBool::new(false);
            let stream = p1s::stream_frames(
                &conf.ip,
                self.link.camera_port(),
                "bblp",
                &conf.access_code,
                |jpeg| {
                    got_frame.store(true, Ordering::Relaxed);
                    self.record(jpeg);
                },
            );
            let err = tokio::select! {
                result = stream => result.err(),
                _ = config.changed() => {
                    delay = MIN_RETRY;
                    continue;
                }
            };
            if got_frame.load(Ordering::Relaxed) {
                // It was working before it dropped.
                delay = MIN_RETRY;
            }
            tracing::info!(
                "camera: stream ended, retrying in {delay:?}: {}",
                err.map_or("".into(), |e| e.to_string())
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = config.changed() => {}
            }
            delay = next_backoff(delay);
        }
    }

    fn record(&self, jpeg: Vec<u8>) {
        let ts = clock::secs((self.clock)());
        if let Err(err) = self.store.insert_frame(ts, &jpeg) {
            tracing::warn!("camera: storing a frame: {err}");
        }
        let _ = self.frames.send(Arc::new((ts, jpeg)));
    }
}

/// Doubles `prev`, capped at MAX_RETRY.
pub fn next_backoff(prev: Duration) -> Duration {
    (prev * 2).min(MAX_RETRY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_a_cap() {
        assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(16)), MAX_RETRY);
        assert_eq!(next_backoff(MAX_RETRY), MAX_RETRY);
    }
}
