//! The ffmpeg process of the phone share: listens on a local TCP port for the
//! tap stream from mpv and writes an HLS live playlist into a folder.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{rec_merge, share};

/// Time for ffmpeg to start listening before mpv connects.
pub const PIPE_WAIT: Duration = Duration::from_millis(800);

pub struct Pipe {
    child: Child,
    started: Instant,
    stopped: bool,
}

impl Pipe {
    pub fn start(ffmpeg: &Path, tap_port: u16, dir: &Path, restart: bool) -> Result<Pipe> {
        std::fs::create_dir_all(dir).context("cannot create the share folder")?;
        let log = File::create(dir.join("ffmpeg.log")).context("cannot create ffmpeg.log")?;
        let mut cmd = Command::new(ffmpeg);
        cmd.args(share::ffmpeg_args(tap_port, dir, restart))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        let child = rec_merge::hide_window(&mut cmd).spawn().context("cannot start ffmpeg")?;
        Ok(Pipe { child, started: Instant::now(), stopped: false })
    }

    pub fn alive(&mut self) -> bool {
        !self.stopped && matches!(self.child.try_wait(), Ok(None))
    }

    pub fn started_at(&self) -> Instant {
        self.started
    }

    /// Kill the child and wait for it, so no process or handle remains.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `live.m3u8` exists and lists at least one segment.
pub fn has_segments(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("live.m3u8")).is_ok_and(|t| {
        t.lines().any(|l| {
            let l = l.trim();
            l.starts_with("seg-") && l.ends_with(".ts")
        })
    })
}

/// Monotonic count of segments the pipe has written: `#EXT-X-MEDIA-SEQUENCE`
/// (0 if absent) plus the listed segments; 0 when the playlist is missing.
pub fn segment_count(dir: &Path) -> u64 {
    let Ok(t) = std::fs::read_to_string(dir.join("live.m3u8")) else { return 0 };
    let mut seq = 0u64;
    let mut listed = 0u64;
    for l in t.lines() {
        let l = l.trim();
        if let Some(v) = l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if l.starts_with("seg-") && l.ends_with(".ts") {
            listed += 1;
        }
    }
    seq.saturating_add(listed)
}

/// Delete the share files (`seg-*.ts`, `live.m3u8`, `ffmpeg.log`) directly in
/// `dir`; nothing else, no recursion.
pub fn sweep(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let ours = name == "live.m3u8"
            || name == "ffmpeg.log"
            || (name.starts_with("seg-") && name.ends_with(".ts"));
        if ours && e.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("iptv-share-pipe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[test]
    fn sweep_removes_only_share_files() {
        let d = scratch("sweep");
        for f in ["seg-1.ts", "seg-22.ts", "live.m3u8", "ffmpeg.log", "keep.txt"] {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("seg-3.ts"), b"x").unwrap();
        sweep(&d);
        let mut left: Vec<String> =
            std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, vec!["keep.txt".to_string(), "sub".to_string()]);
        assert!(d.join("sub").join("seg-3.ts").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn has_segments_needs_a_listed_segment() {
        let d = scratch("seg");
        assert!(!has_segments(&d));
        std::fs::write(d.join("live.m3u8"), "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n").unwrap();
        assert!(!has_segments(&d));
        std::fs::write(d.join("live.m3u8"), "#EXTM3U\n#EXTINF:2.0,\nseg-0.ts\n").unwrap();
        assert!(has_segments(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn segment_count_is_sequence_plus_listed() {
        let d = scratch("count");
        assert_eq!(segment_count(&d), 0);
        let w = |t: &str| std::fs::write(d.join("live.m3u8"), t).unwrap();
        w("#EXTM3U
#EXTINF:2.0,
seg-0.ts
#EXTINF:2.0,
seg-1.ts
");
        assert_eq!(segment_count(&d), 2);
        w("#EXTM3U
#EXT-X-MEDIA-SEQUENCE:5
#EXTINF:2.0,
seg-5.ts
#EXTINF:2.0,
seg-6.ts
#EXTINF:2.0,
seg-7.ts
");
        assert_eq!(segment_count(&d), 8);
        // rolling window: listed count constant, sequence grows
        w("#EXTM3U
#EXT-X-MEDIA-SEQUENCE:6
#EXTINF:2.0,
seg-6.ts
#EXTINF:2.0,
seg-7.ts
#EXTINF:2.0,
seg-8.ts
");
        assert_eq!(segment_count(&d), 9);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pipe_start_fails_cleanly_for_a_missing_ffmpeg() {
        let d = scratch("missing");
        let r = Pipe::start(Path::new("definitely-not-ffmpeg-xyz"), free_port(), &d, false);
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pipe_stop_kills_the_child() {
        let Some(ff) = rec_merge::find_ffmpeg() else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let d = scratch("stop");
        let mut p = Pipe::start(&ff, free_port(), &d, false).unwrap();
        assert!(p.alive());
        p.stop();
        assert!(!p.alive());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pipe_turns_a_tcp_tap_into_hls() {
        let Some(ff) = rec_merge::find_ffmpeg() else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let d = scratch("hls");
        let port = free_port();
        let mut p = Pipe::start(&ff, port, &d, false).unwrap();
        std::thread::sleep(PIPE_WAIT);
        let mut cmd = Command::new(&ff);
        cmd.args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25",
            "-t", "8", "-c:v", "libx264", "-preset", "ultrafast", "-g", "25", "-f", "mpegts"])
            .arg(share::tap_url(port))
            .stdin(Stdio::null());
        let mut src = rec_merge::hide_window(&mut cmd).spawn().unwrap();
        let t0 = Instant::now();
        let mut ok = false;
        while t0.elapsed() < Duration::from_secs(15) {
            if has_segments(&d) {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = src.kill();
        let _ = src.wait();
        p.stop();
        assert!(ok, "no segments within 15 s");
        let _ = std::fs::remove_dir_all(&d);
    }
}
