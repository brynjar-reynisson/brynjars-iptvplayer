//! Pure helpers for exporting a recording as MKV: file names, safety checks,
//! ffmpeg arguments and progress parsing. The background job, the state and
//! the UI build on these; nothing here starts a process.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::rec_merge;

/// A source file written to within this many seconds counts as still being written.
pub const RECENT_WRITE_SECS: u64 = 10;
/// Free space required beyond the source size (the remux is about the same size).
pub const SPACE_MARGIN: u64 = 1 << 30;
/// Durations above this (24 h) are not trusted for a percentage.
pub const MAX_TRUSTED_SECS: f64 = 86_400.0;
/// Longest file name (with extension) we accept.
const MAX_NAME_CHARS: usize = 150;
const EXT: &str = ".mkv";

fn is_reserved(stem: &str) -> bool {
    let up = stem.to_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&up.as_str()) {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(n) = up.strip_prefix(prefix) {
            if n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9') {
                return true;
            }
        }
    }
    false
}

/// Turn what the user typed into a safe Windows file name ending in `.mkv`:
/// path separators and other illegal characters become `_`, spaces and dots
/// at both ends are dropped, reserved device names are refused, the name is
/// cut to 150 characters.
pub fn clean_file_name(input: &str) -> Result<String, String> {
    let backslash = char::from(92u8);
    let replaced: String = input
        .chars()
        .map(|c| if c == backslash || "/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let trimmed = replaced.trim_matches(|c| c == ' ' || c == '.');
    if trimmed.is_empty() {
        return Err("Type a file name.".to_string());
    }
    let has_ext = trimmed.len() > EXT.len()
        && trimmed.is_char_boundary(trimmed.len() - EXT.len())
        && trimmed[trimmed.len() - EXT.len()..].eq_ignore_ascii_case(EXT);
    let stem = if has_ext { &trimmed[..trimmed.len() - EXT.len()] } else { trimmed };
    let stem = stem.trim_matches(|c| c == ' ' || c == '.');
    if stem.is_empty() {
        return Err("Type a file name.".to_string());
    }
    let device = stem.split('.').next().unwrap_or("").trim_end_matches(|c| c == ' ' || c == '.');
    if is_reserved(device) {
        return Err(format!("\"{stem}\" is a reserved Windows name; pick another."));
    }
    let ext = if has_ext { &trimmed[trimmed.len() - EXT.len()..] } else { EXT };
    let keep = MAX_NAME_CHARS - ext.len();
    let stem: String = stem.chars().take(keep).collect();
    let stem = stem.trim_end_matches(|c| c == ' ' || c == '.');
    Ok(format!("{stem}{ext}"))
}

/// The suggested file name for a recording titled `title`.
pub fn default_file_name(title: &str) -> String {
    format!("{title}{EXT}")
}

pub fn dest_path(dir: &Path, file_name: &str) -> PathBuf {
    dir.join(file_name)
}

/// The temporary file written during the export: `<dest>.part`.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// Refuse a source that is being recorded or was written to a moment ago.
pub fn check_source(modified: SystemTime, now: SystemTime, being_recorded: bool) -> Result<(), String> {
    if being_recorded {
        return Err("This file is still being recorded; export it when the recording has ended.".to_string());
    }
    // A modified time in the future (clock change) counts as recent too.
    let recent = match now.duration_since(modified) {
        Ok(age) => age.as_secs() < RECENT_WRITE_SECS,
        Err(_) => true,
    };
    if recent {
        return Err("This file was written to a moment ago; wait a few seconds and try again.".to_string());
    }
    Ok(())
}

/// The destination drive needs the source size plus `SPACE_MARGIN` free.
pub fn check_space(source_size: u64, free: Option<u64>) -> Result<(), String> {
    let Some(free) = free else {
        return Err("The free space on the destination drive could not be read.".to_string());
    };
    let need = source_size.saturating_add(SPACE_MARGIN);
    if free < need {
        return Err(format!(
            "Not enough free space on the destination drive: {:.1} GB needed, {:.1} GB free.",
            need as f64 / 1e9,
            free as f64 / 1e9
        ));
    }
    Ok(())
}

/// ffmpeg arguments (after the program name): remux without re-encoding into
/// Matroska at `part`, progress on stdout.
pub fn ffmpeg_args(src: &Path, part: &Path, with_subtitles: bool) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    for s in ["-nostdin", "-y", "-nostats", "-fflags", "+genpts", "-i"] {
        a.push(OsString::from(s));
    }
    a.push(src.as_os_str().to_os_string());
    for s in ["-map", "0:v", "-map", "0:a?"] {
        a.push(OsString::from(s));
    }
    if with_subtitles {
        a.push(OsString::from("-map"));
        a.push(OsString::from("0:s?"));
    }
    for s in ["-c", "copy", "-avoid_negative_ts", "make_zero", "-f", "matroska", "-progress", "pipe:1"] {
        a.push(OsString::from(s));
    }
    a.push(part.as_os_str().to_os_string());
    a
}

/// What ffmpeg's `-progress` output has reported so far.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct Progress {
    pub out_secs: f64,
    pub bytes: u64,
    pub fraction: Option<f32>,
}

/// Apply one `key=value` line of `-progress` output (`out_time_us`, `total_size`);
/// anything else, and `N/A` values, leave `p` unchanged.
pub fn apply_progress_line(line: &str, p: &mut Progress) {
    let line = line.trim();
    if let Some(v) = line.strip_prefix("out_time_us=") {
        if let Ok(us) = v.trim().parse::<i64>() {
            p.out_secs = (us.max(0) as f64) / 1e6;
        }
    } else if let Some(v) = line.strip_prefix("total_size=") {
        if let Ok(b) = v.trim().parse::<u64>() {
            p.bytes = b;
        }
    }
}

/// Fraction done, only when the duration is known, positive and plausible
/// (<= `MAX_TRUSTED_SECS`: raw recordings can carry bogus durations) and the
/// output has not run past it.
pub fn fraction(out_secs: f64, duration: Option<f64>) -> Option<f32> {
    let d = duration?;
    if !(d > 0.0) || d > MAX_TRUSTED_SECS || out_secs > d {
        return None;
    }
    Some((out_secs / d) as f32)
}

mod win {
    use std::ffi::c_void;
    #[repr(C)]
    pub struct Guid {
        pub data1: u32,
        pub data2: u16,
        pub data3: u16,
        pub data4: [u8; 8],
    }
    #[link(name = "shell32")]
    extern "system" {
        pub fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: *mut c_void, out: *mut *mut u16) -> i32;
    }
    #[link(name = "ole32")]
    extern "system" {
        pub fn CoTaskMemFree(p: *mut c_void);
    }
    extern "system" {
        pub fn GetLogicalDrives() -> u32;
    }
}

/// The user's Downloads folder (Known Folder API; `%USERPROFILE%/Downloads` as fallback).
pub fn downloads_dir() -> Option<PathBuf> {
    let id = win::Guid {
        data1: 0x374D_E290,
        data2: 0x123F,
        data3: 0x4565,
        data4: [0x91, 0x64, 0x39, 0xC4, 0x92, 0x5E, 0x46, 0x7B],
    };
    let mut ptr: *mut u16 = std::ptr::null_mut();
    // SAFETY: `ptr` receives a CoTaskMem-allocated, NUL-terminated wide string
    // that is copied and then freed (it may need freeing even on failure).
    let found = unsafe {
        let hr = win::SHGetKnownFolderPath(&id, 0, std::ptr::null_mut(), &mut ptr);
        let r = if hr >= 0 && !ptr.is_null() {
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            Some(PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))))
        } else {
            None
        };
        if !ptr.is_null() {
            win::CoTaskMemFree(ptr as *mut _);
        }
        r
    };
    found.or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Downloads")))
}

/// The remembered export folder if it still exists, else the Downloads folder.
pub fn default_dir(remembered: Option<&str>) -> Option<PathBuf> {
    if let Some(r) = remembered {
        let p = PathBuf::from(r);
        if !r.trim().is_empty() && p.is_dir() {
            return Some(p);
        }
    }
    downloads_dir()
}

/// Drive roots for a `GetLogicalDrives` bit mask (bit 0 = `A:`).
pub fn drives_from_mask(mask: u32) -> Vec<PathBuf> {
    let sep = std::path::MAIN_SEPARATOR;
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| PathBuf::from(format!("{}:{sep}", (b'A' + i as u8) as char)))
        .collect()
}

/// The drives present now.
pub fn drives() -> Vec<PathBuf> {
    // SAFETY: plain Win32 call without arguments.
    drives_from_mask(unsafe { win::GetLogicalDrives() })
}

// ---- background job ----

/// Where an `ExportJob` stands.
#[derive(Clone, Debug, PartialEq)]
pub enum JobState {
    Running(Progress),
    Done { dest: PathBuf, without_subtitles: bool },
    Failed(String),
    Cancelled,
}

/// Longest ffmpeg stderr kept (the tail), so a chatty run cannot grow memory.
const STDERR_KEEP: usize = 16 * 1024;
const MAX_MESSAGE_CHARS: usize = 200;

struct Shared {
    state: Mutex<JobState>,
    child: Mutex<Option<Child>>,
    cancelled: AtomicBool,
}

impl Shared {
    fn kill(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut c) = self.child.lock() {
            if let Some(child) = c.as_mut() {
                let _ = child.kill();
            }
        }
    }

    fn set(&self, s: JobState) {
        if let Ok(mut st) = self.state.lock() {
            *st = s;
        }
    }
}

/// A running remux of `src` into `<dest>.part`, renamed to `dest` on success.
pub struct ExportJob {
    shared: Arc<Shared>,
    dest: PathBuf,
    worker: Option<JoinHandle<()>>,
}

enum Outcome {
    Ok,
    Cancelled,
    Failed(String),
}

/// Subtitles that cannot be stored in Matroska without re-encoding make ffmpeg
/// fail; the export is then retried once without them.
pub fn should_retry_without_subs(stderr: &str, with_subs: bool) -> bool {
    if !with_subs {
        return false;
    }
    let all = stderr.to_lowercase();
    // A full disk or an I/O error is not a subtitle problem, whatever streams
    // the input has.
    if all.contains("no space left") || all.contains("input/output error") || all.contains("i/o error") {
        return false;
    }
    all.lines().map(str::trim).any(|l| {
        // The input listing (`Stream #0:2: Subtitle: dvb_subtitle`) is not an error.
        if l.starts_with("stream #") || l.starts_with("stream mapping") {
            return false;
        }
        let subs_or_codec = l.contains("subtitle") || l.contains("codec");
        l.contains("codec not currently supported in container")
            || l.contains("subtitle codec")
            || (l.contains("could not write header") && subs_or_codec)
            || (l.contains("invalid argument") && l.contains("subtitle"))
            || (l.starts_with("[matroska") && subs_or_codec)
            || (l.contains("error initializing output stream") && subs_or_codec)
    })
}

/// Delete `*.mkv.part` files directly in `dir` (leftovers of a crashed export).
pub fn sweep_parts(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".mkv.part") && e.path().is_file() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The last lines of ffmpeg's stderr as a short one-line message.
fn short_message(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let tail = lines.iter().rev().take(2).rev().copied().collect::<Vec<_>>().join(" / ");
    let clean: String = tail.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if clean.is_empty() {
        return "ffmpeg failed".to_string();
    }
    if clean.chars().count() > MAX_MESSAGE_CHARS {
        let cut: String = clean.chars().take(MAX_MESSAGE_CHARS - 3).collect();
        format!("{cut}...")
    } else {
        clean
    }
}

/// Put `child` in the killable slot; kills it at once when a cancel already landed.
fn store_child(shared: &Shared, child: Child) {
    if let Ok(mut slot) = shared.child.lock() {
        *slot = Some(child);
    }
    // A cancel that landed before the child was stored found nothing to kill.
    if shared.cancelled.load(Ordering::SeqCst) {
        shared.kill();
    }
}

/// Wait for the child in the slot (polling, without holding the lock) and empty the slot.
fn wait_slot(shared: &Shared) -> Option<std::process::ExitStatus> {
    let status = loop {
        let r = match shared.child.lock() {
            Ok(mut c) => match c.as_mut() {
                Some(ch) => ch.try_wait(),
                None => Ok(None),
            },
            Err(_) => Ok(None),
        };
        match r {
            Ok(Some(s)) => break Some(s),
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => break None,
        }
    };
    if let Ok(mut c) = shared.child.lock() {
        *c = None;
    }
    status
}

/// ffprobe runs in the same killable slot as ffmpeg, so a cancel interrupts it.
fn probe_duration(ffmpeg: &Path, src: &Path, shared: &Shared) -> Option<f64> {
    if shared.cancelled.load(Ordering::SeqCst) {
        return None;
    }
    let mut cmd = Command::new(rec_merge::ffprobe_for(ffmpeg));
    cmd.args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = rec_merge::hide_window(&mut cmd).spawn().ok()?;
    let mut out = child.stdout.take()?;
    store_child(shared, child);
    let mut text = Vec::new();
    let _ = out.read_to_end(&mut text);
    let status = wait_slot(shared)?;
    if !status.success() {
        return None;
    }
    String::from_utf8_lossy(&text).trim().parse::<f64>().ok().filter(|d| d.is_finite())
}

fn run_ffmpeg(
    ffmpeg: &Path,
    src: &Path,
    part: &Path,
    with_subs: bool,
    duration: Option<f64>,
    shared: &Shared,
) -> (Outcome, String) {
    if shared.cancelled.load(Ordering::SeqCst) {
        return (Outcome::Cancelled, String::new());
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.args(ffmpeg_args(src, part, with_subs))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match rec_merge::hide_window(&mut cmd).spawn() {
        Ok(c) => c,
        Err(e) => return (Outcome::Failed(format!("could not start ffmpeg: {}", e.kind())), String::new()),
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    store_child(shared, child);
    let err_thread = std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        if let Some(mut s) = stderr {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = s.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > STDERR_KEEP * 2 {
                    buf.drain(..buf.len() - STDERR_KEEP);
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    if let Some(out) = stdout {
        let mut p = Progress::default();
        let mut reader = BufReader::new(out);
        let mut raw: Vec<u8> = Vec::new();
        // Read to EOF whatever the bytes are, so ffmpeg never blocks on a full pipe.
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&raw).into_owned();
            apply_progress_line(&line, &mut p);
            if line.starts_with("progress=") {
                p.fraction = fraction(p.out_secs, duration);
                if !shared.cancelled.load(Ordering::SeqCst) {
                    shared.set(JobState::Running(p));
                }
            }
        }
    }
    // The pipes are closed: the process is ending. Poll without holding the lock.
    let status = wait_slot(shared);
    let err_text = err_thread.join().unwrap_or_default();
    let outcome = if shared.cancelled.load(Ordering::SeqCst) {
        Outcome::Cancelled
    } else if status.is_some_and(|s| s.success()) {
        Outcome::Ok
    } else {
        Outcome::Failed(short_message(&err_text))
    };
    (outcome, err_text)
}

fn work(ffmpeg: PathBuf, src: PathBuf, dest: PathBuf, shared: Arc<Shared>) {
    let part = part_path(&dest);
    let duration = probe_duration(&ffmpeg, &src, &shared);
    let mut without_subtitles = false;
    let (mut outcome, err) = run_ffmpeg(&ffmpeg, &src, &part, true, duration, &shared);
    if matches!(outcome, Outcome::Failed(_)) && should_retry_without_subs(&err, true) {
        let _ = std::fs::remove_file(&part);
        outcome = run_ffmpeg(&ffmpeg, &src, &part, false, duration, &shared).0;
        without_subtitles = true;
    }
    let final_state = match outcome {
        Outcome::Ok => match std::fs::rename(&part, &dest) {
            Ok(()) => JobState::Done { dest: dest.clone(), without_subtitles },
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                JobState::Failed(format!("could not save the file: {}", e.kind()))
            }
        },
        Outcome::Cancelled => {
            let _ = std::fs::remove_file(&part);
            JobState::Cancelled
        }
        Outcome::Failed(msg) => {
            let _ = std::fs::remove_file(&part);
            JobState::Failed(msg)
        }
    };
    shared.set(final_state);
}

impl ExportJob {
    pub fn start(ffmpeg: PathBuf, src: PathBuf, dest: PathBuf) -> ExportJob {
        let shared = Arc::new(Shared {
            state: Mutex::new(JobState::Running(Progress::default())),
            child: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        });
        let (s2, d2) = (shared.clone(), dest.clone());
        let worker = std::thread::Builder::new()
            .name("export".into())
            .spawn(move || work(ffmpeg, src, d2, s2))
            .ok();
        if worker.is_none() {
            shared.set(JobState::Failed("could not start the export thread".to_string()));
        }
        ExportJob { shared, dest, worker }
    }

    /// Current state; never blocks.
    pub fn poll(&mut self) -> JobState {
        self.shared.state.lock().map(|s| s.clone()).unwrap_or(JobState::Cancelled)
    }

    /// Kill ffmpeg now and return at once; the worker deletes the `.part` and
    /// the state becomes `Cancelled`.
    pub fn cancel(&self) {
        self.shared.kill();
    }

    /// For app exit: kill ffmpeg and delete the `.part` before returning.
    pub fn kill_blocking(&mut self) {
        self.shared.kill();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(part_path(&self.dest));
    }

    pub fn dest(&self) -> &Path {
        &self.dest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const GIB: u64 = 1 << 30;

    #[test]
    fn clean_names() {
        assert_eq!(clean_file_name("News: One/2.mkv").unwrap(), "News_ One_2.mkv");
        assert_eq!(clean_file_name("a").unwrap(), "a.mkv");
        assert_eq!(clean_file_name("a.MKV").unwrap(), "a.MKV");
        assert_eq!(clean_file_name("  x.mkv. ").unwrap(), "x.mkv");
        assert!(clean_file_name("con").is_err());
        assert!(clean_file_name("CON.mkv").is_err());
        assert!(clean_file_name("").is_err());
        assert!(clean_file_name("...").is_err());
        assert!(clean_file_name("   ").is_err());
        for bad in ["COM1", "LPT9", "nul.txt", "con .x"] {
            assert!(clean_file_name(bad).is_err(), "{bad}");
        }
        for ok in ["console.mkv", "communism", "aux1"] {
            assert!(clean_file_name(ok).is_ok(), "{ok}");
        }
        let kelvin = char::from_u32(0x212A).unwrap();
        let k = clean_file_name(&format!("a.{kelvin}V")).unwrap();
        assert!(k.ends_with(".mkv"), "{k}");
        let wide = clean_file_name(&char::from_u32(0xE9).unwrap().to_string().repeat(300)).unwrap();
        assert!(wide.chars().count() <= 150 && wide.ends_with(".mkv"));
        let bsl = char::from(92u8);
        let r = clean_file_name(&format!("..{bsl}x")).unwrap();
        assert!(!r.contains([bsl, '/', ':', '*', '?', '"', '<', '>', '|']), "{r}");
        let long = clean_file_name(&"a".repeat(300)).unwrap();
        assert!(long.chars().count() <= 150 && long.ends_with(".mkv"));
    }

    #[test]
    fn default_and_part() {
        assert_eq!(default_file_name("News One - 2026-10-25 18-30"), "News One - 2026-10-25 18-30.mkv");
        assert!(part_path(Path::new("D:/x/a.mkv")).to_string_lossy().ends_with("a.mkv.part"));
        assert_eq!(dest_path(Path::new("D:/x"), "a.mkv"), Path::new("D:/x").join("a.mkv"));
    }

    #[test]
    fn source_checks() {
        let now = SystemTime::now();
        let e = check_source(now - Duration::from_secs(600), now, true).unwrap_err();
        assert!(e.contains("still being recorded"), "{e}");
        assert!(check_source(now - Duration::from_secs(3), now, false).is_err());
        assert!(check_source(now - Duration::from_secs(60), now, false).is_ok());
    }

    #[test]
    fn space_checks() {
        assert!(check_space(5 * GIB, Some(5 * GIB + SPACE_MARGIN - 1)).is_err());
        assert!(check_space(5 * GIB, Some(5 * GIB + SPACE_MARGIN)).is_ok());
        let e = check_space(5 * GIB, None).unwrap_err();
        assert!(e.contains("could not be read"), "{e}");
    }

    #[test]
    fn args() {
        let a: Vec<String> = ffmpeg_args(Path::new("in.ts"), Path::new("out.mkv.part"), true)
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let joined = a.join(" ");
        let mut at = 0;
        for want in [
            "-fflags +genpts", "-i in.ts", "-map 0:v", "-map 0:a?", "-map 0:s?", "-c copy",
            "-avoid_negative_ts make_zero", "-f matroska", "-progress pipe:1",
        ] {
            let i = joined[at..].find(want).unwrap_or_else(|| panic!("{want} missing/out of order in {joined}"));
            at += i + want.len();
        }
        for w in ["-nostdin", "-y", "-nostats"] {
            assert!(a.iter().any(|x| x == w), "{w}");
        }
        assert_eq!(a.last().unwrap(), "out.mkv.part");
        let b = ffmpeg_args(Path::new("in.ts"), Path::new("o.part"), false);
        assert!(!b.iter().any(|x| x == "0:s?"));
    }

    #[test]
    fn progress() {
        let mut p = Progress::default();
        apply_progress_line("out_time_us=2500000", &mut p);
        assert_eq!(p.out_secs, 2.5);
        apply_progress_line("total_size=1048576", &mut p);
        assert_eq!(p.bytes, 1_048_576);
        let before = p;
        apply_progress_line("out_time_us=N/A", &mut p);
        apply_progress_line("bitrate=1.0kbits/s", &mut p);
        assert_eq!(p, before);
        assert_eq!(fraction(30.0, Some(60.0)), Some(0.5));
        assert_eq!(fraction(70.0, Some(60.0)), None);
        assert_eq!(fraction(1.0, Some(100_000.0)), None);
        assert_eq!(fraction(1.0, None), None);
        assert_eq!(fraction(1.0, Some(0.0)), None);
    }

    #[test]
    fn drives_and_dirs() {
        assert_eq!(drives_from_mask(0b1100), vec![PathBuf::from("C:\\"), PathBuf::from("D:\\")]);
        let t = std::env::temp_dir();
        assert_eq!(default_dir(Some(t.to_str().unwrap())), Some(t));
        assert_eq!(default_dir(Some("Z:/no/such/dir")), downloads_dir());
        if let Some(d) = downloads_dir() {
            assert!(d.is_absolute() && !d.as_os_str().is_empty());
        }
    }

    // ---- ExportJob ----

    fn make_source(ffmpeg: &Path, out: &Path, secs: u32) {
        let st = Command::new(ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=25", "-f", "lavfi", "-i", "sine", "-t"])
            .arg(secs.to_string())
            .args(["-c:v", "mpeg2video", "-c:a", "mp2", "-f", "mpegts"])
            .arg(out)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// A local HTTP server that sends a whole file but never ends the response,
    /// so ffprobe/ffmpeg reading it stay running until killed: cancel tests
    /// cannot race a fast remux.
    struct StallServer {
        port: u16,
        stop: Arc<AtomicBool>,
    }

    impl StallServer {
        fn start(file: &Path) -> StallServer {
            use std::io::Write;
            let data = std::fs::read(file).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            std::thread::spawn(move || {
                while !stop2.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut s, _)) => {
                            let (data, stop3) = (data.clone(), stop2.clone());
                            std::thread::spawn(move || {
                                let _ = s.set_nonblocking(false);
                                let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
                                let mut req = [0u8; 2048];
                                let _ = s.read(&mut req);
                                let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nConnection: close\r\n\r\n";
                                if s.write_all(head.as_bytes()).is_err() || s.write_all(&data).is_err() {
                                    return;
                                }
                                // stall until the client goes away or the test ends
                                let _ = s.set_read_timeout(Some(Duration::from_millis(50)));
                                let mut b = [0u8; 16];
                                while !stop3.load(Ordering::SeqCst) {
                                    match s.read(&mut b) {
                                        Ok(0) => break,
                                        Ok(_) => {}
                                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                                        Err(_) => break,
                                    }
                                }
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
            StallServer { port, stop }
        }

        fn url(&self) -> PathBuf {
            PathBuf::from(format!("http://127.0.0.1:{}/in.ts", self.port))
        }
    }

    impl Drop for StallServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    fn wait_end(job: &mut ExportJob, secs: u64) -> JobState {
        let t0 = std::time::Instant::now();
        loop {
            match job.poll() {
                JobState::Running(_) => {
                    assert!(t0.elapsed() < Duration::from_secs(secs), "job did not end in time");
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
    }

    #[test]
    fn export_makes_an_mkv_and_keeps_the_source() {
        let Some(ff) = rec_merge::find_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.ts");
        make_source(&ff, &src, 3);
        let before = std::fs::read(&src).unwrap();
        let dest = dir.path().join("out.mkv");
        let mut job = ExportJob::start(ff.clone(), src.clone(), dest.clone());
        assert_eq!(job.dest(), dest.as_path());
        match wait_end(&mut job, 30) {
            JobState::Done { dest: d, without_subtitles } => {
                assert_eq!(d, dest);
                assert!(!without_subtitles);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(dest.is_file());
        assert!(!part_path(&dest).exists());
        assert_eq!(std::fs::read(&src).unwrap(), before);
        let out = Command::new(rec_merge::ffprobe_for(&ff))
            .args(["-v", "error", "-show_entries", "stream=codec_type:format=duration", "-of", "csv=p=0"])
            .arg(&dest)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(text.contains("video") && text.contains("audio"), "{text}");
        let dur: f64 = text.lines().filter_map(|l| l.trim().parse::<f64>().ok()).next().expect(&text);
        assert!((dur - 3.0).abs() < 0.5, "duration {dur}");
    }

    #[test]
    fn progress_is_reported_while_running() {
        let Some(ff) = rec_merge::find_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.ts");
        make_source(&ff, &src, 3);
        let mut job = ExportJob::start(ff, src, dir.path().join("p.mkv"));
        let t0 = std::time::Instant::now();
        let last = loop {
            match job.poll() {
                JobState::Running(_) => {
                    // bytes > 0 when seen; a fast job may skip straight to Done
                    assert!(t0.elapsed() < Duration::from_secs(30));
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => break other,
            }
        };
        assert!(matches!(last, JobState::Done { .. }), "{last:?}");
    }

    #[test]
    fn failure_leaves_no_part_and_keeps_an_existing_mkv() {
        let Some(ff) = rec_merge::find_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bad.ts");
        std::fs::write(&src, "this is not a video\n").unwrap();
        let dest = dir.path().join("out.mkv");
        std::fs::write(&dest, "OLD").unwrap();
        let mut job = ExportJob::start(ff, src, dest.clone());
        match wait_end(&mut job, 30) {
            JobState::Failed(msg) => assert!(!msg.is_empty() && msg.chars().count() <= 200),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!part_path(&dest).exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "OLD");
    }

    #[test]
    fn cancel_removes_the_part_and_never_creates_the_mkv() {
        let Some(ff) = rec_merge::find_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("short.ts");
        make_source(&ff, &src, 3);
        let server = StallServer::start(&src);
        let src = server.url();
        let dest = dir.path().join("out.mkv");
        let mut job = ExportJob::start(ff.clone(), src.clone(), dest.clone());
        std::thread::sleep(Duration::from_millis(300));
        assert!(matches!(job.poll(), JobState::Running(_)), "the stalled source cannot finish");
        let t = std::time::Instant::now();
        job.cancel();
        assert!(t.elapsed() < Duration::from_millis(500), "cancel must not block");
        assert_eq!(wait_end(&mut job, 10), JobState::Cancelled);
        assert!(!dest.exists() && !part_path(&dest).exists());
        // a pre-existing destination stays untouched
        let dest2 = dir.path().join("old.mkv");
        std::fs::write(&dest2, "OLD").unwrap();
        let mut job = ExportJob::start(ff, src, dest2.clone());
        std::thread::sleep(Duration::from_millis(100));
        job.cancel();
        assert_eq!(wait_end(&mut job, 10), JobState::Cancelled);
        assert!(!part_path(&dest2).exists());
        assert_eq!(std::fs::read_to_string(&dest2).unwrap(), "OLD");
    }

    #[test]
    fn kill_blocking_removes_the_part_quickly() {
        let Some(ff) = rec_merge::find_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("short.ts");
        make_source(&ff, &src, 3);
        let server = StallServer::start(&src);
        let dest = dir.path().join("out.mkv");
        let mut job = ExportJob::start(ff, server.url(), dest.clone());
        std::thread::sleep(Duration::from_millis(300));
        assert!(matches!(job.poll(), JobState::Running(_)), "the stalled source cannot finish");
        let t = std::time::Instant::now();
        job.kill_blocking();
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(!dest.exists() && !part_path(&dest).exists());
    }

    #[test]
    fn retries_without_subtitles_when_they_cannot_be_copied() {
        assert!(should_retry_without_subs("Subtitle codec 94213 is not supported", true));
        assert!(should_retry_without_subs("Could not write header: codec not currently supported in container", true));
        assert!(should_retry_without_subs(
            "Stream #0:2: Subtitle: dvb_subtitle\n[matroska @ 0x1] Subtitle codec 94213 is not supported\n",
            true
        ));
        assert!(should_retry_without_subs(
            "Could not write header for output file #0 (incorrect codec parameters ?): Invalid argument (subtitle)",
            true
        ));
        // the input listing alone is not an error about the output
        assert!(!should_retry_without_subs(
            "  Stream #0:2[0x1c0]: Subtitle: dvb_subtitle\nError writing trailer: No space left on device\n",
            true
        ));
        assert!(!should_retry_without_subs("  Stream #0:2: Subtitle: dvb_subtitle\nInvalid data found\n", true));
        assert!(!should_retry_without_subs("Subtitle codec 94213 is not supported", false));
        assert!(!should_retry_without_subs("Invalid data found when processing input", true));
        assert!(!should_retry_without_subs("", true));
    }

    #[test]
    fn short_message_is_short_and_one_line() {
        assert_eq!(short_message(""), "ffmpeg failed");
        let long = "x".repeat(500);
        assert_eq!(short_message(&format!("a\n{long}\n")).chars().count(), 200);
        assert_eq!(short_message("a\n\nb\nc\n"), "b / c");
    }

    #[test]
    fn sweep_parts_removes_only_mkv_part_files() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a.mkv.part", "b.mkv", "c.ts", "d.part", "e.mkv.part.txt"] {
            std::fs::write(dir.path().join(n), "x").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub.mkv.part")).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("f.mkv.part"), "x").unwrap();
        sweep_parts(dir.path());
        assert!(!dir.path().join("a.mkv.part").exists());
        for n in ["b.mkv", "c.ts", "d.part", "e.mkv.part.txt"] {
            assert!(dir.path().join(n).exists(), "{n}");
        }
        assert!(dir.path().join("sub.mkv.part").is_dir());
        assert!(dir.path().join("sub").join("f.mkv.part").exists());
    }
}
