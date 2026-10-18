//! Movies and Series browsing state: search box + results (searched off the
//! UI thread by [`SearchWorker`]), the hidden-group filter, and the series
//! show -> season -> episode drill-down. No GUI types.
//!
//! Searches are tagged with a generation; a result is only adopted if it
//! answers the latest request of its browser, so a slow search can never
//! overwrite a newer one (or one made against a swapped library).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iptv::library::{Library, SearchScope};
use iptv::playlist::favorite_key;
use iptv::series::UNSORTED;

use super::nav::ChannelList;

/// Maximum results shown for one query.
pub const SEARCH_LIMIT: usize = 500;

/// Hidden group titles (from `settings.json`), shared with the worker.
pub type Hidden = Arc<HashSet<String>>;

/// Movies matching `query` (entry indices, file order), skipping movies whose
/// group is hidden; at most `limit`. Empty query = no results.
pub fn search_movies(lib: &Library, query: &str, hidden: &HashSet<String>, limit: usize) -> Vec<usize> {
    if hidden.is_empty() {
        return lib.search(SearchScope::Movies, query, limit).into_iter().filter_map(|h| h.entry).collect();
    }
    // The limit applies after filtering, so search without one.
    lib.search(SearchScope::Movies, query, usize::MAX)
        .into_iter()
        .filter_map(|h| h.entry)
        .filter(|&e| !hidden.contains(&lib.entry(e).group))
        .take(limit)
        .collect()
}

fn show_visible(lib: &Library, show: usize, hidden: &HashSet<String>) -> bool {
    lib.shows()[show].seasons.iter().flat_map(|s| &s.episodes).any(|&e| !hidden.contains(&lib.entry(e).group))
}

/// Shows matching `query` (indices into `Library::shows`) that have at least
/// one episode in a visible group; at most `limit`.
pub fn search_shows(lib: &Library, query: &str, hidden: &HashSet<String>, limit: usize) -> Vec<usize> {
    let shows = lib.search(SearchScope::Series, query, if hidden.is_empty() { limit } else { usize::MAX });
    shows.into_iter().filter_map(|h| h.show).filter(|&s| show_visible(lib, s, hidden)).take(limit).collect()
}

/// Movie and series group titles with their entry counts, sorted by title
/// (the group filter's choices).
pub fn vod_groups(lib: &Library) -> Vec<(String, usize)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let episodes = lib.shows().iter().flat_map(|s| &s.seasons).flat_map(|s| &s.episodes);
    for &e in lib.movies().iter().chain(episodes) {
        *counts.entry(lib.entry(e).group.as_str()).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().map(|(g, n)| (g.to_string(), n)).collect();
    v.sort();
    v
}

/// One row of an opened show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeRow {
    /// Season header (`None` = unparsed episodes, labelled "Unsorted").
    Season(Option<u32>),
    Episode(usize),
}

pub fn season_label(n: Option<u32>) -> String {
    match n {
        Some(n) => format!("Season {n}"),
        None => "Unsorted".into(),
    }
}

/// Rows of show `show`: a header per season, then its episodes in visible
/// groups (seasons left empty by the filter are omitted).
pub fn episode_rows(lib: &Library, show: usize, hidden: &HashSet<String>) -> Vec<EpisodeRow> {
    let mut rows = Vec::new();
    for s in &lib.shows()[show].seasons {
        let mut eps = s.episodes.iter().filter(|&&e| !hidden.contains(&lib.entry(e).group)).peekable();
        if eps.peek().is_some() {
            rows.push(EpisodeRow::Season(s.number));
            rows.extend(eps.map(|&e| EpisodeRow::Episode(e)));
        }
    }
    rows
}

/// Episode row text: `S01E02 - Title` for parsed episodes; the full entry
/// name inside `(Unsorted)` or when the name does not parse.
pub fn episode_text(lib: &Library, show: usize, entry: usize) -> String {
    let name = &lib.entry(entry).name;
    if lib.shows()[show].name == UNSORTED {
        return name.clone();
    }
    iptv::series::episode_label(name).unwrap_or_else(|| name.clone())
}

// ---- background search ----------------------------------------------------

pub struct SearchRequest {
    pub gen: u64,
    pub lib: Arc<Library>,
    pub scope: SearchScope,
    pub query: String,
    pub hidden: Hidden,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub gen: u64,
    pub scope: SearchScope,
    pub items: Vec<usize>,
    pub took: Duration,
}

/// Run one request (on any thread).
pub fn run_search(req: &SearchRequest) -> SearchResult {
    let t = Instant::now();
    let items = match req.scope {
        SearchScope::Series => search_shows(&req.lib, &req.query, &req.hidden, SEARCH_LIMIT),
        _ => search_movies(&req.lib, &req.query, &req.hidden, SEARCH_LIMIT),
    };
    SearchResult { gen: req.gen, scope: req.scope, items, took: t.elapsed() }
}

/// Searches on a background thread so typing never blocks a frame. Queued
/// requests are coalesced: only the newest per scope is run.
pub struct SearchWorker {
    tx: Option<Sender<SearchRequest>>,
    rx: Receiver<SearchResult>,
}

impl SearchWorker {
    /// `wake` is called after each result (e.g. request an egui repaint).
    pub fn spawn(wake: impl Fn() + Send + 'static) -> SearchWorker {
        let (tx, req_rx) = channel::<SearchRequest>();
        let (res_tx, rx) = channel();
        std::thread::Builder::new()
            .name("vod-search".into())
            .spawn(move || {
                while let Ok(first) = req_rx.recv() {
                    let mut latest: Vec<SearchRequest> = vec![first];
                    while let Ok(r) = req_rx.try_recv() {
                        latest.retain(|q| q.scope != r.scope);
                        latest.push(r);
                    }
                    for r in latest {
                        // The request (and its Arc<Library>) is dropped after use.
                        if res_tx.send(run_search(&r)).is_err() {
                            return;
                        }
                        wake();
                    }
                }
            })
            .expect("spawn search thread");
        SearchWorker { tx: Some(tx), rx }
    }

    pub fn submit(&self, req: SearchRequest) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(req);
        }
    }

    /// Results that arrived since the last poll.
    pub fn poll(&self) -> Vec<SearchResult> {
        self.rx.try_iter().collect()
    }
}

// ---- browsers ---------------------------------------------------------------

/// Search box text + the result list of one scope (movie entries or shows).
#[derive(Debug)]
pub struct SearchList {
    scope: SearchScope,
    query: String,
    gen: u64,
    searching: bool,
    list: ChannelList,
    /// Identity (movie favorite key / show name) to reselect when results arrive.
    reselect: Option<String>,
}

fn ident(lib: &Library, scope: SearchScope, id: usize) -> String {
    match scope {
        SearchScope::Series => lib.shows()[id].name.clone(),
        _ => favorite_key(lib.entry(id)),
    }
}

impl SearchList {
    pub fn new(scope: SearchScope) -> SearchList {
        SearchList { scope, query: String::new(), gen: 0, searching: false, list: ChannelList::default(), reselect: None }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// A search is running for the current query.
    pub fn searching(&self) -> bool {
        self.searching
    }

    pub fn list(&self) -> &ChannelList {
        &self.list
    }

    pub fn list_mut(&mut self) -> &mut ChannelList {
        &mut self.list
    }

    /// New query text. Returns the request to submit (`None` for a blank
    /// query: results are cleared and the empty-state hint shows).
    pub fn set_query(&mut self, lib: &Arc<Library>, hidden: &Hidden, query: &str) -> Option<SearchRequest> {
        let keep = self.list.current().map(|id| ident(lib, self.scope, id)).or(self.reselect.take());
        self.query = query.to_string();
        self.request(lib, hidden, keep)
    }

    /// Re-run the current query (hidden groups changed, or `lib` replaced
    /// `old`, the library the current results index into).
    pub fn research(&mut self, old: &Library, lib: &Arc<Library>, hidden: &Hidden) -> Option<SearchRequest> {
        let keep = self.list.current().map(|id| ident(old, self.scope, id)).or(self.reselect.take());
        if !std::ptr::eq(old, &**lib) {
            // The old indices mean nothing in the new library.
            self.list = ChannelList::default();
        }
        self.request(lib, hidden, keep)
    }

    fn request(&mut self, lib: &Arc<Library>, hidden: &Hidden, keep: Option<String>) -> Option<SearchRequest> {
        self.gen += 1;
        self.reselect = keep;
        if self.query.trim().is_empty() {
            self.list = ChannelList::default();
            self.searching = false;
            return None;
        }
        self.searching = true;
        Some(SearchRequest {
            gen: self.gen,
            lib: lib.clone(),
            scope: self.scope,
            query: self.query.clone(),
            hidden: hidden.clone(),
        })
    }

    /// Adopt a result if it answers the latest request. `lib` is the library
    /// that request was made against.
    pub fn on_result(&mut self, lib: &Library, r: SearchResult) -> bool {
        if r.gen != self.gen || r.scope != self.scope {
            return false;
        }
        self.searching = false;
        self.list = ChannelList::new(r.items);
        if let Some(k) = self.reselect.take() {
            if let Some(pos) = self.list.items().iter().position(|&id| ident(lib, self.scope, id) == k) {
                self.list.set_cursor(pos);
            }
        }
        true
    }
}

/// An opened show: season headers + episodes, cursor on an episode row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenShow {
    pub show: usize,
    pub name: String,
    rows: Vec<EpisodeRow>,
    cursor: Option<usize>,
}

impl OpenShow {
    pub fn new(lib: &Library, show: usize, hidden: &HashSet<String>) -> OpenShow {
        let rows = episode_rows(lib, show, hidden);
        let cursor = rows.iter().position(|r| matches!(r, EpisodeRow::Episode(_)));
        OpenShow { show, name: lib.shows()[show].name.clone(), rows, cursor }
    }

    pub fn rows(&self) -> &[EpisodeRow] {
        &self.rows
    }

    /// Selected row (always an episode row).
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    pub fn current(&self) -> Option<usize> {
        match self.rows.get(self.cursor?)? {
            EpisodeRow::Episode(e) => Some(*e),
            EpisodeRow::Season(_) => None,
        }
    }

    /// Episode entries in order (the Playback list of an episode).
    pub fn episodes(&self) -> Vec<usize> {
        self.rows
            .iter()
            .filter_map(|r| match r {
                EpisodeRow::Episode(e) => Some(*e),
                EpisodeRow::Season(_) => None,
            })
            .collect()
    }

    /// Select row `i` if it is an episode row; returns the episode.
    pub fn set_cursor(&mut self, i: usize) -> Option<usize> {
        match self.rows.get(i) {
            Some(EpisodeRow::Episode(e)) => {
                self.cursor = Some(i);
                Some(*e)
            }
            _ => None,
        }
    }

    /// Select the row of `entry`; false (cursor unchanged) if not listed.
    pub fn select_entry(&mut self, entry: usize) -> bool {
        match self.rows.iter().position(|r| *r == EpisodeRow::Episode(entry)) {
            Some(i) => {
                self.cursor = Some(i);
                true
            }
            None => false,
        }
    }

    /// Move over episode rows only (headers skipped), wrapping.
    pub fn step(&mut self, delta: i32) -> Option<usize> {
        let eps: Vec<usize> =
            (0..self.rows.len()).filter(|&i| matches!(self.rows[i], EpisodeRow::Episode(_))).collect();
        let n = eps.len() as i64;
        if n == 0 {
            return None;
        }
        let next = match self.cursor.and_then(|c| eps.iter().position(|&r| r == c)) {
            Some(p) => (p as i64 + delta as i64).rem_euclid(n),
            None if delta >= 0 => 0,
            None => n - 1,
        };
        self.cursor = Some(eps[next as usize]);
        self.current()
    }
}

/// Series tab: show search, and an opened show.
#[derive(Debug)]
pub struct SeriesBrowser {
    pub search: SearchList,
    open: Option<OpenShow>,
}

impl Default for SeriesBrowser {
    fn default() -> Self {
        SeriesBrowser { search: SearchList::new(SearchScope::Series), open: None }
    }
}

impl SeriesBrowser {
    pub fn open(&self) -> Option<&OpenShow> {
        self.open.as_ref()
    }

    pub fn open_mut(&mut self) -> Option<&mut OpenShow> {
        self.open.as_mut()
    }

    /// Open the show selected in the search results.
    pub fn open_selected(&mut self, lib: &Library, hidden: &HashSet<String>) -> bool {
        match self.search.list().current() {
            Some(s) if s < lib.shows().len() => {
                self.open = Some(OpenShow::new(lib, s, hidden));
                true
            }
            _ => false,
        }
    }

    /// Back from the episodes to the show list (the show stays selected).
    pub fn back(&mut self) -> bool {
        self.open.take().is_some()
    }

    /// Hidden groups changed: rebuild the open show's rows (cursor kept on
    /// the same episode when it is still listed).
    pub fn refilter(&mut self, lib: &Library, hidden: &HashSet<String>) {
        if let Some(o) = &mut self.open {
            let cur = o.current();
            *o = OpenShow::new(lib, o.show, hidden);
            if let Some(e) = cur {
                o.select_entry(e);
            }
        }
    }

    /// `new` replaced `old`: re-find the open show by name and the selected
    /// episode by favorite key; close it if the show vanished.
    pub fn rebase(&mut self, old: &Library, new: &Library, hidden: &HashSet<String>) {
        let Some(o) = self.open.take() else { return };
        let key = o.current().map(|e| favorite_key(old.entry(e)));
        if let Some(i) = new.shows().iter().position(|s| s.name == o.name) {
            let mut n = OpenShow::new(new, i, hidden);
            if let Some(e) = key.and_then(|k| new.find_by_key(&k)) {
                n.select_entry(e);
            }
            self.open = Some(n);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use iptv::playlist::{Entry, Kind};

    pub(crate) fn vod(kind: Kind, id: &str, name: &str, group: &str) -> Entry {
        let dir = if kind == Kind::Movie { "movie" } else { "series" };
        Entry {
            kind,
            id: id.into(),
            name: name.into(),
            group: group.into(),
            logo: String::new(),
            url: format!("http://host.invalid/{dir}/USER/PASS/{id}.mkv"),
        }
    }

    /// Invented titles only.
    pub(crate) fn vod_lib() -> Library {
        use Kind::*;
        Library::build(vec![
            vod(Movie, "1", "The Paper Lantern", "Movies: Testland"),       // 0
            vod(Movie, "2", "Lantern Bay (2001)", "Movies: Faroe"),         // 1
            vod(Movie, "3", "Le Ch\u{e2}teau Fran\u{e7}ais", "Movies: Testland"), // 2
            vod(Series, "10", "Quiet Harbor S01 E02", "Series: Testland"),  // 3
            vod(Series, "11", "Quiet Harbor S01 E01 - Arrival", "Series: Testland"), // 4
            vod(Series, "12", "SE: Quiet Harbor S02E01", "Series: Faroe"),  // 5
            vod(Series, "13", "Harbor Lights 1x01", "Series: Faroe"),       // 6
            vod(Series, "14", "Odd Lantern Special", "Series: Testland"),   // 7 -> (Unsorted)
            vod(Live, "20", "Lantern TV", "Testland - Main"),               // 8
        ])
    }

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    fn hide(g: &[&str]) -> HashSet<String> {
        g.iter().map(|s| s.to_string()).collect()
    }

    fn shown(lib: &Library, shows: &[usize]) -> Vec<String> {
        shows.iter().map(|&s| lib.shows()[s].name.clone()).collect()
    }

    #[test]
    fn movie_search_is_diacritic_insensitive_and_skips_live() {
        let lib = vod_lib();
        assert_eq!(search_movies(&lib, "lantern", &none(), 500), vec![0, 1]);
        assert_eq!(search_movies(&lib, "chateau francais", &none(), 500), vec![2]);
        assert_eq!(search_movies(&lib, "", &none(), 500), Vec::<usize>::new(), "empty query: no list");
        assert_eq!(search_movies(&lib, "lantern", &none(), 1), vec![0]);
    }

    #[test]
    fn hidden_groups_filter_movies_and_shows() {
        let lib = vod_lib();
        let h = hide(&["Movies: Testland"]);
        assert_eq!(search_movies(&lib, "lantern", &h, 500), vec![1]);
        // Limit applies after filtering.
        assert_eq!(search_movies(&lib, "l", &h, 1), vec![1]);
        assert_eq!(shown(&lib, &search_shows(&lib, "harbor", &none(), 500)), ["Harbor Lights", "Quiet Harbor"]);
        // Quiet Harbor still has an episode in Series: Faroe.
        let h = hide(&["Series: Testland"]);
        assert_eq!(shown(&lib, &search_shows(&lib, "harbor", &h, 500)), ["Harbor Lights", "Quiet Harbor"]);
        let h = hide(&["Series: Testland", "Series: Faroe"]);
        assert!(search_shows(&lib, "harbor", &h, 500).is_empty());
    }

    #[test]
    fn groups_list_counts_vod_only() {
        let lib = vod_lib();
        assert_eq!(
            vod_groups(&lib),
            vec![
                ("Movies: Faroe".to_string(), 1),
                ("Movies: Testland".to_string(), 2),
                ("Series: Faroe".to_string(), 2),
                ("Series: Testland".to_string(), 3),
            ]
        );
    }

    fn show_named(lib: &Library, name: &str) -> usize {
        lib.shows().iter().position(|s| s.name == name).unwrap()
    }

    #[test]
    fn episode_rows_have_season_headers_and_labels() {
        let lib = vod_lib();
        let qh = show_named(&lib, "Quiet Harbor");
        assert_eq!(
            episode_rows(&lib, qh, &none()),
            vec![
                EpisodeRow::Season(Some(1)),
                EpisodeRow::Episode(4),
                EpisodeRow::Episode(3),
                EpisodeRow::Season(Some(2)),
                EpisodeRow::Episode(5),
            ]
        );
        assert_eq!(episode_text(&lib, qh, 4), "S01E01 - Arrival");
        assert_eq!(episode_text(&lib, qh, 5), "S02E01");
        // Season 2 is only in the hidden group: its header goes too.
        assert_eq!(
            episode_rows(&lib, qh, &hide(&["Series: Faroe"])),
            vec![EpisodeRow::Season(Some(1)), EpisodeRow::Episode(4), EpisodeRow::Episode(3)]
        );
        let un = show_named(&lib, UNSORTED);
        assert_eq!(episode_rows(&lib, un, &none()), vec![EpisodeRow::Season(None), EpisodeRow::Episode(7)]);
        assert_eq!(season_label(None), "Unsorted");
        assert_eq!(season_label(Some(3)), "Season 3");
        assert_eq!(episode_text(&lib, un, 7), "Odd Lantern Special", "unsorted: full name");
    }

    #[test]
    fn unsorted_show_is_searchable() {
        let lib = vod_lib();
        assert_eq!(shown(&lib, &search_shows(&lib, "unsorted", &none(), 500)), [UNSORTED]);
    }

    #[test]
    fn open_show_cursor_skips_headers_and_wraps() {
        let lib = vod_lib();
        let mut o = OpenShow::new(&lib, show_named(&lib, "Quiet Harbor"), &none());
        assert_eq!(o.cursor(), Some(1), "first episode selected");
        assert_eq!(o.current(), Some(4));
        assert_eq!(o.step(1), Some(3));
        assert_eq!(o.step(1), Some(5), "skips the Season 2 header");
        assert_eq!(o.cursor(), Some(4));
        assert_eq!(o.step(1), Some(4), "wraps");
        assert_eq!(o.step(-1), Some(5));
        assert_eq!(o.set_cursor(3), None, "header rows cannot be selected");
        assert_eq!(o.current(), Some(5));
        assert_eq!(o.set_cursor(2), Some(3));
        assert!(o.select_entry(4));
        assert!(!o.select_entry(0));
        assert_eq!(o.episodes(), vec![4, 3, 5]);
    }

    fn arc(l: Library) -> Arc<Library> {
        Arc::new(l)
    }

    #[test]
    fn stale_results_are_ignored_and_selection_kept() {
        let lib = arc(vod_lib());
        let h: Hidden = Arc::new(none());
        let mut m = SearchList::new(SearchScope::Movies);
        let r1 = m.set_query(&lib, &h, "lantern").unwrap();
        assert!(m.searching());
        let r2 = m.set_query(&lib, &h, "lantern bay").unwrap();
        assert!(r2.gen > r1.gen);
        // The newer answer arrives first, then the stale one.
        assert!(m.on_result(&lib, run_search(&r2)));
        assert_eq!(m.list().items(), &[1]);
        assert!(!m.on_result(&lib, run_search(&r1)), "stale result dropped");
        assert_eq!(m.list().items(), &[1]);
        assert!(!m.searching());
        // Select, widen the query: the selection follows the movie.
        m.list_mut().set_cursor(0);
        let r3 = m.set_query(&lib, &h, "lantern").unwrap();
        m.on_result(&lib, run_search(&r3));
        assert_eq!(m.list().items(), &[0, 1]);
        assert_eq!(m.list().current(), Some(1));
        // Blank query: cleared at once, nothing to run, late results ignored.
        let r4 = m.set_query(&lib, &h, "lan").unwrap();
        assert!(m.set_query(&lib, &h, "  ").is_none());
        assert!(m.list().is_empty() && !m.searching());
        assert!(!m.on_result(&lib, run_search(&r4)));
    }

    #[test]
    fn library_swap_researches_and_reselects_by_key() {
        let old = arc(vod_lib());
        let h: Hidden = Arc::new(none());
        let mut m = SearchList::new(SearchScope::Movies);
        let r = m.set_query(&old, &h, "lantern").unwrap();
        m.on_result(&old, run_search(&r));
        m.list_mut().set_cursor(1); // Lantern Bay (id 2)
        let new = arc(Library::build(vec![
            vod(Kind::Movie, "9", "Lantern Nine", "Movies: Faroe"),
            vod(Kind::Movie, "2", "Lantern Bay (2001)", "Movies: Faroe"),
        ]));
        let r = m.research(&old, &new, &h).unwrap();
        assert!(m.list().is_empty(), "old indices dropped at once");
        assert!(Arc::ptr_eq(&r.lib, &new));
        m.on_result(&new, run_search(&r));
        assert_eq!(m.list().items(), &[0, 1]);
        assert_eq!(m.list().current(), Some(1), "same movie selected in the new library");
    }

    #[test]
    fn series_drill_down_back_and_rebase() {
        let lib = arc(vod_lib());
        let h: Hidden = Arc::new(none());
        let mut s = SeriesBrowser::default();
        let r = s.search.set_query(&lib, &h, "harbor").unwrap();
        s.search.on_result(&lib, run_search(&r));
        assert!(!s.open_selected(&lib, &h), "nothing selected yet");
        s.search.list_mut().set_cursor(1); // Quiet Harbor
        assert!(s.open_selected(&lib, &h));
        assert_eq!(s.open().unwrap().name, "Quiet Harbor");
        s.open_mut().unwrap().step(1);
        assert_eq!(s.open().unwrap().current(), Some(3));
        // Hiding the Faroe group drops season 2 but keeps the cursor.
        s.refilter(&lib, &hide(&["Series: Faroe"]));
        assert_eq!(s.open().unwrap().rows().len(), 3);
        assert_eq!(s.open().unwrap().current(), Some(3));
        s.refilter(&lib, &h);
        // Refreshed library: the show and episode moved.
        let new = Library::build(vec![
            vod(Kind::Series, "50", "Other Show S01E01", "Series: Faroe"),
            vod(Kind::Series, "10", "Quiet Harbor S01 E02", "Series: Testland"),
        ]);
        s.rebase(&lib, &new, &h);
        let o = s.open().unwrap();
        assert_eq!(new.shows()[o.show].name, "Quiet Harbor");
        assert_eq!(o.current(), Some(1), "same episode by key");
        assert!(s.back());
        assert!(s.open().is_none());
        assert!(!s.back(), "already at the show list");
        // A show that vanished closes.
        s.search.list_mut().set_cursor(0);
        let empty = Library::build(vec![]);
        s.open = Some(OpenShow::new(&new, 1, &h));
        s.rebase(&new, &empty, &h);
        assert!(s.open().is_none());
    }

    #[test]
    fn worker_answers_off_thread_and_the_latest_wins() {
        let lib = arc(vod_lib());
        let h: Hidden = Arc::new(none());
        let (wtx, wrx) = channel();
        let w = SearchWorker::spawn(move || {
            let _ = wtx.send(());
        });
        let mut m = SearchList::new(SearchScope::Movies);
        for q in ["l", "la", "lan", "lantern b"] {
            w.submit(m.set_query(&lib, &h, q).unwrap());
        }
        let mut adopted = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while m.searching() && Instant::now() < deadline {
            let _ = wrx.recv_timeout(Duration::from_millis(100));
            for r in w.poll() {
                adopted += m.on_result(&lib, r) as usize;
            }
        }
        assert_eq!(adopted, 1, "only the answer to the last request is adopted");
        assert_eq!(m.list().items(), &[1]);
    }

    // ---- performance: synthetic ~250k-entry library -----------------------

    /// Deterministic synthetic library: 30k live, 57k movies, 195k episodes
    /// in ~13k shows, invented word-salad titles with some diacritics.
    pub(crate) fn synthetic_lib() -> Library {
        const W: [&str; 24] = [
            "lantern", "harbor", "quiet", "storm", "garden", "silver", "north", "ch\u{e2}teau", "river", "night",
            "f\u{e5}gel", "stone", "summer", "winter", "glass", "iron", "paper", "crow", "valley", "m\u{f6}rk",
            "echo", "bridge", "salt", "ember",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut title = |n: usize| -> String {
            (0..n).map(|_| W[(next() % W.len() as u64) as usize]).collect::<Vec<_>>().join(" ")
        };
        let mut es = Vec::with_capacity(282_000);
        for i in 0..30_000 {
            es.push(vod(Kind::Live, &format!("l{i}"), &title(2), &format!("Land{} - Main", i % 60)));
        }
        for i in 0..57_000 {
            let t = format!("{} ({})", title(3), 1950 + i % 70);
            es.push(vod(Kind::Movie, &format!("m{i}"), &t, &format!("Movies: Lang{}", i % 40)));
        }
        let mut n = 0;
        let mut show = 0;
        while n < 195_000 {
            let name = title(2) + &format!(" {show}");
            let eps = 5 + show % 26;
            for e in 0..eps {
                es.push(vod(
                    Kind::Series,
                    &format!("s{n}"),
                    &format!("{name} S{:02}E{:02}", 1 + e / 10, 1 + e % 10),
                    &format!("Series: Lang{}", show % 30),
                ));
                n += 1;
            }
            show += 1;
        }
        Library::build(es)
    }

    /// `cargo test --release -p iptv-player synthetic_search_perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn synthetic_search_perf() {
        let t = Instant::now();
        let lib = synthetic_lib();
        println!("build: {:?} entries={} shows={}", t.elapsed(), lib.entry_count(), lib.shows().len());
        let t = Instant::now();
        let groups = vod_groups(&lib);
        println!("vod_groups: {:?} ({} groups)", t.elapsed(), groups.len());
        let hidden: HashSet<String> = groups.iter().step_by(3).map(|(g, _)| g.clone()).collect();
        let qs = [
            "a", "e", "l", "la", "lan", "lant", "lantern", "s", "st", "sto", "storm", "chateau", "f\u{e5}g", "fag",
            "mork", "quiet harbor", "silver night river", "zzz", "19", "ember salt", "o", "r", "i", "n",
        ];
        for (label, h) in [("no filter", HashSet::new()), ("1/3 groups hidden", hidden)] {
            let mut times = Vec::new();
            for q in qs {
                let t = Instant::now();
                std::hint::black_box(search_movies(&lib, q, &h, SEARCH_LIMIT));
                times.push(("movies", q, t.elapsed()));
                let t = Instant::now();
                let shows = search_shows(&lib, q, &h, SEARCH_LIMIT);
                times.push(("series", q, t.elapsed()));
                if let Some(&s) = shows.first() {
                    let t = Instant::now();
                    std::hint::black_box(episode_rows(&lib, s, &h));
                    times.push(("episodes", q, t.elapsed()));
                }
            }
            times.sort_by_key(|x| x.2);
            let (k, q, max) = times.last().unwrap();
            let p95 = times[times.len() * 95 / 100].2;
            println!("{label}: {} ops, p95 {:?}, max {:?} ({k} {q:?})", times.len(), p95, max);
        }
    }
}
