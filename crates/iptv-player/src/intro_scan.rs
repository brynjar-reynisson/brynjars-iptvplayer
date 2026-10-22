//! Headless intro scan: a worker thread that opens a fresh [`ThumbMpv`] on a
//! URL, takes the frames at the given times and fingerprints/classifies each.
//! One job at a time; a newer `scan` or `cancel` aborts the running job at the
//! next frame (its result is not sent). The source is closed when the job
//! ends. Never logs or stores the URL beyond the command sent to the worker.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use iptv::intro::{fingerprint, Fingerprint, Prints};

use crate::state::credits::{classify, FrameKind};
use crate::thumb_mpv::ThumbMpv;

const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const THUMB_WIDTH: u32 = 240;

/// The answer to one scan job.
pub struct JobResult {
    pub job: u64,
    pub prints: Vec<(f64, Option<Fingerprint>)>,
    pub kinds: Vec<(f64, Option<FrameKind>)>,
    /// The source could not be opened (text without URL).
    pub error: Option<String>,
}

/// Takes each of `times` in order via `take`; `aborted` is asked before every
/// frame and stops the job (`None`).
#[allow(clippy::type_complexity)]
fn run_job(
    times: &[f64],
    mut take: impl FnMut(f64) -> Option<(Option<Fingerprint>, Option<FrameKind>)>,
    mut aborted: impl FnMut() -> bool,
) -> Option<(Prints, Vec<(f64, Option<FrameKind>)>)> {
    let mut prints = Vec::with_capacity(times.len());
    let mut kinds = Vec::with_capacity(times.len());
    for &t in times {
        if aborted() {
            return None;
        }
        let (p, k) = take(t).unwrap_or((None, None));
        prints.push((t, p));
        kinds.push((t, k));
    }
    Some((prints, kinds))
}

enum Cmd {
    Scan { job: u64, url: String, times: Vec<f64> },
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

    /// Scan `url` at `times` (replaces any running job).
    pub fn scan(&self, job: u64, url: &str, times: Vec<f64>) {
        let _ = self.tx.send(Cmd::Scan { job, url: url.to_string(), times });
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
        let Cmd::Scan { job, url, times } = cmd else { continue };

        let opened = ThumbMpv::new(&ua, THUMB_WIDTH).and_then(|m| m.open(&url, OPEN_TIMEOUT).map(|_| m));
        let mpv = match opened {
            Ok(m) => m,
            Err(e) => {
                let msg = e.to_string();
                eprintln!("intro scan: open failed: {msg}");
                let _ = replies.send(JobResult { job, prints: Vec::new(), kinds: Vec::new(), error: Some(msg) });
                wake();
                continue;
            }
        };
        let out = run_job(
            &times,
            |t| match mpv.frame_at(t, FRAME_TIMEOUT) {
                Ok(th) => Some((
                    Some(fingerprint(&th.rgba, th.width, th.height)),
                    Some(classify(&th.rgba, th.width, th.height)),
                )),
                Err(e) => {
                    eprintln!("intro scan: {e}");
                    None
                }
            },
            || match cmds.try_recv() {
                Ok(c) => {
                    carry = Some(c);
                    true
                }
                Err(_) => false,
            },
        );
        drop(mpv);
        if let Some((prints, kinds)) = out {
            let _ = replies.send(JobResult { job, prints, kinds, error: None });
            wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(v: u8) -> Fingerprint {
        Fingerprint([v; 144])
    }

    #[test]
    fn takes_every_time_in_order() {
        let mut seen = Vec::new();
        let (p, k) = run_job(
            &[0.0, 4.0, 8.0],
            |t| {
                seen.push(t);
                Some((Some(fp(t as u8)), Some(FrameKind::Content)))
            },
            || false,
        )
        .unwrap();
        assert_eq!(seen, vec![0.0, 4.0, 8.0]);
        assert_eq!(p.iter().map(|x| x.0).collect::<Vec<_>>(), vec![0.0, 4.0, 8.0]);
        assert_eq!(p[1].1, Some(fp(4)));
        assert_eq!(k.len(), 3);
        assert_eq!(k[2], (8.0, Some(FrameKind::Content)));
    }

    #[test]
    fn aborted_stops_and_returns_none() {
        let mut taken = 0;
        let mut asks = 0;
        let r = run_job(
            &[0.0, 4.0, 8.0],
            |_| {
                taken += 1;
                None
            },
            || {
                asks += 1;
                asks > 1
            },
        );
        assert!(r.is_none());
        assert_eq!(taken, 1);
    }

    #[test]
    fn a_failed_frame_is_none_in_both_lists() {
        let (p, k) = run_job(
            &[0.0, 4.0],
            |t| if t == 0.0 { None } else { Some((Some(fp(9)), Some(FrameKind::Black))) },
            || false,
        )
        .unwrap();
        assert_eq!(p[0], (0.0, None));
        assert_eq!(k[0], (0.0, None));
        assert_eq!(k[1], (4.0, Some(FrameKind::Black)));
    }
}
