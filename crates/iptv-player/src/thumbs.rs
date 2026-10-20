//! Seek-bar preview frames: a worker thread that owns a second invisible mpv
//! ([`ThumbMpv`]) on the playing movie/episode and answers "frame near time
//! T" requests, newest request first. The same worker runs the tail scan for
//! "Next episode" (credits detection) on that connection. The source is
//! opened on demand (first seek-bar hover / arrow-key seek, or a tail scan)
//! and stays open until playback changes (reset), so the extra provider
//! connection exists from then until the next video or stop. Never logs or
//! stores the URL anywhere but in the command it sends to the worker.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use crate::state::credits::{classify, FrameKind};
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

/// One tail-scan sample: time and what the frame looks like (`None` = the
/// frame could not be taken).
pub type Sample = (f64, Option<FrameKind>);

/// Takes the frame at each of `times` in order via `take`; `aborted` is asked
/// before every frame and stops the scan (`None`) when it says so.
fn run_scan(
    times: &[f64],
    mut take: impl FnMut(f64) -> Option<FrameKind>,
    mut aborted: impl FnMut() -> bool,
) -> Option<Vec<Sample>> {
    let mut out = Vec::with_capacity(times.len());
    for &t in times {
        if aborted() {
            return None;
        }
        out.push((t, take(t)));
    }
    Some(out)
}

/// Whether the newest Want is served before the scan resumes: only when the
/// scan was interrupted and a Want came in since (a Want queued together with
/// the scan waits for it to finish).
fn serve_want_first(scan_interrupted: bool, want_since_interrupt: bool) -> bool {
    scan_interrupted && want_since_interrupt
}

/// Takes the frame for `want` if it belongs to the open source and replies.
fn answer_want(want: Option<(u64, i64)>, mpv: &Option<(u64, ThumbMpv)>, send: &impl Fn(Reply)) {
    if let (Some((gen, bucket)), Some((open_gen, m))) = (want, mpv) {
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

enum Cmd {
    Open { gen: u64, url: String },
    Want { gen: u64, bucket: i64 },
    Scan { gen: u64, times: Vec<f64> },
    Close,
}

pub enum Reply {
    /// The source could not be opened (e.g. the provider refused a second connection).
    OpenFailed { gen: u64 },
    Frame { gen: u64, bucket: i64, thumb: Thumb },
    /// A frame could not be taken.
    Failed { gen: u64, bucket: i64 },
    /// A tail scan finished (sent once per scan, never when it was dropped).
    Scanned { gen: u64, samples: Vec<Sample> },
}

/// A scan in progress. Frames already classified are kept in `memo`, so a
/// scan interrupted by a newer command resumes without taking them again.
struct ScanJob {
    gen: u64,
    times: Vec<f64>,
    memo: HashMap<u64, Option<FrameKind>>,
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

    /// Classify the frames at `times` of the source opened for `gen` (replaces
    /// any earlier scan). Answered once with [`Reply::Scanned`]; dropped
    /// without a reply when the source is closed or replaced.
    pub fn scan(&self, gen: u64, times: Vec<f64>) {
        let _ = self.tx.send(Cmd::Scan { gen, times });
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
    // Commands taken off the channel while a scan checked for newer work.
    let mut pending: VecDeque<Cmd> = VecDeque::new();
    let mut want: Option<(u64, i64)> = None;
    let mut scan: Option<ScanJob> = None;
    // The scan stopped early because a command came in.
    let mut interrupted = false;
    loop {
        // Handle a burst of commands at once: opens/closes in order, only the
        // newest Want and the newest Scan.
        let mut burst: Vec<Cmd> = pending.drain(..).chain(rx.try_iter()).collect();
        if burst.is_empty() && scan.is_none() && want.is_none() {
            match rx.recv() {
                Ok(cmd) => burst.push(cmd),
                Err(_) => return,
            }
            burst.extend(rx.try_iter());
        }
        let mut want_arrived = false;
        for cmd in burst {
            match cmd {
                Cmd::Open { gen, url } => {
                    want = None;
                    scan = None;
                    mpv = None;
                    match ThumbMpv::new(&user_agent, THUMB_WIDTH).and_then(|m| m.open(&url, OPEN_TIMEOUT).map(|_| m)) {
                        Ok(m) => mpv = Some((gen, m)),
                        Err(e) => {
                            eprintln!("thumbnails: {e}");
                            send(Reply::OpenFailed { gen });
                        }
                    }
                }
                Cmd::Want { gen, bucket } => {
                    want = Some((gen, bucket));
                    want_arrived = true;
                }
                Cmd::Scan { gen, times } => scan = Some(ScanJob { gen, times, memo: HashMap::new() }),
                Cmd::Close => {
                    want = None;
                    scan = None;
                    mpv = None;
                }
            }
        }
        // A Want that interrupted the scan is served at once (one frame); the
        // scan then resumes where it was (frames taken so far are memoised).
        if serve_want_first(std::mem::take(&mut interrupted), want_arrived) {
            answer_want(want.take(), &mpv, &send);
            continue;
        }
        // Otherwise the scan goes first; any command arriving meanwhile
        // interrupts it and is handled before it resumes.
        if let Some(job) = &mut scan {
            match &mpv {
                Some((open_gen, m)) if *open_gen == job.gen => {
                    let ScanJob { gen, times, memo } = job;
                    let done = run_scan(
                        times,
                        |t| {
                            *memo.entry(t.to_bits()).or_insert_with(|| match m.frame_at(t, SEEK_TIMEOUT) {
                                Ok(th) => Some(classify(&th.rgba, th.width, th.height)),
                                Err(e) => {
                                    eprintln!("tail scan: {e}");
                                    None
                                }
                            })
                        },
                        || {
                            if pending.is_empty() {
                                pending.extend(rx.try_recv().ok());
                            }
                            !pending.is_empty()
                        },
                    );
                    match done {
                        Some(samples) => {
                            send(Reply::Scanned { gen: *gen, samples });
                            scan = None;
                        }
                        None => {
                            interrupted = true;
                            continue;
                        }
                    }
                }
                // No source for this scan (closed, failed to open, or another video).
                _ => scan = None,
            }
        }
        answer_want(want.take(), &mpv, &send);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_want_that_interrupts_a_scan_is_served_before_it_resumes() {
        assert!(serve_want_first(true, true), "interrupting Want: served at once");
        assert!(!serve_want_first(false, true), "Want queued with the scan: after the scan");
        assert!(!serve_want_first(true, false), "interrupted by something else: scan resumes");
        assert!(!serve_want_first(false, false));
    }

    #[test]
    fn scan_collects_in_order() {
        let mut asked = Vec::new();
        let out = run_scan(
            &[10.0, 20.0, 30.0],
            |t| {
                asked.push(t);
                if t == 20.0 {
                    None
                } else {
                    Some(FrameKind::Credits)
                }
            },
            || false,
        );
        assert_eq!(asked, vec![10.0, 20.0, 30.0]);
        assert_eq!(
            out,
            Some(vec![(10.0, Some(FrameKind::Credits)), (20.0, None), (30.0, Some(FrameKind::Credits))])
        );
    }

    #[test]
    fn scan_aborts_when_told() {
        let taken = std::cell::Cell::new(0);
        let out = run_scan(
            &[1.0, 2.0, 3.0, 4.0],
            |_| {
                taken.set(taken.get() + 1);
                Some(FrameKind::Black)
            },
            || taken.get() >= 2,
        );
        assert_eq!(out, None);
        assert_eq!(taken.get(), 2, "no frame is taken after the abort");
        assert_eq!(run_scan(&[1.0], |_| Some(FrameKind::Black), || true), None);
    }

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
