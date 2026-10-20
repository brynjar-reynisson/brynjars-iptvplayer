//! Dev check of the credits classifier against the frames of a real tail
//! scan saved with `--console` (`<data dir>/scan-frames/<episode>-<secs>/`,
//! files `NN_<secs>s_<letter>.png`, see `scan_dump`):
//! `cargo run -p iptv-player --example scan_frames_check -- <folder>`
//! (or set `IPTV_SCAN_FRAMES`). Prints index, time, the letter saved in the
//! file name (the classifier of that day), the letter now, and the offer
//! time. The frames are never copied anywhere; without a folder (or when it
//! does not exist) the check is skipped.
//!
//! For the folder of the episode the classifier was tuned on (name starts
//! with `The Wheel of Time S01 E07-`, 55 frames) it also asserts the
//! hand-labelled ground truth and prints `RESULT: ALL CHECKS OK`.

use std::path::{Path, PathBuf};

use iptv_player::scan_dump::kind_letter;
use iptv_player::state::credits::{classify, offer_time, FrameKind};

/// Hand-labelled frames of the tuning episode (by eye).
fn ground_truth(index: usize) -> &'static [FrameKind] {
    use FrameKind::*;
    match index {
        // Dark show scenes known to read as Credits (faces/shapes on near
        // black): allowed either way, see docs/follow-ups.md.
        12 | 16 | 28 | 34 | 35 => &[Content, Credits],
        0..=37 => &[Content],
        38 | 39 | 53 | 54 => &[Black],
        40..=50 => &[Credits],
        51 | 52 => &[Card],
        _ => &[],
    }
}
const TUNED_PREFIX: &str = "The Wheel of Time S01 E07-";
const TUNED_FRAMES: usize = 55;
const TUNED_OFFER: f64 = 3453.0;

/// `NN_<secs>s_<letter>.png` -> (index, secs, letter).
fn parse_name(name: &str) -> Option<(usize, f64, char)> {
    let stem = name.strip_suffix(".png")?;
    let mut parts = stem.split('_');
    let index = parts.next()?.parse().ok()?;
    let secs = parts.next()?.strip_suffix('s')?.parse().ok()?;
    let letter = parts.next()?.chars().next()?;
    Some((index, secs, letter))
}

/// RGBA pixels of an 8-bit RGB or RGBA PNG.
fn decode(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!("unsupported bit depth {:?}", info.bit_depth));
    }
    let data = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        other => return Err(format!("unsupported colour type {other:?}")),
    };
    Ok((rgba, info.width, info.height))
}

fn main() {
    let dir: Option<PathBuf> =
        std::env::args_os().nth(1).or_else(|| std::env::var_os("IPTV_SCAN_FRAMES")).map(PathBuf::from);
    let Some(dir) = dir.filter(|d| d.is_dir()) else {
        println!("scan_frames_check: no frames folder (argument or IPTV_SCAN_FRAMES): skipped");
        return;
    };
    let mut frames: Vec<(usize, f64, char, PathBuf)> = std::fs::read_dir(&dir)
        .expect("read frames folder")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            parse_name(&name).map(|(i, t, l)| (i, t, l, e.path()))
        })
        .collect();
    frames.sort_by_key(|f| f.0);
    if frames.is_empty() {
        println!("scan_frames_check: no NN_<secs>s_<letter>.png files: skipped");
        return;
    }

    let mut samples = Vec::new();
    let (mut old, mut new) = (String::new(), String::new());
    println!("idx  secs  old new");
    for (index, secs, letter, path) in &frames {
        let kind = match decode(path) {
            Ok((rgba, w, h)) => Some(classify(&rgba, w, h)),
            Err(e) => {
                println!("{index:02}: cannot decode: {e}");
                None
            }
        };
        let now = kind_letter(kind);
        println!("{index:02} {secs:>6.0}   {letter}   {now}{}", if now != *letter { "  *" } else { "" });
        old.push(*letter);
        new.push(now);
        samples.push((*secs, kind));
    }
    let duration = samples.iter().map(|s| s.0).fold(0.0, f64::max) + 3.0;
    let offer = offer_time(&samples, duration);
    println!("old: {old}");
    println!("new: {new}");
    println!("offer: {offer:.1} s (duration ~{duration:.0} s)");

    let tuned = dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with(TUNED_PREFIX));
    if !tuned || frames.len() != TUNED_FRAMES {
        println!("(no ground truth for this folder)");
        return;
    }
    let mut failures = 0;
    for ((index, secs, _, _), (_, kind)) in frames.iter().zip(&samples) {
        let want = ground_truth(*index);
        if !kind.is_some_and(|k| want.contains(&k)) {
            println!("MISMATCH {index:02} ({secs:.0} s): got {}, want {want:?}", kind_letter(*kind));
            failures += 1;
        }
    }
    if offer != TUNED_OFFER {
        println!("MISMATCH offer: got {offer:.1}, want {TUNED_OFFER:.1}");
        failures += 1;
    }
    if failures > 0 {
        println!("RESULT: {failures} CHECK(S) FAILED");
        std::process::exit(1);
    }
    println!("RESULT: ALL CHECKS OK");
}
