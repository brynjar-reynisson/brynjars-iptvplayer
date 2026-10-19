//! Seek bar logic for movies/episodes: pixel <-> time mapping, arrow-key
//! steps and what position the bar shows while the user drags or while a
//! seek is still being carried out. No GUI types.

use std::time::{Duration, Instant};

/// Left/Right arrow step in seconds.
pub const STEP_SECS: f64 = 10.0;

/// How long the bar keeps showing the target of a seek before it trusts the
/// player's position again.
const PENDING_FOR: Duration = Duration::from_secs(2);

/// A reported position this close to the target counts as arrived.
const ARRIVED_WITHIN: f64 = 1.5;

/// Never seek to the very end (that would end the file at once).
const END_MARGIN: f64 = 1.0;

/// Fraction (0..=1) of the way from `left` to `right` that `x` is at.
pub fn fraction(x: f32, left: f32, right: f32) -> f64 {
    if right <= left {
        return 0.0;
    }
    (((x - left) / (right - left)) as f64).clamp(0.0, 1.0)
}

/// Seek target in seconds for a point `frac` of the way through `dur`.
pub fn target_at(frac: f64, dur: f64) -> f64 {
    (frac.clamp(0.0, 1.0) * dur).min((dur - END_MARGIN).max(0.0))
}

/// Target of a relative step from `pos`, kept inside the video.
pub fn step_target(pos: f64, delta: f64, dur: Option<f64>) -> f64 {
    let t = (pos + delta).max(0.0);
    match dur {
        Some(d) => t.min((d - END_MARGIN).max(0.0)),
        None => t,
    }
}

#[derive(Debug, Default)]
pub struct SeekState {
    /// Position under the mouse while the bar is being dragged.
    scrub: Option<f64>,
    /// Target of the last seek and when it was issued.
    pending: Option<(f64, Instant)>,
}

impl SeekState {
    pub fn dragging(&self) -> bool {
        self.scrub.is_some()
    }

    /// The mouse is dragging at `secs`.
    pub fn drag(&mut self, secs: f64) {
        self.scrub = Some(secs);
    }

    /// The drag ended: the position to seek to, if one was being dragged.
    pub fn release(&mut self, now: Instant) -> Option<f64> {
        let t = self.scrub.take();
        if let Some(t) = t {
            self.pending = Some((t, now));
        }
        t
    }

    /// A seek to `secs` was issued.
    pub fn sought(&mut self, secs: f64, now: Instant) {
        self.pending = Some((secs, now));
    }

    /// Forget everything (another video started).
    pub fn reset(&mut self) {
        *self = SeekState::default();
    }

    /// Position the bar shows: the drag position, else the target of a recent
    /// seek until the player has reached it, else the player's position.
    pub fn shown(&mut self, real: f64, now: Instant) -> f64 {
        if let Some(s) = self.scrub {
            return s;
        }
        match self.pending {
            Some((t, at)) if now.duration_since(at) < PENDING_FOR && (real - t).abs() > ARRIVED_WITHIN => t,
            Some(_) => {
                self.pending = None;
                real
            }
            None => real,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_clamps_to_the_bar() {
        assert_eq!(fraction(150.0, 100.0, 300.0), 0.25);
        assert_eq!(fraction(50.0, 100.0, 300.0), 0.0);
        assert_eq!(fraction(400.0, 100.0, 300.0), 1.0);
        assert_eq!(fraction(10.0, 100.0, 100.0), 0.0);
    }

    #[test]
    fn targets_stay_inside_the_video() {
        assert_eq!(target_at(0.5, 100.0), 50.0);
        assert_eq!(target_at(1.0, 100.0), 99.0);
        assert_eq!(target_at(-1.0, 100.0), 0.0);
        assert_eq!(step_target(5.0, -10.0, Some(100.0)), 0.0);
        assert_eq!(step_target(95.0, 10.0, Some(100.0)), 99.0);
        assert_eq!(step_target(50.0, 10.0, None), 60.0);
        assert_eq!(step_target(0.5, 10.0, Some(0.5)), 0.0);
    }

    #[test]
    fn bar_shows_drag_then_target_until_reached_or_expired() {
        let t0 = Instant::now();
        let mut s = SeekState::default();
        assert_eq!(s.shown(10.0, t0), 10.0);
        s.drag(500.0);
        assert!(s.dragging());
        assert_eq!(s.shown(10.0, t0), 500.0);
        assert_eq!(s.release(t0), Some(500.0));
        assert!(!s.dragging());
        assert_eq!(s.release(t0), None);
        // The player still reports the old position: keep showing the target.
        assert_eq!(s.shown(10.0, t0 + Duration::from_millis(300)), 500.0);
        // Arrived (within 1.5 s): back to the real position.
        assert_eq!(s.shown(499.0, t0 + Duration::from_millis(600)), 499.0);
        assert_eq!(s.shown(10.0, t0 + Duration::from_millis(700)), 10.0, "pending was dropped");
        // A seek that never arrives expires.
        s.sought(900.0, t0);
        assert_eq!(s.shown(10.0, t0 + Duration::from_secs(1)), 900.0);
        assert_eq!(s.shown(10.0, t0 + Duration::from_secs(3)), 10.0);
        s.sought(900.0, t0);
        s.reset();
        assert_eq!(s.shown(10.0, t0), 10.0);
    }
}
