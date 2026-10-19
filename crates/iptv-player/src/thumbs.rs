//! Seek-bar preview frames: a worker thread that owns a second invisible mpv
//! ([`ThumbMpv`]) on the playing movie/episode and answers "frame near time
//! T" requests, newest request first. The source is opened on demand and
//! closed when playback changes, so the extra provider connection only exists
//! while the user hovers the seek bar. Never logs or stores the URL anywhere
//! but in the command it sends to the worker.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use crate::thumb_mpv::{Thumb, ThumbMpv};

/// Width of a preview in pixels.
pub const THUMB_WIDTH: u32 = 240;

/// Previews are taken every `BUCKET_SECS` seconds of the video.
pub const BUCKET_SECS: f64 = 10.0;

const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const SEEK_TIMEOUT: Duration = Duration::from_secs(15);

/// Bucket (index) a position belongs to.
pub fn bucket_of(secs: f64) -> i64 {
    (secs.max(0.0) / BUCKET_SECS).round() as i64
}

/// Time in seconds a bucket is taken at.
pub fn bucket_secs(bucket: i64) -> f64 {
    bucket as f64 * BUCKET_SECS
}

/// The cached bucket closest to `bucket` (itself if cached).
pub fn nearest(cached: impl IntoIterator<Item = i64>, bucket: i64) -> Option<i64> {
    cached.into_iter().min_by_key(|b| ((b - bucket).abs(), *b))
}

enum Cmd {
    Open { gen: u64, url: String },
    Want { gen: u64, bucket: i64 },
    Close,
}

pub enum Reply {
    /// The source could not be opened (e.g. the provider refused a second connection).
    OpenFailed { gen: u64 },
    Frame { gen: u64, bucket: i64, thumb: Thumb },
    /// A frame could not be taken.
    Failed { gen: u64, bucket: i64 },
}

pub struct ThumbService {
    tx: Sender<Cmd>,
    rx: Receiver<Reply>,
}

impl ThumbService {
    /// `wake` is called from the worker when a reply is waiting.
    pub fn new(user_agent: String, wake: impl Fn() + Send + 'static) -> ThumbService {
        let (tx, cmd_rx) = channel();
        let (reply_tx, rx) = channel();
        std::thread::Builder::new()
            .name("thumbs".into())
            .spawn(move || worker(user_agent, cmd_rx, reply_tx, wake))
            .expect("spawn thumbs thread");
        ThumbService { tx, rx }
    }

    /// Open `url` for generation `gen` (replaces any other source).
    pub fn open(&self, gen: u64, url: &str) {
        let _ = self.tx.send(Cmd::Open { gen, url: url.to_string() });
    }

    /// Ask for the frame of `bucket` (only the newest request is answered).
    pub fn want(&self, gen: u64, bucket: i64) {
        let _ = self.tx.send(Cmd::Want { gen, bucket });
    }

    /// Drop the source (and its connection).
    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }

    pub fn poll(&self) -> Vec<Reply> {
        self.rx.try_iter().collect()
    }
}

fn worker(user_agent: String, rx: Receiver<Cmd>, tx: Sender<Reply>, wake: impl Fn()) {
    let mut mpv: Option<(u64, ThumbMpv)> = None;
    let send = |r: Reply| {
        let _ = tx.send(r);
        wake();
    };
    while let Ok(first) = rx.recv() {
        // Handle a burst of commands at once: opens/closes in order, only the
        // newest Want.
        let mut want = None;
        for cmd in std::iter::once(first).chain(rx.try_iter()) {
            match cmd {
                Cmd::Open { gen, url } => {
                    want = None;
                    mpv = None;
                    match ThumbMpv::new(&user_agent, THUMB_WIDTH).and_then(|m| m.open(&url, OPEN_TIMEOUT).map(|_| m)) {
                        Ok(m) => mpv = Some((gen, m)),
                        Err(e) => {
                            eprintln!("thumbnails: {e}");
                            send(Reply::OpenFailed { gen });
                        }
                    }
                }
                Cmd::Want { gen, bucket } => want = Some((gen, bucket)),
                Cmd::Close => {
                    want = None;
                    mpv = None;
                }
            }
        }
        if let (Some((gen, bucket)), Some((open_gen, m))) = (want, &mpv) {
            if gen == *open_gen {
                match m.frame_at(bucket_secs(bucket), SEEK_TIMEOUT) {
                    Ok(thumb) => send(Reply::Frame { gen, bucket, thumb }),
                    Err(e) => {
                        eprintln!("thumbnails: {e}");
                        send(Reply::Failed { gen, bucket });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_round_to_the_nearest_ten_seconds() {
        assert_eq!(bucket_of(0.0), 0);
        assert_eq!(bucket_of(4.9), 0);
        assert_eq!(bucket_of(5.1), 1);
        assert_eq!(bucket_of(3600.0), 360);
        assert_eq!(bucket_of(-3.0), 0);
        assert_eq!(bucket_secs(360), 3600.0);
    }

    #[test]
    fn nearest_prefers_the_exact_then_the_closest_then_the_earlier() {
        assert_eq!(nearest([1, 5, 9], 5), Some(5));
        assert_eq!(nearest([1, 5, 9], 7), Some(5));
        assert_eq!(nearest([1, 5, 9], 8), Some(9));
        assert_eq!(nearest([2, 6], 4), Some(2), "tie: the earlier one");
        assert_eq!(nearest([], 4), None);
    }
}
