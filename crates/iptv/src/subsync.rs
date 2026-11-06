//! Subtitle offset search: finds the constant shift that best aligns the cue
//! start times of a subtitle file with a reference list (e.g. an embedded track).
//! Pure; no I/O.

/// Fewest reference lines for a result to be trusted.
pub const MIN_LINES: usize = 25;
/// Reference lines wanted when re-checking a shift later in the video.
pub const RECHECK_LINES: usize = 60;
/// Shifts smaller than this are not worth applying.
pub const MIN_APPLY_SECS: f64 = 0.15;

const SEARCH_MAX: f64 = 90.0;
const STEP: f64 = 0.05;
const TOLERANCE: f64 = 0.5;
const RIVAL_GAP: f64 = 3.0;

/// Decode subtitle bytes: strip a UTF-8 BOM, UTF-8 if valid else Windows-1252,
/// CRLF/CR normalised to LF.
pub fn decode_srt(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    };
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn cp1252(b: u8) -> char {
    match b {
        0x80 => '\u{20AC}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8E => '\u{017D}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        _ => b as char,
    }
}

/// Sorted start times (seconds) of every `HH:MM:SS,mmm --> ...` line.
pub fn cue_starts(srt: &str) -> Vec<f64> {
    let mut out: Vec<f64> = srt
        .lines()
        .filter_map(|l| {
            let (left, _) = l.split_once("-->")?;
            parse_time(left.trim())
        })
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

fn parse_time(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let h: u32 = parts.next()?.trim().parse().ok()?;
    let m: u32 = parts.next()?.trim().parse().ok()?;
    let sec = parts.next()?.trim();
    if parts.next().is_some() {
        return None;
    }
    let (whole, frac) = match sec.split_once([',', '.']) {
        Some((w, f)) => (w, f),
        None => (sec, ""),
    };
    let whole: u32 = whole.parse().ok()?;
    if !frac.chars().all(|c| c.is_ascii_digit()) || frac.len() > 9 {
        return None;
    }
    let frac_v = if frac.is_empty() {
        0.0
    } else {
        frac.parse::<f64>().ok()? / 10f64.powi(frac.len() as i32)
    };
    Some(h as f64 * 3600.0 + m as f64 * 60.0 + whole as f64 + frac_v)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// What mpv's `sub-delay` needs: reference ~ file + shift.
    pub shift: f64,
    /// Reference starts that have a file start within tolerance at `shift`.
    pub matched: usize,
    /// Number of reference starts.
    pub total: usize,
    /// Best score at a shift more than 3 s from the best one.
    pub rival: usize,
}

impl Match {
    pub fn confident(&self) -> bool {
        self.total >= MIN_LINES && self.matched * 2 >= self.total && self.matched * 2 >= self.rival * 3
    }
}

/// Number of `reference` starts with a `file + s` start within the tolerance.
fn score(reference: &[f64], file: &[f64], s: f64) -> usize {
    reference
        .iter()
        .filter(|&&r| {
            let target = r - s;
            let i = file.partition_point(|&f| f < target - TOLERANCE);
            file.get(i).is_some_and(|&f| (f - target).abs() <= TOLERANCE)
        })
        .count()
}

/// Search shifts -90..=90 s in 0.05 s steps. `None` when either list is empty.
/// Both lists must be sorted ascending.
pub fn best_shift(reference: &[f64], file: &[f64]) -> Option<Match> {
    if reference.is_empty() || file.is_empty() {
        return None;
    }
    let n = (2.0 * SEARCH_MAX / STEP).round() as usize;
    let shifts: Vec<f64> = (0..=n).map(|i| -SEARCH_MAX + i as f64 * STEP).collect();
    let scores: Vec<usize> = shifts.iter().map(|&s| score(reference, file, s)).collect();
    let top = *scores.iter().max()?;
    let best_i = scores.iter().position(|&x| x == top)?;
    let best_s = shifts[best_i];
    let rival = shifts
        .iter()
        .zip(&scores)
        .filter(|(s, _)| (**s - best_s).abs() > RIVAL_GAP)
        .map(|(_, &x)| x)
        .max()
        .unwrap_or(0);
    let mut near: Vec<f64> = shifts
        .iter()
        .zip(&scores)
        .filter(|(s, &x)| x == top && (**s - best_s).abs() <= RIVAL_GAP)
        .map(|(s, _)| *s)
        .collect();
    near.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let plateau = near[near.len() / 2];
    // Chance matches at the plateau edges can skew its middle; the median
    // residual of the matched pairs is exact for a true constant offset.
    let mut residuals: Vec<f64> = reference
        .iter()
        .filter_map(|&r| {
            let target = r - plateau;
            let i = file.partition_point(|&f| f < target - TOLERANCE);
            let f = *file.get(i)?;
            ((f - target).abs() <= TOLERANCE).then_some(r - f)
        })
        .collect();
    residuals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let shift = residuals.get(residuals.len() / 2).copied().unwrap_or(plateau);
    Some(Match { shift, matched: top, total: reference.len(), rival })
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
        fn times(&mut self, n: usize, span: f64) -> Vec<f64> {
            let mut v: Vec<f64> = (0..n).map(|_| self.next() * span).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v
        }
    }

    fn sorted(mut v: Vec<f64>) -> Vec<f64> {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    }

    #[test]
    fn decode_handles_bom_utf8_and_cp1252() {
        assert_eq!(decode_srt(&[0xEF, 0xBB, 0xBF, b'a', b'\r', b'\n', b'b', b'\r', b'c']), "a\nb\nc");
        assert_eq!(decode_srt("caf\u{e9}".as_bytes()), "caf\u{e9}");
        assert_eq!(decode_srt(&[b'c', b'a', b'f', 0xE9]), "caf\u{e9}");
        assert_eq!(decode_srt(&[b'i', b't', 0x92, b's']), "it\u{2019}s");
    }

    #[test]
    fn cue_starts_reads_comma_and_dot_and_sorts() {
        let srt = "1\n00:01:00,500 --> 00:01:02,000\nb\n\n2\n00:00:05.250 --> 00:00:07,000\na\n\n3\n01:00:00,000 --> 01:00:01,000\nc\n";
        assert_eq!(cue_starts(srt), vec![5.25, 60.5, 3600.0]);
    }

    #[test]
    fn finds_a_constant_shift() {
        let file = Lcg(1).times(40, 1200.0);
        let reference: Vec<f64> = file.iter().map(|t| t + 11.9).collect();
        let m = best_shift(&reference, &file).unwrap();
        assert!((m.shift - 11.9).abs() < 0.1, "{m:?}");
        assert!(m.confident(), "{m:?}");
    }

    #[test]
    fn finds_a_small_negative_shift() {
        let file = Lcg(2).times(40, 1200.0);
        let reference: Vec<f64> = file.iter().map(|t| t - 0.45).collect();
        let m = best_shift(&reference, &file).unwrap();
        assert!((m.shift + 0.45).abs() < 0.1, "{m:?}");
        assert!(m.confident(), "{m:?}");
    }

    #[test]
    fn tolerates_missing_and_extra_lines() {
        let mut rng = Lcg(3);
        let file = rng.times(80, 1200.0);
        let mut reference: Vec<f64> = file
            .iter()
            .filter(|_| rng.next() >= 0.25)
            .map(|t| t + 7.3)
            .collect();
        let extras = reference.len() / 5;
        reference.extend(rng.times(extras, 1200.0));
        let reference = sorted(reference);
        let m = best_shift(&reference, &file).unwrap();
        assert!((m.shift - 7.3).abs() < 0.15, "{m:?}");
        assert!(m.confident(), "{m:?}");
    }

    #[test]
    fn too_few_lines_is_not_confident() {
        let file = Lcg(4).times(20, 1200.0);
        let reference: Vec<f64> = file.iter().map(|t| t + 3.0).collect();
        let m = best_shift(&reference, &file).unwrap();
        assert!(!m.confident(), "{m:?}");
    }

    #[test]
    fn unrelated_timing_is_not_confident() {
        let file = Lcg(5).times(40, 1200.0);
        let reference = Lcg(6).times(40, 1200.0);
        let m = best_shift(&reference, &file).unwrap();
        assert!(!m.confident(), "{m:?}");
    }

    #[test]
    fn empty_lists_give_none() {
        assert!(best_shift(&[], &[1.0]).is_none());
        assert!(best_shift(&[1.0], &[]).is_none());
    }
}
