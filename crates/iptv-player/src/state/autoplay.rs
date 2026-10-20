//! "Next episode" autoplay (pure, time passed in as `Instant`).
//!
//! Armed when an episode with a next episode starts. Once playback reaches
//! the tail (`duration - SCAN_TAIL_SECS`) it asks for one tail scan; the scan
//! result gives the offer time (`credits::offer_time`, or the real end when
//! the scan is unavailable). At the offer time the "Next episode" prompt is
//! shown with an `OFFER_SECS` countdown, then the next episode plays. The
//! file finishing while armed (and not dismissed) plays it at once.

use std::time::Instant;

use super::credits::{offer_time, scan_times, FrameKind, SCAN_TAIL_SECS};

/// Countdown of the "Next episode" prompt.
pub const OFFER_SECS: f64 = 8.0;

/// Seeking back more than this before the offer time cancels the prompt.
const SEEK_BACK_SECS: f64 = 5.0;

/// What the app should do after `update`.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Nothing,
    /// Start the tail scan at these times (seconds).
    StartScan(Vec<f64>),
    /// Show the "Next episode" prompt (countdown starts now).
    Offer,
    /// Play the next episode now.
    PlayNext,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Off,
    Waiting,
    Offering { since: Instant },
    Dismissed,
    /// `PlayNext` was returned or the user accepted: nothing more to do.
    Done,
}

#[derive(Debug)]
pub struct Autoplay {
    phase: Phase,
    next: Option<usize>,
    scan_started: bool,
    offer_at: Option<f64>,
}

impl Default for Autoplay {
    fn default() -> Self {
        Self::new()
    }
}

impl Autoplay {
    pub fn new() -> Autoplay {
        Autoplay { phase: Phase::Off, next: None, scan_started: false, offer_at: None }
    }

    /// An episode starts: `next` = the next episode's entry (None or
    /// `enabled == false` => Off).
    pub fn arm(&mut self, next: Option<usize>, enabled: bool) {
        *self = Autoplay::new();
        if enabled && next.is_some() {
            self.next = next;
            self.phase = Phase::Waiting;
        }
    }

    pub fn disarm(&mut self) {
        *self = Autoplay::new();
    }

    /// `position` = (position, duration), `scan` = `None` not available yet,
    /// `Some(None)` unavailable/failed, `Some(Some(samples))` done.
    pub fn update(
        &mut self,
        now: Instant,
        position: Option<(f64, Option<f64>)>,
        scan: Option<Option<Vec<(f64, Option<FrameKind>)>>>,
        finished: bool,
    ) -> Step {
        match self.phase {
            Phase::Off | Phase::Dismissed | Phase::Done => return Step::Nothing,
            Phase::Waiting | Phase::Offering { .. } => {}
        }
        if finished {
            self.phase = Phase::Done;
            return Step::PlayNext;
        }
        let Some((pos, duration)) = position else {
            return self.tick(now);
        };
        let Some(dur) = duration.filter(|d| d.is_finite() && *d > 0.0) else {
            return self.tick(now);
        };
        if self.offer_at.is_none() {
            match scan {
                Some(None) => self.offer_at = Some(dur),
                Some(Some(samples)) => self.offer_at = Some(offer_time(&samples, dur)),
                None => {}
            }
        }
        if !self.scan_started && self.offer_at.is_none() && pos >= dur - SCAN_TAIL_SECS {
            self.scan_started = true;
            let times = scan_times(dur);
            if times.is_empty() {
                // Too short to scan: offer at the real end only.
                self.offer_at = Some(dur);
            } else {
                return Step::StartScan(times);
            }
        }
        let Some(at) = self.offer_at else {
            return Step::Nothing;
        };
        match self.phase {
            Phase::Waiting if pos >= at => {
                self.phase = Phase::Offering { since: now };
                Step::Offer
            }
            Phase::Offering { .. } if pos < at - SEEK_BACK_SECS => {
                self.phase = Phase::Waiting;
                Step::Nothing
            }
            _ => self.tick(now),
        }
    }

    /// Countdown check while offering.
    fn tick(&mut self, now: Instant) -> Step {
        if let Phase::Offering { since } = self.phase {
            if now.saturating_duration_since(since).as_secs_f64() >= OFFER_SECS {
                self.phase = Phase::Done;
                return Step::PlayNext;
            }
        }
        Step::Nothing
    }

    pub fn offering(&self) -> bool {
        matches!(self.phase, Phase::Offering { .. })
    }

    /// Seconds left on the countdown (0 when not offering).
    pub fn remaining(&self, now: Instant) -> f64 {
        match self.phase {
            Phase::Offering { since } => (OFFER_SECS - now.saturating_duration_since(since).as_secs_f64()).max(0.0),
            _ => 0.0,
        }
    }

    pub fn next(&self) -> Option<usize> {
        self.next
    }

    /// Esc / Cancel: no offer again for this episode, and no autoplay when
    /// it finishes.
    pub fn dismiss(&mut self) {
        if !matches!(self.phase, Phase::Off) {
            self.phase = Phase::Dismissed;
        }
    }

    /// Enter / click: the app plays the next episode itself.
    pub fn accept(&mut self) {
        if !matches!(self.phase, Phase::Off) {
            self.phase = Phase::Done;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use FrameKind::*;

    const D: f64 = 2700.0;

    fn armed() -> Autoplay {
        let mut a = Autoplay::new();
        a.arm(Some(7), true);
        a
    }

    fn at(pos: f64) -> Option<(f64, Option<f64>)> {
        Some((pos, Some(D)))
    }

    fn secs(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_secs_f64(s)
    }

    /// Samples every 10 s from `start`, kinds in order.
    fn samples(start: f64, kinds: &[FrameKind]) -> Option<Option<Vec<(f64, Option<FrameKind>)>>> {
        Some(Some(kinds.iter().enumerate().map(|(i, k)| (start + 10.0 * i as f64, Some(*k))).collect()))
    }

    /// Armed, in the tail, scan requested; returns the start instant.
    fn scanning(a: &mut Autoplay) -> Instant {
        let t0 = Instant::now();
        assert!(matches!(a.update(t0, at(D - 400.0), None, false), Step::StartScan(_)));
        t0
    }

    #[test]
    fn no_scan_before_the_tail() {
        let mut a = armed();
        let t0 = Instant::now();
        assert_eq!(a.update(t0, at(100.0), None, false), Step::Nothing);
        assert_eq!(a.update(t0, at(D - SCAN_TAIL_SECS - 1.0), None, false), Step::Nothing);
        // Unknown duration: never scans.
        assert_eq!(a.update(t0, Some((D - 10.0, None)), None, false), Step::Nothing);
        assert_eq!(a.update(t0, None, None, false), Step::Nothing);
    }

    #[test]
    fn scan_starts_in_the_tail() {
        let mut a = armed();
        let t0 = Instant::now();
        assert_eq!(a.update(t0, at(D - SCAN_TAIL_SECS), None, false), Step::StartScan(scan_times(D)));
        // Only once.
        assert_eq!(a.update(t0, at(D - SCAN_TAIL_SECS + 5.0), None, false), Step::Nothing);
        assert_eq!(a.update(t0, at(D - 100.0), None, false), Step::Nothing);
    }

    #[test]
    fn offer_at_credits_start() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        // Content until D-100, credits from D-90.
        let scan = samples(D - 120.0, &[Content, Content, Content, Credits, Credits, Credits]);
        assert_eq!(a.update(t0, at(D - 95.0), scan.clone(), false), Step::Nothing);
        assert!(!a.offering());
        assert_eq!(a.update(t0, at(D - 90.0), scan.clone(), false), Step::Offer);
        assert!(a.offering());
        assert_eq!(a.remaining(t0), OFFER_SECS);
        assert_eq!(a.next(), Some(7));
    }

    #[test]
    fn stinger_delays_offer() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        let scan = samples(D - 60.0, &[Content, Credits, Credits, Content, Black]);
        assert_eq!(a.update(t0, at(D - 50.0), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(t0, at(D - 10.0), scan.clone(), false), Step::Nothing);
        assert!(!a.offering());
        // Only at the real end.
        assert_eq!(a.update(t0, at(D - 1.0), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(t0, at(D - 1.0), scan, true), Step::PlayNext);
    }

    #[test]
    fn scan_unavailable_offers_at_end_only() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        assert_eq!(a.update(t0, at(D - 30.0), Some(None), false), Step::Nothing);
        assert_eq!(a.update(t0, at(D - 0.5), Some(None), false), Step::Nothing);
        assert!(!a.offering());
        assert_eq!(a.update(t0, at(D), Some(None), false), Step::Offer);
    }

    #[test]
    fn countdown_plays_next_after_8s() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        let scan = samples(D - 100.0, &[Credits, Credits, Credits]);
        assert_eq!(a.update(t0, at(D - 100.0), scan.clone(), false), Step::Offer);
        assert_eq!(a.update(secs(t0, 3.0), at(D - 97.0), scan.clone(), false), Step::Nothing);
        assert!((a.remaining(secs(t0, 3.0)) - 5.0).abs() < 1e-6);
        assert_eq!(a.update(secs(t0, 7.9), at(D - 92.1), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(secs(t0, 8.0), at(D - 92.0), scan.clone(), false), Step::PlayNext);
        assert!(!a.offering());
        // Once.
        assert_eq!(a.update(secs(t0, 9.0), at(D - 91.0), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(secs(t0, 9.0), at(D - 91.0), scan, true), Step::Nothing);
    }

    #[test]
    fn dismiss_stops_offer_and_autoplay_at_finish() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        let scan = samples(D - 100.0, &[Credits, Credits, Credits]);
        assert_eq!(a.update(t0, at(D - 100.0), scan.clone(), false), Step::Offer);
        a.dismiss();
        assert!(!a.offering());
        assert_eq!(a.remaining(t0), 0.0);
        assert_eq!(a.update(secs(t0, 20.0), at(D - 80.0), scan.clone(), false), Step::Nothing);
        // Seeking back and forth again: no new offer.
        assert_eq!(a.update(secs(t0, 21.0), at(D - 200.0), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(secs(t0, 22.0), at(D - 50.0), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(secs(t0, 60.0), at(D), scan, true), Step::Nothing);
    }

    #[test]
    fn finished_while_waiting_plays_next() {
        let mut a = armed();
        let t0 = Instant::now();
        // No scan yet, not even in the tail (e.g. seeked to the end).
        assert_eq!(a.update(t0, at(100.0), None, true), Step::PlayNext);
        // Also with the scan still running.
        let mut b = armed();
        let t0 = scanning(&mut b);
        assert_eq!(b.update(t0, at(D), None, true), Step::PlayNext);
    }

    #[test]
    fn seek_back_cancels_offer() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        let scan = samples(D - 100.0, &[Credits, Credits, Credits]);
        assert_eq!(a.update(t0, at(D - 100.0), scan.clone(), false), Step::Offer);
        // A small step back (<= 5 s) keeps the offer.
        assert_eq!(a.update(secs(t0, 1.0), at(D - 104.0), scan.clone(), false), Step::Nothing);
        assert!(a.offering());
        // Further back cancels it.
        assert_eq!(a.update(secs(t0, 2.0), at(D - 300.0), scan.clone(), false), Step::Nothing);
        assert!(!a.offering());
        // The countdown does not fire while waiting.
        assert_eq!(a.update(secs(t0, 20.0), at(D - 280.0), scan.clone(), false), Step::Nothing);
        // Reaching the offer time again offers again, with a fresh countdown.
        assert_eq!(a.update(secs(t0, 30.0), at(D - 100.0), scan.clone(), false), Step::Offer);
        assert_eq!(a.remaining(secs(t0, 30.0)), OFFER_SECS);
        // No second scan request.
        assert_eq!(a.update(secs(t0, 31.0), at(D - 99.0), None, false), Step::Nothing);
    }

    #[test]
    fn disarmed_does_nothing() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        a.disarm();
        assert_eq!(a.next(), None);
        let scan = samples(D - 100.0, &[Credits, Credits, Credits]);
        assert_eq!(a.update(t0, at(D - 100.0), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(t0, at(D), scan, true), Step::Nothing);
        assert!(!a.offering());
        // Disabled setting = Off too.
        let mut b = Autoplay::new();
        b.arm(Some(3), false);
        assert_eq!(b.update(t0, at(D - 400.0), None, false), Step::Nothing);
        assert_eq!(b.update(t0, at(D), None, true), Step::Nothing);
    }

    #[test]
    fn no_next_episode_is_off() {
        let mut a = Autoplay::new();
        a.arm(None, true);
        let t0 = Instant::now();
        assert_eq!(a.next(), None);
        assert_eq!(a.update(t0, at(D - 400.0), None, false), Step::Nothing);
        assert_eq!(a.update(t0, at(D), Some(None), true), Step::Nothing);
        assert!(!a.offering());
    }

    #[test]
    fn accept_ends_the_offer() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        let scan = samples(D - 100.0, &[Credits, Credits, Credits]);
        assert_eq!(a.update(t0, at(D - 100.0), scan.clone(), false), Step::Offer);
        a.accept();
        assert!(!a.offering());
        assert_eq!(a.update(secs(t0, 9.0), at(D - 91.0), scan.clone(), false), Step::Nothing);
        assert_eq!(a.update(secs(t0, 9.0), at(D), scan, true), Step::Nothing);
    }

    #[test]
    fn rearm_resets_for_the_next_episode() {
        let mut a = armed();
        let t0 = scanning(&mut a);
        a.dismiss();
        a.arm(Some(8), true);
        assert_eq!(a.next(), Some(8));
        assert!(matches!(a.update(t0, at(D - 400.0), None, false), Step::StartScan(_)));
    }

    #[test]
    fn short_video_offers_at_end_without_scan() {
        let mut a = armed();
        let t0 = Instant::now();
        let d = 20.0;
        assert_eq!(a.update(t0, Some((5.0, Some(d))), None, false), Step::Nothing);
        assert_eq!(a.update(t0, Some((d, Some(d))), None, false), Step::Offer);
    }
}
