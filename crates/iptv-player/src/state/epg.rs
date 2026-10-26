//! EPG spike: background loader for the programme guide. One worker thread
//! downloads the stream-id -> EPG-channel map and the XMLTV file at startup,
//! then answers per-channel `get_short_epg` requests. The UI only calls
//! `request_short` / `poll` / `xmltv_for` / `short_for`. Never prints URLs.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};

use iptv::epg::{now_and_next, Account, Programme};

/// How many upcoming programmes `get_short_epg` is asked for.
const SHORT_LIMIT: u32 = 6;

enum Reply {
    Guide { ids: HashMap<String, String>, xmltv: HashMap<String, Vec<Programme>> },
    GuideFailed(String),
    Short(String, Result<Vec<Programme>, String>),
}

/// State of the startup download, for the panel's footer line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuideState {
    Unavailable,
    Loading,
    Ready { channels: usize, programmes: usize },
    Failed(String),
}

pub struct EpgLoader {
    tx: Sender<String>,
    rx: Receiver<Reply>,
    state: GuideState,
    ids: HashMap<String, String>,
    xmltv: HashMap<String, Vec<Programme>>,
    requested: HashSet<String>,
    short: HashMap<String, Result<Vec<Programme>, String>>,
}

impl EpgLoader {
    /// `account` None (no readable config / not an Xtream URL) = no guide.
    pub fn new(account: Option<Account>, user_agent: String, wake: impl Fn() + Send + 'static) -> EpgLoader {
        let (tx, req_rx) = channel::<String>();
        let (ans_tx, rx) = channel::<Reply>();
        let state = if account.is_some() { GuideState::Loading } else { GuideState::Unavailable };
        if let Some(account) = account {
            let spawned = std::thread::Builder::new().name("epg".into()).spawn(move || {
                let t = std::time::Instant::now();
                let guide = account
                    .fetch_epg_ids(&user_agent)
                    .and_then(|ids| account.fetch_xmltv(&user_agent).map(|x| (ids, x)));
                let first = match guide {
                    Ok((ids, xmltv)) => {
                        eprintln!(
                            "epg: guide ready after {} ms ({} streams with an EPG id, {} XMLTV channels)",
                            t.elapsed().as_millis(),
                            ids.len(),
                            xmltv.len()
                        );
                        Reply::Guide { ids, xmltv }
                    }
                    Err(e) => {
                        eprintln!("epg: guide download failed: {e}");
                        Reply::GuideFailed(e)
                    }
                };
                if ans_tx.send(first).is_err() {
                    return;
                }
                wake();
                while let Ok(stream_id) = req_rx.recv() {
                    let r = account.fetch_short(&stream_id, SHORT_LIMIT, &user_agent);
                    if ans_tx.send(Reply::Short(stream_id, r)).is_err() {
                        break;
                    }
                    wake();
                }
            });
            if let Err(e) = spawned {
                eprintln!("epg: cannot start worker: {e}");
            }
        }
        EpgLoader {
            tx,
            rx,
            state,
            ids: HashMap::new(),
            xmltv: HashMap::new(),
            requested: HashSet::new(),
            short: HashMap::new(),
        }
    }

    pub fn state(&self) -> &GuideState {
        &self.state
    }

    /// Ask for a stream's short guide (once per session).
    pub fn request_short(&mut self, stream_id: &str) {
        if self.state == GuideState::Unavailable || !self.requested.insert(stream_id.to_string()) {
            return;
        }
        let _ = self.tx.send(stream_id.to_string());
    }

    /// Whether anything arrived (the UI repaints).
    pub fn poll(&mut self) -> bool {
        let mut any = false;
        while let Ok(r) = self.rx.try_recv() {
            any = true;
            match r {
                Reply::Guide { ids, xmltv } => {
                    self.state = GuideState::Ready { channels: xmltv.len(), programmes: xmltv.values().map(Vec::len).sum() };
                    self.ids = ids;
                    self.xmltv = xmltv;
                }
                Reply::GuideFailed(e) => self.state = GuideState::Failed(e),
                Reply::Short(id, r) => {
                    let upcoming = r.as_ref().map(|l| l.len()).unwrap_or(0);
                    let x = self.xmltv_for(&id, iptv::epg::unix_now()).len();
                    let epg_id = self.ids.get(&id).map_or("-", String::as_str);
                    eprintln!("epg: stream {id} ({epg_id}): short guide {upcoming} programmes, XMLTV {x} from now");
                    self.short.insert(id, r);
                }
            }
        }
        any
    }

    /// The EPG channel id of a stream, if the provider has one.
    pub fn epg_id(&self, stream_id: &str) -> Option<&str> {
        self.ids.get(stream_id).map(String::as_str)
    }

    /// XMLTV programmes airing at `now` (unix seconds) and after, for a stream
    /// (`now` 0 = everything).
    pub fn xmltv_for(&self, stream_id: &str, now: i64) -> &[Programme] {
        self.ids
            .get(stream_id)
            .and_then(|e| self.xmltv.get(&e.to_lowercase()))
            .map_or(&[], |l| now_and_next(l, now))
    }

    /// `None` = not answered yet.
    pub fn short_for(&self, stream_id: &str) -> Option<&Result<Vec<Programme>, String>> {
        self.short.get(stream_id)
    }
}
