//! Playlist download and background refresh.
//!
//! Credentials safety: every error string produced here is scrubbed (the raw URL is
//! replaced by its `redact_url` form and reqwest's own URL echo is stripped).

use crate::{cache, library::Library, playlist, redact::redact_url};
use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const SAVE_ATTEMPTS: u32 = 3;
const PROGRESS_STEP: u64 = 256 * 1024;

/// Events emitted by a [`Refresher`].
#[derive(Debug, Clone)]
pub enum RefreshEvent {
    /// Bytes downloaded so far.
    Progress(u64),
    /// Refresh finished; the new library is ready (terminal event).
    Done(Arc<Library>),
    /// Non-fatal problem (e.g. the cache could not be saved after retries). Always
    /// followed by `Done` with the freshly parsed data. Message is redacted.
    Warning(String),
    /// Refresh failed; old cache/data untouched (terminal event). Message is redacted.
    Failed(String),
}

/// Replace the raw URL in `msg` with its redacted form.
fn scrub(msg: &str, url: &str) -> String {
    let mut out = msg.replace(url, &redact_url(url));
    // Also catch a URL as reqwest/url may re-serialise it (e.g. trailing slash added).
    let trimmed = url.trim_end_matches('/');
    if !trimmed.is_empty() {
        out = out.replace(trimmed, &redact_url(trimmed));
    }
    out
}

fn unique_tmp(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(
        ".{}.{}.part",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    dest.with_file_name(name)
}

/// Download `url` to `dest` (atomically: temp file + rename, only if complete).
pub fn download(url: &str, dest: &Path) -> Result<()> {
    download_with_progress(url, dest, &mut |_| {})
}

pub(crate) fn download_with_progress(url: &str, dest: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
    let tmp = unique_tmp(dest);
    let r = download_inner(url, dest, &tmp, progress);
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.map_err(|e| anyhow!("{}", scrub(&format!("{e:#}"), url)))
}

fn download_inner(url: &str, dest: &Path, tmp: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
    // reqwest errors embed the URL; strip it, the caller scrubs whatever remains.
    let net = |e: reqwest::Error| anyhow!("{}", e.without_url());
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| anyhow!("creating download dir: {e}"))?;
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // reqwest::blocking has no per-read timeout; use a generous overall cap instead.
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(net)?;
    let mut resp = client.get(url).send().map_err(net)?;
    if !resp.status().is_success() {
        return Err(anyhow!("server returned HTTP {}", resp.status()));
    }
    let expected = resp.content_length();
    let mut f = std::fs::File::create(tmp).map_err(|e| anyhow!("creating temp file: {e}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    let mut reported = 0u64;
    loop {
        let n = resp.read(&mut buf).map_err(|e| anyhow!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).map_err(|e| anyhow!("writing temp file: {e}"))?;
        total += n as u64;
        if total - reported >= PROGRESS_STEP {
            reported = total;
            progress(total);
        }
    }
    if let Some(exp) = expected {
        if exp != total {
            return Err(anyhow!("download incomplete: got {total} of {exp} bytes"));
        }
    }
    f.flush().map_err(|e| anyhow!("writing temp file: {e}"))?;
    f.sync_all().map_err(|e| anyhow!("syncing temp file: {e}"))?;
    drop(f);
    progress(total);
    std::fs::rename(tmp, dest).map_err(|e| anyhow!("moving download into place: {e}"))?;
    Ok(())
}

/// Load the on-disk cache and build the library. Safe to call from a background thread.
pub fn load_cached(dir: &Path) -> Option<(Arc<Library>, SystemTime)> {
    let c = cache::load(dir)?;
    Some((Arc::new(Library::build(c.entries)), c.saved_at))
}

type Saver = Arc<dyn Fn(&Path, &[playlist::Entry]) -> Result<()> + Send + Sync>;

fn running_dirs() -> &'static Mutex<HashSet<PathBuf>> {
    static R: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

struct RunGuard(PathBuf);
impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Ok(mut s) = running_dirs().lock() {
            s.remove(&self.0);
        }
    }
}

/// Background playlist refresh. Single saver: at most one refresh runs per data dir
/// at a time (`try_spawn` returns `None` while one is running; `spawn` then returns an
/// inert Refresher that never yields events).
pub struct Refresher {
    rx: Receiver<RefreshEvent>,
}

impl Refresher {
    pub fn spawn(url: String, dir: PathBuf) -> Refresher {
        Self::try_spawn(url, dir).unwrap_or_else(|| Refresher { rx: channel().1 })
    }

    pub fn try_spawn(url: String, dir: PathBuf) -> Option<Refresher> {
        Self::try_spawn_with(url, dir, Arc::new(|d, e| cache::save(d, e)), Duration::from_millis(200))
    }

    pub(crate) fn try_spawn_with(url: String, dir: PathBuf, saver: Saver, retry_delay: Duration) -> Option<Refresher> {
        {
            let mut s = running_dirs().lock().ok()?;
            if !s.insert(dir.clone()) {
                return None;
            }
        }
        let guard = RunGuard(dir.clone());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _guard = guard;
            let ev = run(&url, &dir, &saver, retry_delay, &tx);
            let _ = tx.send(ev);
        });
        Some(Refresher { rx })
    }

    /// Non-blocking: next pending event, if any.
    pub fn poll(&self) -> Option<RefreshEvent> {
        match self.rx.try_recv() {
            Ok(e) => Some(e),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

impl std::fmt::Debug for Refresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Refresher")
    }
}

fn run(
    url: &str,
    dir: &Path,
    saver: &Saver,
    retry_delay: Duration,
    tx: &std::sync::mpsc::Sender<RefreshEvent>,
) -> RefreshEvent {
    let m3u = dir.join("cache").join("playlist.m3u.tmp");
    let res = (|| -> Result<RefreshEvent> {
        download_with_progress(url, &m3u, &mut |n| {
            let _ = tx.send(RefreshEvent::Progress(n));
        })?;
        let f = std::fs::File::open(&m3u).map_err(|e| anyhow!("opening downloaded playlist: {e}"))?;
        let entries = playlist::parse(BufReader::new(f));
        if entries.is_empty() {
            return Err(anyhow!("downloaded playlist contained no entries"));
        }
        let mut save_err = None;
        for attempt in 0..SAVE_ATTEMPTS {
            match saver(dir, &entries) {
                Ok(()) => {
                    save_err = None;
                    break;
                }
                Err(e) => {
                    save_err = Some(e);
                    if attempt + 1 < SAVE_ATTEMPTS {
                        std::thread::sleep(retry_delay);
                    }
                }
            }
        }
        if let Some(e) = save_err {
            let _ = tx.send(RefreshEvent::Warning(scrub(
                &format!("could not save playlist cache: {e:#}"),
                url,
            )));
        }
        Ok(RefreshEvent::Done(Arc::new(Library::build(entries))))
    })();
    let _ = std::fs::remove_file(&m3u);
    match res {
        Ok(ev) => ev,
        Err(e) => RefreshEvent::Failed(scrub(&format!("{e:#}"), url)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const M3U: &str = "#EXTM3U\n#EXTINF:-1 tvg-id=\"a\" group-title=\"G\",Chan A\nhttp://example.invalid/live/1\n#EXTINF:-1 tvg-id=\"b\" group-title=\"G\",Chan B\nhttp://example.invalid/live/2\n";

    /// Raw server on 127.0.0.1; `respond` writes whatever it wants per connection.
    fn raw_server(respond: impl Fn(&mut std::net::TcpStream) + Send + 'static) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b);
                respond(&mut s);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn ok_server(body: &'static str) -> String {
        raw_server(move |s| {
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        })
    }

    fn wait(r: &Refresher) -> (Vec<RefreshEvent>, RefreshEvent) {
        let mut seen = vec![];
        for _ in 0..1000 {
            while let Some(e) = r.poll() {
                if matches!(e, RefreshEvent::Done(_) | RefreshEvent::Failed(_)) {
                    return (seen, e);
                }
                seen.push(e);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out");
    }

    fn old_cache(dir: &Path) -> Vec<playlist::Entry> {
        let e = playlist::parse(BufReader::new(
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"o\" group-title=\"Old\",Old Chan\nhttp://example.invalid/live/9\n".as_bytes(),
        ));
        cache::save(dir, &e).unwrap();
        e
    }

    fn no_leftovers(dir: &Path) {
        for e in std::fs::read_dir(dir.join("cache")).unwrap() {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            assert!(n == "playlist.bin", "leftover {n}");
        }
    }

    #[test]
    fn refresh_success_writes_cache_and_emits_done() {
        let base = ok_server(M3U);
        let d = tempfile::tempdir().unwrap();
        let r = Refresher::spawn(format!("{base}/get.m3u"), d.path().to_path_buf());
        let (_, ev) = wait(&r);
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
        assert_eq!(cache::load(d.path()).unwrap().entries.len(), 2);
        no_leftovers(d.path());
        assert!(load_cached(d.path()).is_some());
    }

    #[test]
    fn interrupted_download_emits_failed_and_keeps_old_cache() {
        let base = raw_server(|s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n#EXTM3U\n#EXTINF:-1,Half");
        });
        let d = tempfile::tempdir().unwrap();
        let old = old_cache(d.path());
        let r = Refresher::spawn(format!("{base}/SECRETUSER/SECRETPASS/get.m3u"), d.path().to_path_buf());
        let (_, ev) = wait(&r);
        let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
        assert!(!m.contains("SECRETUSER") && !m.contains("SECRETPASS"), "{m}");
        assert_eq!(cache::load(d.path()).unwrap().entries, old);
        no_leftovers(d.path());
    }

    #[test]
    fn error_message_contains_no_credentials() {
        let b500 = raw_server(|s| {
            let _ = s.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let b404 = raw_server(|s| {
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let closed = raw_server(|_| {}); // closes without answering
        let refused = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            format!("http://127.0.0.1:{p}")
        };
        for base in [b500, b404, closed, refused] {
            for url in [
                format!("{base}/SECRETUSER/SECRETPASS/get.m3u"),
                format!("{base}/get.php?username=SECRETUSER&password=SECRETPASS"),
            ] {
                let d = tempfile::tempdir().unwrap();
                let r = Refresher::spawn(url.clone(), d.path().to_path_buf());
                let (_, ev) = wait(&r);
                let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
                assert!(!m.contains("SECRETUSER") && !m.contains("SECRETPASS"), "{m}");
                let e = download(&url, &d.path().join("x.m3u")).unwrap_err();
                let s = format!("{e:#} {e:?}");
                assert!(!s.contains("SECRETUSER") && !s.contains("SECRETPASS"), "{s}");
            }
        }
    }

    #[test]
    fn save_failure_still_yields_done_with_warning() {
        let base = ok_server(M3U);
        let d = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let c2 = calls.clone();
        let saver: Saver = Arc::new(move |_, _| {
            c2.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("rename failed for http://x/SECRETUSER/SECRETPASS/1"))
        });
        let url = format!("{base}/get.m3u");
        let r = Refresher::try_spawn_with(url, d.path().to_path_buf(), saver, Duration::from_millis(1)).unwrap();
        let (seen, ev) = wait(&r);
        assert!(matches!(ev, RefreshEvent::Done(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(seen.iter().any(|e| matches!(e, RefreshEvent::Warning(_))));
    }

    #[test]
    fn second_spawn_while_running_is_noop() {
        let base = raw_server(|s| {
            std::thread::sleep(Duration::from_millis(500));
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", M3U.len(), M3U);
        });
        let d = tempfile::tempdir().unwrap();
        let r = Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf()).unwrap();
        assert!(Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf()).is_none());
        let (_, ev) = wait(&r);
        assert!(matches!(ev, RefreshEvent::Done(_)));
        std::thread::sleep(Duration::from_millis(100));
        assert!(Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf()).is_some());
    }
}
