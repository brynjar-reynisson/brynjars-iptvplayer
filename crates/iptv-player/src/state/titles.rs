//! Background loader for episode titles (TVmaze). One worker thread owns the
//! on-disk `TitleCache`, fetches one show at a time (300 ms apart) and sends
//! answers back; the UI only calls `request` / `poll` / `get`. Never prints URLs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iptv::episode_titles::{cache_key, clean_query, fetch_titles, Fetched, Lookup, TitleCache, Titles};
use iptv::series::UNSORTED;

/// Production TVmaze API base.
pub const TVMAZE_URL: &str = "https://api.tvmaze.com";
/// Pause between two network fetches.
const FETCH_GAP: Duration = Duration::from_millis(300);

type Answer = (String, Option<Titles>);

pub struct TitleLoader {
    tx: Sender<String>,
    rx: Receiver<Answer>,
    requested: HashSet<String>,
    answers: HashMap<String, Option<Arc<Titles>>>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl TitleLoader {
    pub fn new(data_dir: PathBuf, base_url: String, wake: impl Fn() + Send + 'static) -> TitleLoader {
        let (tx, req_rx) = channel::<String>();
        let (ans_tx, rx) = channel::<Answer>();
        let spawned = std::thread::Builder::new().name("episode-titles".into()).spawn(move || {
            let mut cache = TitleCache::load(&data_dir);
            let mut last_fetch: Option<Instant> = None;
            while let Ok(show) = req_rx.recv() {
                let q = clean_query(&show);
                let result = if q.name.trim().is_empty() {
                    None
                } else {
                    let key = cache_key(&q);
                    match cache.get(&key, now_secs()) {
                        Lookup::Hit(t) => Some(t),
                        Lookup::Miss => None,
                        Lookup::Unknown => {
                            if let Some(t) = last_fetch {
                                std::thread::sleep(FETCH_GAP.saturating_sub(t.elapsed()));
                            }
                            let fetched = fetch_titles(&base_url, &q);
                            last_fetch = Some(Instant::now());
                            let now = now_secs();
                            let out = match fetched {
                                Fetched::Found(t) => {
                                    cache.put_hit(&key, now, &t);
                                    Some(t)
                                }
                                Fetched::NotFound => {
                                    cache.put_miss(&key, now);
                                    None
                                }
                                Fetched::Failed(_) => {
                                    cache.put_error(&key, now);
                                    None
                                }
                            };
                            if let Err(e) = cache.save_if_dirty() {
                                eprintln!("episode titles: cannot save cache: {e:#}");
                            }
                            out
                        }
                    }
                };
                if ans_tx.send((show, result)).is_err() {
                    break;
                }
                wake();
            }
        });
        if let Err(e) = spawned {
            eprintln!("episode titles: cannot start worker: {e}");
        }
        TitleLoader { tx, rx, requested: HashSet::new(), answers: HashMap::new() }
    }

    /// Ask for a show's titles (keyed by the raw show name). Ignored for
    /// `(Unsorted)`, empty names and shows already requested this session.
    pub fn request(&mut self, show: &str) {
        if show == UNSORTED || show.trim().is_empty() || self.requested.contains(show) {
            return;
        }
        self.requested.insert(show.to_string());
        // A dead worker just means no titles.
        let _ = self.tx.send(show.to_string());
    }

    /// Shows whose answer arrived since the last call.
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok((show, t)) = self.rx.try_recv() {
            self.answers.insert(show.clone(), t.filter(|t| !t.is_empty()).map(Arc::new));
            out.push(show);
        }
        out
    }

    pub fn get(&self, show: &str) -> Option<Arc<Titles>> {
        self.answers.get(show).cloned().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    const JSON: &str = r#"{"name":"Fake Show","_embedded":{"episodes":[{"season":1,"number":2,"name":"The Visit"}]}}"#;

    fn server(status_line: &'static str, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut b = [0u8; 4096];
                let n = s.read(&mut b).unwrap_or(0);
                s2.lock().unwrap().push(String::from_utf8_lossy(&b[..n]).into_owned());
                let _ = write!(
                    s,
                    "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    fn wait_poll(l: &mut TitleLoader) -> Vec<String> {
        for _ in 0..200 {
            let p = l.poll();
            if !p.is_empty() {
                return p;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Vec::new()
    }

    #[test]
    fn loader_fetches_once_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let (base, seen) = server("200 OK", JSON);
        let mut l = TitleLoader::new(dir.path().to_path_buf(), base.clone(), || {});
        l.request("Fake Show");
        l.request("Fake Show");
        assert_eq!(wait_poll(&mut l), vec!["Fake Show".to_string()]);
        assert!(l.poll().is_empty());
        assert_eq!(l.get("Fake Show").unwrap().get(1, 2), Some("The Visit"));
        assert_eq!(seen.lock().unwrap().len(), 1);

        let mut l2 = TitleLoader::new(dir.path().to_path_buf(), base, || {});
        l2.request("Fake Show");
        assert_eq!(wait_poll(&mut l2), vec!["Fake Show".to_string()]);
        assert_eq!(l2.get("Fake Show").unwrap().get(1, 2), Some("The Visit"));
        assert_eq!(seen.lock().unwrap().len(), 1, "second loader answers from the cache");
    }

    #[test]
    fn loader_ignores_unsorted() {
        let dir = tempfile::tempdir().unwrap();
        let (base, seen) = server("200 OK", JSON);
        let mut l = TitleLoader::new(dir.path().to_path_buf(), base, || {});
        l.request(UNSORTED);
        l.request("  ");
        std::thread::sleep(Duration::from_millis(200));
        assert!(l.poll().is_empty());
        assert!(l.get(UNSORTED).is_none());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn loader_failure_gives_no_titles_and_no_retry_within_session() {
        let dir = tempfile::tempdir().unwrap();
        let (base, seen) = server("500 Internal Server Error", "");
        let mut l = TitleLoader::new(dir.path().to_path_buf(), base, || {});
        l.request("Fake Show");
        assert_eq!(wait_poll(&mut l), vec!["Fake Show".to_string()]);
        assert!(l.get("Fake Show").is_none());
        l.request("Fake Show");
        std::thread::sleep(Duration::from_millis(200));
        assert!(l.poll().is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
