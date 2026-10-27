//! Joins the pieces of one scheduled recording into a single `.ts` file.
//!
//! While a window is open the app and the background recorder write pieces
//! (`<base>.piece-app1.ts`, `<base>.piece-rec2.ts`, ...) and append what each
//! piece covered to `<base>.journal` (wall-clock begin/end). At the end of the
//! window the pieces are joined in time order into `<base>.ts`. Where the
//! connection was lost for 2 seconds or more, and ffmpeg is available, a
//! two-second title card "Connection was lost for N seconds" is spliced in;
//! without ffmpeg (or if anything about the card fails) the pieces are simply
//! glued together. Nothing here uses a stream URL.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::recordings::{PieceInfo, PIECE_MARK};

/// A card is shown for this long ...
pub const CARD_SECS: u64 = 2;
/// ... and only for gaps at least that long.
pub const MIN_GAP_MS: u64 = CARD_SECS * 1000;
/// A lock older than this is a dead merge.
const LOCK_STALE: Duration = Duration::from_secs(30 * 60);

pub fn journal_path(dir: &Path, base: &str) -> PathBuf {
    dir.join(format!("{base}.journal"))
}

/// Append one finished piece to the journal of `base`.
pub fn journal_append(dir: &Path, base: &str, piece: &PieceInfo) -> std::io::Result<()> {
    let mut line = serde_json::to_string(piece).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(journal_path(dir, base))?;
    f.write_all(line.as_bytes())
}

/// file name -> (begin, end) in ms; unreadable lines are ignored.
fn journal_read(dir: &Path, base: &str) -> HashMap<String, (u64, u64)> {
    let text = std::fs::read_to_string(journal_path(dir, base)).unwrap_or_default();
    text.lines()
        .filter_map(|l| serde_json::from_str::<PieceInfo>(l).ok())
        .map(|p| (p.file, (p.begin, p.end)))
        .collect()
}

/// `Name - 2026-10-25 18-30.piece-app1.ts` -> `Name - 2026-10-25 18-30`.
pub fn base_of(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(".ts")?.split_once(PIECE_MARK).map(|(b, _)| b)
}

/// The text of the card for a gap.
pub fn card_text(gap_ms: u64) -> String {
    let secs = (gap_ms + 500) / 1000;
    if secs < 60 {
        format!("Connection was lost for {secs} seconds")
    } else {
        format!("Connection was lost for {}:{:02} minutes", secs / 60, secs % 60)
    }
}

/// One piece with its time span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub path: PathBuf,
    pub begin: u64,
    pub end: u64,
}

/// The pieces in time order, and for each boundary the gap (ms) between
/// the end of one and the beginning of the next (0 if they overlap).
pub fn order_and_gaps(mut spans: Vec<Span>) -> (Vec<Span>, Vec<u64>) {
    spans.sort_by(|a, b| a.begin.cmp(&b.begin).then_with(|| a.path.cmp(&b.path)));
    let gaps = spans.windows(2).map(|w| w[1].begin.saturating_sub(w[0].end)).collect();
    (spans, gaps)
}

fn ms_of(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The pieces of `base` in `dir`, with the journal's spans (file times when a
/// piece has no journal entry, e.g. its writer was killed).
fn collect(dir: &Path, base: &str) -> Vec<Span> {
    let journal = journal_read(dir, base);
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if base_of(&name) != Some(base) {
                return None;
            }
            let m = e.metadata().ok()?;
            let modified = m.modified().ok()?;
            let (begin, end) = journal
                .get(&name)
                .copied()
                .unwrap_or_else(|| (ms_of(m.created().unwrap_or(modified)), ms_of(modified)));
            Some(Span { path: e.path(), begin, end })
        })
        .collect()
}

/// What [`finalize`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// One file now holds the recording.
    Joined { file: PathBuf, pieces: usize, cards: usize },
    /// A piece is still being written or another process is joining: ask again.
    Busy,
    /// No pieces (already joined, or nothing was recorded).
    Nothing,
    Failed(String),
}

pub(crate) fn hide_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// `ffmpeg` next to the program or on the PATH, if it runs.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("ffmpeg.exe")));
    beside.into_iter().chain([PathBuf::from("ffmpeg")]).find(|c| {
        hide_window(&mut Command::new(c).arg("-version"))
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Join the pieces of `base` into `<base>.ts` (see the module docs). `idle`:
/// a piece written to more recently than this is still being written.
pub fn finalize(dir: &Path, base: &str, ffmpeg: Option<&Path>, idle: Duration, log: &mut dyn FnMut(&str)) -> Outcome {
    let spans = collect(dir, base);
    if spans.is_empty() {
        let _ = std::fs::remove_file(journal_path(dir, base));
        return Outcome::Nothing;
    }
    let now = SystemTime::now();
    let busy = spans.iter().any(|s| {
        std::fs::metadata(&s.path).and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() < idle)
    });
    if busy {
        return Outcome::Busy;
    }
    // One joiner at a time (the app and the recorder may both try).
    let lock = dir.join(format!("{base}.merge.lock"));
    if std::fs::metadata(&lock).and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > LOCK_STALE) {
        let _ = std::fs::remove_file(&lock);
    }
    if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
        return Outcome::Busy;
    }
    let r = join(dir, base, spans, ffmpeg, log);
    let _ = std::fs::remove_file(&lock);
    match r {
        Ok(o) => {
            let _ = std::fs::remove_file(journal_path(dir, base));
            o
        }
        Err(e) => Outcome::Failed(e),
    }
}

fn join(dir: &Path, base: &str, spans: Vec<Span>, ffmpeg: Option<&Path>, log: &mut dyn FnMut(&str)) -> Result<Outcome, String> {
    let (spans, gaps) = order_and_gaps(spans);
    // Work in a folder with plain names: moves are instant and ffmpeg never sees odd characters.
    let work = dir.join(format!(".merge-{}", crate::recordings::now_ms()));
    std::fs::create_dir_all(&work).map_err(|e| format!("cannot create the work folder: {e}"))?;
    let mut files: Vec<PathBuf> = Vec::new();
    for (i, s) in spans.iter().enumerate() {
        let to = work.join(format!("p{i}.ts"));
        std::fs::rename(&s.path, &to).map_err(|e| format!("cannot move a piece: {e}"))?;
        files.push(to);
    }
    let out = work.join("out.ts");
    let cards: Vec<(usize, u64)> = gaps.iter().enumerate().filter(|(_, g)| **g >= MIN_GAP_MS).map(|(i, g)| (i, *g)).collect();
    let mut cards_done = 0;
    let mut joined = false;
    // More than one piece: the pieces' timestamps do not line up (the app's start at 0, the
    // recorder's carry the provider's clock, hours later), so gluing them leaves a jump that
    // makes players show an absurd length. An ffmpeg remux (no re-encoding) makes them continuous.
    if !cards.is_empty() || files.len() > 1 {
        match ffmpeg {
            None if !cards.is_empty() => log("ffmpeg was not found: joining without gap cards"),
            None => log("ffmpeg was not found: the pieces are only glued together, their timestamps may jump (the length shown can be wrong)"),
            Some(ff) => {
                let need: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
                match crate::recordings::free_bytes(dir) {
                    Some(free) if free > need + (1 << 30) => match join_with_cards(&work, &files, &cards, ff) {
                        Ok(()) => {
                            joined = true;
                            cards_done = cards.len();
                        }
                        Err(e) => log(&format!("joining with ffmpeg failed ({e}): the pieces are only glued together")),
                    },
                    _ => log("not enough free disk space for an ffmpeg join: the pieces are only glued together"),
                }
            }
        }
    }
    if !joined {
        let _ = std::fs::remove_file(&out);
        glue(&files, &out).map_err(|e| format!("cannot join the pieces: {e}"))?;
    }
    let target = free_target(dir, base);
    std::fs::rename(&out, &target).map_err(|e| format!("cannot move the joined recording: {e}"))?;
    // The list estimates the length as last write minus creation, and the joined file was only
    // created now: give it the time span of what it holds.
    if let (Some(first), Some(last)) = (spans.first(), spans.iter().map(|s| s.end).max()) {
        stamp_times(&target, first.begin, last);
    }
    let _ = std::fs::remove_dir_all(&work);
    Ok(Outcome::Joined { file: target, pieces: files.len(), cards: cards_done })
}

/// Set a file's creation and last-write time (ms since the epoch); best effort.
fn stamp_times(path: &Path, begin_ms: u64, end_ms: u64) {
    use std::os::windows::fs::FileTimesExt;
    let at = |ms: u64| SystemTime::UNIX_EPOCH + Duration::from_millis(ms);
    let times = std::fs::FileTimes::new().set_created(at(begin_ms)).set_modified(at(end_ms.max(begin_ms)));
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_times(times);
    }
}

fn free_target(dir: &Path, base: &str) -> PathBuf {
    let first = dir.join(format!("{base}.ts"));
    if !first.exists() {
        return first;
    }
    (2u32..).map(|n| dir.join(format!("{base} ({n}).ts"))).find(|p| !p.exists()).expect("unbounded range")
}

/// Append the pieces to `out` in order, deleting each after it was copied
/// (little extra disk space is needed).
fn glue(files: &[PathBuf], out: &Path) -> std::io::Result<()> {
    let mut o = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(out)?);
    for f in files {
        let mut i = std::fs::File::open(f)?;
        std::io::copy(&mut i, &mut o)?;
        drop(i);
        let _ = std::fs::remove_file(f);
    }
    o.flush()
}

/// What the cards must look like to match the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSpec {
    pub width: u32,
    pub height: u32,
    /// `25/1`, `50/1`, `30000/1001`.
    pub fps: String,
    pub pix_fmt: String,
    pub video_encoder: &'static str,
    /// `None` for a stream without sound.
    pub audio: Option<CardAudio>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardAudio {
    pub encoder: &'static str,
    pub sample_rate: u32,
    pub layout: String,
}

/// Read the first video and audio stream of `ffprobe -show_streams -of json`.
pub fn card_spec(json: &str) -> Option<CardSpec> {
    let v: Value = serde_json::from_str(json).ok()?;
    let streams = v.get("streams")?.as_array()?;
    let of = |kind: &str| streams.iter().find(|s| s.get("codec_type").and_then(Value::as_str) == Some(kind));
    let (vs, au) = (of("video")?, of("audio"));
    let str_of = |s: &Value, k: &str| s.get(k).and_then(Value::as_str).map(str::to_string);
    let video_encoder = match str_of(vs, "codec_name")?.as_str() {
        "h264" => "libx264",
        "hevc" => "libx265",
        "mpeg2video" => "mpeg2video",
        _ => return None,
    };
    let audio = match au {
        None => None,
        Some(au) => {
            let encoder = match str_of(au, "codec_name")?.as_str() {
                "aac" => "aac",
                "mp2" => "mp2",
                "ac3" => "ac3",
                _ => return None,
            };
            let channels = au.get("channels").and_then(Value::as_u64).unwrap_or(2);
            Some(CardAudio {
                encoder,
                sample_rate: str_of(au, "sample_rate")?.parse().ok()?,
                layout: if channels == 1 { "mono".into() } else { "stereo".into() },
            })
        }
    };
    let fps = str_of(vs, "r_frame_rate").filter(|f| f.contains('/') && !f.starts_with("0/"))?;
    Some(CardSpec {
        width: vs.get("width")?.as_u64()? as u32 / 2 * 2,
        height: vs.get("height")?.as_u64()? as u32 / 2 * 2,
        fps,
        pix_fmt: str_of(vs, "pix_fmt").unwrap_or_else(|| "yuv420p".into()),
        video_encoder,
        audio,
    })
}

pub(crate) fn ffprobe_for(ffmpeg: &Path) -> PathBuf {
    let sibling = ffmpeg.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    if sibling.is_file() {
        sibling
    } else {
        PathBuf::from("ffprobe")
    }
}

fn windows_font() -> Option<&'static str> {
    ["C:/Windows/Fonts/arial.ttf", "C:/Windows/Fonts/segoeui.ttf", "C:/Windows/Fonts/tahoma.ttf"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let o = hide_window(cmd).output().map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if o.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&o.stderr);
        let tail: Vec<&str> = err.lines().rev().take(3).collect();
        Err(format!("{:?} failed: {}", cmd.get_program(), tail.into_iter().rev().collect::<Vec<_>>().join(" | ")))
    }
}

/// Make the cards, then remux pieces and cards into `work/out.ts` without
/// re-encoding the recording (`-c copy`).
fn join_with_cards(work: &Path, files: &[PathBuf], cards: &[(usize, u64)], ffmpeg: &Path) -> Result<(), String> {
    // Cards must look like the stream; a plain remux needs none of this.
    let card_gear = if cards.is_empty() {
        None
    } else {
        let probe = hide_window(Command::new(ffprobe_for(ffmpeg)).args(["-v", "error", "-show_streams", "-of", "json"]).arg(&files[0]))
            .output()
            .map_err(|e| format!("cannot run ffprobe: {e}"))?;
        let spec = card_spec(&String::from_utf8_lossy(&probe.stdout)).ok_or("the stream's codecs cannot be matched")?;
        let font = windows_font().ok_or("no font found for the card text")?.replace(':', "\\:");
        Some((spec, font))
    };
    let mut list = String::new();
    for (i, f) in files.iter().enumerate() {
        list.push_str(&format!("file '{}'\n", f.file_name().unwrap().to_string_lossy()));
        if let (Some((n, (_, gap))), Some((spec, font))) = (cards.iter().enumerate().find(|(_, (at, _))| *at == i), &card_gear) {
            std::fs::write(work.join(format!("c{n}.txt")), card_text(*gap)).map_err(|e| e.to_string())?;
            let vf = format!(
                "drawtext=fontfile='{font}':textfile=c{n}.txt:fontcolor=white:fontsize=h/14:x=(w-text_w)/2:y=(h-text_h)/2,format={}",
                spec.pix_fmt
            );
            let mut cmd = Command::new(ffmpeg);
            cmd.current_dir(work)
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("color=c=black:s={}x{}:r={}:d={CARD_SECS}", spec.width, spec.height, spec.fps));
            if let Some(a) = &spec.audio {
                cmd.args(["-f", "lavfi", "-i"]).arg(format!("anullsrc=r={}:cl={}:d={CARD_SECS}", a.sample_rate, a.layout));
            }
            cmd.args(["-vf", &vf, "-c:v", spec.video_encoder]);
            match spec.video_encoder {
                "libx264" | "libx265" => cmd.args(["-preset", "veryfast", "-g", "25"]),
                _ => cmd.args(["-q:v", "2"]),
            };
            if let Some(a) = &spec.audio {
                cmd.args(["-c:a", a.encoder]);
            }
            cmd.args(["-t", &CARD_SECS.to_string(), "-f", "mpegts"]).arg(format!("c{n}.ts"));
            run(&mut cmd)?;
            if std::fs::metadata(work.join(format!("c{n}.ts"))).map_or(true, |m| m.len() == 0) {
                return Err("a card came out empty".into());
            }
            list.push_str(&format!("file 'c{n}.ts'\n"));
        }
    }
    std::fs::write(work.join("join.txt"), list).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(ffmpeg);
    cmd.current_dir(work)
        .args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", "join.txt"])
        .args(["-map", "0:v", "-map", "0:a?", "-map", "0:s?", "-c", "copy", "-f", "mpegts", "out.ts"]);
    run(&mut cmd)?;
    if std::fs::metadata(work.join("out.ts")).map_or(true, |m| m.len() == 0) {
        return Err("the joined file came out empty".into());
    }
    Ok(())
}

/// [`finalize`] with ffmpeg looked up, asking again while a piece is still
/// being written (up to about 30 s).
pub fn finalize_retrying(dir: &Path, base: &str, log: &mut dyn FnMut(&str)) -> Outcome {
    let ffmpeg = find_ffmpeg();
    for _ in 0..30 {
        match finalize(dir, base, ffmpeg.as_deref(), Duration::from_secs(3), log) {
            Outcome::Busy => std::thread::sleep(Duration::from_secs(1)),
            o => return o,
        }
    }
    Outcome::Busy
}

/// Join every scheduled recording whose pieces have been left alone for
/// `idle` (a crashed or killed writer, or an app closed with no recorder).
pub fn recover_orphans(dir: &Path, idle: Duration, log: &mut dyn FnMut(&str)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let bases: std::collections::BTreeSet<String> =
        rd.filter_map(|e| e.ok()).filter_map(|e| base_of(&e.file_name().to_string_lossy()).map(str::to_string)).collect();
    let ffmpeg = find_ffmpeg();
    for base in bases {
        match finalize(dir, &base, ffmpeg.as_deref(), idle, log) {
            Outcome::Joined { file, pieces, cards } => {
                log(&format!("joined {pieces} pieces of an unfinished recording into {} ({cards} cards)", file.display()))
            }
            Outcome::Failed(e) => log(&format!("could not join the pieces of {base}: {e}")),
            Outcome::Busy | Outcome::Nothing => {}
        }
    }
}

/// All piece files in `dir`, grouped by recording (for tests and tools).
pub fn pieces_by_base(dir: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut m: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.filter_map(|e| e.ok()) {
            if let Some(b) = base_of(&e.file_name().to_string_lossy()) {
                m.entry(b.to_string()).or_default().push(e.path());
            }
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(_: &str) {}

    fn piece(dir: &Path, name: &str, data: &[u8], begin: u64, end: u64, base: &str) {
        std::fs::write(dir.join(name), data).unwrap();
        journal_append(dir, base, &PieceInfo { file: name.into(), begin, end }).unwrap();
    }

    #[test]
    fn card_text_uses_seconds_then_minutes() {
        assert_eq!(card_text(2000), "Connection was lost for 2 seconds");
        assert_eq!(card_text(12_400), "Connection was lost for 12 seconds");
        assert_eq!(card_text(59_400), "Connection was lost for 59 seconds");
        assert_eq!(card_text(60_000), "Connection was lost for 1:00 minutes");
        assert_eq!(card_text(72_000), "Connection was lost for 1:12 minutes");
        assert_eq!(card_text(3_725_000), "Connection was lost for 62:05 minutes");
    }

    #[test]
    fn pieces_are_ordered_by_time_and_gaps_are_measured() {
        let s = |n: &str, b, e| Span { path: n.into(), begin: b, end: e };
        let (v, gaps) = order_and_gaps(vec![s("c", 30_000, 40_000), s("a", 0, 10_000), s("b", 10_500, 25_000)]);
        assert_eq!(v.iter().map(|x| x.path.to_str().unwrap()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(gaps, [500, 5000]);
        let (_, g) = order_and_gaps(vec![s("a", 0, 10_000), s("b", 9_000, 20_000)]);
        assert_eq!(g, [0], "an overlap is no gap");
    }

    #[test]
    fn names_map_back_to_their_recording() {
        assert_eq!(base_of("IS_ Syn - 2026-10-25 18-30.piece-app1.ts"), Some("IS_ Syn - 2026-10-25 18-30"));
        assert_eq!(base_of("IS_ Syn - 2026-10-25 18-30.ts"), None);
        assert_eq!(base_of("x.piece-rec12.txt"), None);
    }

    #[test]
    fn without_ffmpeg_the_pieces_are_glued_in_time_order() {
        let d = tempfile::tempdir().unwrap();
        let base = "Game - 2026-10-25 18-30";
        // Written out of order; the journal says rec1 came first, with a 12 s gap before app2.
        piece(d.path(), &format!("{base}.piece-app2.ts"), b"BBB", 22_000, 30_000, base);
        piece(d.path(), &format!("{base}.piece-rec1.ts"), b"AAA", 0, 10_000, base);
        let mut logs = Vec::new();
        let o = finalize(d.path(), base, None, Duration::ZERO, &mut |m| logs.push(m.to_string()));
        let Outcome::Joined { file, pieces, cards } = o else { panic!("{o:?}") };
        assert_eq!((pieces, cards), (2, 0));
        assert_eq!(std::fs::read(&file).unwrap(), b"AAABBB");
        assert!(file.ends_with(format!("{base}.ts")));
        assert!(logs.iter().any(|l| l.contains("ffmpeg was not found")), "{logs:?}");
        // Pieces, journal, lock and work folder are gone; the file is listed.
        let left: Vec<String> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, [format!("{base}.ts")]);
        assert_eq!(crate::recordings::list(d.path()).len(), 1);
    }

    #[test]
    fn short_gaps_need_no_card_and_no_ffmpeg() {
        let d = tempfile::tempdir().unwrap();
        let base = "G - 2026-10-25 18-30";
        piece(d.path(), &format!("{base}.piece-app1.ts"), b"1", 0, 10_000, base);
        piece(d.path(), &format!("{base}.piece-rec1.ts"), b"2", 11_000, 20_000, base); // a 1 s gap
        let mut logs = Vec::new();
        let o = finalize(d.path(), base, None, Duration::ZERO, &mut |m| logs.push(m.to_string()));
        assert!(matches!(o, Outcome::Joined { pieces: 2, cards: 0, .. }));
        assert!(logs.iter().any(|l| l.contains("timestamps may jump")), "glued only, and it says so: {logs:?}");
    }

    #[test]
    fn the_joined_file_reports_the_length_of_what_it_holds() {
        let d = tempfile::tempdir().unwrap();
        let base = "G - 2026-10-25 18-30";
        let t0 = 1_761_000_000_000u64;
        piece(d.path(), &format!("{base}.piece-app1.ts"), b"1", t0, t0 + 600_000, base);
        piece(d.path(), &format!("{base}.piece-rec1.ts"), b"2", t0 + 601_000, t0 + 4_500_000, base);
        let Outcome::Joined { file, .. } = finalize(d.path(), base, None, Duration::ZERO, &mut quiet) else { panic!() };
        let len = crate::recordings::list(d.path()).into_iter().find(|r| r.path == file).unwrap().length().unwrap();
        assert_eq!(len, Duration::from_millis(4_500_000), "75 minutes, not the seconds the join took");
    }

    #[test]
    fn a_piece_still_being_written_or_a_lock_means_try_again() {
        let d = tempfile::tempdir().unwrap();
        let base = "G - 2026-10-25 18-30";
        piece(d.path(), &format!("{base}.piece-app1.ts"), b"1", 0, 10_000, base);
        assert_eq!(finalize(d.path(), base, None, Duration::from_secs(60), &mut quiet), Outcome::Busy, "written just now");
        std::fs::write(d.path().join(format!("{base}.merge.lock")), b"").unwrap();
        assert_eq!(finalize(d.path(), base, None, Duration::ZERO, &mut quiet), Outcome::Busy, "someone else is joining");
        std::fs::remove_file(d.path().join(format!("{base}.merge.lock"))).unwrap();
        assert!(matches!(finalize(d.path(), base, None, Duration::ZERO, &mut quiet), Outcome::Joined { .. }));
        assert_eq!(finalize(d.path(), base, None, Duration::ZERO, &mut quiet), Outcome::Nothing, "already joined");
    }

    #[test]
    fn a_second_recording_of_the_same_window_gets_its_own_name() {
        let d = tempfile::tempdir().unwrap();
        let base = "G - 2026-10-25 18-30";
        std::fs::write(d.path().join(format!("{base}.ts")), b"old").unwrap();
        piece(d.path(), &format!("{base}.piece-app1.ts"), b"new", 0, 1000, base);
        let Outcome::Joined { file, .. } = finalize(d.path(), base, None, Duration::ZERO, &mut quiet) else { panic!() };
        assert!(file.ends_with(format!("{base} (2).ts")));
        assert_eq!(std::fs::read(d.path().join(format!("{base}.ts"))).unwrap(), b"old");
    }

    #[test]
    fn orphans_are_joined_only_when_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let base = "G - 2026-10-25 18-30";
        piece(d.path(), &format!("{base}.piece-app1.ts"), b"1", 0, 10_000, base);
        recover_orphans(d.path(), Duration::from_secs(3600), &mut quiet);
        assert_eq!(pieces_by_base(d.path()).len(), 1, "written moments ago: left alone");
        recover_orphans(d.path(), Duration::ZERO, &mut quiet);
        assert!(pieces_by_base(d.path()).is_empty());
        assert_eq!(crate::recordings::list(d.path()).len(), 1);
    }

    #[test]
    fn card_spec_follows_the_stream() {
        let json = r#"{"streams":[
            {"codec_type":"video","codec_name":"h264","width":1919,"height":1080,"r_frame_rate":"50/1","pix_fmt":"yuv420p"},
            {"codec_type":"audio","codec_name":"aac","sample_rate":"48000","channels":2}]}"#;
        let s = card_spec(json).unwrap();
        assert_eq!((s.width, s.height, s.fps.as_str()), (1918, 1080, "50/1"));
        let a = s.audio.clone().unwrap();
        assert_eq!((s.video_encoder, a.encoder, a.sample_rate, a.layout.as_str()), ("libx264", "aac", 48000, "stereo"));
        assert!(card_spec(&json.replace("h264", "av1")).is_none(), "unknown codec: no cards");
        assert!(card_spec(&json.replace("\"aac\"", "\"opus\"")).is_none());
        let silent = r#"{"streams":[{"codec_type":"video","codec_name":"mpeg2video","width":320,"height":180,"r_frame_rate":"25/1"}]}"#;
        let s = card_spec(silent).unwrap();
        assert!(s.audio.is_none() && s.pix_fmt == "yuv420p" && s.video_encoder == "mpeg2video", "a stream without sound still gets cards");
        assert!(card_spec(r#"{"streams":[{"codec_type":"video","codec_name":"h264"}]}"#).is_none(), "no size or rate");
        assert!(card_spec("nonsense").is_none());
    }

    /// Real ffmpeg: a piece starting at 0 (the app's) and one on the provider's clock
    /// (hours later, the recorder's) must not leave a huge jump in the joined file.
    #[test]
    fn timestamps_are_made_continuous_with_real_ffmpeg() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let d = tempfile::tempdir().unwrap();
        let base = "Clip - 2026-10-25 18-30";
        for (n, offset) in [("app1", "0"), ("rec2", "72950")] {
            let out = d.path().join(format!("{base}.piece-{n}.ts"));
            let st = hide_window(Command::new(&ffmpeg).args(["-v", "error", "-y"]))
                .args(["-f", "lavfi", "-i", "testsrc=size=320x180:rate=25:duration=3"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=3"])
                .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac", "-output_ts_offset", offset, "-f", "mpegts"])
                .arg(&out)
                .status()
                .unwrap();
            assert!(st.success());
        }
        journal_append(d.path(), base, &PieceInfo { file: format!("{base}.piece-app1.ts"), begin: 0, end: 3_000 }).unwrap();
        journal_append(d.path(), base, &PieceInfo { file: format!("{base}.piece-rec2.ts"), begin: 3_500, end: 6_500 }).unwrap(); // 0.5 s gap: no card
        let mut logs = Vec::new();
        let o = finalize(d.path(), base, Some(&ffmpeg), Duration::ZERO, &mut |m| logs.push(m.to_string()));
        let Outcome::Joined { file, pieces, cards } = o else { panic!("{o:?} {logs:?}") };
        assert_eq!((pieces, cards), (2, 0));
        let probe = hide_window(Command::new(ffprobe_for(&ffmpeg)).args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]))
            .arg(&file)
            .output()
            .unwrap();
        let secs: f64 = String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap();
        assert!((5.0..8.0).contains(&secs), "3 + 3 s, not hours: got {secs}");
    }

    /// Real ffmpeg: two small clips with a 5 s gap between them -> one file
    /// with a 2 s card in between. Skipped when ffmpeg is not installed.
    #[test]
    fn a_gap_card_is_spliced_in_with_real_ffmpeg() {
        let Some(ffmpeg) = find_ffmpeg() else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let d = tempfile::tempdir().unwrap();
        let base = "Clip - 2026-10-25 18-30";
        for n in ["rec1", "app2"] {
            let out = d.path().join(format!("{base}.piece-{n}.ts"));
            let st = hide_window(Command::new(&ffmpeg).args(["-v", "error", "-y"]))
                .args(["-f", "lavfi", "-i", "testsrc=size=320x180:rate=25:duration=3"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=3"])
                .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "25", "-c:a", "aac", "-f", "mpegts"])
                .arg(&out)
                .status()
                .unwrap();
            assert!(st.success());
        }
        journal_append(d.path(), base, &PieceInfo { file: format!("{base}.piece-rec1.ts"), begin: 0, end: 3_000 }).unwrap();
        journal_append(d.path(), base, &PieceInfo { file: format!("{base}.piece-app2.ts"), begin: 8_000, end: 11_000 }).unwrap();
        let mut logs = Vec::new();
        let o = finalize(d.path(), base, Some(&ffmpeg), Duration::ZERO, &mut |m| logs.push(m.to_string()));
        let Outcome::Joined { file, pieces, cards } = o else { panic!("{o:?} {logs:?}") };
        assert_eq!((pieces, cards), (2, 1), "{logs:?}");
        let probe = hide_window(Command::new(ffprobe_for(&ffmpeg)).args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]))
            .arg(&file)
            .output()
            .unwrap();
        let secs: f64 = String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap();
        assert!((7.0..9.5).contains(&secs), "3 + 2 + 3 s, got {secs}");
        // It must decode end to end.
        let st = hide_window(Command::new(&ffmpeg).args(["-v", "error", "-i"])).arg(&file).args(["-f", "null", "-"]).output().unwrap();
        assert!(st.stderr.is_empty(), "decode errors: {}", String::from_utf8_lossy(&st.stderr));
    }
}
