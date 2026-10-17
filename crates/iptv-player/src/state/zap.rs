//! Channel-zapping debounce: navigation keys move the selection at once, but
//! the stream is only loaded once no further navigation happened for
//! [`ZAP_DELAY`]. Holding Down therefore opens one connection (the final
//! target), not one per key repeat, so it cannot trip a provider's
//! connection limit.

use std::time::{Duration, Instant};

/// Quiet time after the last navigation key before the target is played.
pub const ZAP_DELAY: Duration = Duration::from_millis(300);

/// Holds at most one pending value; each `push` replaces it and restarts
/// the timer. Time is injected for tests.
#[derive(Debug)]
pub struct Debounce<T> {
    pending: Option<(T, Instant)>,
    delay: Duration,
}

impl<T> Debounce<T> {
    pub fn new(delay: Duration) -> Debounce<T> {
        Debounce { pending: None, delay }
    }

    /// Replace the pending value and restart the quiet period at `now`.
    pub fn push(&mut self, value: T, now: Instant) {
        self.pending = Some((value, now));
    }

    pub fn pending(&self) -> Option<&T> {
        self.pending.as_ref().map(|(v, _)| v)
    }

    pub fn pending_mut(&mut self) -> Option<&mut T> {
        self.pending.as_mut().map(|(v, _)| v)
    }

    /// Drop the pending value (click/Enter played something else, Esc).
    pub fn cancel(&mut self) -> Option<T> {
        self.pending.take().map(|(v, _)| v)
    }

    /// The pending value once the quiet period has elapsed (taken).
    pub fn due(&mut self, now: Instant) -> Option<T> {
        match &self.pending {
            Some((_, at)) if now.saturating_duration_since(*at) >= self.delay => self.cancel(),
            _ => None,
        }
    }

    /// Time left until the pending value is due (for a timed repaint).
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.pending.as_ref().map(|(_, at)| self.delay.saturating_sub(now.saturating_duration_since(*at)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::live::tests::sample_lib;
    use crate::state::live::LiveBrowser;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Simulated frame loop: poll every 10 ms until `until`, collecting plays.
    fn run(z: &mut Debounce<usize>, from: Instant, until: Instant, plays: &mut Vec<usize>) {
        let mut t = from;
        while t <= until {
            if let Some(v) = z.due(t) {
                plays.push(v);
            }
            t += ms(10);
        }
    }

    #[test]
    fn ten_rapid_downs_play_only_the_last_target_after_the_delay() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        let mut z = Debounce::new(ZAP_DELAY);
        let t0 = Instant::now();
        let mut plays = vec![];
        let mut last = None;
        // 10 Downs within 100 ms (key repeat), frames polled in between.
        for i in 0..10u64 {
            let now = t0 + ms(i * 10);
            run(&mut z, now, now, &mut plays);
            let target = b.list_mut().step(1).unwrap();
            z.push(target, now);
            last = Some((target, now));
        }
        let (target, at) = last.unwrap();
        assert_eq!(b.list().current(), Some(target), "selection moved on every key");
        run(&mut z, t0 + ms(91), at + ms(299), &mut plays);
        assert!(plays.is_empty(), "nothing loads before {ZAP_DELAY:?} of quiet");
        assert_eq!(z.wait(at + ms(200)), Some(ms(100)));
        run(&mut z, at + ms(300), at + ms(2000), &mut plays);
        assert_eq!(plays, vec![target], "exactly one play, of the 10th target");
        assert_eq!(z.pending(), None);
        assert_eq!(z.wait(at + ms(2000)), None);
    }

    #[test]
    fn click_or_esc_cancels_pending() {
        let mut z = Debounce::new(ZAP_DELAY);
        let t0 = Instant::now();
        z.push(7, t0);
        assert_eq!(z.cancel(), Some(7)); // click/Enter plays immediately instead
        assert_eq!(z.due(t0 + ms(1000)), None);
        z.push(8, t0);
        z.cancel(); // Esc
        assert_eq!(z.due(t0 + ms(1000)), None);
        assert_eq!(z.pending(), None);
    }

    #[test]
    fn navigation_while_the_filter_changes_the_list() {
        let lib = sample_lib();
        let mut b = LiveBrowser::new();
        b.set_library(None, &lib);
        let mut z = Debounce::new(ZAP_DELAY);
        let t0 = Instant::now();
        z.push(b.list_mut().step(1).unwrap(), t0); // SVT1 HD (entry 0)
        b.set_filter(&lib, "sport"); // list is now [3, 4]
        z.push(b.list_mut().step(1).unwrap(), t0 + ms(50));
        assert_eq!(z.pending(), Some(&3), "target comes from the filtered list");
        assert_eq!(z.due(t0 + ms(340)), None, "timer restarted by the second key");
        assert_eq!(z.due(t0 + ms(350)), Some(3));
    }
}
