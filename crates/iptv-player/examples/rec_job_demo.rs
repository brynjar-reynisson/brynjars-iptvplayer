//! Headless end-to-end check of the scheduled recorder:
//! `cargo run -p iptv-player --example rec_job_demo`
//! Builds a scratch data dir with a one-channel cache pointing at a local
//! ffmpeg MPEG-TS server (port 8188), a schedule whose window is the current
//! minute, runs `iptv::rec_job::run`, kills the stream once so the recorder
//! must reconnect, and checks that the two pieces were joined into one file. Needs `ffmpeg`.
//!
//! `rec_job_demo handoff`: the "app" claims the recording for the first 15 s
//! (`iptv::handoff`); the recorder must stand by without connecting (no file),
//! then take over once the claim is released.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use iptv::playlist::{Entry, Kind};

fn spawn_ffmpeg() -> Child {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=25", "-f", "mpegts", "-listen", "1"])
        .arg("http://127.0.0.1:8188/live/u/p/7.ts")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("ffmpeg on PATH")
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("rec-job-demo");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), "playlist_url = \"http://127.0.0.1:8188/get.php?username=u&password=p\"\n")?;
    let entry = Entry {
        kind: Kind::Live,
        id: "7".into(),
        name: "Demo Sport".into(),
        group: "Testland - Main".into(),
        logo: String::new(),
        url: "http://127.0.0.1:8188/live/u/p/7.ts".into(),
    };
    iptv::cache::save(&dir, &[entry.clone()])?;
    // The window is the current minute; start in the next one if it is nearly over.
    let mut now = iptv::recordings::local_now();
    if now.second > 35 {
        std::thread::sleep(Duration::from_secs(61 - now.second as u64));
        now = iptv::recordings::local_now();
    }
    let start = now.hour * 60 + now.minute;
    let mut sc = iptv::schedule::Schedules::load(&dir);
    sc.add("Demo Sport", &iptv::playlist::favorite_key(&entry), 0x7f, start, (start + 1) % 1440).unwrap();
    sc.save_if_dirty()?;

    let handoff = std::env::args().nth(1).as_deref() == Some("handoff");
    let release_at = Duration::from_secs(15);
    let t_start = Instant::now();
    let claimer = handoff.then(|| {
        let dir = dir.clone();
        std::thread::spawn(move || {
            while t_start.elapsed() < release_at {
                iptv::handoff::claim(&dir, "s1").unwrap();
                std::thread::sleep(Duration::from_secs(1));
            }
            iptv::handoff::release(&dir, "s1");
        })
    });
    let mut ff = spawn_ffmpeg();
    let killer = std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut killed = false;
        loop {
            std::thread::sleep(Duration::from_millis(200));
            if !killed && t0.elapsed() > Duration::from_secs(8) {
                let _ = ff.kill();
                killed = true;
            }
            if killed && ff.try_wait().ok().flatten().is_some() && t0.elapsed() < Duration::from_secs(70) {
                // The stream stays away for 5 s: a gap long enough for a card.
                std::thread::sleep(Duration::from_secs(5));
                ff = spawn_ffmpeg();
                killed = true;
                // from now on keep it up
                loop {
                    std::thread::sleep(Duration::from_millis(300));
                    if ff.try_wait().ok().flatten().is_some() {
                        ff = spawn_ffmpeg();
                    }
                    if t0.elapsed() > Duration::from_secs(70) {
                        let _ = ff.kill();
                        return;
                    }
                }
            }
            if t0.elapsed() > Duration::from_secs(70) {
                let _ = ff.kill();
                return;
            }
        }
    });
    let t0 = Instant::now();
    println!("recording for the rest of the minute (up to 60 s)...");
    iptv::rec_job::run(&dir, "s1")?;
    println!("run took {:.0} s", t0.elapsed().as_secs_f64());
    let _ = killer.join();
    println!("--- recorder.log");
    print!("{}", std::fs::read_to_string(dir.join("recorder.log"))?);
    let files = iptv::recordings::list(&iptv::recordings::dir_in(&dir));
    for f in &files {
        println!("{}  {} bytes", f.title, f.size);
    }
    if let Some(c) = claimer {
        let _ = c.join();
        let log = std::fs::read_to_string(dir.join("recorder.log"))?;
        let first = files.iter().map(|f| f.created).min();
        let after_release = first.is_some_and(|c| c.duration_since(std::time::SystemTime::now() - t0.elapsed()).map_or(false, |d| d >= Duration::from_secs(13)));
        let ok = log.contains("standing by") && log.contains("taking over") && after_release && files.len() == 1;
        println!("standby seen: {}, takeover seen: {}, first file after release: {after_release}", log.contains("standing by"), log.contains("taking over"));
        println!("RESULT: {}", if ok { "OK" } else { "FAILED" });
        return Ok(());
    }
    let log = std::fs::read_to_string(dir.join("recorder.log"))?;
    // One file for the whole window, joined from two pieces (the forced reconnect).
    let ok = files.len() == 1 && files[0].size > 1_000_000 && log.contains("joined 2 piece(s)") && log.contains("1 gap card(s)");
    println!("RESULT: {}", if ok { "OK" } else { "FAILED" });
    Ok(())
}
