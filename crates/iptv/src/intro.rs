//! Intro detection: tiny frame fingerprints and cross-episode matching.
//! Pure functions, no I/O.

pub const INTRO_STEP_SECS: f64 = 4.0;
pub const INTRO_SCAN_SECS: f64 = 240.0;
pub const MIN_INTRO_SECS: f64 = 20.0;
pub const MATCH_DIST: f64 = 12.0;

const COLS: usize = 16;
const ROWS: usize = 9;

#[derive(Clone, PartialEq)]
pub struct Fingerprint(pub [u8; 144]);

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fingerprint(mean {})", mean(self) as u32)
    }
}

fn mean(fp: &Fingerprint) -> f64 {
    fp.0.iter().map(|&b| b as f64).sum::<f64>() / fp.0.len() as f64
}

impl Fingerprint {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Fingerprint> {
        let b = s.as_bytes();
        if b.len() != 288 || !b.iter().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 144];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Fingerprint(out))
    }
}

/// RGBA frame -> 16x9 grid of area-averaged luma. Alpha is ignored.
pub fn fingerprint(rgba: &[u8], width: u32, height: u32) -> Fingerprint {
    let (w, h) = (width as usize, height as usize);
    let mut out = [0u8; 144];
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return Fingerprint(out);
    }
    for r in 0..ROWS {
        let y0 = r * h / ROWS;
        let y1 = ((r + 1) * h / ROWS).max(y0 + 1).min(h);
        for c in 0..COLS {
            let x0 = c * w / COLS;
            let x1 = ((c + 1) * w / COLS).max(x0 + 1).min(w);
            let mut sum = 0.0f64;
            let mut n = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    sum += 0.299 * rgba[i] as f64 + 0.587 * rgba[i + 1] as f64 + 0.114 * rgba[i + 2] as f64;
                    n += 1;
                }
            }
            out[r * COLS + c] = (sum / n.max(1) as f64).round().clamp(0.0, 255.0) as u8;
        }
    }
    Fingerprint(out)
}

/// False for black or flat frames (nothing to match on).
pub fn is_informative(fp: &Fingerprint) -> bool {
    let min = *fp.0.iter().min().unwrap_or(&0) as f64;
    let max = *fp.0.iter().max().unwrap_or(&0) as f64;
    mean(fp) >= 20.0 && max - min >= 24.0
}

/// Mean absolute difference.
pub fn distance(a: &Fingerprint, b: &Fingerprint) -> f64 {
    a.0.iter().zip(b.0.iter()).map(|(&x, &y)| (x as f64 - y as f64).abs()).sum::<f64>() / a.0.len() as f64
}

/// Time-sorted fingerprints; `None` = the frame could not be taken.
pub type Prints = Vec<(f64, Option<Fingerprint>)>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

/// Sample times `0, 4, 8, ...` up to `min(INTRO_SCAN_SECS, duration - 3)`.
pub fn scan_times(duration: f64) -> Vec<f64> {
    if !duration.is_finite() || duration < 30.0 {
        return Vec::new();
    }
    let limit = INTRO_SCAN_SECS.min(duration - 3.0);
    let mut v = Vec::new();
    let mut i = 0u32;
    loop {
        let t = i as f64 * INTRO_STEP_SECS;
        if t > limit {
            break;
        }
        v.push(t);
        i += 1;
    }
    v
}

const TIME_TOL: f64 = 0.5;

/// The sample of `list` at time `t` (within `TIME_TOL`): its exact time and print.
fn at(list: &[(f64, Option<Fingerprint>)], t: f64) -> Option<(f64, &Option<Fingerprint>)> {
    list.iter().find(|(rt, _)| (rt - t).abs() < TIME_TOL).map(|(rt, f)| (*rt, f))
}

/// The intro span in `own`'s timeline: the longest run of frames that look the
/// same in `reference` at a constant time offset.
pub fn find_intro(
    own: &[(f64, Option<Fingerprint>)],
    reference: &[(f64, Option<Fingerprint>)],
) -> Option<Span> {
    find_intro_pair(own, reference).map(|p| p.0)
}

/// [`find_intro`] plus the same intro in `reference`'s timeline: (own span,
/// reference span). The reference span runs from the first matched reference
/// time to the last matched reference time + `INTRO_STEP_SECS / 2`.
pub fn find_intro_pair(
    own: &[(f64, Option<Fingerprint>)],
    reference: &[(f64, Option<Fingerprint>)],
) -> Option<(Span, Span)> {
    let mut offsets: Vec<f64> = Vec::new();
    for (ot, _) in own {
        for (rt, _) in reference {
            offsets.push(rt - ot);
        }
    }
    offsets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    offsets.dedup_by(|a, b| (*a - *b).abs() < TIME_TOL);

    /// First and last matched (own time, reference time).
    type Run = Option<((f64, f64), (f64, f64))>;
    type Best = Option<(f64, (f64, f64), (f64, f64))>; // (secs, first, last)
    fn finish(run: &mut Run, best: &mut Best) {
        if let Some((first, last)) = run.take() {
            let secs = last.0 - first.0 + INTRO_STEP_SECS;
            if best.is_none_or(|b| secs > b.0) {
                *best = Some((secs, first, last));
            }
        }
    }

    let mut best: Best = None;
    for d in offsets {
        let mut run: Run = None;
        let mut gap = false;
        for (t, mine) in own {
            let Some((rt, theirs)) = at(reference, t + d) else {
                // no reference sample at this time: the run is over
                finish(&mut run, &mut best);
                gap = false;
                continue;
            };
            let inf_m = mine.as_ref().filter(|f| is_informative(f));
            let inf_r = theirs.as_ref().filter(|f| is_informative(f));
            match (inf_m, inf_r) {
                (Some(a), Some(b)) if distance(a, b) < MATCH_DIST => {
                    run = Some(match run {
                        Some((first, _)) => (first, (*t, rt)),
                        None => ((*t, rt), (*t, rt)),
                    });
                    gap = false;
                }
                (Some(_), Some(_)) => {
                    // informative mismatch ends the run
                    finish(&mut run, &mut best);
                    gap = false;
                }
                _ => {
                    // non-informative on either side: tolerate a single gap
                    if run.is_some() {
                        if gap {
                            finish(&mut run, &mut best);
                            gap = false;
                        } else {
                            gap = true;
                        }
                    }
                }
            }
        }
        finish(&mut run, &mut best);
    }
    let (secs, first, last) = best?;
    if secs >= MIN_INTRO_SECS {
        let half = INTRO_STEP_SECS / 2.0;
        Some((Span { start: first.0, end: last.0 + half }, Span { start: first.1, end: last.1 + half }))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pseudo-random bytes per seed: different seeds differ by far more than
    /// MATCH_DIST, the same seed by 0; informative.
    fn fp(seed: u8) -> Fingerprint {
        let mut x = (seed as u32).wrapping_mul(2654435761).wrapping_add(12345);
        let mut out = [0u8; 144];
        for b in out.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (x >> 24) as u8;
        }
        Fingerprint(out)
    }
    fn black() -> Fingerprint {
        Fingerprint([2; 144])
    }

    /// black x3 (+ `shift` more), `intro_len` intro frames (seeds 1..), then
    /// unique scenes seeded from `scene`.
    fn episode(shift: usize, intro_len: u8, scene: u8, total: usize) -> Prints {
        let mut v: Prints = Vec::new();
        for i in 0..total {
            let f = if i < 3 + shift {
                black()
            } else if i < 3 + shift + intro_len as usize {
                fp((i - 3 - shift) as u8 + 1)
            } else {
                fp(scene.wrapping_add(i as u8))
            };
            v.push((i as f64 * 4.0, Some(f)));
        }
        v
    }

    #[test]
    fn hex_round_trips() {
        let f = fp(7);
        assert_eq!(Fingerprint::from_hex(&f.to_hex()), Some(f));
    }

    #[test]
    fn from_hex_rejects_bad_input() {
        assert_eq!(Fingerprint::from_hex("abcd"), None);
        assert_eq!(Fingerprint::from_hex(&"zz".repeat(144)), None);
        assert_eq!(Fingerprint::from_hex(&"0".repeat(290)), None);
    }

    #[test]
    fn fingerprint_of_black_is_not_informative() {
        let rgba = vec![0u8; 64 * 36 * 4];
        assert!(!is_informative(&fingerprint(&rgba, 64, 36)));
    }

    #[test]
    fn fingerprint_of_a_gradient_is_informative() {
        let (w, h) = (64usize, 36usize);
        let mut rgba = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = (x * 255 / (w - 1)) as u8;
                let i = (y * w + x) * 4;
                rgba[i] = v;
                rgba[i + 1] = v;
                rgba[i + 2] = v;
            }
        }
        let f = fingerprint(&rgba, w as u32, h as u32);
        assert!(is_informative(&f));
        assert!(f.0[0] < f.0[15]);
    }

    #[test]
    fn fingerprint_ignores_alpha() {
        let mut a = vec![100u8; 32 * 18 * 4];
        let mut b = a.clone();
        for p in a.chunks_mut(4) {
            p[3] = 0;
        }
        for p in b.chunks_mut(4) {
            p[3] = 255;
        }
        assert_eq!(fingerprint(&a, 32, 18), fingerprint(&b, 32, 18));
    }

    #[test]
    fn debug_hides_the_bytes() {
        assert_eq!(format!("{:?}", Fingerprint([10; 144])), "Fingerprint(mean 10)");
    }

    #[test]
    fn scan_times_cover_four_minutes() {
        let t = scan_times(2700.0);
        assert_eq!(&t[..2], &[0.0, 4.0]);
        assert_eq!(*t.last().unwrap(), 240.0);
        assert_eq!(t.len(), 61);
        assert_eq!(*scan_times(100.0).last().unwrap(), 96.0);
        assert!(scan_times(29.0).is_empty());
        assert!(scan_times(f64::NAN).is_empty());
    }

    #[test]
    fn finds_the_intro_at_the_same_time() {
        let a = episode(0, 20, 100, 40);
        let b = episode(0, 20, 150, 40);
        assert_eq!(find_intro(&a, &b), Some(Span { start: 12.0, end: 12.0 + 19.0 * 4.0 + 2.0 }));
    }

    #[test]
    fn finds_the_intro_with_an_offset() {
        let own = episode(5, 20, 100, 50); // intro starts 20 s later
        let reference = episode(0, 20, 150, 50);
        assert_eq!(find_intro(&own, &reference), Some(Span { start: 32.0, end: 32.0 + 19.0 * 4.0 + 2.0 }));
    }

    #[test]
    fn black_frames_alone_do_not_match() {
        let v: Prints = (0..60).map(|i| (i as f64 * 4.0, Some(black()))).collect();
        assert_eq!(find_intro(&v, &v), None);
    }

    #[test]
    fn a_short_common_run_is_not_an_intro() {
        let a = episode(0, 4, 100, 40);
        let b = episode(0, 4, 150, 40);
        assert_eq!(find_intro(&a, &b), None);
    }

    #[test]
    fn a_dark_frame_inside_the_intro_does_not_break_it() {
        let a = episode(0, 20, 100, 40);
        let mut b = episode(0, 20, 150, 40);
        b[10].1 = None;
        let mut c = episode(0, 20, 150, 40);
        c[10].1 = Some(black());
        let want = Some(Span { start: 12.0, end: 12.0 + 19.0 * 4.0 + 2.0 });
        assert_eq!(find_intro(&a, &b), want);
        assert_eq!(find_intro(&a, &c), want);
    }

    #[test]
    fn pair_gives_the_reference_side_span_too() {
        let own = episode(5, 20, 100, 50); // intro at 32..108
        let reference = episode(0, 20, 150, 50); // intro at 12..88
        let (o, r) = find_intro_pair(&own, &reference).unwrap();
        assert_eq!(o, Span { start: 32.0, end: 32.0 + 19.0 * 4.0 + 2.0 });
        assert_eq!(r, Span { start: 12.0, end: 12.0 + 19.0 * 4.0 + 2.0 });
        assert_eq!(find_intro(&own, &reference), Some(o));
        assert_eq!(find_intro_pair(&own, &episode(0, 4, 150, 50)), None);
    }

    /// Only the reference's intro part (plus a margin of `margin` samples).
    fn intro_window(v: &Prints, span: Span, margin: f64) -> Prints {
        v.iter().filter(|(t, _)| *t >= span.start - margin && *t <= span.end + margin).cloned().collect()
    }

    #[test]
    fn an_episode_against_its_own_stored_intro_gives_the_intro_not_the_story() {
        let e05 = episode(0, 20, 150, 60);
        let e04 = episode(0, 20, 100, 60);
        let (_, ref_span) = find_intro_pair(&e04, &e05).unwrap();
        let stored = intro_window(&e05, ref_span, 0.0);
        // E05 played later: it matches its own stored intro, not its whole story.
        assert_eq!(find_intro(&e05, &stored), Some(Span { start: 12.0, end: 90.0 }));
        // With a margin of story frames beside the intro, those match too:
        // the span grows by at most the margin, never into the rest.
        let stored = intro_window(&e05, ref_span, 8.0);
        assert_eq!(find_intro(&e05, &stored), Some(Span { start: 12.0, end: 98.0 }));
        // The whole scan stored (the old behaviour) gives the story too.
        assert_eq!(find_intro(&e05, &e05), Some(Span { start: 12.0, end: 59.0 * 4.0 + 2.0 }));
    }

    #[test]
    fn a_stored_intro_window_matches_a_new_episode_with_another_cold_open() {
        let e05 = episode(0, 20, 150, 60);
        let (_, ref_span) = find_intro_pair(&episode(0, 20, 100, 60), &e05).unwrap();
        let stored = intro_window(&e05, ref_span, 8.0);
        let e07 = episode(7, 20, 200, 60); // intro 28 s later
        assert_eq!(find_intro(&e07, &stored), Some(Span { start: 40.0, end: 40.0 + 19.0 * 4.0 + 2.0 }));
    }

    #[test]
    fn different_episodes_without_a_common_run_give_none() {
        let a: Prints = (0..40).map(|i| (i as f64 * 4.0, Some(fp(100 + i as u8)))).collect();
        let b: Prints = (0..40).map(|i| (i as f64 * 4.0, Some(fp(200 + i as u8)))).collect();
        assert_eq!(find_intro(&a, &b), None);
    }
}
