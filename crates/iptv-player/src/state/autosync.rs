//! Automatic timing of an imported subtitle file against an embedded track
//! (pure; the player feeds it the start of the reference track's active line).

use iptv::subsync::{best_shift, MIN_APPLY_SECS, MIN_LINES, RECHECK_LINES};

/// What the caller should do after `step`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    Nothing,
    /// Set `sub-delay` to this absolute value.
    Apply(f64),
    /// Not enough agreement to trust a shift (reported once).
    Unsure,
}

pub struct AutoSync {
    file_starts: Vec<f64>,
    current_delay: f64,
    /// Distinct reference starts in milliseconds.
    seen: Vec<i64>,
    first_done: bool,
    recheck_done: bool,
    manual: bool,
}

impl AutoSync {
    pub fn new(file_starts: Vec<f64>, current_delay: f64) -> AutoSync {
        let mut file_starts = file_starts;
        file_starts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        AutoSync { file_starts, current_delay, seen: Vec::new(), first_done: false, recheck_done: false, manual: false }
    }

    pub fn observe(&mut self, start: Option<f64>) {
        let Some(s) = start else { return };
        if !s.is_finite() {
            return;
        }
        let ms = (s * 1000.0).round() as i64;
        if !self.seen.contains(&ms) {
            self.seen.push(ms);
        }
    }

    pub fn lines_seen(&self) -> usize {
        self.seen.len()
    }

    pub fn manual(&mut self) {
        self.manual = true;
    }

    /// `Some(shift)` when the current lines give a confident shift that
    /// differs enough from the applied delay (and records it as applied).
    fn compute(&mut self) -> Option<Option<f64>> {
        let mut reference: Vec<f64> = self.seen.iter().map(|&m| m as f64 / 1000.0).collect();
        reference.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let m = best_shift(&reference, &self.file_starts)?;
        if !m.confident() {
            return None;
        }
        if (m.shift - self.current_delay).abs() >= MIN_APPLY_SECS {
            self.current_delay = m.shift;
            Some(Some(m.shift))
        } else {
            Some(None)
        }
    }

    pub fn step(&mut self) -> Step {
        if self.manual {
            return Step::Nothing;
        }
        let n = self.seen.len();
        if !self.first_done && n >= MIN_LINES {
            self.first_done = true;
            return match self.compute() {
                Some(Some(x)) => Step::Apply(x),
                Some(None) => Step::Nothing,
                None => Step::Unsure,
            };
        }
        if self.first_done && !self.recheck_done && n >= RECHECK_LINES {
            self.recheck_done = true;
            if let Some(Some(x)) = self.compute() {
                return Step::Apply(x);
            }
        }
        Step::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f64) / ((1u64 << 31) as f64)
        }
    }

    /// `n` starts, roughly every `gap` seconds with jitter.
    fn starts(seed: u64, n: usize, gap: f64) -> Vec<f64> {
        let mut r = Lcg(seed);
        let mut t = 10.0;
        (0..n)
            .map(|_| {
                t += gap * (0.5 + r.next());
                t
            })
            .collect()
    }

    fn feed(a: &mut AutoSync, v: &[f64]) {
        for &x in v {
            a.observe(Some(x));
        }
    }

    #[test]
    fn waits_for_enough_lines() {
        let f = starts(1, 100, 6.0);
        let mut a = AutoSync::new(f.clone(), 0.0);
        let r: Vec<f64> = f.iter().take(24).map(|x| x + 0.45).collect();
        feed(&mut a, &r);
        assert_eq!(a.step(), Step::Nothing);
    }

    #[test]
    fn applies_the_shift_at_25_lines() {
        let f = starts(2, 100, 6.0);
        let mut a = AutoSync::new(f.clone(), 0.0);
        let r: Vec<f64> = f.iter().take(25).map(|x| x + 0.45).collect();
        feed(&mut a, &r);
        match a.step() {
            Step::Apply(x) => assert!((x - 0.45).abs() < 0.1, "{x}"),
            s => panic!("{s:?}"),
        }
        assert_eq!(a.step(), Step::Nothing);
    }

    #[test]
    fn tiny_shift_is_ignored() {
        let f = starts(3, 100, 6.0);
        let mut a = AutoSync::new(f.clone(), 0.0);
        let r: Vec<f64> = f.iter().take(25).map(|x| x + 0.1).collect();
        feed(&mut a, &r);
        assert_eq!(a.step(), Step::Nothing);
    }

    #[test]
    fn unrelated_timing_gives_unsure_once() {
        let f = starts(4, 100, 6.0);
        let mut a = AutoSync::new(f, 0.0);
        let r = starts(99, 25, 7.3);
        feed(&mut a, &r);
        assert_eq!(a.step(), Step::Unsure);
        assert_eq!(a.step(), Step::Nothing);
    }

    #[test]
    fn repeats_and_seeks_do_not_inflate_the_count() {
        let mut a = AutoSync::new(starts(5, 50, 6.0), 0.0);
        for _ in 0..50 {
            a.observe(Some(12.3456));
            a.observe(None);
        }
        assert_eq!(a.lines_seen(), 1);
        a.observe(Some(12.3459)); // same millisecond
        assert_eq!(a.lines_seen(), 1);
        a.observe(Some(20.0));
        assert_eq!(a.lines_seen(), 2);
    }

    #[test]
    fn recheck_at_60_lines_can_correct() {
        let f = starts(6, 120, 6.0);
        let mut a = AutoSync::new(f.clone(), 0.0);
        let first: Vec<f64> = f.iter().take(25).copied().collect();
        feed(&mut a, &first);
        assert_eq!(a.step(), Step::Nothing); // confident, no shift
        let later: Vec<f64> = f.iter().skip(25).take(35).map(|x| x + 0.6).collect();
        feed(&mut a, &later[..20]);
        assert_eq!(a.step(), Step::Nothing);
        feed(&mut a, &later[20..]);
        assert_eq!(a.lines_seen(), 60);
        match a.step() {
            Step::Apply(x) => assert!((x - 0.6).abs() < 0.1, "{x}"),
            s => panic!("{s:?}"),
        }
        assert_eq!(a.step(), Step::Nothing);
    }

    #[test]
    fn manual_nudge_stops_everything() {
        let f = starts(7, 100, 6.0);
        let mut a = AutoSync::new(f.clone(), 0.0);
        let r: Vec<f64> = f.iter().take(30).map(|x| x + 0.45).collect();
        feed(&mut a, &r);
        a.manual();
        assert_eq!(a.step(), Step::Nothing);
    }

    #[test]
    fn few_lines_with_a_wrong_peak_never_apply() {
        // 13 reference lines that happen to match the file at +80 s.
        let f = starts(8, 100, 6.0);
        let mut a = AutoSync::new(f.clone(), 0.0);
        let r: Vec<f64> = f.iter().take(13).map(|x| x + 80.0).collect();
        feed(&mut a, &r);
        assert_eq!(a.step(), Step::Nothing);
    }

    #[test]
    fn dense_file_against_unrelated_reference_does_not_apply() {
        let f = starts(9, 1500, 1.7);
        let mut a = AutoSync::new(f, 0.0);
        let r = starts(77, 28, 2.3);
        feed(&mut a, &r);
        let s = a.step();
        assert!(!matches!(s, Step::Apply(_)), "{s:?}");
    }

    #[test]
    fn dense_file_with_real_subset_applies() {
        // Every ~3 s: at ~1.7 s the chance rival matches (22-25 of 25-30) defeat
        // the 1.5x rule even for a true subset, so such files end up Unsure.
        let f = starts(10, 1000, 3.0);
        let mut rng = Lcg(5);
        let r: Vec<f64> = f.iter().skip(200).take(40).filter(|_| rng.next() > 0.25).map(|x| x + 0.45).collect();
        assert!(r.len() >= 25);
        let mut a = AutoSync::new(f, 0.0);
        feed(&mut a, &r);
        match a.step() {
            Step::Apply(x) => assert!((x - 0.45).abs() < 0.1, "{x}"),
            s => panic!("{s:?}"),
        }
    }
}
