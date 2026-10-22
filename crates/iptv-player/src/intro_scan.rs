//! Headless intro scan: a worker thread that runs one job at a time on a pool
//! of up to [`SCAN_SOURCES`] fresh [`ThumbMpv`] sources on the same URL (one
//! provider connection each; a frame costs ~0.7 s of seek latency per
//! connection, so they work in parallel). Each source pulls the next time
//! from a shared queue and fingerprints/classifies its frame; the results are
//! merged time-sorted. A newer `scan` or `cancel` aborts every source at its
//! next frame (no result is sent for that job). The sources are closed when
//! the job ends. With a dump folder each frame is saved as PNG as it is
//! taken (`--console` tuning aid). Never logs or stores the URL beyond the
//! command sent to the worker.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use iptv::intro::{fingerprint, Fingerprint, Prints};

use crate::scan_dump::write_head_frame;
use crate::state::credits::{classify, FrameKind};
use crate::thumb_mpv::ThumbMpv;

/// Most parallel sources (provider connections) one scan job uses.
pub const SCAN_SOURCES: usize = 4;

const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const THUMB_WIDTH: u32 = 240;
/// How often the worker checks for a newer command while a job runs.
const WATCH_EVERY: Duration = Duration::from_millis(50);

/// The answer to one scan job.
pub struct JobResult {
    pub job: u64,
    pub prints: Vec<(f64, Option<Fingerprint>)>,
    pub kinds: Vec<(f64, Option<FrameKind>)>,
    /// The source could not be opened (text without URL).
    pub error: Option<String>,
}

type Taken = (Option<Fingerprint>, Option<FrameKind>);

/// How a pool run ended.
#[derive(Debug)]
enum Outcome {
    /// Every time taken (a failed frame is `None` in both lists), time-sorted.
    Done(Prints, Vec<(f64, Option<FrameKind>)>),
    /// `watch` asked to stop.
    Aborted,
    /// No source could be opened (the first error text).
    Failed(String),
}

/// Takes every one of `times` exactly once with up to `sources` parallel
/// sources. Source `i` is opened with `open(i)` on its own thread (an error
/// = that source takes nothing, the others finish the queue) and then pulls
/// the next time from the shared queue and calls `take`. `watch` runs on the
/// calling thread while the sources work (it should wait a little, e.g. on a
/// channel with a timeout); `true` stops every source before its next frame.
fn run_pool<S>(
    times: &[f64],
    sources: usize,
    open: impl Fn(usize) -> Result<S, String> + Sync,
    take: impl Fn(&S, f64) -> Option<Taken> + Sync,
    mut watch: impl FnMut() -> bool,
) -> Outcome {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let opened = AtomicUsize::new(0);
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    let results: Mutex<Vec<(f64, Taken)>> = Mutex::new(Vec::with_capacity(times.len()));
    let n = sources.clamp(1, times.len().max(1));
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let (open, take, next, stop, opened, first_error, results) =
                    (&open, &take, &next, &stop, &opened, &first_error, &results);
                scope.spawn(move || {
                    let src = match open(i) {
                        Ok(s) => s,
                        Err(e) => {
                            first_error.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert(e);
                            return;
                        }
                    };
                    opened.fetch_add(1, Ordering::SeqCst);
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        let k = next.fetch_add(1, Ordering::SeqCst);
                        let Some(&t) = times.get(k) else { return };
                        let r = take(&src, t).unwrap_or((None, None));
                        results.lock().unwrap_or_else(|p| p.into_inner()).push((t, r));
                    }
                })
            })
            .collect();
        while !handles.iter().all(|h| h.is_finished()) {
            if stop.load(Ordering::SeqCst) {
                // Waiting for the sources to finish their current frame.
                std::thread::sleep(Duration::from_millis(10));
            } else if watch() {
                stop.store(true, Ordering::SeqCst);
            }
        }
    });
    if stop.load(Ordering::SeqCst) {
        return Outcome::Aborted;
    }
    if opened.load(Ordering::SeqCst) == 0 && !times.is_empty() {
        let e = first_error.into_inner().unwrap_or_else(|p| p.into_inner());
        return Outcome::Failed(e.unwrap_or_else(|| "no source".into()));
    }
    let mut results = results.into_inner().unwrap_or_else(|p| p.into_inner());
    results.sort_by(|a, b| a.0.total_cmp(&b.0));
    let prints = results.iter().map(|(t, (p, _))| (*t, p.clone())).collect();
    let kinds = results.iter().map(|(t, (_, k))| (*t, *k)).collect();
    Outcome::Done(prints, kinds)
}

enum Cmd {
    Scan { job: u64, url: String, times: Vec<f64>, dump: Option<PathBuf> },
    Cancel,
}

pub struct IntroScanner {
    tx: Sender<Cmd>,
    rx: Receiver<JobResult>,
}

impl IntroScanner {
    /// `wake` is called from the worker when a result is waiting.
    pub fn new(user_agent: String, wake: impl Fn() + Send + 'static) -> IntroScanner {
        let (tx, cmd_rx) = channel();
        let (reply_tx, rx) = channel();
        std::thread::Builder::new()
            .name("intro-scan".into())
            .spawn(move || worker(user_agent, cmd_rx, reply_tx, wake))
            .expect("spawn intro-scan thread");
        IntroScanner { tx, rx }
    }

    /// Scan `url` at `times` (replaces any running job). With `dump`, each
    /// frame is saved there as `<secs>s_<letter>.png` as it is taken.
    pub fn scan(&self, job: u64, url: &str, times: Vec<f64>, dump: Option<PathBuf>) {
        let _ = self.tx.send(Cmd::Scan { job, url: url.to_string(), times, dump });
    }

    /// Abort the running job; no result is sent for it.
    pub fn cancel(&self) {
        let _ = self.tx.send(Cmd::Cancel);
    }

    pub fn poll(&self) -> Vec<JobResult> {
        self.rx.try_iter().collect()
    }
}

fn worker(ua: String, cmds: Receiver<Cmd>, replies: Sender<JobResult>, wake: impl Fn()) {
    // A command that arrived while a job ran and still has to be handled.
    let mut carry: Option<Cmd> = None;
    loop {
        let cmd = match carry.take() {
            Some(c) => c,
            None => match cmds.recv() {
                Ok(c) => c,
                Err(_) => return,
            },
        };
        let Cmd::Scan { job, url, times, dump } = cmd else { continue };

        let out = run_pool(
            &times,
            SCAN_SOURCES,
            |_| {
                ThumbMpv::new(&ua, THUMB_WIDTH)
                    .and_then(|m| m.open(&url, OPEN_TIMEOUT).map(|_| m))
                    .map_err(|e| e.to_string())
            },
            |mpv, t| match mpv.frame_at(t, FRAME_TIMEOUT) {
                Ok(th) => {
                    let kind = classify(&th.rgba, th.width, th.height);
                    if let Some(dir) = &dump {
                        if let Err(e) = write_head_frame(dir, t, kind, &th) {
                            eprintln!("intro scan: could not save a frame: {e}");
                        }
                    }
                    Some((Some(fingerprint(&th.rgba, th.width, th.height)), Some(kind)))
                }
                Err(e) => {
                    eprintln!("intro scan: {e}");
                    None
                }
            },
            || match cmds.recv_timeout(WATCH_EVERY) {
                Ok(c) => {
                    carry = Some(c);
                    true
                }
                Err(RecvTimeoutError::Timeout) => false,
                // The app is gone: stop.
                Err(RecvTimeoutError::Disconnected) => true,
            },
        );
        if let Some(dir) = &dump {
            eprintln!("intro scan frames: {}", dir.display());
        }
        let result = match out {
            Outcome::Done(prints, kinds) => JobResult { job, prints, kinds, error: None },
            Outcome::Failed(msg) => {
                eprintln!("intro scan: open failed: {msg}");
                JobResult { job, prints: Vec::new(), kinds: Vec::new(), error: Some(msg) }
            }
            Outcome::Aborted => continue,
        };
        let _ = replies.send(result);
        wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(v: u8) -> Fingerprint {
        Fingerprint([v; 144])
    }

    fn grid(n: usize) -> Vec<f64> {
        (0..n).map(|i| i as f64 * 4.0).collect()
    }

    fn done(o: Outcome) -> (Prints, Vec<(f64, Option<FrameKind>)>) {
        match o {
            Outcome::Done(p, k) => (p, k),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    fn idle() -> bool {
        std::thread::sleep(Duration::from_millis(1));
        false
    }

    #[test]
    fn every_time_is_taken_exactly_once_and_merged_in_time_order() {
        let taken = Mutex::new(Vec::new());
        let times = grid(61);
        let (p, k) = done(run_pool(
            &times,
            4,
            |i| Ok(i),
            |_, t| {
                taken.lock().unwrap().push(t);
                std::thread::sleep(Duration::from_millis(1));
                Some((Some(fp(t as u8)), Some(FrameKind::Content)))
            },
            idle,
        ));
        let mut taken = taken.into_inner().unwrap();
        taken.sort_by(f64::total_cmp);
        assert_eq!(taken, times);
        assert_eq!(p.iter().map(|x| x.0).collect::<Vec<_>>(), times);
        assert_eq!(p[3].1, Some(fp(12)));
        assert_eq!(k.len(), 61);
        assert_eq!(k[60], (240.0, Some(FrameKind::Content)));
    }

    #[test]
    fn the_sources_work_in_parallel() {
        let busy = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        done(run_pool(
            &grid(20),
            4,
            |i| Ok(i),
            |_, _| {
                let now = busy.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                busy.fetch_sub(1, Ordering::SeqCst);
                None
            },
            idle,
        ));
        assert!(most.load(Ordering::SeqCst) > 1);
    }

    #[test]
    fn a_source_that_cannot_open_loses_no_frames() {
        let (p, _) = done(run_pool(
            &grid(10),
            4,
            |i| if i == 0 { Err("refused".into()) } else { Ok(i) },
            |_, t| Some((Some(fp(t as u8)), None)),
            idle,
        ));
        assert_eq!(p.len(), 10);
        assert!(p.iter().all(|(_, f)| f.is_some()));
    }

    #[test]
    fn all_sources_failing_is_an_error_with_the_first_text() {
        match run_pool(&grid(10), 4, |_| Err::<usize, _>("refused".to_string()), |_, _| None, idle) {
            Outcome::Failed(e) => assert_eq!(e, "refused"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn aborted_stops_every_source() {
        let taken = AtomicUsize::new(0);
        let mut asks = 0;
        let out = run_pool(
            &grid(200),
            4,
            |i| Ok(i),
            |_, _| {
                taken.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(2));
                None
            },
            || {
                std::thread::sleep(Duration::from_millis(5));
                asks += 1;
                asks > 1
            },
        );
        assert!(matches!(out, Outcome::Aborted));
        assert!(taken.load(Ordering::SeqCst) < 200);
    }

    #[test]
    fn a_failed_frame_is_none_in_both_lists() {
        let (p, k) = done(run_pool(
            &[0.0, 4.0],
            2,
            |i| Ok(i),
            |_, t| if t == 0.0 { None } else { Some((Some(fp(9)), Some(FrameKind::Black))) },
            idle,
        ));
        assert_eq!(p[0], (0.0, None));
        assert_eq!(k[0], (0.0, None));
        assert_eq!(k[1], (4.0, Some(FrameKind::Black)));
    }

    #[test]
    fn fewer_times_than_sources_opens_only_as_many() {
        let opens = AtomicUsize::new(0);
        let (p, _) = done(run_pool(
            &[0.0, 4.0],
            4,
            |i| {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(i)
            },
            |_, _| None,
            idle,
        ));
        assert_eq!(p.len(), 2);
        assert_eq!(opens.load(Ordering::SeqCst), 2);
    }
}
