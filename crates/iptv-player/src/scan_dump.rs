//! `--console` tuning aid: the frames of a tail scan saved as PNG files
//! under `<data dir>/scan-frames/<episode>-<unix seconds>/`, one per sample,
//! named `NN_<time>s_<L>.png` (L = C credits, B black, K card, X content). Written on
//! the thumbnail worker thread; the folder name comes from the episode's
//! display name only (sanitised), never from the URL.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::state::credits::FrameKind;
use crate::thumb_mpv::Thumb;

/// Longest episode-name part of the folder name, in characters.
const MAX_NAME_CHARS: usize = 60;

/// One letter per sample: C credits, B black, K card, X content, ? no frame.
pub fn kind_letter(kind: Option<FrameKind>) -> char {
    match kind {
        Some(FrameKind::Credits) => 'C',
        Some(FrameKind::Black) => 'B',
        Some(FrameKind::Card) => 'K',
        Some(FrameKind::Content) => 'X',
        None => '?',
    }
}

/// `name` reduced to `[A-Za-z0-9 _.-]` (anything else becomes `_`), without
/// leading/trailing spaces or dots (Windows), at most [`MAX_NAME_CHARS`];
/// `episode` when nothing is left.
pub fn sanitize_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '.' | '-') { c } else { '_' })
        .take(MAX_NAME_CHARS)
        .collect();
    let trimmed = mapped.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() {
        "episode".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Folder for the frames of one scan of `episode`.
pub fn dump_dir(data_dir: &Path, episode: &str, unix_secs: u64) -> PathBuf {
    data_dir.join("scan-frames").join(format!("{}-{unix_secs}", sanitize_name(episode)))
}

/// File name of the `index`-th (time-sorted) sample.
pub fn file_name(index: usize, secs: f64, kind: FrameKind) -> String {
    format!("{index:02}_{secs:.0}s_{}.png", kind_letter(Some(kind)))
}

/// PNG (8-bit RGB; the alpha channel is dropped) of an RGBA frame.
pub fn encode_png(thumb: &Thumb) -> Result<Vec<u8>, String> {
    let (w, h) = (thumb.width as usize, thumb.height as usize);
    if thumb.rgba.len() < w * h * 4 {
        return Err("frame buffer too short".into());
    }
    let rgb: Vec<u8> = thumb.rgba[..w * h * 4].as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, thumb.width, thumb.height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgb).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Writes every sample that has a frame in `frames` (keyed by the time's
/// bits) into `dir`, numbered in time order. Returns how many were written
/// and the first error (its text only: no path). Never panics on I/O errors.
pub fn write_frames(
    dir: &Path,
    samples: &[(f64, Option<FrameKind>)],
    frames: &HashMap<u64, Thumb>,
) -> (usize, Option<String>) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        return (0, Some(e.to_string()));
    }
    let mut sorted: Vec<_> = samples.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut written, mut first_err) = (0, None);
    for (i, (t, kind)) in sorted.iter().enumerate() {
        let (Some(kind), Some(thumb)) = (kind, frames.get(&t.to_bits())) else { continue };
        let result = encode_png(thumb)
            .and_then(|png| std::fs::write(dir.join(file_name(i, *t, *kind)), png).map_err(|e| e.to_string()));
        match result {
            Ok(()) => written += 1,
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    (written, first_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thumb(w: u32, h: u32) -> Thumb {
        let rgba = (0..w * h).flat_map(|i| [(i % 251) as u8, (i * 7 % 256) as u8, (i * 13 % 256) as u8, 255]).collect();
        Thumb { width: w, height: h, rgba }
    }

    #[test]
    fn png_round_trips() {
        let t = thumb(17, 9);
        let bytes = encode_png(&t).unwrap();
        let dec = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (17, 9));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        let rgb: Vec<u8> = t.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        assert_eq!(&buf[..info.buffer_size()], &rgb[..]);
    }

    #[test]
    fn short_buffer_is_an_error() {
        let t = Thumb { width: 4, height: 4, rgba: vec![0; 8] };
        assert!(encode_png(&t).is_err());
    }

    #[test]
    fn names_are_sanitised_and_capped() {
        assert_eq!(sanitize_name("The Office (UK) S01E02"), "The Office _UK_ S01E02");
        assert_eq!(sanitize_name("a/b\\c:d?e*f"), "a_b_c_d_e_f");
        assert_eq!(sanitize_name("Þáttur ë"), "__ttur _");
        assert_eq!(sanitize_name(" .. "), "episode");
        assert_eq!(sanitize_name(""), "episode");
        assert_eq!(sanitize_name(&"x".repeat(200)).len(), MAX_NAME_CHARS);
        assert_eq!(sanitize_name("http://user:pw@host/x"), "http___user_pw_host_x");
    }

    #[test]
    fn dump_dir_and_file_names() {
        let d = dump_dir(Path::new("data"), "Show S01E01", 1_700_000_000);
        assert_eq!(d, Path::new("data").join("scan-frames").join("Show S01E01-1700000000"));
        assert_eq!(file_name(3, 3453.0, FrameKind::Black), "03_3453s_B.png");
        assert_eq!(file_name(48, 3570.4, FrameKind::Credits), "48_3570s_C.png");
        assert_eq!(kind_letter(None), '?');
        assert_eq!(kind_letter(Some(FrameKind::Content)), 'X');
        assert_eq!(kind_letter(Some(FrameKind::Card)), 'K');
        assert_eq!(file_name(51, 3558.0, FrameKind::Card), "51_3558s_K.png");
    }

    #[test]
    fn write_frames_numbers_in_time_order_and_skips_failed_frames() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("scan");
        let samples = vec![
            (20.0, Some(FrameKind::Credits)),
            (5.0, Some(FrameKind::Content)),
            (10.0, None),
            (15.0, Some(FrameKind::Black)),
        ];
        let mut frames = HashMap::new();
        for t in [20.0f64, 5.0, 15.0] {
            frames.insert(t.to_bits(), thumb(4, 3));
        }
        let (n, err) = write_frames(&out, &samples, &frames);
        assert_eq!((n, err), (3, None));
        let mut names: Vec<String> =
            std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["00_5s_X.png", "02_15s_B.png", "03_20s_C.png"]);
    }
}
