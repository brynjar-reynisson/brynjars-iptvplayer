//! Recordings of live channels: the `recordings` folder beside `config.toml`
//! (`.ts` files written by mpv's `stream-record`), the rules for when to
//! record (max length, minimum free disk space) as a pure state machine, and
//! listing / deleting the finished files. No URL is ever used here.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// A recording stops by itself after this long.
pub const MAX_RECORD: Duration = Duration::from_secs(3 * 3600);
/// Never record (or keep recording) with less free disk space than this.
pub const MIN_FREE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// Longest channel name used in a file name, in characters.
const MAX_NAME_CHARS: usize = 60;

pub fn dir_in(data_dir: &Path) -> PathBuf {
    data_dir.join("recordings")
}

/// `name` made safe for a file name: letters, digits and a few punctuation
/// marks are kept, anything else (path separators, `:` ...) becomes `_`.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || " _-.()&'+".contains(c) { c } else { '_' })
        .take(MAX_NAME_CHARS)
        .collect();
    let s = s.trim().trim_matches('.').trim();
    if s.is_empty() {
        "Recording".to_string()
    } else {
        s.to_string()
    }
}

/// `Channel - 2026-10-23 20-30.ts`, a later part `... - part 2.ts`.
pub fn file_name(channel: &str, stamp: &str, part: u32) -> String {
    let base = format!("{} - {stamp}", sanitize_name(channel));
    if part <= 1 {
        format!("{base}.ts")
    } else {
        format!("{base} - part {part}.ts")
    }
}

#[cfg(windows)]
mod sys {
    #[repr(C)]
    #[derive(Default)]
    pub struct SystemTime {
        pub year: u16,
        pub month: u16,
        pub day_of_week: u16,
        pub day: u16,
        pub hour: u16,
        pub minute: u16,
        pub second: u16,
        pub millis: u16,
    }
    extern "system" {
        pub fn GetLocalTime(t: *mut SystemTime);
        pub fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
}

/// The local wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNow {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    /// 0 = Sunday ... 6 = Saturday.
    pub weekday: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

impl LocalNow {
    /// This time `secs` later (less than a day); the date is not carried, only
    /// the weekday and the clock.
    pub fn plus_secs(self, secs: u32) -> LocalNow {
        let t = self.hour as u32 * 3600 + self.minute as u32 * 60 + self.second as u32 + secs;
        let days = t / 86400;
        let t = t % 86400;
        LocalNow {
            weekday: ((self.weekday as u32 + days) % 7) as u16,
            hour: (t / 3600) as u16,
            minute: (t / 60 % 60) as u16,
            second: (t % 60) as u16,
            ..self
        }
    }
}

pub fn local_now() -> LocalNow {
    #[cfg(windows)]
    {
        let mut t = sys::SystemTime::default();
        unsafe { sys::GetLocalTime(&mut t) };
        LocalNow {
            year: t.year,
            month: t.month,
            day: t.day,
            weekday: t.day_of_week,
            hour: t.hour,
            minute: t.minute,
            second: t.second,
        }
    }
    #[cfg(not(windows))]
    {
        // No local time zone support off Windows: UTC.
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let day = secs / 86400;
        LocalNow {
            year: 1970,
            month: 1,
            day: 1,
            weekday: ((day + 4) % 7) as u16,
            hour: (secs / 3600 % 24) as u16,
            minute: (secs / 60 % 60) as u16,
            second: (secs % 60) as u16,
        }
    }
}

/// Local date and time for file names: `2026-10-23 20-30`.
pub fn local_stamp() -> String {
    let t = local_now();
    format!("{:04}-{:02}-{:02} {:02}-{:02}", t.year, t.month, t.day, t.hour, t.minute)
}

/// Free bytes available to this user on the disk holding `dir` (which must
/// exist); `None` if it cannot be read.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        let ok = unsafe { sys::GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
        (ok != 0).then_some(avail)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// [`MAX_RECORD`] reached.
    MaxTime,
    /// Free disk space fell below [`MIN_FREE_BYTES`].
    LowSpace,
}

impl StopReason {
    pub fn message(self) -> &'static str {
        match self {
            StopReason::MaxTime => "Recording stopped: the time limit was reached.",
            StopReason::LowSpace => "Recording stopped: less than 10 GB of disk space left.",
        }
    }
}

/// Why a recording cannot start.
pub fn start_refusal(free: Option<u64>) -> Option<String> {
    match free {
        None => Some("Not recording: the free disk space could not be read.".into()),
        Some(f) if f < MIN_FREE_BYTES => {
            Some(format!("Not recording: only {:.1} GB of disk space left (10 GB needed).", f as f64 / 1e9))
        }
        Some(_) => None,
    }
}

/// What the app must do with mpv's `stream-record` after [`Recorder::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Start writing to this file.
    Begin(PathBuf),
    /// The stream dropped (reconnecting): stop writing; the next part starts
    /// when it plays again.
    End,
    /// The recording is over (the part, if any, is ended too).
    Stop(StopReason),
}

/// One running recording of one channel. It spans reconnects: every time the
/// stream plays again a new part file starts.
#[derive(Debug)]
pub struct Recorder {
    dir: PathBuf,
    channel: String,
    stamp: String,
    started: Instant,
    max: Duration,
    part: u32,
    current: Option<PathBuf>,
}

impl Recorder {
    pub fn new(dir: &Path, channel: &str, stamp: &str, now: Instant) -> Recorder {
        Self::with_max(dir, channel, stamp, now, MAX_RECORD)
    }

    /// Like [`new`](Self::new) with another length limit (scheduled recordings).
    pub fn with_max(dir: &Path, channel: &str, stamp: &str, now: Instant, max: Duration) -> Recorder {
        Recorder {
            max,
            dir: dir.to_path_buf(),
            channel: channel.to_string(),
            stamp: stamp.to_string(),
            started: now,
            part: 0,
            current: None,
        }
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    /// The stream was reloaded: the part in progress is over (the next
    /// `poll` while playing starts a new one).
    pub fn interrupt(&mut self) {
        self.current = None;
    }

    /// The file being written right now.
    pub fn current(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// Decide what to do now. `playing`: the stream plays (not reconnecting);
    /// `free`: free disk space (`None` = unknown, treated as low).
    pub fn poll(&mut self, now: Instant, playing: bool, free: Option<u64>) -> Step {
        let stop = if self.elapsed(now) >= self.max {
            Some(StopReason::MaxTime)
        } else if free.is_none_or(|f| f < MIN_FREE_BYTES) {
            Some(StopReason::LowSpace)
        } else {
            None
        };
        if let Some(r) = stop {
            self.current = None;
            return Step::Stop(r);
        }
        match (playing, self.current.is_some()) {
            (true, false) => {
                self.part += 1;
                let path = self.free_path();
                self.current = Some(path.clone());
                Step::Begin(path)
            }
            (false, true) => {
                self.current = None;
                Step::End
            }
            _ => Step::Nothing,
        }
    }

    /// A file name for the next part that does not exist yet.
    fn free_path(&self) -> PathBuf {
        let first = self.dir.join(file_name(&self.channel, &self.stamp, self.part));
        if !first.exists() {
            return first;
        }
        (2u32..)
            .map(|n| {
                let stem = file_name(&self.channel, &self.stamp, self.part);
                self.dir.join(format!("{} ({n}).ts", stem.trim_end_matches(".ts")))
            })
            .find(|p| !p.exists())
            .expect("unbounded range")
    }
}

/// A finished (or running) recording file.
#[derive(Debug, Clone)]
pub struct Recording {
    pub path: PathBuf,
    /// File name without `.ts`.
    pub title: String,
    pub size: u64,
    /// When the file was created (the start of this part).
    pub created: SystemTime,
    pub modified: SystemTime,
}

impl Recording {
    /// Approximate length: last write minus creation.
    pub fn length(&self) -> Option<Duration> {
        self.modified.duration_since(self.created).ok()
    }

    /// Key for the resume position of this file.
    pub fn key(&self) -> String {
        format!("rec:{}", self.title)
    }
}

/// The `.ts` files in `dir`, newest first. A missing folder is an empty list.
pub fn list(dir: &Path) -> Vec<Recording> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<Recording> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("ts") {
                return None;
            }
            let m = e.metadata().ok().filter(|m| m.is_file())?;
            let modified = m.modified().ok()?;
            Some(Recording {
                title: path.file_stem()?.to_string_lossy().into_owned(),
                size: m.len(),
                created: m.created().unwrap_or(modified),
                modified,
                path,
            })
        })
        .collect();
    v.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.title.cmp(&b.title)));
    v
}

/// Delete a recording; only a `.ts` file directly inside `dir` is accepted.
pub fn delete(dir: &Path, path: &Path) -> std::io::Result<()> {
    let inside = path.parent() == Some(dir) && path.extension().and_then(|x| x.to_str()) == Some("ts");
    if !inside {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a recording in the recordings folder"));
    }
    std::fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn names_are_safe_and_parts_are_numbered() {
        assert_eq!(sanitize_name("SVT1 HD"), "SVT1 HD");
        assert_eq!(sanitize_name("A/B:C*?"), "A_B_C__");
        assert_eq!(sanitize_name("  .. "), "Recording");
        assert_eq!(sanitize_name(&"x".repeat(200)).chars().count(), MAX_NAME_CHARS);
        assert_eq!(file_name("SVT1", "2026-10-23 20-30", 1), "SVT1 - 2026-10-23 20-30.ts");
        assert_eq!(file_name("SVT1", "2026-10-23 20-30", 2), "SVT1 - 2026-10-23 20-30 - part 2.ts");
    }

    #[test]
    fn refuses_to_start_below_ten_gb_or_when_unknown() {
        assert!(start_refusal(Some(10 * GB)).is_none());
        assert!(start_refusal(Some(10 * GB - 1)).unwrap().contains("10 GB"));
        assert!(start_refusal(None).unwrap().contains("could not be read"));
    }

    #[test]
    fn parts_follow_the_stream_and_the_limits_stop_it() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut r = Recorder::new(d.path(), "Game", "2026-10-23 20-30", t0);
        let big = Some(50 * GB);
        assert_eq!(r.poll(t0, false, big), Step::Nothing, "not playing yet: nothing to write");
        let Step::Begin(p1) = r.poll(t0, true, big) else { panic!("begin") };
        assert!(p1.ends_with("Game - 2026-10-23 20-30.ts"));
        assert_eq!(r.current(), Some(p1.as_path()));
        assert_eq!(r.poll(t0, true, big), Step::Nothing);
        assert_eq!(r.poll(t0, false, big), Step::End, "reconnecting");
        assert!(r.current().is_none());
        let Step::Begin(p2) = r.poll(t0, true, big) else { panic!("begin 2") };
        assert!(p2.ends_with("Game - 2026-10-23 20-30 - part 2.ts"));
        let late = t0 + MAX_RECORD;
        assert_eq!(r.poll(late - Duration::from_secs(1), true, big), Step::Nothing);
        assert_eq!(r.poll(late, true, big), Step::Stop(StopReason::MaxTime));
        assert!(r.current().is_none());
    }

    #[test]
    fn low_or_unknown_space_stops() {
        let d = tempfile::tempdir().unwrap();
        let t0 = Instant::now();
        let mut r = Recorder::new(d.path(), "Game", "s", t0);
        r.poll(t0, true, Some(50 * GB));
        assert_eq!(r.poll(t0, true, Some(9 * GB)), Step::Stop(StopReason::LowSpace));
        let mut r = Recorder::new(d.path(), "Game", "s", t0);
        assert_eq!(r.poll(t0, true, None), Step::Stop(StopReason::LowSpace));
    }

    #[test]
    fn an_existing_file_is_never_reused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Game - s.ts"), b"old").unwrap();
        let t0 = Instant::now();
        let mut r = Recorder::new(d.path(), "Game", "s", t0);
        let Step::Begin(p) = r.poll(t0, true, Some(50 * GB)) else { panic!("begin") };
        assert!(p.ends_with("Game - s (2).ts"), "{p:?}");
    }

    #[test]
    fn lists_newest_first_and_deletes_only_inside_the_folder() {
        let d = tempfile::tempdir().unwrap();
        assert!(list(&d.path().join("missing")).is_empty());
        let a = d.path().join("A.ts");
        std::fs::write(&a, b"12345").unwrap();
        std::fs::write(d.path().join("notes.txt"), b"x").unwrap();
        let l = list(d.path());
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].title.as_str(), l[0].size, l[0].key().as_str()), ("A", 5, "rec:A"));
        let other = tempfile::tempdir().unwrap();
        let b = other.path().join("B.ts");
        std::fs::write(&b, b"x").unwrap();
        assert!(delete(d.path(), &b).is_err());
        assert!(b.exists());
        assert!(delete(d.path(), &d.path().join("notes.txt")).is_err());
        delete(d.path(), &a).unwrap();
        assert!(list(d.path()).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn reads_free_space_and_local_time() {
        let d = tempfile::tempdir().unwrap();
        assert!(free_bytes(d.path()).is_some_and(|f| f > 0));
        let s = local_stamp();
        assert_eq!(s.len(), "2026-10-23 20-30".len(), "{s}");
    }
}
