//! Pure helpers for exporting a recording as MKV: file names, safety checks,
//! ffmpeg arguments and progress parsing. The background job, the state and
//! the UI build on these; nothing here starts a process.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    let has_ext = trimmed.len() > EXT.len() && trimmed.to_lowercase().ends_with(EXT);
    let stem = if has_ext { &trimmed[..trimmed.len() - EXT.len()] } else { trimmed };
    let stem = stem.trim_matches(|c| c == ' ' || c == '.');
    if stem.is_empty() {
        return Err("Type a file name.".to_string());
    }
    if is_reserved(stem) {
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
            assert_eq!(d.file_name().unwrap(), "Downloads");
        }
    }
}
