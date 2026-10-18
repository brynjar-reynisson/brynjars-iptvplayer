//! Headless end-to-end check of the retry loop with real libmpv:
//! `cargo run -p iptv-player --example retry_demo`
//!
//! Drives `Supervisor<MpvBackend>` (vo=null, ao=null) against local servers
//! only (fake credentials in the paths, to show log redaction):
//! 1. ffmpeg MPEG-TS stream up -> Playing
//! 2. ffmpeg killed -> Reconnecting
//! 3. ffmpeg restarted -> Playing again without user action
//! 4. nothing listening -> Reconnecting with network backoff
//! 5. HTTP 401, then 458 -> Reconnecting { kind: Auth } with auth backoff;
//!    HTTP 407 (off-air channel) -> Reconnecting { kind: Unavailable }
//!
//! Prints a status timeline. Needs `ffmpeg` on PATH.
//!
//! Real-channel check (one connection, nothing printed that could identify
//! the provider or the account):
//! `retry_demo --url-from-playlist <file.m3u> <group-title> <index> <secs>`
//! plays the `index`-th entry of that group for `secs` seconds and prints the
//! masked log lines and the status timeline. The URL is never printed: every
//! `scheme://` token, host-like token and URL segment is masked.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iptv::supervisor::{PlayerEvent, RetryKind, Status, Supervisor};
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
    /// Real-channel mode: mask URLs, hosts and these segments in log lines.
    mask: Option<Vec<String>>,
}

/// Real-channel mode output filter: every token containing `://` becomes
/// `<url>`, every host-like token (`a.b`, `1.2.3.4:80`) `<host>`, and every
/// secret segment `<redacted>`.
fn mask_line(line: &str, secrets: &[String]) -> String {
    let is_sep = |c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>' | '`' | '(' | ')' | ',');
    let mut out = String::new();
    let mut tok = String::new();
    let flush = |tok: &mut String, out: &mut String| {
        let t = tok.trim_end_matches(['.', ':', ';']);
        let tail = &tok[t.len()..];
        let parts: Vec<&str> = t.split(['.', ':']).collect();
        let host_like = parts.len() >= 2
            && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
            && !t.chars().all(|c| c.is_ascii_digit() || c == '.')
            || parts.len() >= 4 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
        if tok.contains("://") {
            out.push_str("<url>");
        } else if host_like && t.contains('.') {
            out.push_str("<host>");
            out.push_str(tail);
        } else {
            out.push_str(tok);
        }
        tok.clear();
    };
    for c in line.chars() {
        if is_sep(c) {
            flush(&mut tok, &mut out);
            out.push(c);
        } else {
            tok.push(c);
        }
    }
    flush(&mut tok, &mut out);
    for sec in secrets {
        out = out.replace(sec.as_str(), "<redacted>");
    }
    out
}

/// Host, port, userinfo, path segments and query values of `url` (>= 3 chars).
fn secret_segments(url: &str) -> Vec<String> {
    let after = url.split_once("://").map_or(url, |(_, a)| a);
    let mut v: Vec<String> = after
        .split(['/', '?', '&', '=', '@', ':', '#'])
        .filter(|p| p.len() >= 3)
        .map(str::to_string)
        .collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.len()));
    v
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
                let line = match &self.mask {
                    Some(secrets) => mask_line(&line, secrets),
                    None => line,
                };
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

/// User-Agent header of every request the reject server received (None = absent).
fn seen_user_agents() -> &'static std::sync::Mutex<Vec<Option<String>>> {
    static S: std::sync::OnceLock<std::sync::Mutex<Vec<Option<String>>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Tiny HTTP server that rejects every request with `code` (switchable).
fn auth_server(code: Arc<AtomicU16>) {
    let l = TcpListener::bind(("127.0.0.1", AUTH_PORT)).expect("bind auth port");
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let ua = req
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("user-agent")))
                .map(|(_, v)| v.trim().to_string());
            seen_user_agents().lock().unwrap().push(ua);
            let c = code.load(Ordering::SeqCst);
            let (reason, ctype, body) = match c {
                401 => ("Unauthorized", "text/plain", "denied"),
                // Like the off-air event channels: instant 407, text/html.
                407 => ("Proxy Authentication Required", "text/html", "<html><body>off air</body></html>"),
                _ => ("Max Connections Reached", "text/plain", "denied"),
            };
            let _ = write!(
                s,
                "HTTP/1.1 {c} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// `--url-from-playlist <file> <group> <index> <secs>`: play one real entry,
/// printing only masked text (never the URL, host or account segments).
fn real_channel(args: &[String]) -> anyhow::Result<()> {
    use anyhow::Context;
    let [file, group, index, secs] = args else {
        anyhow::bail!("usage: retry_demo --url-from-playlist <file.m3u> <group-title> <index> <secs>");
    };
    let index: usize = index.parse().context("index")?;
    let secs: f64 = secs.parse().context("secs")?;
    let f = std::fs::File::open(file).context("open playlist")?;
    let entries = iptv::playlist::parse(std::io::BufReader::new(f));
    let in_group: Vec<_> = entries.iter().filter(|e| &e.group == group).collect();
    let e = in_group.get(index).with_context(|| format!("group has {} entries", in_group.len()))?;
    let url = e.url.clone();
    let secrets = secret_segments(&url);
    println!("real channel: group entry {index} of {}, kind {}, playing {secs} s", in_group.len(), e.kind.as_str());
    let backend = MpvBackend::new(MpvOptions::default())?;
    let mut d = Demo {
        sup: Supervisor::new(backend),
        t0: Instant::now(),
        last: None,
        ffmpeg: None,
        keep_stream: false,
        timeline: Vec::new(),
        mask: Some(secrets),
    };
    d.sup.play(url, e.kind == iptv::playlist::Kind::Live, Instant::now());
    d.run_for(secs);
    d.sup.stop();
    d.run_for(1.0);
    println!("\ntimeline:");
    for (t, s) in &d.timeline {
        println!("  {t:7.2}s {s:?}");
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--url-from-playlist") {
        return real_channel(&args[1..]);
    }
    let backend = MpvBackend::new(MpvOptions::default())?;
    println!("libmpv: {}", backend.mpv_version());
    let mut d = Demo {
        sup: Supervisor::new(backend),
        t0: Instant::now(),
        last: None,
        ffmpeg: None,
        keep_stream: true,
        timeline: Vec::new(),
        mask: None,
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
    ok &= d.expect(t, "Reconnecting after kill", |s| matches!(s, Status::Reconnecting { kind: RetryKind::Network, .. }));

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
    ok &= d.expect(t, "Reconnecting (network)", |s| matches!(s, Status::Reconnecting { kind: RetryKind::Network, .. }));
    ok &= d.expect(t, "network backoff reached attempt >= 4", |s| {
        matches!(s, Status::Reconnecting { attempt, kind: RetryKind::Network } if attempt >= 4)
    });

    d.note("5a. HTTP 401");
    let code = Arc::new(AtomicU16::new(401));
    auth_server(code.clone());
    let t = d.t();
    d.sup.play(format!("http://127.0.0.1:{AUTH_PORT}/live/demo_user/demo_pass/3.ts"), true, Instant::now());
    d.run_for(8.0);
    ok &= d.expect(t, "Reconnecting{kind: Auth} on 401", |s| matches!(s, Status::Reconnecting { kind: RetryKind::Auth, .. }));

    {
        let seen = seen_user_agents().lock().unwrap();
        let want = iptv::config::DEFAULT_USER_AGENT;
        let ua_ok = !seen.is_empty() && seen.iter().all(|u| u.as_deref() == Some(want));
        println!("  stream requests: {}, User-Agent seen: {:?}", seen.len(), seen.first());
        println!("CHECK stream requests send User-Agent {want:?}: {}", if ua_ok { "OK" } else { "FAILED" });
        ok &= ua_ok;
    }

    d.note("5b. HTTP 458");
    code.store(458, Ordering::SeqCst);
    let t = d.t();
    d.sup.play(format!("http://127.0.0.1:{AUTH_PORT}/live/demo_user/demo_pass/4.ts"), true, Instant::now());
    d.run_for(11.0);
    ok &= d.expect(t, "Reconnecting{kind: Auth} on 458", |s| matches!(s, Status::Reconnecting { kind: RetryKind::Auth, .. }));

    d.note("5c. HTTP 407 (off-air channel)");
    code.store(407, Ordering::SeqCst);
    let t = d.t();
    d.sup.play(format!("http://127.0.0.1:{AUTH_PORT}/live/demo_user/demo_pass/5.ts"), true, Instant::now());
    d.run_for(11.0);
    ok &= d.expect(t, "Reconnecting{kind: Unavailable} on 407", |s| {
        matches!(s, Status::Reconnecting { kind: RetryKind::Unavailable, .. })
    });
    let never_auth = !d
        .timeline
        .iter()
        .any(|(at, s)| *at >= t && matches!(s, Status::Reconnecting { kind: RetryKind::Auth, .. }));
    println!("CHECK 407 never reported as Auth: {}", if never_auth { "OK" } else { "FAILED" });
    ok &= never_auth;

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
