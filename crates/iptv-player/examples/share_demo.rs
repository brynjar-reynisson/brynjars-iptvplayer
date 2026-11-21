//! Headless end-to-end check of the phone share:
//! `cargo run --release -p iptv-player --example share_demo`
//! A local ffmpeg MPEG-TS source (h264 + aac, port 8483) plays in a real libmpv
//! (`MpvBackend`); the same Sharer / Pipe / segment_count loop as
//! `App::update_share` taps it (ffmpeg listens on 8485) and a `Server` serves
//! the result on 8484. Needs `ffmpeg` and `ffprobe` (libx264) on PATH; uses
//! 127.0.0.1 ports 8483-8485 only (it refuses to start when one is taken).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use iptv::share::{self, tap_url};
use iptv::share_pipe::{self, Pipe};
use iptv::share_server::Server;
use iptv::supervisor::{Backend, PlayerEvent};
use iptv_player::mpv_backend::{MpvBackend, MpvOptions};
use iptv_player::state::share::{port_free, RestartBudget, ShareInput, ShareStep, Sharer};

const SRC_PORT: u16 = 8483;
const SHARE_PORT: u16 = 8484;
const TAP_PORT: u16 = 8485;

fn spawn_source(n: u32) -> Child {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=25"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "25", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac", "-f", "mpegts", "-listen", "1"])
        .arg(format!("http://127.0.0.1:{SRC_PORT}/live/u/p/{n}.ts"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("ffmpeg on PATH")
}

fn powershell(script: &str) -> String {
    let out = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]).output().expect("powershell");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Number of ffmpeg processes whose command line contains `needle`.
fn ffmpeg_count(needle: &str) -> usize {
    let s = format!(
        "@(Get-CimInstance Win32_Process -Filter \"Name='ffmpeg.exe'\" | Where-Object {{ $_.CommandLine -like '*{needle}*' }}).Count"
    );
    powershell(&s).parse().unwrap_or(0)
}

fn kill_ffmpeg(needle: &str) {
    let s = format!(
        "Get-CimInstance Win32_Process -Filter \"Name='ffmpeg.exe'\" | Where-Object {{ $_.CommandLine -like '*{needle}*' }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}"
    );
    powershell(&s);
}

/// One plain HTTP/1.1 GET; returns (status, body).
fn get(path: &str) -> (u16, Vec<u8>) {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", SHARE_PORT)) else { return (0, Vec::new()) };
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..split.min(buf.len())]).to_string();
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, buf[split..].to_vec())
}

struct Demo {
    mpv: MpvBackend,
    ffmpeg: PathBuf,
    share_dir: PathBuf,
    server: Option<Server>,
    pipe: Option<Pipe>,
    sharer: Sharer,
    budget: RestartBudget,
    pipe_gen: Option<u32>,
    segs: (Instant, u64),
    generation: u32,
    playing: bool,
    /// (generation, restart) of every StartPipe, in order.
    starts: Vec<(u32, bool)>,
    /// Highest number of our pipe ffmpeg processes seen at once is checked by the caller.
    budget_stop: bool,
}

impl Demo {
    /// What `App::update_share` does each frame.
    fn tick(&mut self) {
        for (gen, ev) in self.mpv.poll_events() {
            if gen != self.generation {
                continue;
            }
            match ev {
                PlayerEvent::Started => self.playing = true,
                PlayerEvent::Ended | PlayerEvent::Error(_) => self.playing = false,
                _ => {}
            }
        }
        let _ = self.mpv.take_log();
        if self.server.is_none() {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.segs.0) >= Duration::from_millis(500) {
            self.segs = (now, share_pipe::segment_count(&self.share_dir));
        }
        let pipe_alive = self.pipe.as_mut().is_some_and(|p| p.alive());
        let step = self.sharer.poll(
            now,
            ShareInput { live_playing: self.playing, generation: self.generation, pipe_alive, segments: self.segs.1 },
        );
        match step {
            ShareStep::Nothing | ShareStep::Stop(_) => {}
            ShareStep::StartPipe { restart } => {
                if let Some(mut old) = self.pipe.take() {
                    old.stop();
                }
                let gen = self.generation;
                let prev = self.pipe_gen.replace(gen);
                self.starts.push((gen, restart));
                if restart && self.budget.on_restart(now, prev, gen) {
                    self.budget_stop = true;
                    return;
                }
                if !restart {
                    share_pipe::sweep(&self.share_dir);
                }
                self.pipe = Some(Pipe::start(&self.ffmpeg, TAP_PORT, &self.share_dir, restart).expect("start pipe"));
            }
            ShareStep::StartTap => self.mpv.set_property("stream-record", &tap_url(TAP_PORT)),
            ShareStep::StopPipe => {
                if let Some(mut p) = self.pipe.take() {
                    p.stop();
                }
                self.mpv.set_property("stream-record", "");
            }
        }
        if self.sharer.is_live() {
            self.budget.reset();
        }
        if let Some(s) = &self.server {
            s.set_status("Demo Sport", self.sharer.is_live());
        }
    }

    fn pump(&mut self, secs: f64) {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < end {
            self.tick();
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// Tick until `f` is true or `secs` pass.
    fn wait_for(&mut self, secs: f64, mut f: impl FnMut(&mut Demo) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < end {
            self.tick();
            if f(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        false
    }

    fn playlist(&self) -> String {
        std::fs::read_to_string(self.share_dir.join("live.m3u8")).unwrap_or_default()
    }
}

struct Checks {
    failed: u32,
}

impl Checks {
    fn check(&mut self, ok: bool, what: &str) {
        if ok {
            println!("ok: {what}");
        } else {
            println!("FAILED: {what}");
            self.failed += 1;
        }
    }
}

fn first_segment(playlist: &str) -> Option<String> {
    playlist.lines().map(str::trim).find(|l| l.starts_with("seg-") && l.ends_with(".ts")).map(String::from)
}

fn ffprobe_codecs(file: &Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .expect("ffprobe on PATH");
    String::from_utf8_lossy(&out.stdout).replace("\r", "").lines().map(str::trim).collect::<Vec<_>>().join(",")
}

fn main() -> anyhow::Result<()> {
    for p in [SRC_PORT, SHARE_PORT, TAP_PORT] {
        if !port_free(p) {
            println!("port {p} on 127.0.0.1 is already in use; the demo needs 8483, 8484 and 8485 free. Nothing was started.");
            std::process::exit(2);
        }
    }
    let Some(ffmpeg) = iptv::rec_merge::find_ffmpeg() else {
        println!("ffmpeg not found");
        std::process::exit(2);
    };
    let data = std::env::temp_dir().join("share-demo");
    let _ = std::fs::remove_dir_all(&data);
    let share_dir = data.join("share");
    std::fs::create_dir_all(&share_dir)?;
    let token = share::new_token()?;
    let mut c = Checks { failed: 0 };

    let mut src = spawn_source(1);
    std::thread::sleep(Duration::from_millis(800));
    let mut mpv = MpvBackend::new(MpvOptions::default())?;
    mpv.load(&format!("http://127.0.0.1:{SRC_PORT}/live/u/p/1.ts"), None, 1);
    let server = Server::start(SHARE_PORT, token.clone(), share_dir.clone())?;
    let mut d = Demo {
        mpv,
        ffmpeg,
        share_dir: share_dir.clone(),
        server: Some(server),
        pipe: None,
        sharer: Sharer::new(),
        budget: RestartBudget::default(),
        pipe_gen: None,
        segs: (Instant::now() - Duration::from_secs(1), 0),
        generation: 1,
        playing: false,
        starts: Vec::new(),
        budget_stop: false,
    };

    // (1) the playlist is served and lists a segment
    let list_path = format!("/{token}/live.m3u8");
    let mut listed = None;
    let ok = d.wait_for(20.0, |_| {
        let (st, body) = get(&list_path);
        if st == 200 {
            if let Some(s) = first_segment(&String::from_utf8_lossy(&body)) {
                listed = Some(s);
                return true;
            }
        }
        false
    });
    c.check(ok, "playlist answers 200 within 20 s and lists a segment");
    d.pump(3.0);
    c.check(d.sharer.is_live(), "sharer reports live (segment counter grew after the tap)");

    // (2) a segment is MPEG-TS with h264 + aac
    let seg = first_segment(&d.playlist()).or(listed).unwrap_or_default();
    let (st, body) = get(&format!("/{token}/{seg}"));
    c.check(st == 200 && body.first() == Some(&0x47), "segment is served and starts with the TS sync byte 0x47");
    let probe_file = data.join("probe.ts");
    std::fs::write(&probe_file, &body)?;
    let codecs = ffprobe_codecs(&probe_file);
    c.check(codecs.contains("h264") && codecs.contains("aac"), &format!("ffprobe finds h264 and aac ({codecs})"));

    // (3) wrong token and traversal
    let (st, _) = get(&format!("/{}/live.m3u8", "0".repeat(32)));
    c.check(st == 404, "wrong token = 404");
    let (st, _) = get(&format!("/{token}/..%2f..%2fconfig.toml"));
    let (st2, _) = get(&format!("/{token}/../config.toml"));
    c.check(st == 404 && st2 == 404, "traversal attempts = 404");

    // (4) channel switch
    let count_before = share_pipe::segment_count(&share_dir);
    let starts_before = d.starts.len();
    let _ = src.kill();
    let _ = src.wait();
    let mut src2 = spawn_source(2);
    std::thread::sleep(Duration::from_millis(800));
    d.generation = 2;
    d.playing = false;
    d.mpv.load(&format!("http://127.0.0.1:{SRC_PORT}/live/u/p/2.ts"), None, 2);
    let mut max_pipes = 0;
    let mut saw_restart = false;
    let mut next_count = Instant::now();
    let ok = d.wait_for(40.0, |d| {
        saw_restart = d.starts[starts_before..].iter().any(|&(g, r)| g == 2 && r);
        if Instant::now() >= next_count {
            max_pipes = max_pipes.max(ffmpeg_count(&format!("127.0.0.1:{TAP_PORT}")));
            next_count = Instant::now() + Duration::from_secs(2);
        }
        saw_restart && d.sharer.is_live() && d.playlist().contains("#EXT-X-DISCONTINUITY")
    });
    c.check(saw_restart, "channel switch started a new pipe with restart=true");
    c.check(ok, "after the switch the sharer is live again and the playlist has #EXT-X-DISCONTINUITY");
    d.pump(3.0);
    let count_after = share_pipe::segment_count(&share_dir);
    c.check(count_after > count_before, &format!("media sequence keeps growing ({count_before} -> {count_after})"));
    max_pipes = max_pipes.max(ffmpeg_count(&format!("127.0.0.1:{TAP_PORT}")));
    c.check(max_pipes == 1, &format!("one pipe ffmpeg at a time (max seen {max_pipes})"));
    c.check(!d.budget_stop, "channel switch did not use up the restart budget");

    // (5) the pipe's ffmpeg dies mid-run
    let starts_before = d.starts.len();
    let count_before = share_pipe::segment_count(&share_dir);
    kill_ffmpeg(&format!("127.0.0.1:{TAP_PORT}"));
    let ok = d.wait_for(40.0, |d| {
        d.starts.len() > starts_before && d.sharer.is_live() && share_pipe::segment_count(&share_dir) > count_before + 1
    });
    c.check(ok && d.starts[starts_before..].iter().all(|&(g, r)| g == 2 && r), "killed pipe was restarted (same generation) and segments resumed");
    let (st, _) = get(&list_path);
    c.check(st == 200 && d.playlist().contains("#EXT-X-DISCONTINUITY"), "playlist still served after the restart");

    // (6) stop: nothing is left
    if let Some(mut p) = d.pipe.take() {
        p.stop();
    }
    d.mpv.set_property("stream-record", "");
    if let Some(s) = d.server.take() {
        s.stop();
    }
    share_pipe::sweep(&share_dir);
    d.mpv.stop();
    let _ = src2.kill();
    let _ = src2.wait();
    std::thread::sleep(Duration::from_millis(500));
    c.check(port_free(SHARE_PORT), "share port is free after stop");
    let leftovers: Vec<String> = std::fs::read_dir(&share_dir)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n == "live.m3u8" || (n.starts_with("seg-") && n.ends_with(".ts")))
        .collect();
    c.check(leftovers.is_empty(), &format!("no seg-*.ts or live.m3u8 remain ({} found)", leftovers.len()));
    let procs = ffmpeg_count(&format!("127.0.0.1:{TAP_PORT}")) + ffmpeg_count(&format!("127.0.0.1:{SRC_PORT}"));
    c.check(procs == 0, &format!("no ffmpeg process of the demo remains ({procs})"));
    c.check(port_free(TAP_PORT) && port_free(SRC_PORT), "tap and source ports are free");

    let _ = std::fs::remove_dir_all(&data);
    if c.failed == 0 {
        println!("RESULT: ALL CHECKS OK");
        Ok(())
    } else {
        println!("RESULT: {} CHECK(S) FAILED", c.failed);
        std::process::exit(1);
    }
}
