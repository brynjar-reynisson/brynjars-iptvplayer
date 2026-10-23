//! Headless check of recording with real libmpv: `cargo run -p iptv-player --example record_demo`
//! Plays a local ffmpeg MPEG-TS stream, sets mpv's `stream-record` mid-playback,
//! clears it, sets it again for a second part, and prints the file sizes.
//! Needs `ffmpeg` on PATH; uses port 8189 on 127.0.0.1.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use iptv::supervisor::Backend;
use iptv_player::mpv_backend::{MpvBackend, MpvOptions};

fn pump(b: &mut MpvBackend, secs: f64) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        let _ = b.poll_events();
        for l in b.take_log() {
            println!("   log {l}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("record-demo");
    std::fs::create_dir_all(&dir)?;
    let (a, b2) = (dir.join("a.ts"), dir.join("b.ts"));
    let _ = (std::fs::remove_file(&a), std::fs::remove_file(&b2));
    let mut ff = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=25", "-f", "mpegts", "-listen", "1"])
        .arg("http://127.0.0.1:8189/live/u/p/1.ts")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::sleep(Duration::from_millis(800));
    let mut b = MpvBackend::new(MpvOptions::default())?;
    b.load("http://127.0.0.1:8189/live/u/p/1.ts", None, 1);
    pump(&mut b, 3.0);
    println!("record part a (4 s)");
    b.set_property("stream-record", a.to_str().unwrap());
    pump(&mut b, 4.0);
    b.set_property("stream-record", "");
    println!("stopped; a.ts = {:?} bytes", std::fs::metadata(&a).map(|m| m.len()));
    pump(&mut b, 2.0);
    println!("record part b (3 s)");
    b.set_property("stream-record", b2.to_str().unwrap());
    pump(&mut b, 3.0);
    b.set_property("stream-record", "");
    pump(&mut b, 1.0);
    let (sa, sb) = (std::fs::metadata(&a).map(|m| m.len()).unwrap_or(0), std::fs::metadata(&b2).map(|m| m.len()).unwrap_or(0));
    println!("a.ts = {sa} bytes, b.ts = {sb} bytes (dir {})", dir.display());
    b.stop();
    let _ = ff.kill();
    println!("RESULT: {}", if sa > 50_000 && sb > 20_000 && sa > sb { "OK" } else { "FAILED" });
    Ok(())
}
