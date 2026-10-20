//! Closing-credits detection for "Next episode" (pure, no mpv/egui).
//!
//! `scan_times` plans which tail frames to sample, `classify` labels one RGBA
//! preview frame (Credits / Black / Content), and `offer_time` picks when to
//! offer the next episode: at the start of the final credits stretch, or at
//! the real end when anything after the credits looks like content (stingers).

use crate::thumbs::BUCKET_SECS;

/// How far before the end of the video the tail scan starts.
pub const SCAN_TAIL_SECS: f64 = 480.0;

/// What a sampled frame looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Credits,
    Black,
    Content,
}

/// The luma plane is sampled down by `width / PLANE_MAX_WIDTH` (previews are
/// ~240 wide: mpv returns 241 for 1280x720, which must not halve them).
const PLANE_MAX_WIDTH: u32 = 240;
/// Black: mean luma below this ...
const BLACK_MAX_MEAN: f64 = 20.0;
/// ... and standard deviation below this (credits on black have ~40-60).
const BLACK_MAX_STD: f64 = 8.0;
/// Credits background (median luma) must be at most this dark.
const BG_MAX_LUMA: u8 = 110;
/// A background pixel is within this of the median luma ...
const BG_TOLERANCE: u8 = 24;
/// ... and at least this fraction of the frame is background ("plain").
const BG_MIN_FRACTION: f64 = 0.80;
/// A text pixel is this much brighter than the median ...
const TEXT_CONTRAST: u8 = 70;
/// ... and at least this bright.
const TEXT_MIN_LUMA: u8 = 100;
/// Text pixels cover this fraction of the frame (small, but not a speck).
const TEXT_MIN_FRACTION: f64 = 0.002;
const TEXT_MAX_FRACTION: f64 = 0.15;
/// Fraction of text pixels with a text pixel directly above or below:
/// glyphs are several pixels tall, random speckle (stars, noise) is not
/// (a 1-in-60 starfield has ~0.03). Small credits text downscaled to the
/// 240 px preview has 1 px tall horizontal strokes: measured 0.31-0.34 on
/// 22 px text of a 720p video, so 0.5 was too strict.
const TEXT_MIN_COHERENCE: f64 = 0.2;
/// Longest horizontal run of text pixels, as a fraction of the plane width
/// (a word, not a lit wall or a horizon line).
const MAX_RUN_FRACTION: u32 = 6; // width / 6
/// A text band (one line of text) is 2 ..= height / 8 rows tall.
const BAND_MIN_ROWS: usize = 2;
const BAND_MAX_DIVISOR: usize = 8;
/// Credits need at least this many separate text bands.
const MIN_BANDS: usize = 3;

/// Labels one RGBA frame (row-major, 4 bytes per pixel, as
/// `thumb_mpv::Thumb`). A buffer too short for `width x height` is Content.
pub fn classify(rgba: &[u8], width: u32, height: u32) -> FrameKind {
    let Some((luma, pw, ph)) = luma_plane(rgba, width, height) else {
        return FrameKind::Content;
    };
    let n = luma.len() as f64;
    let mean = luma.iter().map(|&l| l as f64).sum::<f64>() / n;
    let var = luma.iter().map(|&l| (l as f64 - mean).powi(2)).sum::<f64>() / n;
    if mean < BLACK_MAX_MEAN && var.sqrt() < BLACK_MAX_STD {
        return FrameKind::Black;
    }
    if is_credits(&luma, pw, ph) {
        FrameKind::Credits
    } else {
        FrameKind::Content
    }
}

/// Luma (0..=255) sampled with a step of `width / 240` (at least 1), so the
/// plane is 240..480 wide for wider frames and a 241 px preview keeps every pixel.
fn luma_plane(rgba: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, usize, usize)> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || rgba.len() < w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    let step = (width / PLANE_MAX_WIDTH).max(1) as usize;
    let (pw, ph) = (w.div_ceil(step), h.div_ceil(step));
    let mut plane = Vec::with_capacity(pw * ph);
    for y in (0..h).step_by(step) {
        for x in (0..w).step_by(step) {
            let i = (y * w + x) * 4;
            let (r, g, b) = (rgba[i] as u32, rgba[i + 1] as u32, rgba[i + 2] as u32);
            plane.push(((54 * r + 183 * g + 19 * b) >> 8) as u8);
        }
    }
    Some((plane, pw, ph))
}

fn median(luma: &[u8]) -> u8 {
    let mut hist = [0usize; 256];
    for &l in luma {
        hist[l as usize] += 1;
    }
    let half = luma.len() / 2;
    let mut seen = 0;
    for (v, &c) in hist.iter().enumerate() {
        seen += c;
        if seen > half {
            return v as u8;
        }
    }
    255
}

fn is_credits(luma: &[u8], pw: usize, ph: usize) -> bool {
    let n = luma.len() as f64;
    let m = median(luma);
    if m > BG_MAX_LUMA {
        return false;
    }
    let bg = luma.iter().filter(|&&l| l.abs_diff(m) <= BG_TOLERANCE).count() as f64;
    if bg / n < BG_MIN_FRACTION {
        return false;
    }
    let text_min = m.saturating_add(TEXT_CONTRAST).max(TEXT_MIN_LUMA);
    let text: Vec<bool> = luma.iter().map(|&l| l >= text_min).collect();
    let text_count = text.iter().filter(|&&t| t).count();
    let coverage = text_count as f64 / n;
    if !(TEXT_MIN_FRACTION..=TEXT_MAX_FRACTION).contains(&coverage) {
        return false;
    }
    let coherent = (0..ph)
        .flat_map(|y| (0..pw).map(move |x| (x, y)))
        .filter(|&(x, y)| text[y * pw + x])
        .filter(|&(x, y)| (y > 0 && text[(y - 1) * pw + x]) || (y + 1 < ph && text[(y + 1) * pw + x]))
        .count();
    if (coherent as f64) < TEXT_MIN_COHERENCE * text_count as f64 {
        return false;
    }

    // Per row: number of text runs, or None when the row has no text or a
    // run too long for a word.
    let max_run = (pw as u32 / MAX_RUN_FRACTION).max(2) as usize;
    let rows: Vec<Option<usize>> = (0..ph)
        .map(|y| {
            let row = &text[y * pw..(y + 1) * pw];
            let (mut runs, mut len) = (0, 0);
            for &t in row.iter().chain(std::iter::once(&false)) {
                if t {
                    len += 1;
                } else if len > 0 {
                    if len > max_run {
                        return None;
                    }
                    runs += 1;
                    len = 0;
                }
            }
            (runs > 0).then_some(runs)
        })
        .collect();

    // Bands = maximal stretches of text rows; a band counts when it is
    // line-sized and at least one of its rows holds several runs (words).
    let band_max = (ph / BAND_MAX_DIVISOR).max(BAND_MIN_ROWS + 1);
    let mut bands = 0;
    let mut y = 0;
    while y < ph {
        if rows[y].is_none() {
            y += 1;
            continue;
        }
        let start = y;
        let mut most_runs = 0;
        while y < ph {
            match rows[y] {
                Some(r) => most_runs = most_runs.max(r),
                None => break,
            }
            y += 1;
        }
        let height = y - start;
        if (BAND_MIN_ROWS..=band_max).contains(&height) && most_runs >= 2 {
            bands += 1;
        }
    }
    bands >= MIN_BANDS
}

/// Times (seconds) to sample for the tail scan: from `duration -
/// SCAN_TAIL_SECS` (at least 0) every `BUCKET_SECS`, and finally
/// `duration - 3`. A step closer than half a bucket to that last time is
/// skipped. Empty for videos shorter than 30 s (or an unknown duration).
pub fn scan_times(duration: f64) -> Vec<f64> {
    if !duration.is_finite() || duration < 30.0 {
        return Vec::new();
    }
    let last = duration - 3.0;
    let start = (duration - SCAN_TAIL_SECS).max(0.0);
    let mut times: Vec<f64> = (0..)
        .map(|i| start + i as f64 * BUCKET_SECS)
        .take_while(|&t| t < last - BUCKET_SECS / 2.0)
        .collect();
    times.push(last);
    times
}

/// When to offer the next episode, from tail samples `(time, kind)` (`None`
/// = that frame could not be read). The earliest sample time T such that
/// every sample from T on is Credits or Black and at least one is Credits;
/// `duration` (the real end) if a frame failed, the last sample is Content,
/// or no such T exists. Content after credits (a stinger) therefore moves the
/// offer behind it.
pub fn offer_time(samples: &[(f64, Option<FrameKind>)], duration: f64) -> f64 {
    let mut sorted: Vec<(f64, FrameKind)> = Vec::with_capacity(samples.len());
    for &(t, k) in samples {
        match k {
            Some(k) => sorted.push((t, k)),
            None => return duration,
        }
    }
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let tail_start = sorted.iter().rposition(|&(_, k)| k == FrameKind::Content).map_or(0, |i| i + 1);
    let tail = &sorted[tail_start..];
    if tail.iter().any(|&(_, k)| k == FrameKind::Credits) {
        tail[0].0
    } else {
        duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FrameKind::*;

    const W: u32 = 240;
    const H: u32 = 135;

    /// RGBA frame whose pixel (x, y) is `f(x, y)` = (r, g, b).
    fn frame(f: impl Fn(u32, u32) -> (u8, u8, u8)) -> Vec<u8> {
        let mut v = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            for x in 0..W {
                let (r, g, b) = f(x, y);
                v.extend_from_slice(&[r, g, b, 255]);
            }
        }
        v
    }

    /// Is (x, y) inside a "text line" at `top` (5 px tall): words of 3 px wide
    /// glyphs with 2 px gaps, 6 px between words, centred.
    fn in_text_line(x: u32, y: u32, top: u32) -> bool {
        if y < top || y >= top + 5 || !(60..180).contains(&x) {
            return false;
        }
        let in_word = (x - 60) % 30; // 24 px word (5 glyphs) + 6 px space
        in_word < 24 && in_word % 5 < 3
    }

    /// Dark background with bright glyph rows at the given tops, plus a grey
    /// anti-aliasing fringe one pixel right of each glyph.
    fn text_frame(tops: &[u32]) -> Vec<u8> {
        frame(|x, y| {
            if tops.iter().any(|&t| in_text_line(x, y, t)) {
                (235, 235, 235)
            } else if x > 0 && tops.iter().any(|&t| in_text_line(x - 1, y, t)) {
                (110, 110, 110)
            } else {
                (8, 8, 12)
            }
        })
    }

    /// Small deterministic pseudo-random noise in 0..n.
    fn noise(x: u32, y: u32, n: u32) -> u32 {
        let mut h = x.wrapping_mul(374_761_393) ^ y.wrapping_mul(668_265_263);
        h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
        (h ^ (h >> 16)) % n
    }

    #[test]
    fn black_frame_is_black() {
        let f = frame(|x, y| {
            let n = noise(x, y, 6) as u8;
            (3 + n, 3 + n, 4 + n)
        });
        assert_eq!(classify(&f, W, H), Black);
    }

    #[test]
    fn white_text_rows_on_dark_is_credits() {
        let f = text_frame(&[20, 40, 60, 80, 100]);
        assert_eq!(classify(&f, W, H), Credits);
    }

    /// Like the 240 px preview of small credits text: "E"-shaped 3x5 glyphs
    /// whose horizontal strokes are 1 px tall (coherence ~0.45).
    #[test]
    fn thin_stroke_text_is_credits() {
        let glyph = |x: u32, y: u32, top: u32| {
            if y < top || y >= top + 5 || !(60..180).contains(&x) {
                return false;
            }
            let (gx, gy) = ((x - 60) % 5, y - top); // 3 px glyph + 2 px gap
            gx < 3 && (gx == 0 || gy % 2 == 0)
        };
        let f = frame(|x, y| if [20, 45, 70, 95].iter().any(|&t| glyph(x, y, t)) { (150, 150, 150) } else { (0, 0, 0) });
        assert_eq!(classify(&f, W, H), Credits);
    }

    /// mpv's previews of 1280x720 come out 241 px wide: that must not halve
    /// the plane (thin text would be lost).
    #[test]
    fn preview_241_wide_keeps_thin_text() {
        let (w, h) = (241, 136);
        let glyph = |x: u32, y: u32, top: u32| {
            if y < top || y >= top + 5 || !(60..180).contains(&x) {
                return false;
            }
            let (gx, gy) = ((x - 60) % 5, y - top);
            gx < 3 && (gx == 0 || gy % 2 == 0)
        };
        let mut f = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let v = if [20, 45, 70, 95].iter().any(|&t| glyph(x, y, t)) { 150 } else { 0 };
                f.extend_from_slice(&[v, v, v, 255]);
            }
        }
        assert_eq!(classify(&f, w, h), Credits);
    }

    #[test]
    fn three_text_rows_on_dark_blue_is_credits() {
        let f = frame(|x, y| {
            if [30, 60, 90].iter().any(|&t| in_text_line(x, y, t)) {
                (240, 240, 200)
            } else {
                (10, 20, 70)
            }
        });
        assert_eq!(classify(&f, W, H), Credits);
    }

    #[test]
    fn noisy_gradient_scene_is_content() {
        let f = frame(|x, y| {
            let n = noise(x, y, 60);
            ((x / 2 + n) as u8, (y + n) as u8, (100 + n) as u8)
        });
        assert_eq!(classify(&f, W, H), Content);
    }

    #[test]
    fn dark_scene_with_detail_is_content() {
        // Night scene: mostly dark, a lit window and some textured ground.
        let f = frame(|x, y| {
            if (150..190).contains(&x) && (20..60).contains(&y) {
                (200, 180, 120)
            } else if y > 100 {
                let n = noise(x, y, 70) as u8;
                (20 + n, 25 + n, 20 + n)
            } else {
                (12, 12, 20)
            }
        });
        assert_eq!(classify(&f, W, H), Content);
    }

    #[test]
    fn bright_flat_frame_is_content() {
        let f = frame(|_, _| (230, 230, 225));
        assert_eq!(classify(&f, W, H), Content);
    }

    #[test]
    fn single_title_line_is_not_credits() {
        let f = text_frame(&[60]);
        assert_eq!(classify(&f, W, H), Content);
    }

    #[test]
    fn starfield_is_not_credits() {
        let f = frame(|x, y| if noise(x, y, 60) == 0 { (220, 220, 230) } else { (5, 5, 10) });
        assert_ne!(classify(&f, W, H), Credits);
    }

    #[test]
    fn short_buffer_is_content() {
        assert_eq!(classify(&[0; 16], W, H), Content);
    }

    #[test]
    fn wide_frame_is_downsampled() {
        // Same layout at 4x the size still reads as credits.
        let (w, h) = (W * 4, H * 4);
        let small = text_frame(&[20, 40, 60, 80]);
        let mut big = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let i = (((y / 4) * W + x / 4) * 4) as usize;
                big.extend_from_slice(&small[i..i + 4]);
            }
        }
        assert_eq!(classify(&big, w, h), Credits);
    }

    // --- decision ---

    fn s(kinds: &[Option<FrameKind>]) -> Vec<(f64, Option<FrameKind>)> {
        kinds.iter().enumerate().map(|(i, k)| (1000.0 + 10.0 * i as f64, *k)).collect()
    }

    const D: f64 = 1100.0;

    #[test]
    fn offer_at_start_of_final_credits() {
        let v = s(&[Some(Content), Some(Content), Some(Credits), Some(Credits), Some(Credits)]);
        assert_eq!(offer_time(&v, D), 1020.0);
    }

    #[test]
    fn stinger_after_credits_moves_offer_to_the_end() {
        let v = s(&[Some(Content), Some(Credits), Some(Credits), Some(Content), Some(Black)]);
        assert_eq!(offer_time(&v, D), D);
    }

    #[test]
    fn stinger_then_more_credits_offers_after_the_stinger() {
        let v = s(&[Some(Credits), Some(Black), Some(Content), Some(Black), Some(Credits)]);
        assert_eq!(offer_time(&v, D), 1030.0);
    }

    #[test]
    fn all_black_tail_offers_at_end() {
        let v = s(&[Some(Black), Some(Black), Some(Black)]);
        assert_eq!(offer_time(&v, D), D);
    }

    #[test]
    fn all_content_tail_offers_at_end() {
        let v = s(&[Some(Content), Some(Content), Some(Content)]);
        assert_eq!(offer_time(&v, D), D);
    }

    #[test]
    fn last_sample_content_offers_at_end() {
        let v = s(&[Some(Credits), Some(Credits), Some(Content)]);
        assert_eq!(offer_time(&v, D), D);
    }

    #[test]
    fn failed_frame_offers_at_end() {
        let v = s(&[Some(Content), Some(Credits), None, Some(Credits)]);
        assert_eq!(offer_time(&v, D), D);
    }

    #[test]
    fn no_samples_offers_at_end() {
        assert_eq!(offer_time(&[], D), D);
    }

    #[test]
    fn black_gap_inside_credits_is_neutral() {
        let v = s(&[Some(Content), Some(Credits), Some(Black), Some(Credits), Some(Black)]);
        assert_eq!(offer_time(&v, D), 1010.0);
    }

    #[test]
    fn scan_times_short_video_is_empty() {
        assert!(scan_times(29.9).is_empty());
        assert!(scan_times(0.0).is_empty());
        assert!(scan_times(f64::NAN).is_empty());
    }

    #[test]
    fn scan_times_cover_the_tail_and_end_at_duration_minus_3() {
        let t = scan_times(2700.0);
        assert_eq!(t[0], 2700.0 - SCAN_TAIL_SECS);
        assert_eq!(*t.last().unwrap(), 2697.0);
        assert!(t.windows(2).all(|p| p[1] > p[0] && p[1] - p[0] <= BUCKET_SECS + 1e-9));
        assert_eq!(t[1] - t[0], BUCKET_SECS);
        assert_eq!(t.len(), 49); // 2220..=2690 step 10 (48) + 2697
    }

    #[test]
    fn scan_times_start_at_zero_for_short_videos() {
        let t = scan_times(45.0);
        assert_eq!(t, vec![0.0, 10.0, 20.0, 30.0, 42.0]);
    }
}
