//! Phone share: pure helpers (token, routes, link, status JSON, viewer count, ffmpeg arguments).
//! No I/O besides the OS random generator. Never log the token.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

pub const SEG_SECS: u32 = 2;
pub const LIST_SIZE: u32 = 8;
pub const VIEWER_WINDOW: Duration = Duration::from_secs(15);
const MAX_VIEWERS: usize = 64;
const MAX_TARGET: usize = 2048;
const MAX_CHANNEL_CHARS: usize = 100;

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(algorithm: *mut core::ffi::c_void, buffer: *mut u8, len: u32, flags: u32) -> i32;
}
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;

/// 128 random bits as 32 lowercase hex characters.
pub fn new_token() -> anyhow::Result<String> {
    let mut b = [0u8; 16];
    // SAFETY: valid buffer of 16 bytes; null algorithm handle with the system RNG flag.
    let status = unsafe {
        BCryptGenRandom(std::ptr::null_mut(), b.as_mut_ptr(), b.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    };
    if status != 0 {
        anyhow::bail!("could not generate a random token (status {status:#x})");
    }
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub fn is_valid_token(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Debug, PartialEq)]
pub enum Route {
    Page,
    HlsJs,
    Playlist,
    Segment(String),
    Status,
    NotFound,
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn route(method: &str, target: &str, token: &str) -> Route {
    if method != "GET" && method != "HEAD" {
        return Route::NotFound;
    }
    if target.len() > MAX_TARGET || !target.starts_with('/') {
        return Route::NotFound;
    }
    let path = target.split(['?', '#']).next().unwrap_or("");
    if path.contains('%') || path.contains('\\') || path.contains("..") {
        return Route::NotFound;
    }
    let rest = &path[1..];
    let Some((tok, tail)) = rest.split_once('/') else {
        return Route::NotFound;
    };
    if !ct_eq(tok.as_bytes(), token.as_bytes()) {
        return Route::NotFound;
    }
    match tail {
        "" => Route::Page,
        "hls.js" => Route::HlsJs,
        "live.m3u8" => Route::Playlist,
        "status" => Route::Status,
        t => {
            let digits = t.strip_prefix("seg-").and_then(|r| r.strip_suffix(".ts"));
            match digits {
                Some(d) if (1..=9).contains(&d.len()) && d.bytes().all(|c| c.is_ascii_digit()) => {
                    Route::Segment(t.to_string())
                }
                _ => Route::NotFound,
            }
        }
    }
}

pub fn link(host: Option<&str>, port: u16, token: &str) -> String {
    match host {
        Some(h) => format!("https://{h}/{token}/"),
        None => format!("http://localhost:{port}/{token}/"),
    }
}

pub fn status_json(channel: &str, live: bool) -> String {
    let mut s = String::from("{\"channel\":\"");
    for c in channel.chars().take(MAX_CHANNEL_CHARS) {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push_str(&format!("\",\"live\":{live}}}"));
    s
}

#[derive(Debug, Default)]
pub struct Viewers {
    seen: HashMap<String, Instant>,
}

impl Viewers {
    pub fn record(&mut self, who: &str, now: Instant) {
        self.expire(now);
        if !self.seen.contains_key(who) && self.seen.len() >= MAX_VIEWERS {
            // Drop the oldest to stay bounded.
            if let Some(old) = self.seen.iter().min_by_key(|(_, t)| **t).map(|(k, _)| k.clone()) {
                self.seen.remove(&old);
            }
        }
        self.seen.insert(who.to_string(), now);
    }

    pub fn count(&mut self, now: Instant) -> usize {
        self.expire(now);
        self.seen.len()
    }

    fn expire(&mut self, now: Instant) {
        self.seen.retain(|_, t| now.saturating_duration_since(*t) <= VIEWER_WINDOW);
    }
}

pub fn tap_url(tap_port: u16) -> String {
    format!("tcp://127.0.0.1:{tap_port}/tap.ts")
}

pub fn ffmpeg_args(tap_port: u16, dir: &Path, restart: bool) -> Vec<String> {
    let mut flags = String::from("delete_segments+omit_endlist");
    if restart {
        flags.push_str("+append_list+discont_start");
    }
    let seg = dir.join("seg-%d.ts");
    let list = dir.join("live.m3u8");
    let mut v: Vec<String> = Vec::new();
    for a in ["-hide_banner", "-loglevel", "warning", "-f", "mpegts", "-i"] {
        v.push(a.into());
    }
    v.push(format!("tcp://127.0.0.1:{tap_port}?listen=1&listen_timeout=30000"));
    for a in ["-c", "copy", "-f", "hls", "-hls_time"] {
        v.push(a.into());
    }
    v.push(SEG_SECS.to_string());
    v.push("-hls_list_size".into());
    v.push(LIST_SIZE.to_string());
    v.push("-hls_segment_filename".into());
    v.push(seg.to_string_lossy().into_owned());
    v.push("-hls_flags".into());
    v.push(flags);
    v.push(list.to_string_lossy().into_owned());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn token_is_32_lower_hex_and_unique() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert!(is_valid_token(&a));
        assert_ne!(a, b);
        assert!(!is_valid_token("ABCDEF0123456789abcdef0123456789"));
        assert!(!is_valid_token("abc"));
    }

    #[test]
    fn routes_accept_the_known_paths() {
        assert_eq!(route("GET", &format!("/{T}/"), T), Route::Page);
        assert_eq!(route("HEAD", &format!("/{T}/"), T), Route::Page);
        assert_eq!(route("GET", &format!("/{T}/?x=1#f"), T), Route::Page);
        assert_eq!(route("GET", &format!("/{T}/hls.js"), T), Route::HlsJs);
        assert_eq!(route("GET", &format!("/{T}/live.m3u8"), T), Route::Playlist);
        assert_eq!(route("GET", &format!("/{T}/status"), T), Route::Status);
        assert_eq!(
            route("GET", &format!("/{T}/seg-12.ts"), T),
            Route::Segment("seg-12.ts".into())
        );
    }

    #[test]
    fn routes_reject_everything_else() {
        let bad = [
            format!("/{T}/seg-../x"),
            format!("/{T}/%2e%2e/"),
            format!("/{T}/seg-1.ts/"),
            format!("/{T}/seg-1234567890.ts"),
            format!("/{T}/seg-.ts"),
            format!("/{T}/seg-a.ts"),
            format!("/{T}x/"),
            format!("/{T}"),
            "/ffffffffffffffffffffffffffffffff/".to_string(),
            "/".to_string(),
            "//".to_string(),
            format!("//{T}/"),
            format!("/{T}//status"),
            format!("/{T}\\status"),
            format!("/{T}/live.m3u8/x"),
            format!("/{}", "a".repeat(10_000)),
            "*".to_string(),
        ];
        for t in &bad {
            assert_eq!(route("GET", t, T), Route::NotFound, "{}", &t[..t.len().min(40)]);
        }
        assert_eq!(route("POST", &format!("/{T}/"), T), Route::NotFound);
        assert_eq!(route("get", &format!("/{T}/"), T), Route::NotFound);
    }

    #[test]
    fn link_with_and_without_host() {
        assert_eq!(link(Some("iptv.example.org"), 8480, T), format!("https://iptv.example.org/{T}/"));
        assert_eq!(link(None, 8480, T), format!("http://localhost:8480/{T}/"));
    }

    #[test]
    fn status_json_escapes_quotes_and_controls() {
        assert_eq!(
            status_json("A \"B\"\\\n\u{1}", true),
            "{\"channel\":\"A \\\"B\\\"\\\\\\n\\u0001\",\"live\":true}"
        );
        assert_eq!(status_json("x", false), "{\"channel\":\"x\",\"live\":false}");
        let long = "é".repeat(150);
        let j = status_json(&long, true);
        assert_eq!(j.matches('é').count(), 100);
    }

    #[test]
    fn viewers_count_distinct_within_window() {
        let t0 = Instant::now();
        let mut v = Viewers::default();
        assert_eq!(v.count(t0), 0);
        v.record("a", t0);
        v.record("a", t0 + Duration::from_secs(1));
        v.record("b", t0 + Duration::from_secs(2));
        assert_eq!(v.count(t0 + Duration::from_secs(3)), 2);
        // a last seen at 1 s, expires after 16 s; b at 2 s stays until 17 s.
        assert_eq!(v.count(t0 + Duration::from_secs(16) + Duration::from_millis(500)), 1);
        assert_eq!(v.count(t0 + Duration::from_secs(40)), 0);
    }

    #[test]
    fn viewers_are_bounded() {
        let t0 = Instant::now();
        let mut v = Viewers::default();
        for i in 0..200 {
            v.record(&format!("w{i}"), t0 + Duration::from_millis(i));
        }
        assert_eq!(v.count(t0 + Duration::from_secs(1)), 64);
    }

    #[test]
    fn ffmpeg_args_first_and_restart() {
        let dir = Path::new("D:/data/share");
        let a = ffmpeg_args(8481, dir, false);
        let s = a.join(" ");
        assert!(s.starts_with("-hide_banner -loglevel warning -f mpegts -i tcp://127.0.0.1:8481?listen=1&listen_timeout=30000 -c copy -f hls -hls_time 2 -hls_list_size 8 -hls_segment_filename "));
        assert!(a.contains(&"delete_segments+omit_endlist".to_string()));
        assert!(a.last().unwrap().ends_with("live.m3u8"));
        assert!(a.iter().any(|x| x.ends_with("seg-%d.ts")));
        let r = ffmpeg_args(8481, dir, true);
        assert!(r.contains(&"delete_segments+omit_endlist+append_list+discont_start".to_string()));
        assert_eq!(tap_url(8481), "tcp://127.0.0.1:8481/tap.ts");
    }
}
