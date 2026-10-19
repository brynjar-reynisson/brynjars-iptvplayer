//! Playlist download and background refresh.
//!
//! Credentials safety: every error string produced here is scrubbed (the raw URL is
//! replaced by its `redact_url` form and reqwest's own URL echo is stripped).

use crate::config::DEFAULT_USER_AGENT;
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
    if url.is_empty() {
        return msg.to_string();
    }
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

/// `ua` trimmed, or [`DEFAULT_USER_AGENT`] when blank.
fn effective_user_agent(ua: &str) -> &str {
    let t = ua.trim();
    if t.is_empty() {
        DEFAULT_USER_AGENT
    } else {
        t
    }
}

/// Download `url` to `dest` (atomically: temp file + rename, only if complete),
/// sending [`DEFAULT_USER_AGENT`].
pub fn download(url: &str, dest: &Path) -> Result<()> {
    download_with_progress(url, dest, DEFAULT_USER_AGENT, &mut |_| {})
}

pub(crate) fn download_with_progress(
    url: &str,
    dest: &Path,
    user_agent: &str,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    let tmp = unique_tmp(dest);
    let r = download_inner(url, dest, &tmp, effective_user_agent(user_agent), progress);
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.map_err(|e| anyhow!("{}", scrub(&format!("{e:#}"), url)))
}

/// Text for a non-success playlist status. A status without a standard
/// reason (e.g. 454) is how some providers block unknown clients, so hint at
/// the User-Agent.
fn status_error(status: reqwest::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("server returned HTTP {} {reason}", status.as_u16()),
        None => format!("server returned HTTP {} (blocked? check user_agent in config.toml)", status.as_u16()),
    }
}

fn download_inner(url: &str, dest: &Path, tmp: &Path, user_agent: &str, progress: &mut dyn FnMut(u64)) -> Result<()> {
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
    // Client-level User-Agent: also sent on followed (same-host) redirects.
    let client = reqwest::blocking::Client::builder()
        .user_agent(user_agent)
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
        return Err(anyhow!("{}", status_error(resp.status())));
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
pub fn load_cached(dir: &Path, top: &[String]) -> Option<(Arc<Library>, SystemTime)> {
    let c = cache::load(dir)?;
    Some((Arc::new(Library::build_with_top(c.entries, top)), c.saved_at))
}

/// Where a refresh reads the playlist from.
#[derive(Clone)]
pub enum Source {
    /// Download over HTTP(S) (credentials in the URL; never printed).
    Url(String),
    /// Parse a local `.m3u` file (dev/testing: `--playlist-file`).
    File(PathBuf),
}

/// Manual Debug: the URL is redacted.
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Url(u) => write!(f, "Url({})", redact_url(u)),
            Source::File(p) => write!(f, "File({})", p.display()),
        }
    }
}

/// Options for a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOptions {
    /// Skip ONLY the "much smaller than the cached playlist" safety check
    /// (the user confirmed the shrink is real). Empty playlists still fail.
    pub force: bool,
    /// User-Agent for the download (blank = [`DEFAULT_USER_AGENT`]); unused
    /// for [`Source::File`].
    pub user_agent: String,
    /// Live countries listed first in the resulting library (see
    /// [`Library::build_with_top`]).
    pub top_countries: Vec<String>,
}

impl Default for RefreshOptions {
    fn default() -> Self {
        RefreshOptions { force: false, user_agent: DEFAULT_USER_AGENT.to_string(), top_countries: Vec::new() }
    }
}

const SHRINK_MARKER: &str = "much smaller than the cached one";

/// True if a `RefreshEvent::Failed` message is the shrink safety refusal,
/// i.e. a forced refresh would replace the cache.
pub fn is_shrink_refusal(msg: &str) -> bool {
    msg.contains(SHRINK_MARKER)
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
    /// Download `url` (sending `user_agent`; blank = default) into `dir`'s cache.
    pub fn spawn(url: String, dir: PathBuf, user_agent: &str) -> Refresher {
        Self::spawn_source(Source::Url(url), dir, RefreshOptions { force: false, user_agent: user_agent.into(), ..Default::default() })
    }

    /// Like [`Refresher::spawn`] but replaces the cache even when the new
    /// playlist is much smaller than the cached one.
    pub fn spawn_forced(url: String, dir: PathBuf, user_agent: &str) -> Refresher {
        Self::spawn_source(Source::Url(url), dir, RefreshOptions { force: true, user_agent: user_agent.into(), ..Default::default() })
    }

    pub fn try_spawn(url: String, dir: PathBuf, user_agent: &str) -> Option<Refresher> {
        Self::try_spawn_source(Source::Url(url), dir, RefreshOptions { force: false, user_agent: user_agent.into(), ..Default::default() })
    }

    /// General form: any [`Source`], with [`RefreshOptions`].
    pub fn spawn_source(source: Source, dir: PathBuf, opts: RefreshOptions) -> Refresher {
        Self::try_spawn_source(source, dir, opts).unwrap_or_else(|| {
            let (tx, rx) = channel();
            let _ = tx.send(RefreshEvent::Failed("refresh already running".into()));
            Refresher { rx }
        })
    }

    pub fn try_spawn_source(source: Source, dir: PathBuf, opts: RefreshOptions) -> Option<Refresher> {
        Self::try_spawn_full(source, dir, opts, Arc::new(cache::save), Duration::from_millis(200))
    }

    #[cfg(test)]
    pub(crate) fn try_spawn_with(url: String, dir: PathBuf, saver: Saver, retry_delay: Duration) -> Option<Refresher> {
        Self::try_spawn_full(Source::Url(url), dir, RefreshOptions::default(), saver, retry_delay)
    }

    fn try_spawn_full(
        source: Source,
        dir: PathBuf,
        opts: RefreshOptions,
        saver: Saver,
        retry_delay: Duration,
    ) -> Option<Refresher> {
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
                run(&source, &opts, &dir, &saver, retry_delay, &tx)
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
    source: &Source,
    opts: &RefreshOptions,
    dir: &Path,
    saver: &Saver,
    retry_delay: Duration,
    tx: &std::sync::mpsc::Sender<RefreshEvent>,
) -> RefreshEvent {
    let m3u = dir.join("cache").join("playlist.m3u.tmp");
    sweep_stale(&dir.join("cache"));
    // Only a URL source has credentials to scrub.
    let url = match source {
        Source::Url(u) => u.as_str(),
        Source::File(_) => "",
    };
    let res = (|| -> Result<RefreshEvent> {
        let entries = match source {
            Source::Url(url) => {
                download_with_progress(url, &m3u, &opts.user_agent, &mut |n| {
                    let _ = tx.send(RefreshEvent::Progress(n));
                })?;
                let f = std::fs::File::open(&m3u).map_err(|e| anyhow!("opening downloaded playlist: {e}"))?;
                playlist::parse(BufReader::new(f))
            }
            Source::File(p) => {
                let f = std::fs::File::open(p).map_err(|e| anyhow!("opening playlist file: {}", e.kind()))?;
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                let entries = playlist::parse(BufReader::new(f));
                let _ = tx.send(RefreshEvent::Progress(len));
                entries
            }
        };
        if entries.is_empty() {
            return Err(anyhow!("downloaded playlist contained no entries"));
        }
        // A truncated body without Content-Length must not replace a good cache.
        if let (false, Some(old)) = (opts.force, cache::load(dir)) {
            if entries.len() * 2 < old.entries.len() {
                return Err(anyhow!(
                    "downloaded playlist is {SHRINK_MARKER} ({} vs {} entries); keeping the old data",
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
        Ok(RefreshEvent::Done(Arc::new(Library::build_with_top(entries, &opts.top_countries))))
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
    use crate::config::DEFAULT_USER_AGENT;
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
        let r = Refresher::spawn(format!("{base}/get.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT);
        let (_, ev) = wait(&r);
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
        assert_eq!(cache::load(d.path()).unwrap().entries.len(), 2);
        no_leftovers(d.path());
        assert!(load_cached(d.path(), &[]).is_some());
    }

    #[test]
    fn interrupted_download_emits_failed_and_keeps_old_cache() {
        let base = raw_server(|s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n#EXTM3U\n#EXTINF:-1,Half");
        });
        let d = tempfile::tempdir().unwrap();
        let old = old_cache(d.path());
        let r = Refresher::spawn(format!("{base}/SECRETUSER/SECRETPASS/get.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT);
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
                let r = Refresher::spawn(url.clone(), d.path().to_path_buf(), DEFAULT_USER_AGENT);
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
        let r = Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT).unwrap();
        assert!(Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT).is_none());
        let (_, ev) = wait(&r);
        assert!(matches!(ev, RefreshEvent::Done(_)));
        // guard is released before the terminal event: immediate respawn works
        assert!(Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT).is_some());
    }

    #[test]
    fn spawn_while_running_emits_failed_already_running() {
        let base = raw_server(|s| {
            std::thread::sleep(Duration::from_millis(500));
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", M3U.len(), M3U);
        });
        let d = tempfile::tempdir().unwrap();
        let r = Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT);
        let r2 = Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT);
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
        assert!(Refresher::try_spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT).is_some());
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
        let r = Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT);
        let (_, ev) = wait(&r);
        let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
        assert!(m.contains("2 vs 10 entries"), "{m}");
        assert_eq!(cache::load(d.path()).unwrap().entries, e);
        no_leftovers(d.path());
    }

    fn ten_entry_cache(dir: &Path) -> Vec<playlist::Entry> {
        let mut big = String::from("#EXTM3U\n");
        for i in 0..10 {
            big.push_str(&format!("#EXTINF:-1 tvg-id=\"i{i}\" group-title=\"G\",Chan {i}\nhttp://example.invalid/live/{i}\n"));
        }
        let e = playlist::parse(BufReader::new(big.as_bytes()));
        cache::save(dir, &e).unwrap();
        e
    }

    #[test]
    fn shrink_refusal_is_detectable() {
        let d = tempfile::tempdir().unwrap();
        ten_entry_cache(d.path());
        let base = ok_server(M3U);
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT));
        let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
        assert!(is_shrink_refusal(&m), "{m}");
        assert!(!is_shrink_refusal("server returned HTTP 500"));
    }

    #[test]
    fn forced_refresh_skips_only_the_size_check() {
        let d = tempfile::tempdir().unwrap();
        ten_entry_cache(d.path());
        let base = ok_server(M3U); // 2 entries < 50% of 10
        let r = Refresher::spawn_forced(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT);
        let (_, ev) = wait(&r);
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
        assert_eq!(cache::load(d.path()).unwrap().entries.len(), 2);
        no_leftovers(d.path());
        // Forced still fails on an empty playlist and keeps the cache.
        let empty = ok_server("#EXTM3U\n");
        let (_, ev) = wait(&Refresher::spawn_forced(format!("{empty}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT));
        assert!(matches!(ev, RefreshEvent::Failed(_)), "{ev:?}");
        assert_eq!(cache::load(d.path()).unwrap().entries.len(), 2);
    }

    #[test]
    fn file_source_parses_local_playlist_into_cache() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("local.m3u");
        std::fs::write(&src, M3U).unwrap();
        let r = Refresher::spawn_source(Source::File(src.clone()), d.path().to_path_buf(), RefreshOptions::default());
        let (seen, ev) = wait(&r);
        let RefreshEvent::Done(lib) = ev else { panic!("{ev:?}") };
        assert_eq!(lib.entry_count(), 2);
        assert!(seen.iter().any(|e| matches!(e, RefreshEvent::Progress(n) if *n == M3U.len() as u64)));
        assert_eq!(cache::load(d.path()).unwrap().entries.len(), 2);
        assert!(src.exists(), "source file must be left alone");
        // Missing file: Failed, cache untouched.
        let missing = d.path().join("nope.m3u");
        let (_, ev) = wait(&Refresher::spawn_source(Source::File(missing), d.path().to_path_buf(), RefreshOptions::default()));
        assert!(matches!(ev, RefreshEvent::Failed(_)), "{ev:?}");
        assert_eq!(cache::load(d.path()).unwrap().entries.len(), 2);
    }

    #[test]
    fn source_debug_redacts_url() {
        let s = format!("{:?}", Source::Url("http://h.invalid/SECRETUSER/SECRETPASS/get.m3u".into()));
        assert!(!s.contains("SECRETUSER") && !s.contains("SECRETPASS"), "{s}");
    }

    #[test]
    fn debug_of_done_does_not_dump_urls() {
        let d = tempfile::tempdir().unwrap();
        let base = ok_server(M3U);
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT));
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
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/SECRETUSER/SECRETPASS/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT));
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
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT));
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
    }

    /// Like the real provider: HTTP 454 when the request has no User-Agent,
    /// else 200 with the playlist. Records every User-Agent it sees.
    fn ua_server(seen: Arc<Mutex<Vec<String>>>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut b = [0u8; 4096];
                let n = s.read(&mut b).unwrap_or(0);
                let req = String::from_utf8_lossy(&b[..n]).into_owned();
                let ua = req
                    .lines()
                    .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("user-agent")))
                    .map(|(_, v)| v.trim().to_string());
                match ua {
                    Some(ua) => {
                        seen.lock().unwrap().push(ua);
                        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", M3U.len(), M3U);
                    }
                    None => {
                        let _ = s.write_all(b"HTTP/1.1 454 \r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn ua_server_rejects_requests_without_user_agent() {
        // The old behaviour (no User-Agent header) is what the provider blocks.
        let base = ua_server(Arc::new(Mutex::new(vec![])));
        let addr = base.trim_start_matches("http://");
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(b"GET /a.m3u HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        assert!(resp.starts_with("HTTP/1.1 454"), "{resp}");
    }

    #[test]
    fn download_sends_default_user_agent() {
        let seen = Arc::new(Mutex::new(vec![]));
        let base = ua_server(seen.clone());
        let d = tempfile::tempdir().unwrap();
        download(&format!("{base}/USER/PASS/a.m3u"), &d.path().join("p.m3u")).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![DEFAULT_USER_AGENT.to_string()]);
    }

    #[test]
    fn refresher_sends_configured_user_agent_also_after_redirect() {
        let seen = Arc::new(Mutex::new(vec![]));
        let target = ua_server(seen.clone());
        let base = raw_server(move |s| {
            let _ = write!(s, "HTTP/1.1 302 Found\r\nLocation: {target}/b.m3u\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let d = tempfile::tempdir().unwrap();
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), "Custom/1.0"));
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
        assert_eq!(*seen.lock().unwrap(), vec!["Custom/1.0".to_string()]);
        // Blank falls back to the default.
        let d = tempfile::tempdir().unwrap();
        let (_, ev) = wait(&Refresher::spawn_forced(format!("{base}/a.m3u"), d.path().to_path_buf(), "  "));
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
        assert_eq!(seen.lock().unwrap().last().unwrap(), DEFAULT_USER_AGENT);
    }

    #[test]
    fn unknown_status_hints_at_user_agent() {
        let base = raw_server(|s| {
            let _ = s.write_all(b"HTTP/1.1 454 \r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let d = tempfile::tempdir().unwrap();
        let url = format!("{base}/SECRETUSER/SECRETPASS/a.m3u");
        let (_, ev) = wait(&Refresher::spawn(url, d.path().to_path_buf(), DEFAULT_USER_AGENT));
        let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
        assert_eq!(m, "server returned HTTP 454 (blocked? check user_agent in config.toml)");
        // Known statuses keep their reason and get no hint.
        let base = raw_server(|s| {
            let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let d = tempfile::tempdir().unwrap();
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/SECRETUSER/SECRETPASS/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT));
        let RefreshEvent::Failed(m) = ev else { panic!("{ev:?}") };
        assert_eq!(m, "server returned HTTP 404 Not Found");
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
        let (_, ev) = wait(&Refresher::spawn(format!("{base}/a.m3u"), d.path().to_path_buf(), DEFAULT_USER_AGENT));
        assert!(matches!(ev, RefreshEvent::Done(_)), "{ev:?}");
        assert!(!c.join("playlist.m3u.tmp").exists() && !c.join("playlist.m3u.tmp.1.2.part").exists());
        assert!(c.join("keep.txt").exists());
    }
}
