//! Scheduled recordings (`schedules.json` beside `positions.json`): which
//! channel to record on which weekdays between which times. Windows Task
//! Scheduler starts `iptv-player.exe --record <id>` at the start time (see
//! [`crate::tasks`]); the recorder ([`crate::rec_job`]) looks the schedule up
//! here, so the task carries no credentials. Same safety rules as settings:
//! atomic writes, and an unreadable file is moved aside to
//! `schedules.json.bad[.N]`, never deleted or overwritten.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::recordings::LocalNow;

const FORMAT_VERSION: u64 = 1;
/// Longest window accepted.
pub const MAX_MINUTES: u16 = 12 * 60;

/// Weekday names, Monday first (bit 0 of [`Schedule::days`] is Monday).
pub const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Short id used on the task's command line (`s1`, `s2`, ...).
    pub id: String,
    /// Channel name (file names, display).
    pub name: String,
    /// Favorite key of the channel (looked up in the cached playlist).
    pub key: String,
    /// Weekdays, bit 0 = Monday ... bit 6 = Sunday.
    pub days: u8,
    /// Start, minutes after local midnight.
    pub start: u16,
    /// End, minutes after local midnight; an end before the start means the
    /// next day.
    pub end: u16,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// `1110` -> `18:30`.
pub fn fmt_minutes(m: u16) -> String {
    format!("{:02}:{:02}", m / 60 % 24, m % 60)
}

/// `18:30` / `8:05` -> minutes after midnight.
pub fn parse_hhmm(text: &str) -> Option<u16> {
    let (h, m) = text.trim().split_once(':')?;
    let (h, m): (u16, u16) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    (h < 24 && m < 60 && m.to_string().len() <= 2).then_some(h * 60 + m)
}

/// `Sun`, `Mon-Fri`-style summary is not needed: `Mon Wed Sun`.
pub fn fmt_days(days: u8) -> String {
    let v: Vec<&str> = (0..7).filter(|d| days & (1 << d) != 0).map(|d| DAY_NAMES[d]).collect();
    if v.len() == 7 {
        "every day".to_string()
    } else {
        v.join(" ")
    }
}

impl Schedule {
    /// Length of the window in minutes.
    pub fn length_minutes(&self) -> u16 {
        (self.end + 1440 - self.start) % 1440
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.days & 0x7f == 0 {
            Err("Pick at least one weekday.")
        } else if self.start >= 1440 || self.end >= 1440 || self.start == self.end {
            Err("The start and end times must differ.")
        } else if self.length_minutes() > MAX_MINUTES {
            Err("A recording can be at most 12 hours long.")
        } else if self.key.is_empty() || self.name.trim().is_empty() {
            Err("No channel.")
        } else {
            Ok(())
        }
    }

    /// One-line description: `Sýn Sport · Sun · 18:30-19:45`.
    pub fn summary(&self) -> String {
        format!(
            "{} · {} · {}-{}{}",
            self.name,
            fmt_days(self.days),
            fmt_minutes(self.start),
            fmt_minutes(self.end),
            if self.enabled { "" } else { " (off)" }
        )
    }
}

/// Seconds left of the schedule's window at `now`, if `now` is inside one
/// (a window that began yesterday and runs past midnight counts too).
pub fn remaining_secs(s: &Schedule, now: LocalNow) -> Option<u64> {
    if s.validate().is_err() {
        return None;
    }
    let today = (now.weekday as usize + 6) % 7; // Monday = 0
    let yesterday = (today + 6) % 7;
    let now_s = now.hour as i64 * 3600 + now.minute as i64 * 60 + now.second as i64;
    let start_s = s.start as i64 * 60;
    let end_s = start_s + s.length_minutes() as i64 * 60;
    if s.days & (1 << today) != 0 && now_s >= start_s && now_s < end_s {
        return Some((end_s - now_s) as u64);
    }
    if s.days & (1 << yesterday) != 0 && end_s > 86400 && now_s < end_s - 86400 {
        return Some((end_s - 86400 - now_s) as u64);
    }
    None
}

fn days_in_month(y: u16, m: u16) -> u16 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// The day before (year, month, day).
fn previous_day(y: u16, m: u16, d: u16) -> (u16, u16, u16) {
    if d > 1 {
        (y, m, d - 1)
    } else if m > 1 {
        (y, m - 1, days_in_month(y, m - 1))
    } else {
        (y - 1, 12, 31)
    }
}

/// `2026-10-25 18-30`: the date and start time of the window that is open at
/// `now` with `remaining` seconds left (the same for everything that records
/// this window, so they agree on the file name).
pub fn window_start_stamp(s: &Schedule, now: LocalNow, remaining: u64) -> String {
    let elapsed = (s.length_minutes() as u64 * 60).saturating_sub(remaining);
    let now_s = now.hour as u64 * 3600 + now.minute as u64 * 60 + now.second as u64;
    let (y, m, d) = if now_s >= elapsed { (now.year, now.month, now.day) } else { previous_day(now.year, now.month, now.day) };
    format!("{y:04}-{m:02}-{d:02} {:02}-{:02}", s.start / 60 % 24, s.start % 60)
}

/// File name (without extension) of the recording of the window that is open
/// at `now` with `remaining` seconds left: `Channel - 2026-10-25 18-30`.
pub fn session_base(s: &Schedule, now: LocalNow, remaining: u64) -> String {
    format!("{} - {}", crate::recordings::sanitize_name(&s.name), window_start_stamp(s, now, remaining))
}

/// The window of `s` is not open at `now` but opens within `secs` seconds.
pub fn opens_within(s: &Schedule, now: LocalNow, secs: u32) -> bool {
    remaining_secs(s, now).is_none() && remaining_secs(s, now.plus_secs(secs)).is_some()
}

#[derive(Debug, Serialize, Deserialize)]
struct SchedulesFile {
    version: u64,
    /// The exe the Windows tasks were created for; when it differs from the
    /// running one (the app was moved) the tasks are created again.
    #[serde(default)]
    exe: Option<String>,
    #[serde(default)]
    items: Vec<Schedule>,
}

#[derive(Debug)]
pub struct Schedules {
    dir: PathBuf,
    items: Vec<Schedule>,
    exe: Option<String>,
    dirty: bool,
    blocked: Option<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("schedules.json")
}

impl Schedules {
    pub fn load(dir: &Path) -> Schedules {
        Self::load_with(dir, |from, to| std::fs::rename(from, to))
    }

    fn load_with(dir: &Path, mover: impl Fn(&Path, &Path) -> std::io::Result<()>) -> Schedules {
        let path = path_in(dir);
        let mut s = Schedules { dir: dir.to_path_buf(), items: Vec::new(), exe: None, dirty: false, blocked: None };
        let unreadable = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return s,
            Err(_) => true,
            Ok(bytes) => match serde_json::from_slice::<SchedulesFile>(&bytes) {
                Ok(f) if f.version == FORMAT_VERSION => {
                    s.items = f.items;
                    s.exe = f.exe;
                    false
                }
                _ => true,
            },
        };
        if unreadable {
            let bad = crate::fsutil::backup_path(&path);
            if let Err(e) = mover(&path, &bad) {
                s.blocked = Some(format!(
                    "{} is unreadable and could not be moved aside ({e}); refusing to overwrite it",
                    path.display()
                ));
            }
        }
        s
    }

    pub fn items(&self) -> &[Schedule] {
        &self.items
    }

    pub fn get(&self, id: &str) -> Option<&Schedule> {
        self.items.iter().find(|s| s.id == id)
    }

    /// The exe the tasks were created for.
    pub fn exe(&self) -> Option<&str> {
        self.exe.as_deref()
    }

    pub fn set_exe(&mut self, exe: &str) {
        if self.exe.as_deref() != Some(exe) {
            self.exe = Some(exe.to_string());
            self.dirty = true;
        }
    }

    /// Add a schedule (validated); returns a copy with its new id.
    pub fn add(&mut self, name: &str, key: &str, days: u8, start: u16, end: u16) -> Result<Schedule, &'static str> {
        let next = self.items.iter().filter_map(|s| s.id.strip_prefix('s')?.parse::<u32>().ok()).max().unwrap_or(0) + 1;
        let s = Schedule {
            id: format!("s{next}"),
            name: name.trim().to_string(),
            key: key.to_string(),
            days: days & 0x7f,
            start,
            end,
            enabled: true,
        };
        s.validate()?;
        self.items.push(s.clone());
        self.dirty = true;
        Ok(s)
    }

    /// Replace what schedule `id` records (validated); the id and the on/off
    /// state stay. Returns the old schedule (to roll back) and the new one.
    pub fn update(&mut self, id: &str, name: &str, key: &str, days: u8, start: u16, end: u16) -> Result<(Schedule, Schedule), &'static str> {
        let i = self.items.iter().position(|s| s.id == id).ok_or("That schedule no longer exists.")?;
        let new = Schedule { name: name.trim().to_string(), key: key.to_string(), days: days & 0x7f, start, end, ..self.items[i].clone() };
        new.validate()?;
        let old = std::mem::replace(&mut self.items[i], new.clone());
        self.dirty = true;
        Ok((old, new))
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.items.len();
        self.items.retain(|s| s.id != id);
        self.dirty |= self.items.len() != n;
        self.items.len() != n
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Option<Schedule> {
        let s = self.items.iter_mut().find(|s| s.id == id)?;
        if s.enabled != enabled {
            s.enabled = enabled;
            self.dirty = true;
        }
        Some(s.clone())
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(msg) = &self.blocked {
            return Err(anyhow!("{msg}"));
        }
        let path = path_in(&self.dir);
        let file = SchedulesFile { version: FORMAT_VERSION, exe: self.exe.clone(), items: self.items.clone() };
        let json = serde_json::to_vec_pretty(&file).context("serializing schedules")?;
        crate::fsutil::atomic_write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUN: u8 = 1 << 6;
    const MON: u8 = 1;

    fn at(weekday: u16, h: u16, m: u16, s: u16) -> LocalNow {
        LocalNow { year: 2026, month: 10, day: 25, weekday, hour: h, minute: m, second: s }
    }

    fn game() -> Schedule {
        Schedule {
            id: "s1".into(),
            name: "Syn Sport".into(),
            key: "live:1".into(),
            days: SUN,
            start: 18 * 60 + 30,
            end: 19 * 60 + 45,
            enabled: true,
        }
    }

    #[test]
    fn times_parse_and_format() {
        assert_eq!(parse_hhmm("18:30"), Some(1110));
        assert_eq!(parse_hhmm(" 8:05 "), Some(485));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("1230"), None);
        assert_eq!(fmt_minutes(1185), "19:45");
        assert_eq!(fmt_days(SUN | MON), "Mon Sun");
        assert_eq!(fmt_days(0x7f), "every day");
    }

    #[test]
    fn window_on_sunday_1830_to_1945() {
        let s = game();
        assert_eq!(s.length_minutes(), 75);
        assert_eq!(remaining_secs(&s, at(0, 18, 29, 59)), None, "a second before");
        assert_eq!(remaining_secs(&s, at(0, 18, 30, 0)), Some(75 * 60));
        assert_eq!(remaining_secs(&s, at(0, 19, 0, 0)), Some(45 * 60), "started late: the rest");
        assert_eq!(remaining_secs(&s, at(0, 19, 44, 59)), Some(1));
        assert_eq!(remaining_secs(&s, at(0, 19, 45, 0)), None, "over");
        assert_eq!(remaining_secs(&s, at(1, 19, 0, 0)), None, "Monday is not scheduled");
    }

    #[test]
    fn update_keeps_the_id_and_the_on_off_state() {
        let d = tempfile::tempdir().unwrap();
        let mut all = Schedules::load(d.path());
        let a = all.add("A", "live:1", 64, 1110, 1185).unwrap();
        all.set_enabled(&a.id, false);
        let (old, new) = all.update(&a.id, " B ", "live:2", 64, 1110, 1200).unwrap();
        assert_eq!((old.end, new.end, new.name.as_str(), new.enabled, new.id.as_str()), (1185, 1200, "B", false, "s1"));
        assert_eq!(all.get("s1").unwrap().key, "live:2");
        assert!(all.update("s1", "B", "live:2", 0, 1110, 1200).is_err(), "no weekday");
        assert_eq!(all.get("s1").unwrap().end, 1200, "a refused edit changes nothing");
        assert!(all.update("s9", "B", "k", 1, 0, 60).is_err());
    }

    #[test]
    fn the_window_start_stamp_is_the_same_whenever_it_is_asked() {
        let s = game(); // Sundays 18:30-19:45 (75 min)
        let a = window_start_stamp(&s, at(0, 18, 30, 1), 74 * 60 + 59);
        let b = window_start_stamp(&s, at(0, 19, 10, 0), 35 * 60);
        assert_eq!(a, "2026-10-25 18-30");
        assert_eq!(a, b);
        // A window that began yesterday and runs past midnight keeps yesterday's date.
        let mut late = game();
        late.start = 23 * 60;
        late.end = 60;
        let mut now = at(1, 0, 30, 0);
        now.day = 1;
        now.month = 3;
        assert_eq!(window_start_stamp(&late, now, 30 * 60), "2026-02-28 23-00");
        now.month = 1;
        assert_eq!(window_start_stamp(&late, now, 30 * 60), "2025-12-31 23-00");
    }

    #[test]
    fn opens_within_looks_ahead_across_midnight() {
        let s = game(); // Sundays 18:30-19:45
        assert!(opens_within(&s, at(0, 18, 29, 30), 45));
        assert!(!opens_within(&s, at(0, 18, 28, 0), 45), "too early");
        assert!(!opens_within(&s, at(0, 18, 31, 0), 45), "already open");
        assert!(!opens_within(&s, at(1, 18, 29, 30), 45), "Monday");
        let mut late = game();
        late.start = 0; // Sunday 00:00-01:00
        late.end = 60;
        assert!(opens_within(&late, at(6, 23, 59, 30), 45), "Saturday 23:59:30 -> Sunday 00:00");
        assert!(!opens_within(&late, at(0, 23, 59, 30), 45), "Sunday night -> Monday is not scheduled");
    }

    #[test]
    fn a_window_past_midnight_belongs_to_the_day_it_started() {
        let mut s = game();
        s.start = 23 * 60;
        s.end = 60; // 23:00-01:00 on Sundays
        assert_eq!(s.length_minutes(), 120);
        assert_eq!(remaining_secs(&s, at(0, 23, 30, 0)), Some(90 * 60));
        assert_eq!(remaining_secs(&s, at(1, 0, 30, 0)), Some(30 * 60), "Monday 00:30 is still Sunday's");
        assert_eq!(remaining_secs(&s, at(1, 1, 0, 0)), None);
        assert_eq!(remaining_secs(&s, at(0, 0, 30, 0)), None, "Sunday 00:30 belongs to Saturday's window");
    }

    #[test]
    fn invalid_schedules_are_rejected() {
        let mut s = game();
        s.days = 0;
        assert!(s.validate().is_err());
        let mut s = game();
        s.end = s.start;
        assert!(s.validate().is_err());
        let mut s = game();
        s.start = 6 * 60;
        s.end = 19 * 60;
        assert!(s.validate().unwrap_err().contains("12 hours"));
        assert_eq!(remaining_secs(&s, at(0, 7, 0, 0)), None);
    }

    #[test]
    fn ids_grow_and_the_file_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let mut sc = Schedules::load(d.path());
        let a = sc.add("A", "live:1", SUN, 1110, 1185).unwrap();
        let b = sc.add("B", "live:2", MON, 600, 660).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("s1", "s2"));
        assert!(sc.add("C", "live:3", 0, 600, 660).is_err());
        sc.remove("s1");
        assert_eq!(sc.add("D", "live:4", SUN, 600, 660).unwrap().id, "s3", "ids are not reused");
        sc.set_enabled("s2", false);
        sc.set_exe("C:/x/iptv-player.exe");
        sc.save_if_dirty().unwrap();
        let sc = Schedules::load(d.path());
        assert_eq!(sc.items().len(), 2);
        assert!(!sc.get("s2").unwrap().enabled);
        assert_eq!(sc.exe(), Some("C:/x/iptv-player.exe"));
        assert!(sc.get("s2").unwrap().summary().contains("(off)"));
    }

    #[test]
    fn corrupt_file_moved_aside_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut sc = Schedules::load(d.path());
        assert!(!f.exists());
        sc.add("A", "live:1", SUN, 1110, 1185).unwrap();
        sc.save_if_dirty().unwrap();
        assert_eq!(std::fs::read(d.path().join("schedules.json.bad")).unwrap(), b"{oops");
    }

    #[test]
    fn failed_move_aside_blocks_save() {
        let d = tempfile::tempdir().unwrap();
        let f = path_in(d.path());
        std::fs::write(&f, b"{oops").unwrap();
        let mut sc = Schedules::load_with(d.path(), |_, _| {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "locked"))
        });
        sc.add("A", "live:1", SUN, 1110, 1185).unwrap();
        assert!(sc.save_if_dirty().unwrap_err().to_string().contains("schedules.json"));
        assert_eq!(std::fs::read(&f).unwrap(), b"{oops");
    }
}
