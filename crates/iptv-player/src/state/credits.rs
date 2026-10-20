//! Closing-credits detection for "Next episode" (pure, no mpv/egui).
//!
//! `scan_times` plans which tail frames to sample, `classify` labels one RGBA
//! preview frame (Credits / Black / Card / Content), and `offer_time` picks when to
//! offer the next episode: at the start of the final credits stretch, or at
//! the real end when content follows the credits (stingers: two or more
//! consecutive Content samples; a single one is noise). `refine_times`
//! plans extra frames around such single samples, so a short stinger the
//! 10 s grid caught once still shows up twice.

use crate::thumbs::BUCKET_SECS;

/// How far before the end of the video the tail scan starts.
pub const SCAN_TAIL_SECS: f64 = 480.0;

/// What a sampled frame looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Credits,
    Black,
    /// A plain-background card (end logo, production-company card, a lone
    /// title) of any luminance: neutral like Black, never Content and never
    /// Credits.
    Card,
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
/// Text pixels cover this fraction of the frame (small, but not a speck:
/// 0.001 is ~33 pixels of a 240 px preview). A sparse page of five or six
/// short lines of small grey credits text measured 0.0019 on a real episode,
/// so 0.002 was too strict.
const TEXT_MIN_FRACTION: f64 = 0.001;
const TEXT_MAX_FRACTION: f64 = 0.15;
/// Fraction of text pixels with at least a faint text pixel (see
/// [`TEXT_WEAK_CONTRAST`]) directly above or below: glyphs are several
/// pixels tall, random speckle (stars, noise) is not (a 1-in-60 starfield
/// has ~0.03). Small credits text downscaled to the 240 px preview has 1 px
/// tall horizontal strokes: measured 0.31-0.34 on 22 px text of a 720p
/// video, so 0.5 was too strict.
const TEXT_MIN_COHERENCE: f64 = 0.2;
/// A vertical neighbour counts for coherence when it is at least half as far
/// above the background as text must be (hysteresis): the strokes of small
/// grey text are anti-aliased, their upper/lower pixels are dimmer than the
/// text threshold. With full text pixels only, thin real credits measured
/// 0.13-0.32; with this rule 0.87-0.97 (a starfield stays ~0, its
/// neighbours are background).
const TEXT_WEAK_CONTRAST: u8 = TEXT_CONTRAST / 2;
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
    } else if is_card(&luma) {
        FrameKind::Card
    } else {
        FrameKind::Content
    }
}

/// A card pixel is within this of the dominant luma ("flat" background) ...
const CARD_FLAT_TOLERANCE: u8 = 10;
/// ... and at least this fraction of the frame is flat background. Dark
/// show scenes look plain at a glance but are gradients: at most 0.83 of a
/// real episode's darkest scenes was within 10 of their dominant luma, while
/// two end-logo cards (dark textured, white) measured 0.94-0.95.
const CARD_MIN_FLAT: f64 = 0.90;
/// A foreground pixel differs this much from the dominant luma ...
const CARD_FG_CONTRAST: u8 = 40;
/// ... and the foreground is at least this fraction (a logo or a line of
/// text, not an empty flat frame; at most 1 - CARD_MIN_FLAT by the above).
const CARD_MIN_FOREGROUND: f64 = 0.002;

/// Plain-background card of any luminance (checked after credits): the
/// dominant luma covers most of the frame and something visible sits on it.
fn is_card(luma: &[u8]) -> bool {
    let n = luma.len() as f64;
    let bg = mode(luma);
    let flat = luma.iter().filter(|&&l| l.abs_diff(bg) <= CARD_FLAT_TOLERANCE).count() as f64;
    let fg = luma.iter().filter(|&&l| l.abs_diff(bg) > CARD_FG_CONTRAST).count() as f64;
    flat / n >= CARD_MIN_FLAT && fg / n >= CARD_MIN_FOREGROUND
}

/// The most common luma, counting each value with its neighbours within 2
/// (so a slightly noisy background still has one clear peak).
fn mode(luma: &[u8]) -> u8 {
    let mut hist = [0usize; 256];
    for &l in luma {
        hist[l as usize] += 1;
    }
    (0..256usize)
        .max_by_key(|&v| (v.saturating_sub(2)..=(v + 2).min(255)).map(|i| hist[i]).sum::<usize>())
        .unwrap_or(0) as u8
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
    let weak_min = m.saturating_add(TEXT_WEAK_CONTRAST).max(TEXT_MIN_LUMA / 2);
    let text: Vec<bool> = luma.iter().map(|&l| l >= text_min).collect();
    let weak: Vec<bool> = luma.iter().map(|&l| l >= weak_min).collect();
    let text_count = text.iter().filter(|&&t| t).count();
    let coverage = text_count as f64 / n;
    if !(TEXT_MIN_FRACTION..=TEXT_MAX_FRACTION).contains(&coverage) {
        return false;
    }
    let coherent = (0..ph)
        .flat_map(|y| (0..pw).map(move |x| (x, y)))
        .filter(|&(x, y)| text[y * pw + x])
        .filter(|&(x, y)| (y > 0 && weak[(y - 1) * pw + x]) || (y + 1 < ph && weak[(y + 1) * pw + x]))
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

/// Gap between a refinement frame and the isolated Content sample it checks.
pub const REFINE_OFFSET_SECS: f64 = 5.0;
/// Most isolated Content samples refined per scan (those closest to the end).
pub const MAX_REFINED: usize = 12;

/// The neighbours (previous / next sample) of index `i` in `kinds`.
fn neighbours<T: Copy>(kinds: &[T], i: usize) -> impl Iterator<Item = T> + '_ {
    let prev = i.checked_sub(1).map(|j| kinds[j]);
    let next = kinds.get(i + 1).copied();
    prev.into_iter().chain(next)
}

/// Sample `i` (of time-sorted labels; `None` = failed frame) is Content
/// and none of its existing neighbours (one for the first/last sample) is:
/// a single-sample flash, read as noise (a logo card the classifier missed,
/// a lone misread credits frame). Black and Card neighbours are neutral. Two or more consecutive Content samples are real content.
fn isolated_content(kinds: &[Option<FrameKind>], i: usize) -> bool {
    kinds[i] == Some(FrameKind::Content)
        && kinds.len() > 1
        && neighbours(kinds, i).all(|k| k != Some(FrameKind::Content))
}

/// Sample `i` is Credits and all its existing neighbours (at least one) are
/// Content: a stray credits-looking frame inside the show.
fn isolated_credits(kinds: &[Option<FrameKind>], i: usize) -> bool {
    kinds[i] == Some(FrameKind::Credits)
        && kinds.len() > 1
        && neighbours(kinds, i).all(|k| k == Some(FrameKind::Content))
}

/// Extra times to sample around isolated Content samples (see
/// [`offer_time`]), so a short stinger the 10 s grid caught only once shows
/// up as two consecutive Content samples: `t - 5` and `t + 5` for at most
/// [`MAX_REFINED`] of them (those closest to the end first), clamped to
/// `0 ..= duration - 3`, without times already sampled, sorted, no duplicates.
pub fn refine_times(samples: &[(f64, Option<FrameKind>)], duration: f64) -> Vec<f64> {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let kinds: Vec<Option<FrameKind>> = sorted.iter().map(|s| s.1).collect();
    let last = (duration - 3.0).max(0.0);
    let sampled = |t: f64| sorted.iter().any(|s| (s.0 - t).abs() < 1e-6);
    let mut out: Vec<f64> = Vec::new();
    for i in (0..sorted.len()).rev().filter(|&i| isolated_content(&kinds, i)).take(MAX_REFINED) {
        for t in [sorted[i].0 - REFINE_OFFSET_SECS, sorted[i].0 + REFINE_OFFSET_SECS] {
            let t = t.clamp(0.0, last);
            if !sampled(t) && !out.iter().any(|&o| (o - t).abs() < 1e-6) {
                out.push(t);
            }
        }
    }
    out.sort_by(f64::total_cmp);
    out
}

/// When to offer the next episode, from tail samples `(time, kind)` (`None`
/// = that frame could not be read; any failed frame = `duration`, the real
/// end).
///
/// Noise first, on the original labels of the time-sorted samples: an
/// isolated Content sample (no Content neighbour) becomes neutral (it never
/// blocks, never counts as credits, never starts the offer); an isolated
/// Credits sample (only Content neighbours) becomes Content. Then the offer
/// is at the earliest Credits, Black or Card sample after the last Content
/// sample, if that tail holds at least one Credits sample; else `duration`.
/// Black and Card are neutral: they never end the tail and never count as
/// credits on their own.
/// Content after credits (a stinger: two or more consecutive Content
/// samples) therefore moves the offer behind it.
pub fn offer_time(samples: &[(f64, Option<FrameKind>)], duration: f64) -> f64 {
    if samples.iter().any(|s| s.1.is_none()) {
        return duration;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let kinds: Vec<Option<FrameKind>> = sorted.iter().map(|s| s.1).collect();
    // `None` = neutral (isolated Content).
    let smoothed: Vec<Option<FrameKind>> = (0..kinds.len())
        .map(|i| {
            if isolated_content(&kinds, i) {
                None
            } else if isolated_credits(&kinds, i) {
                Some(FrameKind::Content)
            } else {
                kinds[i]
            }
        })
        .collect();
    let tail_start = smoothed.iter().rposition(|&k| k == Some(FrameKind::Content)).map_or(0, |i| i + 1);
    let tail = &sorted[tail_start..];
    let tail_kinds = &smoothed[tail_start..];
    if !tail_kinds.contains(&Some(FrameKind::Credits)) {
        return duration;
    }
    tail.iter().zip(tail_kinds).find(|(_, k)| k.is_some()).map_or(duration, |(s, _)| s.0)
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

    /// One line of text on a plain background is not credits. It was
    /// Content before the Card kind; it is now a neutral Card (a title card).
    #[test]
    fn single_title_line_is_not_credits() {
        let f = text_frame(&[60]);
        assert_eq!(classify(&f, W, H), Card);
    }

    /// Like a sparse page of real credits at 240 px: five short two-column
    /// lines of small grey text whose strokes are 1 px tall and alternate
    /// between the two rows of a line, each with a dim anti-aliased pixel
    /// above or below (only the hysteresis makes them coherent).
    #[test]
    fn sparse_small_grey_credits_are_credits() {
        let f = frame(|x, y| {
            let words = (76..106).contains(&x) || (124..164).contains(&x);
            let Some(top) = [40u32, 50, 60, 70, 80].into_iter().find(|&t| (t..t + 2).contains(&y)) else {
                return (0, 0, 0);
            };
            let row = y - top; // 0 or 1
            let v = match (words, x % 4, row) {
                (true, 0, 0) | (true, 2, 1) => 140, // stroke
                (true, 0, 1) | (true, 2, 0) => 70,  // its anti-aliased neighbour
                _ => 0,
            };
            (v, v, v)
        });
        assert_eq!(classify(&f, W, H), Credits);
    }

    #[test]
    fn logo_card_on_white_is_card() {
        let f = frame(|x, y| {
            let logo = (95..145).contains(&x) && (40..70).contains(&y) && (x / 5 + y / 5) % 2 == 0;
            let tagline = in_text_line(x, y, 85);
            if logo || tagline {
                (40, 40, 50)
            } else {
                let n = noise(x, y, 4) as u8;
                (250 - n, 250 - n, 250 - n)
            }
        });
        assert_eq!(classify(&f, W, H), Card);
    }

    #[test]
    fn coloured_logo_on_dark_textured_background_is_card() {
        let f = frame(|x, y| {
            if (100..140).contains(&x) && (35..65).contains(&y) && (x - 100) % 8 < 5 {
                (230, 120, 30) // orange logo strokes
            } else if in_text_line(x, y, 75) || in_text_line(x, y, 88) {
                (160, 160, 160)
            } else {
                let n = noise(x, y, 8) as u8; // fine texture
                (6 + n, 6 + n, 8 + n)
            }
        });
        assert_eq!(classify(&f, W, H), Card);
    }

    #[test]
    fn busy_scene_is_content() {
        // Sky gradient, a textured ground and a figure.
        let f = frame(|x, y| {
            if (100..120).contains(&x) && (50..110).contains(&y) {
                (90, 60, 40)
            } else if y < 60 {
                (120 + y as u8, 150 + y as u8, 190)
            } else {
                let n = noise(x, y, 80) as u8;
                (40 + n, 50 + n, 30 + n / 2)
            }
        });
        assert_eq!(classify(&f, W, H), Content);
    }

    /// Dark scenes look plain but are gradients: never a Card.
    #[test]
    fn dark_gradient_scene_is_content() {
        let f = frame(|x, y| {
            if (150..170).contains(&x) && (40..70).contains(&y) {
                (200, 160, 120) // a lit face
            } else {
                let v = (5 + x / 6) as u8; // 5 ..= 44 across the frame
                (v, v, v + 2)
            }
        });
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

    /// A stinger is two or more consecutive Content samples (a single one
    /// is noise since the noise rule; it was one sample before).
    #[test]
    fn stinger_after_credits_moves_offer_to_the_end() {
        let v = s(&[Some(Content), Some(Credits), Some(Credits), Some(Content), Some(Content), Some(Black)]);
        assert_eq!(offer_time(&v, D), D);
    }

    /// Two-sample stinger (was one sample before the noise rule).
    #[test]
    fn stinger_then_more_credits_offers_after_the_stinger() {
        let v = s(&[Some(Credits), Some(Black), Some(Content), Some(Content), Some(Black), Some(Credits)]);
        assert_eq!(offer_time(&v, D), 1040.0);
    }

    #[test]
    fn two_content_samples_after_the_credits_offer_at_end() {
        let v = s(&[Some(Content), Some(Credits), Some(Credits), Some(Credits), Some(Content), Some(Content)]);
        assert_eq!(offer_time(&v, D), D);
    }

    #[test]
    fn lone_content_inside_the_credits_is_noise() {
        let v = s(&[Some(Content), Some(Content), Some(Credits), Some(Credits), Some(Content), Some(Credits), Some(Black)]);
        assert_eq!(offer_time(&v, D), 1020.0);
    }

    #[test]
    fn lone_content_as_last_sample_is_noise() {
        let v = s(&[Some(Content), Some(Credits), Some(Credits), Some(Content)]);
        assert_eq!(offer_time(&v, D), 1010.0);
    }

    #[test]
    fn noise_never_starts_the_offer() {
        // X X | x(noise) B C: the offer starts at the black frame, not the noise.
        let v = s(&[Some(Content), Some(Content), Some(Black), Some(Content), Some(Black), Some(Credits)]);
        assert_eq!(offer_time(&v, D), 1020.0);
        // X c x C C (as in the real episode): the stray c (only Content
        // neighbours on the original labels) is content, x is noise, the
        // offer starts at the first credits after it.
        let v = s(&[Some(Content), Some(Credits), Some(Content), Some(Credits), Some(Credits)]);
        assert_eq!(offer_time(&v, D), 1030.0);
    }

    #[test]
    fn noise_alone_is_not_credits() {
        let v = s(&[Some(Black), Some(Content), Some(Black)]);
        assert_eq!(offer_time(&v, D), D);
    }

    #[test]
    fn stray_credits_inside_the_show_is_content() {
        // X C X X B C: the stray C is content, offer at the black frame.
        let v = s(&[Some(Content), Some(Credits), Some(Content), Some(Content), Some(Black), Some(Credits)]);
        assert_eq!(offer_time(&v, D), 1040.0);
    }

    /// Real episode (duration 3573 s): credits start at the BB (3453 s);
    /// lone X samples inside the credits and before the final black frame
    /// used to push the offer to the end.
    fn real_episode() -> (Vec<(f64, Option<FrameKind>)>, f64) {
        let letters = "XXXXXXXXXXXXCXXXCXXXXXXXXXXXCXXXXXCXBBCCCCXCCCCXB";
        assert_eq!(letters.len(), 49);
        let v = letters
            .chars()
            .enumerate()
            .map(|(i, c)| {
                let t = if i == 48 { 3570.0 } else { 3093.0 + 10.0 * i as f64 };
                let k = match c {
                    'C' => Credits,
                    'B' => Black,
                    _ => Content,
                };
                (t, Some(k))
            })
            .collect();
        (v, 3573.0)
    }

    /// The same episode with the current classifier, on the time grid of its
    /// saved scan frames (55 samples incl. refinement): scenes X (some dark
    /// ones read as C), BB at 3453/3463, credits 3473..=3553, two end-logo
    /// cards K at 3558/3563, BB at 3568/3570.
    #[test]
    fn real_episode_frames_offer_at_the_credits_start() {
        let letters = "XXXXXXXXXXXXCXXXCXXXXXXXXXXXCXXXXXCCXXBBCCCCCCCCCCCKKBB";
        let mut times: Vec<f64> = (0..34).map(|i| 3093.0 + 10.0 * i as f64).collect(); // 3093..=3423
        times.extend([3433.0, 3438.0, 3443.0, 3448.0, 3453.0, 3463.0, 3473.0, 3483.0, 3493.0, 3503.0]);
        times.extend([3508.0, 3513.0, 3518.0, 3523.0, 3533.0, 3543.0, 3553.0, 3558.0, 3563.0, 3568.0, 3570.0]);
        assert_eq!((letters.len(), times.len()), (55, 55));
        let v: Vec<_> = times
            .iter()
            .zip(letters.chars())
            .map(|(&t, c)| {
                let k = match c {
                    'C' => Credits,
                    'B' => Black,
                    'K' => Card,
                    _ => Content,
                };
                (t, Some(k))
            })
            .collect();
        assert_eq!(offer_time(&v, 3573.0), 3453.0);
        assert!(refine_times(&v, 3573.0).is_empty(), "no isolated content left to refine");
    }

    /// Card is neutral exactly like Black: swapping one for the other never
    /// changes the offer or the refinement plan.
    #[test]
    fn card_is_neutral_like_black() {
        let patterns: [&[FrameKind]; 6] = [
            &[Content, Content, Black, Content, Black, Credits],
            &[Content, Credits, Black, Credits, Black],
            &[Black, Content, Black],
            &[Credits, Credits, Content, Black],
            &[Credits, Black, Content, Content, Black, Credits],
            &[Black, Black, Black],
        ];
        for p in patterns {
            let black = s(&p.iter().map(|&k| Some(k)).collect::<Vec<_>>());
            let card: Vec<_> = black.iter().map(|&(t, k)| (t, if k == Some(Black) { Some(Card) } else { k })).collect();
            assert_eq!(offer_time(&card, D), offer_time(&black, D), "{p:?}");
            assert_eq!(refine_times(&card, D), refine_times(&black, D), "{p:?}");
        }
    }

    #[test]
    fn real_episode_offers_at_the_credits_start() {
        let (v, d) = real_episode();
        assert_eq!(offer_time(&v, d), 3453.0);
        let mut rev = v.clone();
        rev.reverse();
        assert_eq!(offer_time(&rev, d), 3453.0, "input order does not matter");
    }

    #[test]
    fn refine_times_of_the_real_episode() {
        let (v, d) = real_episode();
        // Isolated X: index 35 (3443), 42 (3513), 47 (3563).
        assert_eq!(refine_times(&v, d), vec![3438.0, 3448.0, 3508.0, 3518.0, 3558.0, 3568.0]);
        // Next to the last sample, +5 clamps to the already sampled duration - 3.
        let v = vec![(3560.0, Some(Credits)), (3568.0, Some(Content)), (3570.0, Some(Black))];
        assert_eq!(refine_times(&v, d), vec![3563.0]);
    }

    #[test]
    fn refine_times_skip_runs_and_clamp_to_zero() {
        let v = vec![(0.0, Some(Content)), (10.0, Some(Credits)), (20.0, Some(Content)), (30.0, Some(Content))];
        // 0.0 is isolated: -5 clamps to the sampled 0.0, +5 stays; the run at 20/30 is not refined.
        assert_eq!(refine_times(&v, 100.0), vec![5.0]);
        assert!(refine_times(&[], 100.0).is_empty());
        assert!(refine_times(&[(10.0, Some(Content))], 100.0).is_empty(), "no neighbour: not isolated");
    }

    #[test]
    fn refine_times_deduplicate_and_keep_the_last_twelve() {
        // On a 5 s grid every midpoint is already sampled (or clamps onto 0).
        let v = vec![(0.0, Some(Content)), (5.0, Some(Credits)), (10.0, Some(Content)), (15.0, Some(Credits))];
        assert!(refine_times(&v, 100.0).is_empty());
        // Two isolated X sharing a midpoint: 0 + 5 = 10 - 5.
        let v = vec![(0.0, Some(Content)), (7.0, Some(Credits)), (10.0, Some(Content)), (30.0, Some(Credits))];
        assert_eq!(refine_times(&v, 100.0), vec![5.0, 15.0], "5.0 once");
        let many: Vec<_> = (0..40)
            .map(|i| (i as f64 * 20.0, Some(if i % 2 == 0 { Content } else { Credits })))
            .collect();
        let r = refine_times(&many, 1000.0);
        assert_eq!(r.len(), 2 * MAX_REFINED);
        // The 12 isolated X closest to the end: 760, 720, ..., 320.
        assert_eq!(r[0], 315.0);
        assert_eq!(*r.last().unwrap(), 765.0);
        let mut d = r.clone();
        d.dedup();
        assert_eq!(d, r);
    }

    #[test]
    fn refined_short_stinger_blocks_but_a_logo_card_does_not() {
        // Grid: C C X B (X = 1 sample at 1020). Refinement samples 1015, 1025.
        let base = s(&[Some(Credits), Some(Credits), Some(Content), Some(Black)]);
        assert_eq!(refine_times(&base, D), vec![1015.0, 1025.0]);
        // 10 s stinger 1018..1028: 1025 is content too -> two consecutive X.
        let mut stinger = base.clone();
        stinger.extend([(1015.0, Some(Credits)), (1025.0, Some(Content))]);
        assert_eq!(offer_time(&stinger, D), D);
        // Logo card: only 1020 is content -> still noise.
        let mut card = base.clone();
        card.extend([(1015.0, Some(Credits)), (1025.0, Some(Black))]);
        assert_eq!(offer_time(&card, D), 1000.0);
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

    /// Two Content samples at the end (one alone is noise since the noise rule).
    #[test]
    fn last_samples_content_offers_at_end() {
        let v = s(&[Some(Credits), Some(Credits), Some(Content), Some(Content)]);
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
