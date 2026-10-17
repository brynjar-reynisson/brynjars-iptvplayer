//! Headless end-to-end check of the retry loop with real libmpv:
//! `cargo run -p iptv-player --example retry_demo`
//!
//! Drives `Supervisor<MpvBackend>` (vo=null, ao=null) against local servers
//! only (fake credentials in the paths, to show log redaction):
//! 1. ffmpeg MPEG-TS stream up -> Playing
//! 2. ffmpeg killed -> Reconnecting
//! 3. ffmpeg restarted -> Playing again without user action
//! 4. nothing listening -> Reconnecting with network backoff
//! 5. HTTP 401, then 458 -> Reconnecting { auth: true } with auth backoff
//!
//! Prints a status timeline. Needs `ffmpeg` on PATH.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iptv::supervisor::{PlayerEvent, Status, Supervisor};
use iptv_player::mpv_backend::{MpvBackend, MpvOptions};

const STREAM_PORT: u16 = 8099;
const DEAD_PORT: u16 = 8098;
const AUTH_PORT: u16 = 8097;

struct Demo {
    sup: Supervisor<MpvBackend>,
    t0: Instant,
    last: Option<Status>,
    ffmpeg: Option<Child>,
    /// Keep ffmpeg running (restart when it exits after a client leaves).
    keep_stream: bool,
    /// Every status change: (seconds since start, status).
    timeline: Vec<(f64, Status)>,
}

fn spawn_ffmpeg() -> Child {
    Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=25",
            "-f", "mpegts", "-listen", "1",
        ])
        .arg(format!("http://127.0.0.1:{STREAM_PORT}/live/demo_user/demo_pass/1.ts"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("ffmpeg on PATH")
}

impl Demo {
    fn t(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    fn note(&self, msg: &str) {
        println!("{:7.2}s  ---- {msg}", self.t());
    }

    fn run_for(&mut self, secs: f64) {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < end {
            if self.keep_stream {
                let exited = match self.ffmpeg.as_mut() {
                    Some(c) => c.try_wait().ok().flatten().is_some(),
                    None => true,
                };
                if exited {
                    self.ffmpeg = Some(spawn_ffmpeg());
                }
            }
            let now = Instant::now();
            for (generation, ev) in self.sup.backend_mut().poll_events() {
                let current = generation == self.sup.generation();
                match &ev {
                    PlayerEvent::Position(_) | PlayerEvent::Duration(_) => {}
                    _ => println!(
                        "{:7.2}s    event gen={generation}{} {ev:?}",
                        self.t(),
                        if current { "" } else { " (stale)" }
                    ),
                }
                self.sup.on_event(generation, ev, now);
            }
            self.sup.tick(now);
            for line in self.sup.backend_mut().take_log() {
                println!("{:7.2}s    log {line}", self.t());
            }
            let s = self.sup.status();
            if self.last != Some(s) {
                let retry = self.sup.next_retry_in(now).map(|d| format!(" retry in {:.1}s", d.as_secs_f64()));
                println!("{:7.2}s  STATUS {s:?}{}", self.t(), retry.unwrap_or_default());
                self.timeline.push((self.t(), s));
                self.last = Some(s);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill_stream(&mut self) {
        self.keep_stream = false;
        if let Some(mut c) = self.ffmpeg.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn expect(&self, since: f64, what: &str, pred: impl Fn(Status) -> bool) -> bool {
        let ok = self.timeline.iter().any(|(t, s)| *t >= since && pred(*s));
        println!("CHECK {what}: {}", if ok { "OK" } else { "FAILED" });
        ok
    }
}

/// Tiny HTTP server that rejects every request with `code` (switchable).
fn auth_server(code: Arc<AtomicU16>) {
    let l = TcpListener::bind(("127.0.0.1", AUTH_PORT)).expect("bind auth port");
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let c = code.load(Ordering::SeqCst);
            let reason = if c == 401 { "Unauthorized" } else { "Max Connections Reached" };
            let body = "denied";
            let _ = write!(
                s,
                "HTTP/1.1 {c} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
}

/// Step 7, backend directly: a VOD load with `start_at` never reports a
/// position before the resume point, and a user pause is not buffering.
fn vod_start_and_pause(d: &mut Demo) -> anyhow::Result<bool> {
    use iptv::supervisor::Backend;
    d.note("7. VOD: load local 30 s file with start_at=12.5, then pause 3 s");
    let path = std::env::temp_dir().join("iptv_retry_demo_vod.ts");
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=25", "-t", "30", "-f", "mpegts"])
        .arg(&path)
        .status()?;
    anyhow::ensure!(st.success(), "ffmpeg could not write the test file");
    let b = d.sup.backend_mut();
    b.load(path.to_str().unwrap(), Some(12.5), 1000);
    let mut positions = vec![];
    let mut others = vec![];
    let mut paused_at = None;
    let end = Instant::now() + Duration::from_secs(7);
    while Instant::now() < end {
        for (g, ev) in b.poll_events() {
            match ev {
                PlayerEvent::Position(p) => positions.push(p),
                e => {
                    println!("{:7.2}s    event gen={g} {e:?}", d.t0.elapsed().as_secs_f64());
                    others.push(e)
                }
            }
        }
        if paused_at.is_none() && others.contains(&PlayerEvent::Started) {
            std::thread::sleep(Duration::from_millis(500));
            b.set_pause(true);
            paused_at = Some(Instant::now());
            println!("{:7.2}s    user pause", d.t0.elapsed().as_secs_f64());
        }
        if paused_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            b.set_pause(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    b.stop();
    let _ = std::fs::remove_file(&path);
    let first = positions.first().copied();
    println!(
        "  positions: {} events, first {:?}, last {:?}",
        positions.len(),
        first,
        positions.last()
    );
    let ok1 = first.is_some_and(|p| p >= 12.0);
    let ok2 = !others.contains(&PlayerEvent::Buffering(true));
    println!("CHECK first position >= start_at: {}", if ok1 { "OK" } else { "FAILED" });
    println!("CHECK pause not reported as Buffering(true): {}", if ok2 { "OK" } else { "FAILED" });
    Ok(ok1 && ok2)
}

fn main() -> anyhow::Result<()> {
    let backend = MpvBackend::new(MpvOptions::default())?;
    println!("libmpv: {}", backend.mpv_version());
    let mut d = Demo {
        sup: Supervisor::new(backend),
        t0: Instant::now(),
        last: None,
        ffmpeg: None,
        keep_stream: true,
        timeline: Vec::new(),
    };
    let mut ok = true;
    let url = format!("http://127.0.0.1:{STREAM_PORT}/live/demo_user/demo_pass/1.ts");

    d.note("1. start ffmpeg stream, play (live)");
    d.ffmpeg = Some(spawn_ffmpeg());
    std::thread::sleep(Duration::from_millis(800));
    let t = d.t();
    d.sup.play(url.clone(), true, Instant::now());
    d.run_for(8.0);
    ok &= d.expect(t, "stream plays", |s| s == Status::Playing);

    d.note("2. kill ffmpeg");
    let t = d.t();
    d.kill_stream();
    d.run_for(12.0);
    ok &= d.expect(t, "Reconnecting after kill", |s| matches!(s, Status::Reconnecting { auth: false, .. }));

    d.note("3. restart ffmpeg (no user action)");
    let t = d.t();
    d.keep_stream = true;
    d.run_for(12.0);
    ok &= d.expect(t, "playback resumes", |s| s == Status::Playing);
    d.kill_stream();

    d.note("4. nothing listening (connection refused)");
    let t = d.t();
    d.sup.play(format!("http://127.0.0.1:{DEAD_PORT}/live/demo_user/demo_pass/2.ts"), true, Instant::now());
    d.run_for(10.0);
    ok &= d.expect(t, "Reconnecting (network)", |s| matches!(s, Status::Reconnecting { auth: false, .. }));
    ok &= d.expect(t, "network backoff reached attempt >= 4", |s| {
        matches!(s, Status::Reconnecting { attempt, auth: false } if attempt >= 4)
    });

    d.note("5a. HTTP 401");
    let code = Arc::new(AtomicU16::new(401));
    auth_server(code.clone());
    let t = d.t();
    d.sup.play(format!("http://127.0.0.1:{AUTH_PORT}/live/demo_user/demo_pass/3.ts"), true, Instant::now());
    d.run_for(8.0);
    ok &= d.expect(t, "Reconnecting{auth:true} on 401", |s| matches!(s, Status::Reconnecting { auth: true, .. }));

    d.note("5b. HTTP 458");
    code.store(458, Ordering::SeqCst);
    let t = d.t();
    d.sup.play(format!("http://127.0.0.1:{AUTH_PORT}/live/demo_user/demo_pass/4.ts"), true, Instant::now());
    d.run_for(11.0);
    ok &= d.expect(t, "Reconnecting{auth:true} on 458", |s| matches!(s, Status::Reconnecting { auth: true, .. }));

    d.note("6. user stop");
    d.sup.stop();
    d.run_for(2.0);
    ok &= d.sup.status() == Status::Idle;

    ok &= vod_start_and_pause(&mut d)?;

    println!("\nRESULT: {}", if ok { "ALL CHECKS OK" } else { "SOME CHECKS FAILED" });
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}
