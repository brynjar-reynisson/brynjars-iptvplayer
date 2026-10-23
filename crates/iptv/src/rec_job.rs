//! The headless recorder started by Task Scheduler (`iptv-player.exe --record
//! <id>`): finds the schedule's channel in the cached playlist and copies its
//! raw stream into `recordings/` until the end of the window, reconnecting
//! when the stream drops. It needs no window, no libmpv and no running app,
//! and keeps Windows awake while it works. Progress goes to `recorder.log` in
//! the data directory; the stream URL is never written anywhere.

use anyhow::{anyhow, bail, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::recordings::{self, Recorder, Step, StopReason};
use crate::schedule::{self, Schedules};

const READ_CHUNK: usize = 64 * 1024;
/// A stream that sends nothing for this long is dropped and reopened.
const STALL: Duration = Duration::from_secs(20);
const DISK_EVERY: Duration = Duration::from_secs(5);
/// The log is moved to `recorder.log.old` above this size.
const LOG_MAX: u64 = 256 * 1024;

struct Log {
    path: PathBuf,
    id: String,
}

impl Log {
    fn open(dir: &Path, id: &str) -> Log {
        let path = dir.join("recorder.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_MAX) {
            let _ = std::fs::rename(&path, dir.join("recorder.log.old"));
        }
        Log { path, id: id.to_string() }
    }

    fn line(&self, msg: &str) {
        let t = recordings::local_now();
        let line = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} [{}] {msg}\n",
            t.year, t.month, t.day, t.hour, t.minute, t.second, self.id
        );
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Keeps the computer from sleeping while alive.
struct KeepAwake;

impl KeepAwake {
    fn new() -> KeepAwake {
        #[cfg(windows)]
        unsafe {
            SetThreadExecutionState(0x8000_0001); // ES_CONTINUOUS | ES_SYSTEM_REQUIRED
        }
        KeepAwake
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            SetThreadExecutionState(0x8000_0000); // ES_CONTINUOUS
        }
    }
}

#[cfg(windows)]
extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}

/// Run schedule `id` once (now). `Ok` also when there is nothing to do
/// (disabled, outside the window).
pub fn run(data_dir: &Path, id: &str) -> Result<()> {
    let log = Log::open(data_dir, id);
    let r = run_inner(data_dir, id, &log);
    if let Err(e) = &r {
        log.line(&format!("error: {e:#}"));
    }
    r
}

fn run_inner(data_dir: &Path, id: &str, log: &Log) -> Result<()> {
    let schedules = Schedules::load(data_dir);
    let s = schedules.get(id).cloned().ok_or_else(|| anyhow!("there is no schedule {id}"))?;
    if !s.enabled {
        log.line("schedule is off; nothing to do");
        return Ok(());
    }
    let Some(remaining) = schedule::remaining_secs(&s, recordings::local_now()) else {
        log.line("outside the schedule's window; nothing to do");
        return Ok(());
    };
    let begin = Instant::now();
    let deadline = begin + Duration::from_secs(remaining);
    log.line(&format!("starting {}: {remaining} s left of the window", s.summary()));

    let cfg = crate::config::load(data_dir)?;
    let (lib, _) = crate::fetch::load_cached(data_dir, &cfg.top_countries)
        .ok_or_else(|| anyhow!("there is no cached playlist; start the app once to download it"))?;
    let entry = lib
        .find_by_key(&s.key)
        .ok_or_else(|| anyhow!("the channel {} is not in the cached playlist", s.name))?;
    let url = lib.entry(entry).url.clone();
    drop(lib);
    if url.to_ascii_lowercase().contains(".m3u8") {
        bail!("{} is an HLS (.m3u8) stream, which cannot be recorded this way", s.name);
    }

    let dir = recordings::dir_in(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| anyhow!("cannot create the recordings folder: {e}"))?;
    let mut free = recordings::free_bytes(&dir);
    if let Some(why) = recordings::start_refusal(free) {
        bail!("{why}");
    }
    let _awake = KeepAwake::new();
    let client = reqwest::blocking::Client::builder()
        .user_agent(cfg.user_agent)
        .connect_timeout(Duration::from_secs(15))
        .timeout(None::<Duration>)
        .build()
        .map_err(|e| anyhow!("{}", e.without_url()))?;
    let mut rec = Recorder::with_max(&dir, &s.name, &recordings::local_stamp(), begin, Duration::from_secs(remaining));
    let mut free_at = Instant::now();
    let mut failures = 0u32;
    let mut bytes_total = 0u64;

    while Instant::now() < deadline {
        let resp = match client.get(&url).send() {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                failures += 1;
                log.line(&format!("the server answered HTTP {}", r.status().as_u16()));
                wait(retry_delay(failures, Some(r.status().as_u16())), deadline);
                continue;
            }
            Err(e) => {
                failures += 1;
                log.line(&format!("could not connect: {}", e.without_url()));
                wait(retry_delay(failures, None), deadline);
                continue;
            }
        };
        log.line("connected");
        // Blocking reqwest has no read timeout: a thread reads, this loop
        // gives up on a stream that sends nothing for STALL.
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<Vec<u8>>>();
        std::thread::spawn(move || {
            let mut resp = resp;
            loop {
                let mut b = vec![0u8; READ_CHUNK];
                match resp.read(&mut b) {
                    Ok(n) => {
                        b.truncate(n);
                        if tx.send(Ok(b)).is_err() || n == 0 {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        let mut out: Option<std::io::BufWriter<std::fs::File>> = None;
        let mut got_data = false;
        let end = loop {
            let wait_for = STALL.min(deadline.saturating_duration_since(Instant::now()));
            let chunk = match rx.recv_timeout(wait_for) {
                Ok(Ok(c)) if c.is_empty() => break "the stream ended".to_string(),
                Ok(Ok(c)) => c,
                Ok(Err(e)) => break format!("the stream broke: {e}"),
                Err(_) if Instant::now() >= deadline => break "the window is over".to_string(),
                Err(_) => break "the stream sent nothing for 20 s".to_string(),
            };
            let n = chunk.len();
            let now = Instant::now();
            if now.duration_since(free_at) >= DISK_EVERY {
                free_at = now;
                free = recordings::free_bytes(&dir).or(free);
            }
            match rec.poll(now, true, free) {
                Step::Begin(path) => match std::fs::File::create(&path) {
                    Ok(f) => {
                        log.line(&format!("recording to {}", path.file_name().unwrap_or_default().to_string_lossy()));
                        out = Some(std::io::BufWriter::with_capacity(256 * 1024, f));
                    }
                    Err(e) => bail!("cannot create the recording file: {e}"),
                },
                Step::Stop(why) => {
                    if let Some(mut o) = out.take() {
                        let _ = o.flush();
                    }
                    let msg = match why {
                        StopReason::MaxTime => "the window is over".to_string(),
                        StopReason::LowSpace => why.message().to_string(),
                    };
                    log.line(&format!("done: {msg} ({} MB written)", bytes_total / 1_000_000));
                    return Ok(());
                }
                Step::End | Step::Nothing => {}
            }
            if let Some(o) = &mut out {
                o.write_all(&chunk).map_err(|e| anyhow!("cannot write the recording: {e}"))?;
                bytes_total += n as u64;
                got_data = true;
            }
        };
        if let Some(mut o) = out.take() {
            let _ = o.flush();
        }
        rec.interrupt();
        failures = if got_data { 0 } else { failures + 1 };
        if Instant::now() < deadline {
            log.line(&format!("{end}; reconnecting"));
            wait(retry_delay(failures.max(1), None), deadline);
        }
    }
    log.line(&format!("done: the window is over ({} MB written)", bytes_total / 1_000_000));
    Ok(())
}

/// Seconds to wait before the next attempt: short for network trouble, longer
/// when the account is rejected (connection limit).
fn retry_delay(failures: u32, status: Option<u16>) -> Duration {
    let auth = matches!(status, Some(401 | 403 | 458 | 459));
    let steps: &[u64] = if auth { &[3, 6, 12, 15] } else { &[1, 2, 4, 8, 10] };
    Duration::from_secs(steps[(failures.saturating_sub(1) as usize).min(steps.len() - 1)])
}

/// Sleep `d`, but not past `deadline`.
fn wait(d: Duration, deadline: Instant) {
    let until = (Instant::now() + d).min(deadline);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(200).min(until.saturating_duration_since(Instant::now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_and_auth_waits_longer() {
        assert_eq!(retry_delay(1, None), Duration::from_secs(1));
        assert_eq!(retry_delay(50, None), Duration::from_secs(10));
        assert_eq!(retry_delay(1, Some(458)), Duration::from_secs(3));
        assert_eq!(retry_delay(9, Some(401)), Duration::from_secs(15));
        assert_eq!(retry_delay(2, Some(500)), Duration::from_secs(2));
    }

    #[test]
    fn nothing_to_do_without_a_schedule_or_outside_the_window() {
        let d = tempfile::tempdir().unwrap();
        assert!(run(d.path(), "s9").is_err(), "unknown id");
        let mut sc = Schedules::load(d.path());
        // A window that is surely not now: all days, one minute, 12 hours away.
        let now = recordings::local_now();
        let far = ((now.hour * 60 + now.minute + 720) % 1440) as u16;
        sc.add("A", "live:1", 0x7f, far, (far + 1) % 1440).unwrap();
        sc.save_if_dirty().unwrap();
        assert!(run(d.path(), "s1").is_ok());
        let log = std::fs::read_to_string(d.path().join("recorder.log")).unwrap();
        assert!(log.contains("outside the schedule's window"), "{log}");
    }
}
