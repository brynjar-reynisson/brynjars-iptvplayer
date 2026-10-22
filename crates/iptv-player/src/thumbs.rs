//! Seek-bar preview frames: a worker thread that owns a second invisible mpv
//! ([`ThumbMpv`]) on the playing movie/episode and answers "frame near time
//! T" requests, newest request first. The same worker runs the tail scan for
//! "Next episode" (credits detection) on that connection: a first pass over
//! the planned times, then a refinement pass around isolated Content samples
//! (`credits::refine_times`), answered once with the merged samples. The source is
//! opened on demand (first seek-bar hover / arrow-key seek, or a tail scan)
//! and stays open until playback changes (reset), so the extra provider
//! connection exists from then until the next video or stop, except that a
//! finished tail scan closes it when no preview is wanted
//! ([`Reply::Closed`]; the next hover opens it again). Never logs or
//! stores the URL anywhere but in the command it sends to the worker.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use crate::scan_dump;
use crate::state::credits::{classify, refine_times, FrameKind};
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

/// Takes the frame at each of `times` in order via `take`, keeping every
/// result in `memo` (so a resumed scan never takes a frame twice). `aborted`
/// is asked before each frame that is not memoised yet, except the first new
/// one of this call, and stops the scan (`None`) when it says so: every
/// resume makes progress, so a stream of preview requests cannot starve it.
fn run_scan(
    times: &[f64],
    memo: &mut HashMap<u64, Option<FrameKind>>,
    mut take: impl FnMut(f64) -> Option<FrameKind>,
    mut aborted: impl FnMut() -> bool,
) -> Option<Vec<Sample>> {
    let mut out = Vec::with_capacity(times.len());
    let mut took_new = false;
    for &t in times {
        let kind = match memo.get(&t.to_bits()) {
            Some(k) => *k,
            None => {
                if took_new && aborted() {
                    return None;
                }
                took_new = true;
                let k = take(t);
                memo.insert(t.to_bits(), k);
                k
            }
        };
        out.push((t, kind));
    }
    Some(out)
}

/// After the first pass of a scan over `times` gave `first`: all times
/// (sorted) including the refinement frames around isolated Content samples
/// ([`refine_times`]), or `None` when there is nothing to refine. Running
/// [`run_scan`] again over them with the same memo takes only the new frames
/// and returns the merged, time-sorted samples.
fn with_refinement(times: &[f64], first: &[Sample], duration: f64) -> Option<Vec<f64>> {
    let extra = refine_times(first, duration);
    if extra.is_empty() {
        return None;
    }
    let mut all = times.to_vec();
    all.extend(extra);
    all.sort_by(f64::total_cmp);
    Some(all)
}

/// After a finished scan: close the source (the second provider connection)
/// unless a preview frame is waiting to be taken from it.
fn close_after_scan(want_pending: bool) -> bool {
    !want_pending
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
    Scan { gen: u64, times: Vec<f64>, duration: f64, dump: Option<PathBuf> },
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
    /// The source of `gen` was closed after its scan (no preview was
    /// wanted); the next request must open it again.
    Closed { gen: u64 },
}

/// A scan in progress. Frames already classified are kept in `memo`, so a
/// scan interrupted by a newer command resumes without taking them again.
struct ScanJob {
    gen: u64,
    times: Vec<f64>,
    duration: f64,
    /// The refinement frames were added to `times` (or there were none).
    refined: bool,
    memo: HashMap<u64, Option<FrameKind>>,
    /// `--console`: save every frame here when the scan finishes.
    dump: Option<PathBuf>,
    /// Frames kept for `dump` (empty without it), keyed by the time's bits.
    frames: HashMap<u64, Thumb>,
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
    /// any earlier scan), then the refinement frames around isolated Content
    /// samples (`duration` = the video's). Answered once with
    /// [`Reply::Scanned`] (all samples, time-sorted); dropped without a reply
    /// when the source is closed or replaced. With `dump`, the frames are
    /// saved there as PNG files before the reply.
    pub fn scan(&self, gen: u64, times: Vec<f64>, duration: f64, dump: Option<PathBuf>) {
        let _ = self.tx.send(Cmd::Scan { gen, times, duration, dump });
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
                Cmd::Scan { gen, times, duration, dump } => {
                    scan = Some(ScanJob {
                        gen,
                        times,
                        duration,
                        refined: false,
                        memo: HashMap::new(),
                        dump,
                        frames: HashMap::new(),
                    })
                }
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
                    let ScanJob { gen, times, duration, refined, memo, dump, frames } = job;
                    let done = run_scan(
                        times,
                        memo,
                        |t| match m.frame_at(t, SEEK_TIMEOUT) {
                            Ok(th) => {
                                let kind = classify(&th.rgba, th.width, th.height);
                                if dump.is_some() {
                                    frames.insert(t.to_bits(), th);
                                }
                                Some(kind)
                            }
                            Err(e) => {
                                eprintln!("tail scan: {e}");
                                None
                            }
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
                            // Second pass: frames around isolated Content
                            // samples, same connection, memo and abort rules.
                            if !std::mem::replace(refined, true) {
                                if let Some(all) = with_refinement(times, &samples, *duration) {
                                    *times = all;
                                    continue;
                                }
                            }
                            if let Some(dir) = dump {
                                let (_, err) = scan_dump::write_frames(dir, &samples, frames);
                                if let Some(e) = err {
                                    eprintln!("tail scan: could not save frames: {e}");
                                }
                                frames.clear();
                            }
                            let gen = *gen;
                            send(Reply::Scanned { gen, samples });
                            scan = None;
                            // Do not keep an idle second connection open.
                            let want_pending = want.is_some() || pending.iter().any(|c| matches!(c, Cmd::Want { .. }));
                            if close_after_scan(want_pending) {
                                mpv = None;
                                send(Reply::Closed { gen });
                            }
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
    fn source_closes_after_a_scan_unless_a_preview_is_wanted() {
        assert!(close_after_scan(false));
        assert!(!close_after_scan(true));
    }

    #[test]
    fn scan_collects_in_order() {
        let mut asked = Vec::new();
        let out = run_scan(
            &[10.0, 20.0, 30.0],
            &mut HashMap::new(),
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
        let mut memo = HashMap::new();
        let out = run_scan(
            &[1.0, 2.0, 3.0, 4.0],
            &mut memo,
            |_| {
                taken.set(taken.get() + 1);
                Some(FrameKind::Black)
            },
            || taken.get() >= 2,
        );
        assert_eq!(out, None);
        assert_eq!(taken.get(), 2, "no frame is taken after the abort");
        assert_eq!(memo.len(), 2, "taken frames are memoised");
    }

    #[test]
    fn every_resume_takes_a_new_frame_even_when_always_aborted() {
        // A preview request arrives before every frame: each call still
        // takes one new frame, so the scan finishes after one call per time.
        let times = [1.0, 2.0, 3.0];
        let mut memo = HashMap::new();
        let mut taken = Vec::new();
        let mut calls = 0;
        let out = loop {
            calls += 1;
            assert!(calls <= times.len(), "the scan must make progress");
            let r = run_scan(
                &times,
                &mut memo,
                |t| {
                    taken.push(t);
                    Some(FrameKind::Credits)
                },
                || true,
            );
            if let Some(s) = r {
                break s;
            }
        };
        assert_eq!(taken, vec![1.0, 2.0, 3.0], "no frame taken twice");
        assert_eq!(out.len(), 3);
        assert_eq!(calls, 3);
    }

    #[test]
    fn refinement_pass_takes_only_the_new_frames_and_merges_in_time_order() {
        use FrameKind::*;
        // First pass: C C X B (X at 20 isolated) -> refinement at 15 and 25.
        let times = vec![0.0, 10.0, 20.0, 30.0];
        let kind_at = |t: f64| {
            Some(match t {
                t if t == 20.0 || t == 25.0 => Content,
                t if t == 30.0 => Black,
                _ => Credits,
            })
        };
        let mut memo = HashMap::new();
        let mut taken = Vec::new();
        let first = run_scan(&times, &mut memo, |t| {
            taken.push(t);
            kind_at(t)
        }, || false)
        .unwrap();
        let all = with_refinement(&times, &first, 100.0).expect("isolated X is refined");
        assert_eq!(all, vec![0.0, 10.0, 15.0, 20.0, 25.0, 30.0]);
        let merged = run_scan(&all, &mut memo, |t| {
            taken.push(t);
            kind_at(t)
        }, || false)
        .unwrap();
        assert_eq!(taken, vec![0.0, 10.0, 20.0, 30.0, 15.0, 25.0], "no frame taken twice");
        assert_eq!(
            merged,
            vec![
                (0.0, Some(Credits)),
                (10.0, Some(Credits)),
                (15.0, Some(Credits)),
                (20.0, Some(Content)),
                (25.0, Some(Content)),
                (30.0, Some(Black)),
            ]
        );
        // Nothing isolated: no second pass.
        assert_eq!(with_refinement(&times, &[(0.0, Some(Credits)), (10.0, Some(Black))], 100.0), None);
    }

    #[test]
    fn memoised_frames_are_not_checked_for_an_abort() {
        let mut memo = HashMap::new();
        memo.insert(1.0f64.to_bits(), Some(FrameKind::Black));
        memo.insert(2.0f64.to_bits(), None);
        let aborts = std::cell::Cell::new(0);
        let out = run_scan(
            &[1.0, 2.0],
            &mut memo,
            |_| unreachable!("memoised"),
            || {
                aborts.set(aborts.get() + 1);
                true
            },
        );
        assert_eq!(out, Some(vec![(1.0, Some(FrameKind::Black)), (2.0, None)]));
        assert_eq!(aborts.get(), 0);
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
