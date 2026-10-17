//! Playlist download and background refresh.
//!
//! Credentials safety: every error string produced here is scrubbed (the raw URL is
//! replaced by its `redact_url` form and reqwest's own URL echo is stripped).

use crate::{cache, library::Library, playlist, redact::redact_url};
use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const SAVE_ATTEMPTS: u32 = 3;
const PROGRESS_STEP: u64 = 256 * 1024;

/// Events emitted by a [`Refresher`].
#[derive(Clone)]
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

/// Manual Debug: never dumps the Library (it would print every stream URL).
impl std::fmt::Debug for RefreshEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshEvent::Progress(n) => write!(f, "Progress({n})"),
            RefreshEvent::Done(l) => write!(f, "Done(<library: {} entries>)", l.entry_count()),
            RefreshEvent::Warning(m) => write!(f, "Warning({m:?})"),
            RefreshEvent::Failed(m) => write!(f, "Failed({m:?})"),
        }
    }
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
    crate::fsutil::unique_tmp_path(dest, "part")
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
    // Only follow redirects to the same host: path credentials must not reach a third party.
    let policy = reqwest::redirect::Policy::custom(|a| {
        let same = a.previous().last().and_then(|u| u.host_str()) == a.url().host_str();
        if a.previous().len() >= 10 {
            a.error("too many redirects")
        } else if same {
            a.follow()
        } else {
            a.error("blocked redirect to a different host")
        }
    });
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // Blocking reqwest has no per-read timeout, only an overall one. A stalled
        // connection can therefore hold the worker for up to this long (the UI stays
        // usable meanwhile), so keep it generous for large playlists.
        .timeout(Duration::from_secs(1800))
        .redirect(policy)
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
    crate::fsutil::rename_with_retry(tmp, dest, SAVE_ATTEMPTS, Duration::from_millis(100))
        .map_err(|e| anyhow!("moving download into place: {e}"))
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
/// at a time (`try_spawn` returns `None` while one is running; `spawn` then returns a
/// Refresher whose first event is `Failed("refresh already running")`).
pub struct Refresher {
    rx: Receiver<RefreshEvent>,
}

impl Refresher {
    pub fn spawn(url: String, dir: PathBuf) -> Refresher {
        Self::try_spawn(url, dir).unwrap_or_else(|| {
            let (tx, rx) = channel();
            let _ = tx.send(RefreshEvent::Failed("refresh already running".into()));
            Refresher { rx }
        })
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
            // Fixed message on panic: the payload could contain the URL.
            let ev = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&url, &dir, &saver, retry_delay, &tx)
            }))
            .unwrap_or_else(|_| RefreshEvent::Failed("internal error".into()));
            // Release the single-run guard before the terminal event so a UI reacting
            // to it can immediately spawn again.
            drop(guard);
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

/// Remove leftovers of a crashed run (only `*.part` and `playlist.m3u.tmp` in the cache dir).
fn sweep_stale(cache_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(cache_dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.ends_with(".part") || n == "playlist.m3u.tmp" {
            let _ = std::fs::remove_file(e.path());
        }
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
    sweep_stale(&dir.join("cache"));
    let res = (|| -> Result<RefreshEvent> {
        download_with_progress(url, &m3u, &mut |n| {
            let _ = tx.send(RefreshEvent::Progress(n));
        })?;
        let f = std::fs::File::open(&m3u).map_err(|e| anyhow!("opening downloaded playlist: {e}"))?;
        let entries = playlist::parse(BufReader::new(f));
        if entries.is_empty() {
            return Err(anyhow!("downloaded playlist contained no entries"));
        }
        // A truncated body without Content-Length must not replace a good cache.
        if let Some(old) = cache::load(dir) {
            if entries.len() * 2 < old.entries.len() {
                return Err(anyhow!(
                    "downloaded playlist is much smaller than the cached one ({} vs {} entries); keeping the old data",
                    entries.len(),
                    old.entries.len()
                ));
            }
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
    use std::sync::atomic::{AtomicU64, Ordering};

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
        // guard is released before the terminal event: immediate respawn works
        assert!(Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf()).is_some());
    }

    #[test]
    fn spawn_while_running_emits_failed_already_running() {
        let base = raw_server(|s| {
            std::thread::sleep(Duration::from_millis(500));
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", M3U.len(), M3U);
        });
        let d = tempfile::tempdir().unwrap();
        let r = Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf());
        let r2 = Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf());
        let (_, ev) = wait(&r2);
        assert!(matches!(&ev, RefreshEvent::Failed(m) if m == "refresh already running"), "{ev:?}");
        assert!(matches!(wait(&r).1, RefreshEvent::Done(_)));
    }

    #[test]
    fn worker_panic_yields_failed_internal_error() {
        let base = ok_server(M3U);
        let d = tempfile::tempdir().unwrap();
        let saver: Saver = Arc::new(|_, _| panic!("boom http://x/SECRETUSER/SECRETPASS/1"));
        let r = Refresher::try_spawn_with(format!("{base}/a.m3u"), d.path().to_path_buf(), saver, Duration::from_millis(1))
            .unwrap();
        let (_, ev) = wait(&r);
        assert!(matches!(&ev, RefreshEvent::Failed(m) if m == "internal error"), "{ev:?}");
        assert!(Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf()).is_some());
    }

    #[test]
    fn much_smaller_playlist_keeps_old_cache() {
        let d = tempfile::tempdir().unwrap();
        let mut big = String::from("#EXTM3U\n");
        for i in 0..10 {
            big.push_str(&format!("#EXTINF:-1 tvg-id=\"i{i}\" group-title=\"G\",Chan {i}\nhttp://example.invalid/live/{i}\n"));
        }
        let e = playlist::parse(BufReader::new(big.as_bytes()));
        assert_eq!(e.len(), 10);
        cache::save(d.path(), &e).unwrap();
        let base = ok_server(M3U); // 2 entries < 50% of 10
        let r = Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf());
        let (_, ev) = wait(&r);
        let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
        assert!(m.contains("2 vs 10 entries"), "{m}");
        assert_eq!(cache::load(d.path()).unwrap().entries, e);
        no_leftovers(d.path());
    }

    #[test]
    fn debug_of_done_does_not_dump_urls() {
        let d = tempfile::tempdir().unwrap();
        let base = ok_server(M3U);
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf()));
        let s = format!("{ev:?}");
        assert!(!s.contains("example.invalid") && s.contains("2 entries"), "{s}");
    }

    #[test]
    fn cross_host_redirect_is_blocked() {
        let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h2 = hit.clone();
        let target = raw_server(move |s| {
            h2.store(true, Ordering::SeqCst);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let target = target.replace("127.0.0.1", "localhost");
        let base = raw_server(move |s| {
            let _ = write!(s, "HTTP/1.1 302 Found\r\nLocation: {target}/SECRETUSER/SECRETPASS/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let d = tempfile::tempdir().unwrap();
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/SECRETUSER/SECRETPASS/a.m3u"), d.path().to_path_buf()));
        let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
        assert!(!m.contains("SECRETUSER") && !m.contains("SECRETPASS"), "{m}");
        assert!(!hit.load(Ordering::SeqCst), "redirect target was contacted");
    }

    #[test]
    fn same_host_redirect_is_followed() {
        let ok = ok_server(M3U);
        let base = raw_server(move |s| {
            let _ = write!(s, "HTTP/1.1 302 Found\r\nLocation: {ok}/b.m3u\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let d = tempfile::tempdir().unwrap();
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf()));
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
    }

    #[test]
    fn stale_part_files_are_swept() {
        let d = tempfile::tempdir().unwrap();
        old_cache(d.path());
        let c = d.path().join("cache");
        std::fs::write(c.join("playlist.m3u.tmp"), b"x").unwrap();
        std::fs::write(c.join("playlist.m3u.tmp.1.2.part"), b"x").unwrap();
        std::fs::write(c.join("keep.txt"), b"x").unwrap();
        let base = ok_server(M3U);
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf()));
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
        assert!(!c.join("playlist.m3u.tmp").exists() && !c.join("playlist.m3u.tmp.1.2.part").exists());
        assert!(c.join("keep.txt").exists());
    }
}
