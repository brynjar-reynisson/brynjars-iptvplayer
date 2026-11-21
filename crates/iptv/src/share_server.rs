//! Phone share: loopback HTTP server (std::net only). Serves the page, hls.js, the HLS
//! playlist/segments written by ffmpeg into `dir`, and a status JSON. Never logs the token.

use crate::share::{self, Route, Viewers};
use anyhow::Context;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 32;
const MAX_REQUEST_BYTES: usize = 8 * 1024;
#[cfg(not(test))]
const READ_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

const PAGE: &str = include_str!("../assets/share.html");
const HLS_JS: &[u8] = include_bytes!("../assets/hls.min.js");

struct Shared {
    token: String,
    dir: PathBuf,
    status: Mutex<(String, bool)>,
    viewers: Mutex<Viewers>,
    stop: AtomicBool,
    active: AtomicUsize,
}

pub struct Server {
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    pub fn start(port: u16, token: String, dir: PathBuf) -> anyhow::Result<Server> {
        if !share::is_valid_token(&token) {
            anyhow::bail!("invalid share token");
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                anyhow::anyhow!("port {port} is already in use")
            } else {
                anyhow::anyhow!("could not listen on port {port}: {e}")
            }
        })?;
        let shared = Arc::new(Shared {
            token,
            dir,
            status: Mutex::new((String::new(), false)),
            viewers: Mutex::new(Viewers::default()),
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        });
        let sh = shared.clone();
        let accept = std::thread::Builder::new()
            .name("share-accept".into())
            .spawn(move || accept_loop(listener, sh))
            .context("start share thread")?;
        Ok(Server { shared, accept: Some(accept) })
    }

    pub fn set_status(&self, channel: &str, live: bool) {
        if let Ok(mut s) = self.shared.status.lock() {
            *s = (channel.to_string(), live);
        }
    }

    pub fn viewers(&self) -> usize {
        self.shared.viewers.lock().map(|mut v| v.count(Instant::now())).unwrap_or(0)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            // The accept loop polls the stop flag every ACCEPT_POLL; wait at most 2 s, then detach.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: TcpListener, sh: Arc<Shared>) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    loop {
        if sh.stop.load(Ordering::SeqCst) {
            break;
        }
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(_) => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
        };
        let _ = stream.set_nonblocking(false);
        if sh.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            sh.active.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let sh2 = sh.clone();
        let spawned = std::thread::Builder::new().name("share-conn".into()).spawn(move || {
            handle(stream, &sh2);
            sh2.active.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            sh.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Reads the request head (up to 8 KiB, overall deadline `READ_TIMEOUT`).
fn read_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Some(buf);
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            return None;
        }
        let left = deadline.checked_duration_since(Instant::now())?;
        stream.set_read_timeout(Some(left.max(Duration::from_millis(1)))).ok()?;
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn forwarded_for(head: &str) -> Option<String> {
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("x-forwarded-for") {
                let first = v.split(',').next().unwrap_or("").trim();
                if !first.is_empty() {
                    return Some(first.chars().take(64).collect());
                }
            }
        }
    }
    None
}

fn handle(mut stream: TcpStream, sh: &Shared) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let Some(raw) = read_head(&mut stream) else {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };
    let head = String::from_utf8_lossy(&raw);
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let route = share::route(method, target, &sh.token);
    let head_only = method == "HEAD";
    let (status, ctype, body): (&str, &str, Vec<u8>) = match route {
        Route::Page => ("200 OK", "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        Route::HlsJs => ("200 OK", "application/javascript", HLS_JS.to_vec()),
        Route::Status => {
            let (ch, live) = sh.status.lock().map(|s| s.clone()).unwrap_or_default();
            ("200 OK", "application/json", share::status_json(&ch, live).into_bytes())
        }
        Route::Playlist => match std::fs::read(sh.dir.join("live.m3u8")) {
            Ok(b) => {
                let who = forwarded_for(&head).unwrap_or_else(|| {
                    stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default()
                });
                if let Ok(mut v) = sh.viewers.lock() {
                    v.record(&who, Instant::now());
                }
                ("200 OK", "application/vnd.apple.mpegurl", b)
            }
            Err(_) => not_found(),
        },
        Route::Segment(name) => match std::fs::read(sh.dir.join(&name)) {
            Ok(b) => ("200 OK", "video/mp2t", b),
            Err(_) => not_found(),
        },
        Route::NotFound => not_found(),
    };
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(&body);
    }
    let _ = stream.write_all(&out);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn not_found() -> (&'static str, &'static str, Vec<u8>) {
    ("404 Not Found", "text/plain", Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    const T: &str = "0123456789abcdef0123456789abcdef";

    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("iptv-share-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn raw(port: u16, req: &[u8]) -> Vec<u8> {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let _ = s.write_all(req);
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        out
    }

    fn get(port: u16, path: &str) -> (String, Vec<u8>) {
        request(port, "GET", path, "")
    }

    fn request(port: u16, method: &str, path: &str, extra: &str) -> (String, Vec<u8>) {
        let req = format!("{method} {path} HTTP/1.1\r\nHost: x\r\n{extra}\r\n");
        let out = raw(port, req.as_bytes());
        match out.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(i) => (
                String::from_utf8_lossy(&out[..i]).into_owned(),
                out[i + 4..].to_vec(),
            ),
            None => (String::from_utf8_lossy(&out).into_owned(), Vec::new()),
        }
    }

    fn start(name: &str) -> (Server, u16, PathBuf) {
        let port = free_port();
        let dir = scratch(name);
        let s = Server::start(port, T.to_string(), dir.clone()).unwrap();
        (s, port, dir)
    }

    fn header_has(head: &str, h: &str) -> bool {
        head.to_ascii_lowercase().contains(&h.to_ascii_lowercase())
    }

    #[test]
    fn serves_page_playlist_segment_and_status() {
        let (s, port, dir) = start("serve");
        std::fs::write(dir.join("live.m3u8"), "#EXTM3U\n").unwrap();
        std::fs::write(dir.join("seg-5.ts"), [1u8, 2, 3]).unwrap();
        std::fs::write(dir.join("seg-123456789.ts"), [9u8]).unwrap();
        s.set_status("News 1", true);

        let (h, b) = get(port, &format!("/{T}/"));
        assert!(h.starts_with("HTTP/1.1 200"), "{h}");
        assert!(header_has(&h, "content-type: text/html; charset=utf-8"));
        assert!(header_has(&h, "cache-control: no-store"));
        assert!(header_has(&h, "x-content-type-options: nosniff"));
        assert!(header_has(&h, "referrer-policy: no-referrer"));
        assert!(header_has(&h, "connection: close"));
        assert!(String::from_utf8_lossy(&b).contains("<video"));

        let (h, b) = get(port, &format!("/{T}/hls.js"));
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(header_has(&h, "content-type: application/javascript"));
        assert!(b.len() > 100_000);

        let (h, b) = get(port, &format!("/{T}/live.m3u8"));
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(header_has(&h, "content-type: application/vnd.apple.mpegurl"));
        assert!(header_has(&h, "cache-control: no-store"));
        assert_eq!(b, b"#EXTM3U\n");

        let (h, b) = get(port, &format!("/{T}/seg-5.ts"));
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(header_has(&h, "content-type: video/mp2t"));
        assert_eq!(b, vec![1, 2, 3]);

        let (h, b) = get(port, &format!("/{T}/seg-123456789.ts"));
        assert!(h.starts_with("HTTP/1.1 200"));
        assert_eq!(b, vec![9]);

        let (h, b) = get(port, &format!("/{T}/status"));
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(header_has(&h, "application/json"));
        assert_eq!(b, br#"{"channel":"News 1","live":true}"#);

        let (h, b) = request(port, "HEAD", &format!("/{T}/live.m3u8"), "");
        assert!(h.starts_with("HTTP/1.1 200"));
        assert!(header_has(&h, "content-length: 8"));
        assert!(b.is_empty());
        s.stop();
    }

    #[test]
    fn wrong_token_and_unknown_path_are_empty_404s() {
        let (s, port, _d) = start("notfound");
        for p in [
            "/ffffffffffffffffffffffffffffffff/".to_string(),
            "/".to_string(),
            format!("/{T}/nope"),
            format!("/{T}/seg-1.ts"),
        ] {
            let (h, b) = get(port, &p);
            assert!(h.starts_with("HTTP/1.1 404"), "{p}: {h}");
            assert!(b.is_empty());
            assert!(header_has(&h, "cache-control: no-store"));
        }
        s.stop();
    }

    #[test]
    fn traversal_attempts_are_404() {
        let (s, port, dir) = start("trav");
        std::fs::write(dir.parent().unwrap().join("secret.txt"), "x").unwrap();
        for p in [
            format!("/{T}/seg-1.ts/../../x"),
            format!("/{T}/%2e%2e/secret.txt"),
            format!("/{T}/..%2fsecret.txt"),
            format!("/{T}/..\\secret.txt"),
            format!("/{T}/../secret.txt"),
        ] {
            let (h, b) = get(port, &p);
            assert!(h.starts_with("HTTP/1.1 404"), "{h}");
            assert!(b.is_empty());
        }
        let _ = std::fs::remove_file(dir.parent().unwrap().join("secret.txt"));
        s.stop();
    }

    #[test]
    fn missing_playlist_is_404_not_5xx() {
        let (s, port, _d) = start("noplaylist");
        let (h, b) = get(port, &format!("/{T}/live.m3u8"));
        assert!(h.starts_with("HTTP/1.1 404"), "{h}");
        assert!(b.is_empty());
        s.stop();
    }

    #[test]
    fn post_is_404() {
        let (s, port, _d) = start("post");
        let (h, _) = request(port, "POST", &format!("/{T}/"), "Content-Length: 0\r\n");
        assert!(h.starts_with("HTTP/1.1 404"), "{h}");
        s.stop();
    }

    #[test]
    fn oversized_request_line_is_closed() {
        let (s, port, _d) = start("big");
        let mut req = b"GET /".to_vec();
        req.extend(std::iter::repeat(b'a').take(20_000));
        let t = Instant::now();
        let out = raw(port, &req);
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(!String::from_utf8_lossy(&out).contains("200 OK"));
        s.stop();
    }

    #[test]
    fn slow_client_is_dropped() {
        let (s, port, _d) = start("slow");
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(12))).unwrap();
        let t = Instant::now();
        let mut buf = [0u8; 16];
        let n = c.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0);
        assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
        s.stop();
    }

    #[test]
    fn port_in_use_is_a_clear_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let e = Server::start(port, T.to_string(), scratch("inuse")).err().unwrap();
        assert!(e.to_string().contains("in use"), "{e}");
    }

    #[test]
    fn invalid_token_is_refused() {
        for t in ["", "abc", "ABCDEF0123456789abcdef0123456789"] {
            assert!(Server::start(free_port(), t.to_string(), scratch("badtok")).is_err());
        }
    }

    #[test]
    fn viewers_count_from_forwarded_for() {
        let (s, port, dir) = start("viewers");
        std::fs::write(dir.join("live.m3u8"), "#EXTM3U\n").unwrap();
        assert_eq!(s.viewers(), 0);
        // Page and status requests do not count.
        get(port, &format!("/{T}/"));
        get(port, &format!("/{T}/status"));
        assert_eq!(s.viewers(), 0);
        request(port, "GET", &format!("/{T}/live.m3u8"), "X-Forwarded-For: 203.0.113.5, 10.0.0.1\r\n");
        request(port, "GET", &format!("/{T}/live.m3u8"), "X-Forwarded-For: 203.0.113.5\r\n");
        assert_eq!(s.viewers(), 1);
        request(port, "GET", &format!("/{T}/live.m3u8"), "X-Forwarded-For: 198.51.100.7\r\n");
        assert_eq!(s.viewers(), 2);
        request(port, "GET", &format!("/{T}/live.m3u8"), "");
        assert_eq!(s.viewers(), 3);
        s.stop();
    }

    #[test]
    fn stop_releases_the_port() {
        let (s, port, _d) = start("stop");
        let t = Instant::now();
        s.stop();
        assert!(t.elapsed() < Duration::from_secs(2));
        TcpListener::bind(("127.0.0.1", port)).expect("port free after stop");
    }

    #[test]
    fn stop_is_bounded_and_drop_releases_the_port() {
        let (s, port, _d) = start("bounded");
        // An idle client holding a connection must not delay stop.
        let _c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let t = Instant::now();
        s.stop();
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        let (s2, port2, _d2) = start("dropped");
        drop(s2);
        TcpListener::bind(("127.0.0.1", port2)).expect("port free after drop");
    }

    #[test]
    fn tap_overlay_is_above_waiting_and_hides_it() {
        let page = include_str!("../assets/share.html");
        let z = |sel: &str| -> u32 {
            let i = page.find(sel).expect(sel);
            let rest = &page[i..page[i..].find('}').map(|e| i + e).unwrap()];
            let k = rest.find("z-index:").expect("z-index");
            rest[k + 8..].trim_start().chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap()
        };
        assert!(z("#tap {") > z("#wait {"));
        assert!(page.contains("if (on) { wait.hidden = true; }"));
        assert!(page.contains("if (on) { tap.hidden = true; }"));
    }

    #[test]
    fn page_makes_no_external_requests() {
        let page = include_str!("../assets/share.html");
        assert!(!page.contains("http://"));
        assert!(!page.contains("https://"));
        assert!(!page.contains("//cdn"));
    }

    #[test]
    fn vendored_hls_is_javascript() {
        let js = include_bytes!("../assets/hls.min.js");
        assert!(js.len() > 100_000);
        assert!(js.starts_with(b"!function"));
    }
}
