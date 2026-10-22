//! The eframe application: tabs (Live | Favorites | Movies | Series), browse
//! lists beside the video, a full player view, keyboard handling and the
//! status line. Thin over `crate::state`, which holds all testable logic.
//!
//! Adding a tab: give it a browser struct in `state`, a renderer in `ui`,
//! and arms in `App::tab_list`, `App::step`, `App::activate` and
//! `App::selected_entry`. Movies/Series (Task 12) use `state::vod`: their
//! searches run on a `SearchWorker` thread; Up/Down only move the cursor
//! there (no zap), click/Enter play at once, as VOD.

pub mod fonts;
pub mod help;
pub mod live;
pub mod player;
pub mod previews;
pub mod setup;
pub mod theme;
pub mod vod;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use iptv::favorites::Favorites;
use iptv::fetch::{self, RefreshEvent, RefreshOptions, Refresher, Source};
use iptv::library::{Library, SearchScope};
use iptv::playlist::{favorite_key, Kind};
use iptv::positions::Positions;
use iptv::recent_series::RecentSeries;
use iptv::settings::Settings;
use iptv::subtitles;
use iptv::supervisor::Status;

use crate::state::autoplay::{Autoplay, NextFail, Step, OFFER_SECS};
use crate::state::data::{Action, DataState, Phase, RefreshState, SourceStatus};
use crate::state::favs::{toggle_key, toggle_target, FavBrowser};
use crate::state::cursor;
use crate::state::help::HelpTab;
use crate::state::live::LiveBrowser;
use crate::intro_scan::IntroScanner;
use crate::state::next::{
    continue_target, neighbour_episode, next_episode, shift_right, show_episodes, show_of, NextCache, ShiftRight,
};
use crate::state::skip_intro::{self, IntroKey, KeyGate, SkipIntro, Which};
use iptv::intro::INTRO_SCAN_SECS;
use iptv::intros::Intros;
use crate::state::titles::{TitleLoader, TVMAZE_URL};
use crate::state::playback::{enter_action, queue_zap, rebase_pending, EnterAction, Playback};
use crate::state::vod::{
    playing_title, visible_counts, vod_groups, Hidden, SearchList, SearchRequest, SearchWorker, SeriesBrowser, VodGroup,
};
use crate::state::zap::{Debounce, ZAP_DELAY};
use crate::state::seek::{self, SeekState};
use previews::{Previews, ScanState};
use crate::state::{format_bytes, format_hms, safe_text, tab_enter_visible, Tab};

/// How long the played/total time stays visible after the mouse last moved.
const TIME_SHOW: Duration = Duration::from_secs(3);
/// Height of the strip below a movie/episode that holds the seek bar.
const SEEK_BAR_H: f32 = 28.0;
/// How long the preview stays above the dot after an arrow-key seek.
const KEY_PREVIEW: Duration = Duration::from_millis(1500);
use live::{ListView, RowAction};
use player::Player;

/// Command-line options.
pub struct Options {
    pub data_dir: PathBuf,
    /// Dev: parse this local `.m3u` instead of downloading `playlist_url`.
    pub playlist_file: Option<PathBuf>,
    /// Process start, for startup timings on stderr.
    pub t0: Instant,
    /// Started with `--console`: also save tail-scan frames (tuning aid).
    pub console: bool,
}

pub fn run(opts: Options) -> anyhow::Result<()> {
    let native = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 760.0])
            .with_title("IPTV Player")
            .with_icon(egui::IconData {
                rgba: include_bytes!("../../assets/icon_64.rgba").to_vec(),
                width: 64,
                height: 64,
            }),
        ..Default::default()
    };
    eframe::run_native("IPTV Player", native, Box::new(move |cc| Ok(Box::new(App::new(cc, opts)))))
        .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Browse,
    /// Video fills the window.
    Player,
}

/// How often the position of a playing movie/episode is written to disk.
const POSITION_SAVE_EVERY: Duration = Duration::from_secs(15);

/// Show name and favorite key of the episode `pb` plays, for the recent-series
/// history; `None` for live, movies and `(Unsorted)` episodes.
fn recent_id(pb: &Playback) -> Option<(String, String)> {
    let e = pb.entry_index().filter(|_| !pb.is_live())?;
    let lib = pb.library();
    if lib.entry(e).kind != Kind::Series {
        return None;
    }
    let show = show_of(lib, e)?;
    Some((lib.shows()[show].name.clone(), pb.key().to_string()))
}

/// Ends the process at once without running any exit handlers.
/// `std::process::exit` (ExitProcess) runs DLL detach code, which can block
/// behind a libmpv thread stuck in a network call (a terminal then waits for
/// Ctrl+C even though the window is gone); TerminateProcess cannot block.
fn hard_exit() -> ! {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn TerminateProcess(process: isize, exit_code: u32) -> i32;
        }
        unsafe {
            TerminateProcess(GetCurrentProcess(), 0);
        }
    }
    std::process::exit(0)
}

/// `h:mm:ss` / `m:ss` for a position in seconds.
fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// UI build time above this is logged (responsiveness diagnostics).
const SLOW_FRAME: Duration = Duration::from_millis(25);

type CacheResult = Option<(Arc<Library>, SystemTime)>;

fn filter_id() -> egui::Id {
    egui::Id::unique("channel_filter")
}

/// `(user_agent, top_countries)` from `config.toml` if it loads, else the
/// default UA and no top countries (also with `--playlist-file`).
fn configured_prefs(data_dir: &std::path::Path) -> (String, Vec<String>) {
    iptv::config::load(data_dir).map_or_else(
        |_| (iptv::config::DEFAULT_USER_AGENT.to_string(), Vec::new()),
        |c| (c.user_agent, c.top_countries),
    )
}

/// Where the playlist comes from: `--playlist-file`, else `config.toml`.
fn resolve_source(data_dir: &std::path::Path, file: &Option<PathBuf>) -> (Option<Source>, SourceStatus) {
    if let Some(f) = file {
        return (Some(Source::File(f.clone())), SourceStatus::Available);
    }
    match iptv::config::load(data_dir) {
        Ok(c) => (Some(Source::Url(c.playlist_url)), SourceStatus::Available),
        // config errors name the file/field, never the URL.
        Err(e) => (
            None,
            SourceStatus::Missing { config_path: data_dir.join("config.toml"), detail: safe_text(&format!("{e:#}")) },
        ),
    }
}

struct App {
    ctx: egui::Context,
    data_dir: PathBuf,
    playlist_file: Option<PathBuf>,
    source: Option<Source>,
    /// User-Agent for playlist downloads (the player got it at startup).
    user_agent: String,
    /// Live countries listed first (`top_countries` in config.toml).
    top_countries: Vec<String>,
    /// View to return to when leaving fullscreen.
    fs_return_view: Option<View>,
    /// Last mouse move over the video or key press (shows the seek bar for `TIME_SHOW`).
    activity: Option<Instant>,
    /// Last mouse movement/click (the cursor hides in fullscreen after `cursor::HIDE_AFTER`).
    cursor_moved: Instant,
    cursor_hidden: bool,
    was_fullscreen: bool,
    /// Title overlay text and when it last changed (fullscreen shows it again then).
    shown_title: String,
    title_changed: Instant,
    /// Seek bar state (movies/episodes).
    seek: SeekState,
    /// Preview frames above the seek bar.
    previews: Previews,
    /// Last arrow-key seek (the preview shows above the dot for `KEY_PREVIEW`).
    key_seek: Option<Instant>,
    /// "Next episode" offer / autoplay of the playing episode.
    autoplay: Autoplay,
    /// Headless scans of episode openings (own pool of connections, one job at a time).
    intro_scanner: IntroScanner,
    /// Per-show intro references (`intros.json`).
    intros: Intros,
    /// "Skip intro" flow of the playing episode.
    skip_intro: SkipIntro,
    /// URLs the intro scans open: the playing episode, its neighbour (first
    /// time for a show). Never logged.
    intro_urls: (String, Option<String>),
    /// Display names of those two episodes (`--console` frame folders).
    intro_names: (String, Option<String>),
    /// Show of the playing episode (key of its `intros.json` reference).
    intro_show: String,
    /// Last known duration of the playing episode (the scans' `duration`).
    intro_duration: Option<f64>,
    /// The intro decision of the playing episode was logged (or nothing to decide).
    intro_logged: bool,
    /// Next episode of the playing one, for the ">>" button (cached: the
    /// lookup scans every show).
    next_cache: NextCache,
    /// The help card is open (on this tab).
    help: Option<HelpTab>,
    /// Modal asking for the playlist URL while `config.toml` has none.
    url_prompt: Option<setup::UrlPrompt>,
    /// The subtitle menu above the video is open.
    sub_menu: bool,
    /// Highlighted row of the subtitle menu (0 = Off, then the tracks).
    sub_cursor: usize,
    /// Scroll the subtitle menu to `sub_cursor` on the next draw (keyboard moves).
    sub_follow: bool,
    /// Where movies/episodes stopped (`positions.json`), for resuming.
    positions: Positions,
    /// Last time the playing position was written to disk.
    positions_saved: Instant,
    /// The last series watched (`recent-series.json`): the list under the
    /// Series search box and the continue button.
    recent: RecentSeries,
    /// Selected row of that list (keyboard).
    recent_cursor: Option<usize>,
    recent_view: ListView,
    data: DataState,
    cache_rx: Option<Receiver<CacheResult>>,
    refresher: Option<Refresher>,
    /// Library the browse state currently indexes into.
    shown_lib: Option<Arc<Library>>,
    favs: Favorites,
    live: LiveBrowser,
    fav: FavBrowser,
    tab: Tab,
    view: View,
    player: Result<Player, String>,
    playback: Option<Playback>,
    /// Debounced Up/Down channel switch (see `state::zap`).
    zap: Debounce<Playback>,
    /// Transient on-screen message (favorite toggled, save error, ...).
    message: Option<String>,
    filter: String,
    focus_filter: bool,
    scroll_to: Option<usize>,
    country_view: ListView,
    /// Country row to scroll into view (keyboard country navigation).
    country_scroll: Option<usize>,
    channel_view: ListView,
    fav_view: ListView,
    /// `settings.json` (hidden movie/series groups).
    settings: Settings,
    hidden: Hidden,
    movies: SearchList,
    series: SeriesBrowser,
    search: SearchWorker,
    /// Episode titles from TVmaze (background worker).
    titles: TitleLoader,
    /// Movie/series group titles + counts for the filter menu (computed off
    /// the UI thread per library: ~30 ms on 250k entries).
    groups: Option<Vec<VodGroup>>,
    groups_rx: Option<Receiver<Vec<VodGroup>>>,
    /// Filter text of the Groups menu.
    group_filter: String,
    /// Focus the Movies/Series search box on the next frame.
    focus_search: bool,
    /// Tab button that has keyboard focus this frame (Enter on it opens the tab).
    focused_tab: Option<Tab>,
    focused_tab_id: Option<egui::Id>,
    last_tab: Tab,
    movie_view: ListView,
    show_view: ListView,
    episode_view: ListView,
    t0: Instant,
    /// `--console`: tail scans save their frames under `scan-frames/`.
    console: bool,
    first_frame: bool,
    ready_logged: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, opts: Options) -> App {
        fonts::install_fallbacks(&cc.egui_ctx);
        // First run: write a default config.toml (never over an existing one;
        // not with --playlist-file, which needs no config).
        let create_note = if opts.playlist_file.is_none() {
            iptv::config::ensure_default(&opts.data_dir)
                .err()
                .map(|e| format!("Could not create config.toml: {}", e.kind()))
        } else {
            None
        };
        let url_prompt = if opts.playlist_file.is_none() {
            setup::Problem::of(&opts.data_dir).map(|p| setup::UrlPrompt::new(p, create_note))
        } else {
            None
        };
        let (source, status) = resolve_source(&opts.data_dir, &opts.playlist_file);
        let (user_agent, top_countries) = configured_prefs(&opts.data_dir);
        let (data, actions) = DataState::new(status);
        theme::apply(&cc.egui_ctx, iptv::theme::load(&opts.data_dir));
        let player = Player::new(cc, &user_agent).map_err(|e| safe_text(&format!("{e:#}")));
        if let Err(e) = &player {
            eprintln!("player unavailable: {e}");
        }
        let favs = Favorites::load(&opts.data_dir);
        let positions = Positions::load(&opts.data_dir);
        let recent = RecentSeries::load(&opts.data_dir);
        let settings = Settings::load(&opts.data_dir);
        let hidden: Hidden = Arc::new(settings.hidden_groups.iter().cloned().collect());
        let user_agent_for_previews = user_agent.clone();
        let wake = cc.egui_ctx.clone();
        let search = SearchWorker::spawn(move || wake.request_repaint());
        let wake = cc.egui_ctx.clone();
        let titles = TitleLoader::new(opts.data_dir.clone(), TVMAZE_URL.to_string(), move || wake.request_repaint());
        let wake = cc.egui_ctx.clone();
        let intro_scanner = IntroScanner::new(user_agent.clone(), move || wake.request_repaint());
        let intros = Intros::load(&opts.data_dir);
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            data_dir: opts.data_dir,
            playlist_file: opts.playlist_file,
            source,
            user_agent,
            top_countries,
            fs_return_view: None,
            activity: None,
            cursor_moved: Instant::now(),
            cursor_hidden: false,
            was_fullscreen: false,
            shown_title: String::new(),
            title_changed: Instant::now(),
            seek: SeekState::default(),
            previews: Previews::new(user_agent_for_previews, cc.egui_ctx.clone()),
            key_seek: None,
            autoplay: Autoplay::new(),
            intro_scanner,
            intros,
            skip_intro: SkipIntro::new(),
            intro_urls: (String::new(), None),
            intro_names: (String::new(), None),
            intro_show: String::new(),
            intro_duration: None,
            intro_logged: true,
            next_cache: NextCache::default(),
            help: None,
            url_prompt,
            sub_menu: false,
            sub_cursor: 0,
            sub_follow: false,
            positions,
            positions_saved: Instant::now(),
            recent,
            recent_cursor: None,
            recent_view: ListView::default(),
            data,
            cache_rx: None,
            refresher: None,
            shown_lib: None,
            favs,
            live: LiveBrowser::new(),
            fav: FavBrowser::default(),
            tab: Tab::Live,
            view: View::Browse,
            player,
            playback: None,
            zap: Debounce::new(ZAP_DELAY),
            message: None,
            filter: String::new(),
            focus_filter: false,
            scroll_to: None,
            country_view: ListView::default(),
            country_scroll: None,
            channel_view: ListView::default(),
            fav_view: ListView::default(),
            settings,
            hidden,
            movies: SearchList::new(SearchScope::Movies),
            series: SeriesBrowser::default(),
            search,
            titles,
            groups: None,
            groups_rx: None,
            group_filter: String::new(),
            focus_search: false,
            focused_tab: None,
            focused_tab_id: None,
            last_tab: Tab::Live,
            movie_view: ListView::default(),
            show_view: ListView::default(),
            episode_view: ListView::default(),
            t0: opts.t0,
            console: opts.console,
            first_frame: false,
            ready_logged: false,
        };
        app.fav.reload(None, &app.favs);
        app.run_actions(actions);
        app
    }

    // ---- background work -------------------------------------------------

    fn run_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::LoadCache => {
                    // No usable playlist URL: a cached library would be misleading
                    // behind the prompt. Treat it as "no cache"; saving the URL
                    // starts a normal refresh.
                    if self.url_prompt.is_some() {
                        let a = self.data.on_cache_loaded(None, SystemTime::now());
                        self.run_actions(a);
                        continue;
                    }
                    let (tx, rx) = channel();
                    let dir = self.data_dir.clone();
                    let ctx = self.ctx.clone();
                    let top = self.top_countries.clone();
                    std::thread::spawn(move || {
                        let r = fetch::load_cached(&dir, &top);
                        let _ = tx.send(r);
                        ctx.request_repaint();
                    });
                    self.cache_rx = Some(rx);
                }
                Action::StartRefresh { force } => {
                    if let Some(src) = &self.source {
                        self.refresher =
                            Some(Refresher::spawn_source(src.clone(), self.data_dir.clone(), RefreshOptions {
                                force,
                                user_agent: self.user_agent.clone(),
                                top_countries: self.top_countries.clone(),
                            }));
                    }
                }
            }
        }
    }

    /// Draws the playlist URL prompt (if open) and acts on its buttons.
    fn url_prompt_ui(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.url_prompt else { return };
        let outcome = setup::show(ctx, prompt, &self.data_dir.join("config.toml"));
        let save = match outcome {
            setup::Outcome::None => return,
            setup::Outcome::Quit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            setup::Outcome::Save(text) => Some(text),
            setup::Outcome::Reload => None,
        };
        if let Some(text) = save {
            if let Err(e) = iptv::config::set_playlist_url(&self.data_dir, &text) {
                if let Some(p) = &mut self.url_prompt {
                    p.set_error(safe_text(&format!("{e:#}")));
                }
                return;
            }
        }
        // Saved (or reloaded): close when the config now loads, else show why not.
        match setup::Problem::of(&self.data_dir) {
            None => {
                self.url_prompt = None;
                self.request_refresh(false);
            }
            Some(problem) => {
                if let Some(p) = &mut self.url_prompt {
                    p.set_problem(problem);
                }
            }
        }
    }

    fn request_refresh(&mut self, force: bool) {
        if self.source.is_none() {
            // config.toml may have been created since startup.
            let (source, status) = resolve_source(&self.data_dir, &self.playlist_file);
            self.source = source;
            self.data.set_source(status);
        }
        // Picks up a `user_agent` / `top_countries` edited in config.toml since
        // startup (streams keep the startup user agent until restart).
        (self.user_agent, self.top_countries) = configured_prefs(&self.data_dir);
        // Refresh also retries episode-title lookups that found nothing.
        self.titles.retry_failed();
        let a = self.data.request_refresh(force);
        self.run_actions(a);
    }

    fn poll_background(&mut self) {
        if let Some(rx) = &self.cache_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.cache_rx = None;
                    let a = self.data.on_cache_loaded(r, SystemTime::now());
                    self.run_actions(a);
                }
                Err(TryRecvError::Disconnected) => {
                    self.cache_rx = None;
                    let a = self.data.on_cache_loaded(None, SystemTime::now());
                    self.run_actions(a);
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(r) = &self.refresher {
            let mut done = false;
            while let Some(ev) = r.poll() {
                done |= matches!(ev, RefreshEvent::Done(_) | RefreshEvent::Failed(_));
                match &ev {
                    RefreshEvent::Failed(m) => eprintln!("refresh failed: {}", safe_text(m)),
                    RefreshEvent::Done(l) => eprintln!("refresh done: {} entries", l.entry_count()),
                    _ => {}
                }
                self.data.on_refresh_event(ev);
            }
            if done {
                self.refresher = None;
            }
        }
        self.sync_library();
        if let Some(rx) = &self.groups_rx {
            match rx.try_recv() {
                Ok(g) => {
                    self.groups = Some(g);
                    self.groups_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.groups_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        // Search answers: only the latest request of each list is adopted,
        // and that request was made against the shown library.
        let (results, dead) = self.search.poll();
        if let Some(lib) = self.shown_lib.clone() {
            for r in results {
                if r.took > Duration::from_millis(16) {
                    eprintln!("search took {} ms", r.took.as_millis());
                }
                // Only scroll the list that is on screen (the scroll target is shared).
                let visible = match r.scope {
                    SearchScope::Series => self.tab == Tab::Series && self.series.open().is_none(),
                    _ => self.tab == Tab::Movies,
                };
                let adopted = match r.scope {
                    SearchScope::Series => self.series.search.on_result(&lib, r),
                    _ => self.movies.on_result(&lib, r),
                };
                if adopted && visible {
                    self.scroll_to = Some(self.vod_cursor().unwrap_or(0));
                }
            }
        }
        if dead {
            self.restart_search();
        }
    }

    fn submit(&mut self, req: Option<SearchRequest>) {
        if let Some(r) = req {
            if !self.search.submit(r) {
                self.restart_search();
            }
        }
    }

    /// The search thread died: stop "Searching…", say so, start a new one.
    fn restart_search(&mut self) {
        eprintln!("search worker stopped; restarting it");
        self.movies.abort();
        self.series.search.abort();
        let wake = self.ctx.clone();
        self.search = SearchWorker::spawn(move || wake.request_repaint());
        self.message = Some("Search stopped unexpectedly and was restarted: edit the query to search again.".into());
    }

    /// Cursor row of the visible Movies/Series list.
    fn vod_cursor(&self) -> Option<usize> {
        match self.tab {
            Tab::Movies => self.movies.list().cursor(),
            Tab::Series => match self.series.open() {
                Some(o) => o.cursor(),
                None => self.series.search.list().cursor(),
            },
            _ => None,
        }
    }

    /// Adopt a new library (cache loaded or refresh done) atomically. The
    /// playback (and a pending zap) are re-pointed at it by favorite key, so
    /// the old library (~250 MB) is released.
    fn sync_library(&mut self) {
        let Some(new) = self.data.library().cloned() else { return };
        if self.shown_lib.as_ref().is_some_and(|l| Arc::ptr_eq(l, &new)) {
            return;
        }
        self.live.set_library(self.shown_lib.as_deref(), &new);
        self.live.set_filter(&new, &self.filter);
        self.fav.reload(Some(&new), &self.favs);
        if let Some(pb) = &mut self.playback {
            pb.rebase(new.clone());
        }
        rebase_pending(&mut self.zap, new.clone());
        let old = self.shown_lib.clone().unwrap_or_else(|| new.clone());
        let (m, s) = (self.movies.research(&old, &new, &self.hidden), self.series.search.research(&old, &new, &self.hidden));
        self.submit(m);
        self.submit(s);
        self.series.rebase(&old, &new, &self.hidden);
        drop(old);
        // Group list for the filter menu, off the UI thread.
        let (tx, rx) = channel();
        let (lib, ctx) = (new.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(vod_groups(&lib));
            ctx.request_repaint();
        });
        self.groups_rx = Some(rx);
        self.scroll_to = self.live.list().cursor();
        self.shown_lib = Some(new.clone());
        if !self.ready_logged {
            self.ready_logged = true;
            eprintln!(
                "startup: library ready after {} ms ({} entries)",
                self.t0.elapsed().as_millis(),
                new.entry_count()
            );
        }
    }

    // ---- playback ----------------------------------------------------------

    /// Load `pb` now (click/Enter, or a zap that became due). Cancels any
    /// pending zap.
    fn start(&mut self, pb: Playback) {
        self.zap.cancel();
        self.save_position();
        let resume = if pb.is_live() { None } else { self.positions.resume_for(pb.key()) };
        match &mut self.player {
            Ok(p) => {
                p.play_from(pb.url(), pb.is_live(), resume);
                let pref = Self::sub_show(&pb).and_then(|show| self.settings.sub_pref(&show).cloned());
                p.set_want_sub(pref);
            }
            Err(e) => self.message = Some(format!("Playback unavailable: {e}")),
        }
        self.sub_menu = false;
        self.seek.reset();
        self.previews.reset();
        // Episodes with a next episode get the "Next episode" offer.
        let next = pb
            .entry_index()
            .filter(|&e| !pb.is_live() && pb.library().entry(e).kind == Kind::Series)
            .and_then(|e| next_episode(pb.library(), e, &self.hidden));
        let enabled = self.settings.autoplay_next();
        match (next, enabled) {
            (Some(n), true) => eprintln!("autoplay: armed (next episode: {})", pb.library().entry(n).name),
            (None, _) => {
                let is_episode =
                    pb.entry_index().is_some_and(|e| !pb.is_live() && pb.library().entry(e).kind == Kind::Series);
                let why = if is_episode { "no next episode in the show" } else { "live or not a series episode" };
                eprintln!("autoplay: not armed: {why}");
            }
            (Some(_), false) => eprintln!("autoplay: not armed: autoplay_next is off in settings.json"),
        }
        self.autoplay.arm(next, enabled);
        self.arm_skip_intro(&pb, resume);
        // Replaces any older message (e.g. "Resuming at ..." of the last video).
        self.message = resume.map(|t| format!("Resuming at {}", clock(t)));
        if let Some((show, key)) = recent_id(&pb) {
            self.recent.touch(&show, &key);
            self.save_recent();
        }
        self.playback = Some(pb);
    }

    /// The playing episode reached its credits (or finished): the continue
    /// button then goes on to the next episode.
    fn mark_credits(&mut self, pb: &Playback) {
        if let Some((show, key)) = recent_id(pb) {
            self.recent.mark_credits(&show, &key);
            self.save_recent();
        }
    }

    fn save_recent(&mut self) {
        if let Err(e) = self.recent.save_if_dirty() {
            eprintln!("could not save recent series: {}", safe_text(&format!("{e:#}")));
        }
    }

    /// Shown rows of the recent-series list: nothing typed in the Series search box.
    fn recent_listed(&self) -> bool {
        self.tab == Tab::Series
            && self.view == View::Browse
            && self.series.open().is_none()
            && self.series.search.query().trim().is_empty()
            && !self.recent.items().is_empty()
    }

    /// Open recent show `i` at its recorded episode (no playback).
    fn open_recent(&mut self, i: usize) {
        let (Some(lib), Some(mut r)) = (self.shown_lib.clone(), self.recent.items().get(i).cloned()) else { return };
        r.credits = false; // the recorded episode itself, not the one after it
        match continue_target(&lib, &r, &self.hidden) {
            Some(t) => {
                self.series.open_show(&lib, t.show, Some(t.entry), &self.hidden);
                self.scroll_to = self.vod_cursor();
            }
            None => self.message = Some(format!("{} is not in the current playlist.", r.show)),
        }
    }

    /// The continue button / Shift+P: open the most recent show at the
    /// episode to continue with (the next one after the credits) and play it.
    fn continue_recent(&mut self) {
        let (Some(lib), Some(r)) = (self.shown_lib.clone(), self.recent.latest().cloned()) else { return };
        let Some(t) = continue_target(&lib, &r, &self.hidden) else {
            self.message = Some(format!("{} is not in the current playlist.", r.show));
            return;
        };
        self.tab = Tab::Series;
        self.view = View::Browse;
        self.series.open_show(&lib, t.show, Some(t.entry), &self.hidden);
        self.scroll_to = self.vod_cursor();
        if self.playing_entry() != Some(t.entry) {
            self.play_from(show_episodes(&lib, t.show, &self.hidden), t.entry, Tab::Series);
        } else {
            self.zap.cancel();
        }
    }

    /// Per frame: drive the "Next episode" offer (tail scan, countdown).
    /// `finished` = the episode reached its end this frame. Returns true if
    /// the next episode was started.
    fn update_autoplay(&mut self, finished: bool) -> bool {
        let Ok(p) = &self.player else { return false };
        let position = p.position();
        let now = Instant::now();
        // The countdown is held while paused, buffering or reconnecting.
        self.autoplay.set_running(now, p.status() == Status::Playing && !p.paused());
        let scan = match self.previews.scan_result() {
            ScanState::NotStarted | ScanState::Running => None,
            ScanState::Unavailable => Some(None),
            ScanState::Done(samples) => Some(Some(samples)),
        };
        let had_offer_time = self.autoplay.offer_at().is_some();
        let step = self.autoplay.update(now, position, scan, finished);
        if !had_offer_time {
            if let Some(t) = self.autoplay.offer_at() {
                let d = position.and_then(|(_, d)| d).unwrap_or(0.0);
                eprintln!("autoplay: offer time {t:.1} s (duration {d:.1} s)");
            }
        }
        match step {
            Step::StartScan(times) => {
                // `StartScan` is only asked for with a known duration.
                let duration = position.and_then(|(_, d)| d).unwrap_or(0.0);
                eprintln!(
                    "autoplay: tail scan requested at {:.1} s of {duration:.1} s ({} frames)",
                    position.map_or(0.0, |(p, _)| p),
                    times.len()
                );
                if let Some(pb) = &self.playback {
                    let url = pb.url().to_string();
                    let dump = self.console.then(|| {
                        let unix = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                        crate::scan_dump::dump_dir(&self.data_dir, pb.name(), unix)
                    });
                    self.previews.start_scan(&url, times, duration, dump);
                }
                false
            }
            Step::PlayNext => {
                if let Some(pb) = self.playback.clone() {
                    self.mark_credits(&pb);
                }
                self.play_next_episode(true)
            }
            Step::Offer => {
                eprintln!("autoplay: offer shown at {:.1} s", position.map_or(0.0, |(p, _)| p));
                if let Some(pb) = self.playback.clone() {
                    self.mark_credits(&pb);
                }
                false
            }
            Step::Nothing => false,
        }
    }

    /// A new playback: arm the "Skip intro" flow for a sorted series episode
    /// (stored reference for its show, or a neighbour episode to compare
    /// with); live, movies and `(Unsorted)` never scan, nor does an episode
    /// that resumes at or after `INTRO_SCAN_SECS` (nothing could be offered).
    fn arm_skip_intro(&mut self, pb: &Playback, resume: Option<f64>) {
        self.intro_scanner.cancel();
        self.intro_duration = None;
        let Some((show, _)) = recent_id(pb).filter(|_| resume.is_none_or(|r| r < INTRO_SCAN_SECS)) else {
            self.skip_intro.disarm();
            self.intro_logged = true;
            return;
        };
        let lib = pb.library();
        let neighbour = pb.entry_index().and_then(|e| neighbour_episode(lib, e, &self.hidden));
        let stored = self.intros.get(&show).cloned();
        eprintln!(
            "intro: armed (stored reference: {}, neighbour episode: {})",
            if stored.is_some() { "yes" } else { "no" },
            if neighbour.is_some() { "yes" } else { "no" }
        );
        self.skip_intro.arm(stored, neighbour.is_some());
        self.intro_urls = (pb.url().to_string(), neighbour.map(|n| lib.entry(n).url.clone()));
        self.intro_names = (pb.name().to_string(), neighbour.map(|n| lib.entry(n).name.clone()));
        self.intro_show = show;
        self.intro_logged = false;
    }

    /// Per frame: drive the "Skip intro" flow (scan results in, next scan or
    /// reference save out). Never logs a URL.
    fn update_skip_intro(&mut self) {
        let Ok(p) = &self.player else { return };
        if let Some(d) = p.position().and_then(|(_, d)| d) {
            self.intro_duration = Some(d);
        }
        // Always the playing episode's duration (also for the Reference scan).
        let duration = self.intro_duration;
        let mut actions = Vec::new();
        for r in self.intro_scanner.poll() {
            if r.error.is_some() {
                self.skip_intro.on_failed(r.job);
            } else {
                actions.push(self.skip_intro.on_result(r.job, r.prints, r.kinds, duration.unwrap_or(0.0)));
            }
        }
        actions.push(self.skip_intro.update(duration));
        for a in actions {
            match a {
                skip_intro::Action::None => {}
                skip_intro::Action::Scan { job, which, times } => {
                    let (name, url, episode, suffix) = match which {
                        Which::Own => ("own", Some(&self.intro_urls.0), Some(&self.intro_names.0), "intro"),
                        Which::Refine => ("refine", Some(&self.intro_urls.0), Some(&self.intro_names.0), "intro"),
                        Which::Reference => {
                            ("reference", self.intro_urls.1.as_ref(), self.intro_names.1.as_ref(), "intro-ref")
                        }
                    };
                    match url {
                        Some(url) => {
                            eprintln!("intro: scanning {name} ({} frames)", times.len());
                            // `--console`: keep the frames for tuning.
                            let dump = episode.filter(|_| self.console).map(|ep| {
                                let unix =
                                    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                                crate::scan_dump::suffixed_dump_dir(&self.data_dir, ep, suffix, unix)
                            });
                            self.intro_scanner.scan(job, url, times, dump);
                        }
                        None => self.skip_intro.on_failed(job),
                    }
                }
                skip_intro::Action::SaveReference(prints) => {
                    eprintln!("intro: reference saved for the show");
                    self.intros.put(&self.intro_show, prints);
                    if let Err(e) = self.intros.save_if_dirty() {
                        eprintln!("could not save intros: {}", safe_text(&format!("{e:#}")));
                    }
                }
            }
        }
        if !self.intro_logged {
            match (self.skip_intro.state_name(), self.skip_intro.span(), self.skip_intro.target()) {
                ("ready", Some((s, e)), Some(t)) => {
                    eprintln!("intro: span {s:.0}-{e:.0} s, skip to {t:.0} s");
                    self.intro_logged = true;
                }
                ("off", ..) => {
                    eprintln!("intro: none");
                    self.intro_logged = true;
                }
                _ => {}
            }
        }
    }

    /// The "Skip intro" prompt shows now (inside the intro, no seek drag).
    fn intro_offering(&self) -> bool {
        let Ok(p) = &self.player else { return false };
        let live = self.playback.as_ref().is_none_or(Playback::is_live);
        !live && !self.seek.dragging() && p.position().is_some_and(|(pos, _)| self.skip_intro.offering(pos))
    }

    /// "Skip intro" clicked or Enter: jump to the first scene after the intro.
    fn accept_intro(&mut self) {
        let Some(t) = self.skip_intro.target() else { return };
        eprintln!("intro: skipped to {t:.0} s");
        self.skip_intro.accept();
        if let Ok(p) = &mut self.player {
            p.seek_to(t);
            self.seek.sought(t, Instant::now());
        }
    }

    fn dismiss_intro(&mut self) {
        eprintln!("intro: prompt dismissed");
        self.skip_intro.dismiss();
    }

    /// Play the episode after the playing one (autoplay, "Next episode"
    /// accepted or the ">>" button), through `start` (resume, subtitle
    /// preference, positions). With `finished` (autoplay/offer) the old
    /// episode's position is forgotten; the ">>" button skips an unfinished
    /// episode, so its position stays (beyond what `start`'s save already
    /// forgets near the end). The open show's cursor moves to the new
    /// episode. False if there is no next episode (any more: e.g. the
    /// playlist changed).
    fn play_next_episode(&mut self, finished: bool) -> bool {
        match self.try_play_next_episode(finished) {
            Ok(name) => {
                eprintln!("autoplay: starting next episode: {name}");
                true
            }
            Err(f) => {
                eprintln!("autoplay: next episode could not be started: {}", f.reason());
                self.message = Some("Next episode unavailable".into());
                false
            }
        }
    }

    /// The playing episode has a next one (cached, cheap every frame).
    fn next_available(&mut self) -> bool {
        self.playback
            .as_ref()
            .is_some_and(|pb| !pb.is_live() && self.next_cache.get(pb.library(), pb.entry_index(), &self.hidden).is_some())
    }

    /// The ">>" button or Shift+Right: skip to the next episode now, independent of
    /// `autoplay_next` and the offer. The skipped episode keeps its position.
    fn skip_to_next_episode(&mut self) {
        eprintln!("autoplay: next episode button clicked");
        self.play_next_episode(false);
    }

    fn try_play_next_episode(&mut self, finished: bool) -> Result<String, NextFail> {
        let pb = self.playback.as_ref().ok_or(NextFail::NothingPlaying)?;
        let cur = pb.entry_index().ok_or(NextFail::NotInPlaylist)?;
        let lib = pb.library().clone();
        let next = next_episode(&lib, cur, &self.hidden).ok_or(NextFail::NoNextEpisode)?;
        let show = show_of(&lib, next).ok_or(NextFail::NoShow)?;
        let old_key = pb.key().to_string();
        let list = show_episodes(&lib, show, &self.hidden);
        self.start(Playback::new(lib.clone(), list, next, Tab::Series));
        if finished {
            self.positions.forget(&old_key);
            if let Err(e) = self.positions.save_if_dirty() {
                eprintln!("could not save positions: {}", safe_text(&format!("{e:#}")));
            }
        }
        if self.shown_lib.as_ref().is_some_and(|l| Arc::ptr_eq(l, &lib)) {
            if let Some(o) = self.series.open_mut().filter(|o| o.show == show) {
                o.select_entry(next);
                if self.tab == Tab::Series {
                    self.scroll_to = self.vod_cursor();
                }
            }
        }
        Ok(lib.entry(next).name.clone())
    }

    /// Open the subtitle menu on the row in use, or close it.
    fn toggle_sub_menu(&mut self) {
        self.sub_menu = !self.sub_menu;
        if self.sub_menu {
            let current = self.player.as_ref().ok().and_then(Player::current_sub);
            self.sub_cursor = self.sub_rows().iter().position(|r| *r == current).unwrap_or(0);
            self.sub_follow = true;
        }
    }

    /// Menu rows: Off, then the tracks of the playing video.
    fn sub_rows(&self) -> Vec<Option<i64>> {
        let tracks = self.player.as_ref().map(Player::sub_tracks).unwrap_or_default();
        std::iter::once(None).chain(tracks.iter().map(|t| Some(t.id))).collect()
    }

    /// Up/Down move the highlight (wrapping), Enter picks the row and closes the menu.
    fn sub_menu_keys(&mut self, up: bool, down: bool, enter: bool) {
        let rows = self.sub_rows();
        let n = rows.len();
        self.sub_cursor = self.sub_cursor.min(n - 1);
        self.sub_follow |= up || down;
        if up {
            self.sub_cursor = (self.sub_cursor + n - 1) % n;
        }
        if down {
            self.sub_cursor = (self.sub_cursor + 1) % n;
        }
        if enter {
            self.choose_sub(rows[self.sub_cursor]);
        }
    }

    /// Switch subtitles now (playback continues), remember the choice for the show, close the menu.
    fn choose_sub(&mut self, id: Option<i64>) {
        self.sub_menu = false;
        if let Ok(p) = &mut self.player {
            let choice = p.select_sub(id);
            self.remember_sub(choice);
        }
    }

    /// Show name of an episode (the key of its remembered subtitle choice).
    fn sub_show(pb: &Playback) -> Option<String> {
        let kind = pb.entry_index().map(|i| pb.library().entry(i).kind)?;
        (kind == Kind::Series).then(|| iptv::series::parse_episode(pb.name()).map(|r| r.show)).flatten()
    }

    /// The user picked subtitles: remember the choice for the show's next episodes.
    fn remember_sub(&mut self, choice: subtitles::SubChoice) {
        let Some(show) = self.playback.as_ref().and_then(Self::sub_show) else { return };
        self.settings.set_sub_pref(&show, choice);
        if let Err(e) = self.settings.save() {
            self.message = Some(safe_text(&format!("Could not save settings: {e:#}")));
        }
    }

    /// Remember where the playing movie/episode is (live is never saved).
    fn save_position(&mut self) {
        self.positions_saved = Instant::now();
        if let (Some(pb), Ok(p)) = (&self.playback, &self.player) {
            if !pb.is_live() {
                if let Some((pos, dur)) = p.position() {
                    self.positions.record(pb.key(), pos, dur);
                }
            }
        }
        if let Err(e) = self.positions.save_if_dirty() {
            eprintln!("could not save positions: {}", safe_text(&format!("{e:#}")));
        }
    }

    fn play_from(&mut self, list: Vec<usize>, entry: usize, tab: Tab) {
        if let Some(lib) = self.shown_lib.clone() {
            self.start(Playback::new(lib, list, entry, tab));
        }
    }

    /// Fullscreen = the OS window fills the screen and the player fills the
    /// window (no channel selector). Leaving restores the view from before.
    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
        if on {
            // Already fullscreen: keep the view to return to.
            self.fs_return_view.get_or_insert(self.view);
            if self.playback.is_some() {
                self.view = View::Player;
            }
        } else if let Some(v) = self.fs_return_view.take() {
            self.view = v;
        }
    }

    /// Hides the mouse cursor in fullscreen after `cursor::HIDE_AFTER` without
    /// movement; any movement, click or scroll shows it again at once.
    fn update_cursor(&mut self, ctx: &egui::Context) {
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let moved = ctx.input(|i| {
            i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_pressed() || i.smooth_scroll_delta != egui::Vec2::ZERO
        });
        // Entering fullscreen counts as activity (cursor and title show first).
        if moved || (fullscreen && !self.was_fullscreen) {
            self.cursor_moved = Instant::now();
        }
        self.was_fullscreen = fullscreen;
        let still = self.cursor_moved.elapsed();
        let hide = cursor::hidden(fullscreen, still);
        if hide != self.cursor_hidden {
            self.cursor_hidden = hide;
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(!hide));
        }
        if fullscreen && !hide {
            ctx.request_repaint_after(cursor::HIDE_AFTER.saturating_sub(still));
        }
        if fullscreen && cursor::title_visible(true, still) {
            ctx.request_repaint_after(cursor::TITLE_SHOW.saturating_sub(still));
        }
    }

    fn stop(&mut self) {
        self.zap.cancel();
        self.save_position();
        if let Ok(p) = &mut self.player {
            p.stop();
        }
        self.playback = None;
        self.sub_menu = false;
        self.previews.reset();
        self.autoplay.disarm();
        self.intro_scanner.cancel();
        self.skip_intro.disarm();
        self.intro_logged = true;
        self.view = View::Browse;
    }

    /// Load the zap target once navigation has been quiet for ZAP_DELAY.
    fn fire_zap(&mut self) {
        if let Some(pb) = self.zap.due(Instant::now()) {
            self.start(pb);
        }
    }

    /// Entry (in the shown library) that is loaded in the player.
    fn playing_entry(&self) -> Option<usize> {
        let lib = self.shown_lib.as_ref()?;
        self.playback.as_ref().filter(|p| p.same_library(lib)).and_then(Playback::entry_index)
    }

    /// The pending zap target, else the loaded entry (Up/Down start point).
    fn target_entry(&self) -> Option<usize> {
        let lib = self.shown_lib.as_ref()?;
        self.zap.pending().filter(|p| p.same_library(lib)).and_then(Playback::entry_index).or(self.playing_entry())
    }

    /// VOD reached its natural end and autoplay did not start the next
    /// episode (see `update_autoplay`): stop and leave the list it was
    /// started from with it selected. The visible tab is not changed.
    fn on_finished(&mut self) {
        eprintln!("autoplay: finished without autoplay (state: {})", self.autoplay.state_name());
        let Some(pb) = self.playback.take() else { return };
        if pb.is_live() {
            self.playback = Some(pb);
            return;
        }
        self.positions.forget(pb.key());
        self.mark_credits(&pb);
        self.stop();
        self.message = Some(format!("Finished: {}", pb.name()));
        let e = pb.entry_index().filter(|_| self.shown_lib.as_ref().is_some_and(|l| pb.same_library(l)));
        if let Some(e) = e {
            match pb.tab {
                Tab::Movies => {
                    self.movies.list_mut().select_entry(e);
                }
                Tab::Series => {
                    if let Some(o) = self.series.open_mut() {
                        o.select_entry(e);
                    }
                }
                _ => {}
            }
        }
        if pb.tab == self.tab {
            self.scroll_to = self.vod_cursor();
        }
    }

    /// The Up/Down list of the visible tab.
    fn tab_list(&self) -> Vec<usize> {
        match self.tab {
            Tab::Live => self.live.list().items().to_vec(),
            Tab::Favorites => self.fav.playable_entries(),
            Tab::Movies => self.movies.list().items().to_vec(),
            Tab::Series => self.series.open().map(|o| o.episodes()).unwrap_or_default(),
        }
    }

    /// Up/Down: move the selection at once; if a live channel is playing,
    /// queue the new channel as a debounced zap (loaded after ZAP_DELAY
    /// without further navigation, so key repeat never opens a connection
    /// per key). In the player view, walk the list live playback started
    /// from. Movies/Series lists only move the cursor (VOD never auto-plays).
    fn step(&mut self, delta: i32) {
        let now = Instant::now();
        if self.view == View::Player {
            let base = self.zap.pending().or(self.playback.as_ref()).cloned();
            if let Some(mut pb) = base.filter(Playback::is_live) {
                if let Some(e) = pb.step(delta) {
                    if pb.tab == Tab::Live && self.shown_lib.as_ref().is_some_and(|l| pb.same_library(l)) {
                        self.live.list_mut().select_entry(e);
                        self.scroll_to = self.live.list().cursor();
                    }
                    queue_zap(&mut self.zap, self.playback.as_ref(), pb, now);
                }
            }
            return;
        }
        match self.tab {
            Tab::Movies => {
                self.movies.list_mut().step(delta);
                self.scroll_to = self.vod_cursor();
                return;
            }
            Tab::Series if self.recent_listed() => {
                let n = self.recent.items().len() as i32;
                let next = match self.recent_cursor {
                    Some(c) => (c as i32 + delta).rem_euclid(n),
                    None if delta >= 0 => 0,
                    None => n - 1,
                };
                self.recent_cursor = Some(next as usize);
                self.scroll_to = self.recent_cursor;
                return;
            }
            Tab::Series => {
                let _ = match self.series.open_mut() {
                    Some(o) => o.step(delta),
                    None => self.series.search.list_mut().step(delta),
                };
                self.scroll_to = self.vod_cursor();
                return;
            }
            _ => {}
        }
        let playing = self.target_entry();
        let picked = match self.tab {
            Tab::Live => {
                // Nothing selected but the playing channel is listed: step from it.
                if let (None, Some(p)) = (self.live.list().cursor(), playing) {
                    self.live.list_mut().select_entry(p);
                }
                let e = self.live.list_mut().step(delta);
                self.scroll_to = self.live.list().cursor();
                e
            }
            Tab::Favorites => {
                let e = self.fav.step(delta);
                self.scroll_to = self.fav.cursor();
                e
            }
            Tab::Movies | Tab::Series => None,
        };
        // Only while live plays (VOD playback never zaps).
        let active = self.playback.as_ref().is_some_and(Playback::is_live) || self.zap.pending().is_some();
        if let (Some(e), true, Some(lib)) = (picked, active, self.shown_lib.clone()) {
            let target = Playback::new(lib, self.tab_list(), e, self.tab);
            queue_zap(&mut self.zap, self.playback.as_ref(), target, now);
        }
    }

    /// Enter: play the selected row; on the row already loaded, open the
    /// full player view (cancelling a pending zap, no reload). Series show
    /// list: open the show. Movies/shows with nothing selected: select the
    /// first result.
    fn activate(&mut self, ctx: &egui::Context) {
        match self.tab {
            Tab::Movies if self.movies.list().cursor().is_none() => {
                self.movies.list_mut().step(1);
                self.scroll_to = self.vod_cursor();
                return;
            }
            Tab::Series if self.recent_listed() => {
                // Nothing selected: the most recent show.
                self.open_recent(self.recent_cursor.unwrap_or(0));
                return;
            }
            Tab::Series if self.series.open().is_none() => {
                if self.series.search.list().cursor().is_none() {
                    self.series.search.list_mut().step(1);
                }
                if let Some(lib) = self.shown_lib.clone() {
                    if self.series.open_selected(&lib, &self.hidden) {
                        self.scroll_to = self.vod_cursor();
                    }
                }
                return;
            }
            _ => {}
        }
        match enter_action(self.selected_entry(), self.playing_entry()) {
            EnterAction::OpenPlayer => {
                self.zap.cancel();
                self.set_fullscreen(ctx, true);
            }
            EnterAction::Play(e) => self.play_from(self.tab_list(), e, self.tab),
            EnterAction::Nothing if self.tab == Tab::Favorites && self.fav.cursor().is_some() => {
                self.message = Some("This favorite is not in the current playlist.".into())
            }
            EnterAction::Nothing => {}
        }
    }

    fn selected_entry(&self) -> Option<usize> {
        match self.tab {
            Tab::Live => self.live.list().current(),
            Tab::Favorites => self.fav.play_target(),
            Tab::Movies => self.movies.list().current(),
            Tab::Series => self.series.open().and_then(|o| o.current()),
        }
    }

    /// Series: back from the episodes to the show list.
    fn series_back(&mut self) {
        if self.series.back() {
            self.focus_search = true;
            self.scroll_to = self.vod_cursor();
        }
    }

    /// Groups hidden/shown: persist (reverted on a save error), then re-run
    /// both searches.
    fn set_groups_hidden(&mut self, groups: &[String], hidden: bool) {
        let before = self.settings.hidden_groups.clone();
        for g in groups {
            self.settings.set_hidden(g, hidden);
        }
        if let Err(e) = self.settings.save() {
            self.settings.hidden_groups = before;
            self.message = Some(safe_text(&format!("Could not save settings: {e:#}")));
            return;
        }
        self.hidden = Arc::new(self.settings.hidden_groups.iter().cloned().collect::<HashSet<_>>());
        if let Some(lib) = self.shown_lib.clone() {
            let (m, s) = (self.movies.research(&lib, &lib, &self.hidden), self.series.search.research(&lib, &lib, &self.hidden));
            self.submit(m);
            self.submit(s);
            self.series.refilter(&lib, &self.hidden);
        }
    }

    fn set_vod_query(&mut self, tab: Tab, text: String) {
        let Some(lib) = self.shown_lib.clone() else { return };
        let req = match tab {
            Tab::Series => self.series.search.set_query(&lib, &self.hidden, &text),
            _ => self.movies.set_query(&lib, &self.hidden, &text),
        };
        self.submit(req);
    }

    /// `F`: Favorites tab = the row under the cursor; otherwise the playing
    /// channel, else the selected one (see `state::favs::toggle_target`).
    fn toggle_favorite(&mut self) {
        let fav_row = self.fav.cursor().and_then(|c| self.fav.rows().get(c)).cloned();
        let playing = self.playback.as_ref().map(|p| (p.key().to_string(), p.name().to_string()));
        let selected = self.shown_lib.as_ref().zip(self.selected_entry()).map(|(lib, e)| {
            let en = lib.entry(e);
            (favorite_key(en), en.name.clone())
        });
        let Some((key, label)) = toggle_target(self.tab, fav_row.as_ref(), playing, selected) else { return };
        self.message = Some(match toggle_key(&mut self.favs, &key, &label) {
            Ok(true) => format!("Added to favorites: {label}"),
            Ok(false) => format!("Removed from favorites: {label}"),
            Err(e) => safe_text(&e),
        });
        self.fav.reload(self.shown_lib.as_deref(), &self.favs);
    }

    /// Move the playing movie/episode by `delta` seconds.
    fn seek_by(&mut self, delta: f64) {
        let Ok(p) = &mut self.player else { return };
        let Some((pos, dur)) = p.position() else { return };
        // Step from what the bar shows, so quick presses add up.
        let now = Instant::now();
        let from = self.seek.shown(pos, now);
        let target = seek::step_target(from, delta, dur);
        p.seek_to(target);
        self.seek.sought(target, now);
        self.key_seek = Some(now);
        self.activity = Some(now);
    }

    // ---- input -------------------------------------------------------------

    fn open_help(&mut self, ctx: &egui::Context) {
        // Drop keyboard focus from the text boxes so typing does not land in them.
        for id in [filter_id(), vod::movie_search_id(), vod::series_search_id()] {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
        self.sub_menu = false;
        self.help = Some(HelpTab::Keys);
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers as M};
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { pressed: true, .. }))) {
            self.activity = Some(Instant::now());
        }
        // The playlist URL prompt is modal: no shortcut works behind it.
        if self.url_prompt.is_some() {
            return;
        }
        let focused = [filter_id(), vod::movie_search_id(), vod::series_search_id()]
            .into_iter()
            .find(|id| ctx.memory(|m| m.has_focus(*id)));
        // Help card: F1 or ? (not while typing) toggles it. While open it owns
        // the keyboard: Esc/F1/? close it, nothing else reaches the app.
        let question = ctx.input_mut(|i| {
            let q = i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "?"));
            if q {
                // Shift+/ would otherwise also count as the "/" filter key.
                i.consume_key(M::SHIFT, Key::Slash);
            }
            q
        });
        let f1 = ctx.input_mut(|i| i.consume_key(M::NONE, Key::F1));
        if self.help.is_some() {
            if f1 || question || ctx.input_mut(|i| i.consume_key(M::NONE, Key::Escape)) {
                self.help = None;
            } else if let Some(tab) = self.help {
                // Tab / Shift+Tab switch the card's tabs (`help::show` keeps egui's
                // own Tab focus movement away from the widgets behind it).
                let next = ctx.input_mut(|i| i.consume_key(M::NONE, Key::Tab));
                let prev = ctx.input_mut(|i| i.consume_key(M::SHIFT, Key::Tab));
                if next || prev {
                    self.help = Some(tab.cycle(next));
                }
            }
            return;
        }
        if f1 || (question && focused.is_none() && !ctx.egui_wants_keyboard_input()) {
            self.open_help(ctx);
            return;
        }
        // F11 / Alt+Enter: before plain Enter is consumed below.
        let toggle_fs = ctx.input_mut(|i| i.consume_key(M::NONE, Key::F11) | i.consume_key(M::ALT, Key::Enter));
        let (up, down, enter, esc, f5, ctrl_f) = ctx.input_mut(|i| {
            (
                i.consume_key(M::NONE, Key::ArrowUp),
                i.consume_key(M::NONE, Key::ArrowDown),
                i.consume_key(M::NONE, Key::Enter),
                i.consume_key(M::NONE, Key::Escape),
                i.consume_key(M::NONE, Key::F5),
                i.consume_key(M::COMMAND, Key::F),
            )
        });
        // Esc closes the subtitle menu before it does anything else.
        let esc = if esc && self.sub_menu {
            self.sub_menu = false;
            false
        } else {
            esc
        };
        let typing = ctx.egui_wants_keyboard_input();
        // The "Next episode" offer owns Esc (dismiss only: no stop, no leaving
        // fullscreen this press) and, in the player view (also fullscreen),
        // Enter (play now). In the browse view Enter keeps its list meaning;
        // the offer's buttons can still be clicked there.
        let offer_keys = self.autoplay.offering() && !typing && !self.sub_menu;
        let offer_enter = offer_keys && enter && self.view == View::Player && self.focused_tab.is_none();
        let offer_esc = offer_keys && esc && !offer_enter;
        if offer_enter {
            eprintln!("autoplay: offer accepted");
            self.autoplay.accept();
            self.play_next_episode(true);
        } else if offer_esc {
            eprintln!("autoplay: offer dismissed");
            self.autoplay.dismiss();
        }
        let (enter, esc) = if offer_enter || offer_esc { (false, false) } else { (enter, esc) };
        // The "Skip intro" prompt: the same rules, unless the "Next episode"
        // offer shows (that one wins the keys).
        let intro_gate = KeyGate {
            offering: self.intro_offering(),
            next_offer: self.autoplay.offering(),
            typing,
            sub_menu: self.sub_menu,
            player_view: self.view == View::Player,
            tab_focused: self.focused_tab.is_some(),
        };
        let intro_key = intro_gate.key(enter, esc);
        match intro_key {
            IntroKey::Accept => self.accept_intro(),
            IntroKey::Dismiss => self.dismiss_intro(),
            IntroKey::Ignore => {}
        }
        let (enter, esc) = if intro_key == IntroKey::Ignore { (enter, esc) } else { (false, false) };
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if toggle_fs || (esc && fullscreen) {
            // Esc only leaves fullscreen; it does nothing else this press.
            self.set_fullscreen(ctx, !fullscreen);
        }
        let esc = esc && !fullscreen;
        // The open subtitle menu owns Up/Down/Enter.
        let (up, down, enter) = if self.sub_menu {
            self.sub_menu_keys(up, down, enter);
            (false, false, false)
        } else {
            (up, down, enter)
        };
        // Shift+Right = the ">>" button. Consumed first: the Right handlers
        // below ignore extra Shift and would otherwise seek / act as Enter.
        // Shift+Left is left alone (still seeks back like Left).
        let has_next = self.next_available();
        match shift_right(typing, self.focused_tab.is_some(), self.sub_menu, has_next) {
            ShiftRight::Ignore => {}
            ShiftRight::Swallow => {
                ctx.input_mut(|i| i.consume_key(M::SHIFT, Key::ArrowRight));
            }
            ShiftRight::PlayNext => {
                if ctx.input_mut(|i| i.consume_key(M::SHIFT, Key::ArrowRight)) {
                    self.skip_to_next_episode();
                }
            }
        }
        // Shift+P = the continue button (not while typing or in the subtitle menu).
        if !typing && !self.sub_menu && self.view == View::Browse && ctx.input_mut(|i| i.consume_key(M::SHIFT, Key::P)) {
            self.continue_recent();
        }
        // Left/Right step through the playing movie/episode (before the
        // browse keys below, which would otherwise take them).
        if self.playback.as_ref().is_some_and(|p| !p.is_live()) && !typing && !self.sub_menu && self.focused_tab.is_none() {
            let dir = ctx.input_mut(|i| {
                i.consume_key(M::NONE, Key::ArrowRight) as i32 - i.consume_key(M::NONE, Key::ArrowLeft) as i32
            });
            if dir != 0 {
                self.seek_by(dir as f64 * seek::STEP_SECS);
            }
        }
        // Live browse: Left goes to the country list, Right = Enter. Not while a
        // tab button has focus (arrows move between the buttons then).
        let live_keys = self.tab == Tab::Live
            && self.view == View::Browse
            && !fullscreen
            && !self.sub_menu
            && !typing
            && self.focused_tab.is_none();
        let country_active = live_keys && self.live.countries_focused();
        let (left, right) = if live_keys {
            ctx.input_mut(|i| (i.consume_key(M::NONE, Key::ArrowLeft), i.consume_key(M::NONE, Key::ArrowRight)))
        } else {
            (false, false)
        };
        let enter = enter || right;
        // C opens/closes the subtitle menu (movies and episodes only).
        let vod_playing = self.playback.as_ref().is_some_and(|p| !p.is_live());
        let (fav, space, slash, back, sub_key) = if typing {
            (false, false, false, false, false)
        } else {
            ctx.input_mut(|i| {
                (
                    i.consume_key(M::NONE, Key::F),
                    i.consume_key(M::NONE, Key::Space),
                    i.consume_key(M::NONE, Key::Slash),
                    // Only consumed where it means "back" (it scrolls elsewhere).
                    self.tab == Tab::Series
                        && self.view == View::Browse
                        && self.series.open().is_some()
                        && (i.consume_key(M::NONE, Key::Backspace) | i.consume_key(M::NONE, Key::ArrowLeft)),
                    vod_playing && i.consume_key(M::NONE, Key::C),
                )
            })
        };
        if back {
            self.series_back();
        }
        if sub_key {
            self.toggle_sub_menu();
        }
        if left {
            self.live.focus_countries();
            self.country_scroll = self.live.row_index();
        }
        if (up || down) && country_active {
            if let Some(lib) = self.shown_lib.clone() {
                self.live.step_row(&lib, if up { -1 } else { 1 });
                self.country_scroll = self.live.row_index();
                self.scroll_to = Some(0);
            }
        } else {
            if up {
                self.step(-1);
            }
            if down {
                self.step(1);
            }
        }
        if enter {
            if country_active {
                // Into the channels: select the first one; below it is played.
                self.live.focus_channels();
                self.scroll_to = self.live.list().cursor();
            }
            if let Some(id) = focused {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
            if let Some((tab, focus_search)) = tab_enter_visible(self.focused_tab, !fullscreen && self.view == View::Browse) {
                // Enter on a focused tab button opens that tab.
                self.tab = tab;
                self.view = View::Browse;
                self.focus_search = focus_search;
                self.focused_tab = None;
                if let Some(id) = self.focused_tab_id.take() {
                    ctx.memory_mut(|m| m.surrender_focus(id));
                }
            } else if fullscreen && self.playback.is_some() {
                // Enter in fullscreen leaves it again.
                self.set_fullscreen(ctx, false);
            } else if country_active && self.selected_entry() == self.playing_entry() {
                // Entering the channels of the country that already plays: no fullscreen.
            } else {
                self.activate(ctx);
            }
        }
        if esc {
            if let Some(id) = focused {
                ctx.memory_mut(|m| m.surrender_focus(id));
            } else if self.tab == Tab::Live && self.view == View::Browse && self.live.countries_focused() {
                self.live.focus_channels();
            } else if self.view == View::Player || self.playback.is_some() {
                self.stop();
            } else {
                match self.tab {
                    Tab::Series if self.series.open().is_some() => self.series_back(),
                    Tab::Series if !self.series.search.query().is_empty() => {
                        self.set_vod_query(Tab::Series, String::new())
                    }
                    Tab::Movies if !self.movies.query().is_empty() => self.set_vod_query(Tab::Movies, String::new()),
                    _ if !self.filter.is_empty() => self.set_filter(String::new()),
                    _ => {}
                }
            }
        }
        if f5 {
            self.request_refresh(false);
        }
        if ctrl_f || slash {
            self.view = View::Browse;
            match self.tab {
                Tab::Movies => self.focus_search = true,
                Tab::Series => {
                    self.series.back();
                    self.focus_search = true;
                }
                _ => {
                    self.tab = Tab::Live;
                    self.live.focus_channels();
                    self.focus_filter = true;
                }
            }
        }
        if fav {
            self.toggle_favorite();
        }
        if space {
            if let Ok(p) = &mut self.player {
                p.toggle_pause();
            }
        }
    }

    fn set_filter(&mut self, text: String) {
        self.filter = text;
        if let Some(lib) = self.shown_lib.clone() {
            self.live.set_filter(&lib, &self.filter);
            self.scroll_to = Some(self.live.list().cursor().unwrap_or(0));
        }
    }

    // ---- drawing -----------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                self.focused_tab = None;
                self.focused_tab_id = None;
                for t in Tab::ALL {
                    let resp = ui.selectable_label(self.tab == t, t.label());
                    if resp.has_focus() {
                        self.focused_tab = Some(t);
                        self.focused_tab_id = Some(resp.id);
                    }
                    if resp.clicked() {
                        self.tab = t;
                    }
                }
                self.continue_button(ui);
                ui.separator();
                let running = matches!(self.data.refresh(), RefreshState::Running { .. });
                if ui.add_enabled(!running, egui::Button::new("Refresh")).on_hover_text("F5").clicked() {
                    self.request_refresh(false);
                }
                if let RefreshState::Failed { can_force: true, .. } = self.data.refresh() {
                    if ui
                        .button("Force refresh")
                        .on_hover_text("Replace the cached playlist even though the new one is much smaller")
                        .clicked()
                    {
                        self.request_refresh(true);
                    }
                }
                if self.tab == Tab::Live {
                    ui.separator();
                    let mut text = self.filter.clone();
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut text)
                            .id(filter_id())
                            .hint_text("Filter channels (Ctrl+F)")
                            .desired_width(240.0),
                    );
                    if self.focus_filter {
                        r.request_focus();
                        self.focus_filter = false;
                    }
                    if r.changed() {
                        self.set_filter(text);
                    }
                }
                if ui.button("?").on_hover_text("Keyboard shortcuts and settings (F1)").clicked() {
                    self.open_help(ui.ctx());
                }
            });
        });
    }

    /// Play triangle + the name of the most recent show, right of the tabs.
    /// The triangle is a shape: the default fonts lack the symbol glyph.
    fn continue_button(&mut self, ui: &mut egui::Ui) {
        let Some(show) = self.recent.latest().map(|r| r.show.clone()) else { return };
        let name = if show.chars().count() > 28 {
            format!("{}…", show.chars().take(27).collect::<String>())
        } else {
            show.clone()
        };
        let r = ui
            .add_enabled(self.shown_lib.is_some(), egui::Button::new(format!("      {name}")))
            .on_hover_text(format!("Continue {show} (Shift+P)"));
        let c = egui::pos2(r.rect.left() + 14.0, r.rect.center().y);
        let color = if r.enabled() { egui::Color32::from_rgb(40, 170, 90) } else { ui.visuals().weak_text_color() };
        let pts = vec![c + egui::vec2(-4.0, -6.0), c + egui::vec2(6.0, 0.0), c + egui::vec2(-4.0, 6.0)];
        ui.painter().add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
        if r.clicked() {
            self.continue_recent();
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                match self.data.refresh() {
                    RefreshState::Running { bytes, forced } => {
                        ui.spinner();
                        let what = if *forced { "Force-refreshing playlist" } else { "Refreshing playlist" };
                        ui.label(format!("{what}… {}", format_bytes(*bytes)));
                    }
                    RefreshState::Failed { msg, can_force } => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("Refresh failed: {}", safe_text(msg)));
                        if *can_force {
                            ui.label(
                                "— if your subscription really shrank, press “Force refresh”; \
                                 otherwise try again later.",
                            );
                        }
                    }
                    RefreshState::Idle => {
                        if let Some(lib) = &self.shown_lib {
                            let live: usize = lib.countries().iter().map(|c| c.channels.len()).sum();
                            ui.label(format!(
                                "{} entries · {} live channels in {} countries · {} movies · {} shows · {} favorites",
                                lib.entry_count(),
                                live,
                                lib.countries().len(),
                                lib.movies().len(),
                                lib.shows().len(),
                                self.favs.list().len()
                            ));
                        }
                    }
                }
                if let Some(w) = self.data.warning().map(safe_text) {
                    ui.separator();
                    ui.colored_label(ui.visuals().warn_fg_color, format!("Warning: {w}"));
                    if ui.small_button("x").clicked() {
                        self.data.dismiss_warning();
                    }
                }
                if let Some(m) = self.message.clone() {
                    ui.separator();
                    ui.label(m);
                    if ui.small_button("x").clicked() {
                        self.message = None;
                    }
                }
            });
        });
    }

    /// Main area when no library is available yet.
    fn phase_screen(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| match self.data.phase().clone() {
                Phase::Loading => {
                    ui.spinner();
                    match self.data.refresh() {
                        RefreshState::Running { bytes, .. } => {
                            ui.label(format!("Downloading playlist… {}", format_bytes(*bytes)))
                        }
                        _ => ui.label("Loading…"),
                    };
                }
                // The URL prompt (a modal over this screen) does the asking.
                Phase::NoConfig { .. } => {
                    ui.weak("Waiting for the playlist URL…");
                }
                Phase::RefreshFailed { msg, can_force } => {
                    ui.heading("Could not load the playlist");
                    ui.colored_label(ui.visuals().error_fg_color, safe_text(&msg));
                    if ui.button("Try again").clicked() {
                        self.request_refresh(false);
                    }
                    if can_force && ui.button("Force refresh").clicked() {
                        self.request_refresh(true);
                    }
                }
                Phase::Ready => {}
            });
        });
    }

    /// Seek bar in `strip` (below the video): elapsed time, a thin line with a
    /// dot at the position, total time. Click or drag to move; the seek
    /// happens on release.
    fn seek_bar(&mut self, ui: &mut egui::Ui, strip: egui::Rect, position: Option<(f64, Option<f64>)>) {
        const PAD: f32 = 12.0;
        const TIME_W: f32 = 70.0;
        let painter = ui.painter_at(strip);
        painter.rect_filled(strip, 0.0, egui::Color32::BLACK);
        let Some((pos, Some(dur))) = position.filter(|(_, d)| d.is_some_and(|d| d > 0.0)) else {
            return; // duration unknown (yet): nothing to seek in
        };
        let now = Instant::now();
        // Only after mouse/keyboard activity (or while dragging / a menu is open).
        let left = self.activity.map_or(Duration::ZERO, |t| TIME_SHOW.saturating_sub(t.elapsed()));
        if left.is_zero() && !self.seek.dragging() && !self.sub_menu {
            return;
        }
        ui.ctx().request_repaint_after(left);
        let bar = egui::Rect::from_min_max(
            egui::pos2(strip.left() + PAD + TIME_W, strip.top()),
            egui::pos2(strip.right() - PAD - TIME_W, strip.bottom()),
        );
        let resp = ui.interact(bar, egui::Id::unique("seek_bar"), egui::Sense::click_and_drag());
        let at = |x: f32| seek::target_at(seek::fraction(x, bar.left(), bar.right()), dur);
        if resp.drag_started() || resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.seek.drag(at(p.x));
            }
        }
        let mut target = None;
        if resp.drag_stopped() {
            target = self.seek.release(now);
        } else if resp.clicked() {
            target = resp.interact_pointer_pos().map(|p| at(p.x));
        }
        if let Some(t) = target {
            if let Ok(p) = &mut self.player {
                p.seek_to(t);
            }
            self.seek.sought(t, now);
        }
        let shown = self.seek.shown(pos, now).clamp(0.0, dur);
        let active = resp.hovered() || self.seek.dragging();
        let white = egui::Color32::from_white_alpha(if active { 255 } else { 200 });
        let (y, x0, x1) = (bar.center().y, bar.left(), bar.right());
        let x = x0 + (x1 - x0) * (shown / dur) as f32;
        painter.line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], egui::Stroke::new(3.0, egui::Color32::from_white_alpha(70)));
        painter.line_segment([egui::pos2(x0, y), egui::pos2(x, y)], egui::Stroke::new(3.0, white));
        painter.circle_filled(egui::pos2(x, y), if active { 8.0 } else { 6.0 }, white);
        let font = egui::FontId::monospace(13.0);
        let dim = egui::Color32::from_white_alpha(200);
        painter.text(egui::pos2(strip.left() + PAD, y), egui::Align2::LEFT_CENTER, format_hms(shown), font.clone(), dim);
        painter.text(egui::pos2(strip.right() - PAD, y), egui::Align2::RIGHT_CENTER, format_hms(dur), font.clone(), dim);
        let mouse = resp.hovered() || self.seek.dragging();
        if mouse {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        // Time and preview frame above the mouse, or above the dot for a moment
        // after an arrow-key seek.
        let keying = self.key_seek.map_or(Duration::ZERO, |t| KEY_PREVIEW.saturating_sub(t.elapsed()));
        if !keying.is_zero() {
            ui.ctx().request_repaint_after(keying);
        }
        let anchor = match ui.ctx().pointer_latest_pos() {
            Some(m) if mouse => Some((m.x, if self.seek.dragging() { shown } else { at(m.x) })),
            _ if !keying.is_zero() => Some((x, shown)),
            _ => None,
        };
        if let Some((ax, secs)) = anchor {
            let url = self.playback.as_ref().map(|p| p.url().to_string());
            if let Some(tex) = url.and_then(|u| self.previews.at(&u, secs)) {
                let size = tex.size_vec2();
                let cx = ax.clamp(strip.left() + size.x / 2.0, strip.right() - size.x / 2.0);
                let r = egui::Rect::from_center_size(egui::pos2(cx, strip.top() - 32.0 - size.y / 2.0), size);
                let pt = ui.painter();
                pt.image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                pt.rect_stroke(r, 0.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(160)), egui::StrokeKind::Outside);
            }
            let galley = ui.painter().layout_no_wrap(format_hms(secs), font, egui::Color32::WHITE);
            let w = galley.size() + egui::vec2(12.0, 6.0);
            let cx = ax.clamp(bar.left(), bar.right());
            let bg = egui::Rect::from_center_size(egui::pos2(cx, strip.top() - 14.0), w);
            let pt = ui.painter();
            pt.rect_filled(bg, 4.0, egui::Color32::from_black_alpha(200));
            pt.galley(bg.center() - galley.size() / 2.0, galley, egui::Color32::WHITE);
        }
    }

    fn video_area(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = ui.max_rect();
            let mut toggle_pause = false;
            let mut toggle_fs = false;
            let mut toggle_sub = false;
            let mut pick_sub: Option<Option<i64>> = None;
            let mut bar_job: Option<(egui::Rect, Option<(f64, Option<f64>)>)> = None;
            // "Next episode" offer clicked: Some(true) = play now, Some(false) = cancel.
            let mut offer: Option<bool> = None;
            // "Skip intro" clicked: Some(true) = skip, Some(false) = cancel.
            let mut intro: Option<bool> = None;
            // ">>" (next episode) button clicked.
            let mut next_clicked = false;
            match (&self.player, &self.playback) {
                (Ok(p), Some(pb)) => {
                    // Movies/episodes: a strip below the video holds the seek bar.
                    let full = rect;
                    let (rect, bar_rect) = if pb.is_live() {
                        (rect, None)
                    } else {
                        let split = rect.bottom() - SEEK_BAR_H;
                        (
                            egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), split)),
                            Some(egui::Rect::from_min_max(egui::pos2(rect.left(), split), rect.max)),
                        )
                    };
                    let fav = if self.favs.contains(pb.key()) { "  · Favorite" } else { "" };
                    let gone = if pb.in_playlist() { "" } else { "  (not in playlist)" };
                    let base = pb
                        .entry_index()
                        .and_then(|e| playing_title(pb.library(), e, |show| self.titles.get(show)))
                        .unwrap_or_else(|| pb.name().to_string());
                    let mut title = format!("{base}{fav}{gone}");
                    if let Some(next) = self.zap.pending() {
                        title = format!("{title}  →  {}…", next.name());
                    }
                    if title != self.shown_title {
                        self.shown_title = title.clone();
                        self.title_changed = Instant::now();
                    }
                    let fs = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
                    let since = self.cursor_moved.elapsed().min(self.title_changed.elapsed());
                    let show_title = cursor::title_visible(fs, since);
                    // In fullscreen the buttons go with the title (the subtitle menu keeps them).
                    let chrome = show_title || self.sub_menu;
                    if fs && show_title {
                        ui.ctx().request_repaint_after(cursor::TITLE_SHOW.saturating_sub(since));
                    }
                    p.show(ui, rect, &title, show_title);
                    let r = ui.interact(rect, egui::Id::unique("video"), egui::Sense::click());
                    if r.double_clicked() {
                        toggle_fs = true;
                    }
                    // Mouse or keyboard activity shows the seek bar for `TIME_SHOW`.
                    if ui.ctx().input(|i| i.pointer.delta() != egui::Vec2::ZERO) && ui.rect_contains_pointer(full) {
                        self.activity = Some(Instant::now());
                    }
                    bar_job = bar_rect.map(|bar| (bar, p.position()));
                    // Fullscreen button: a square outline (no symbol fonts), bottom right.
                    let btn = egui::Rect::from_min_size(rect.right_bottom() - egui::vec2(44.0, 32.0), egui::vec2(32.0, 26.0));
                    // Subtitle button ("CC"), movies and episodes only, left of the play/pause button.
                    if chrome && !pb.is_live() {
                        let sbtn = btn.translate(egui::vec2(-76.0, 0.0));
                        let s_resp = ui.interact(sbtn, egui::Id::unique("sub_btn"), egui::Sense::click());
                        let tracks = if self.sub_menu || s_resp.hovered() { p.sub_tracks() } else { Vec::new() };
                        let current = p.current_sub();
                        let sa = if s_resp.hovered() || self.sub_menu { 230 } else { 140 };
                        let sp = ui.painter_at(rect);
                        sp.rect_filled(sbtn, 4.0, egui::Color32::from_black_alpha(sa / 2));
                        sp.text(
                            sbtn.center(),
                            egui::Align2::CENTER_CENTER,
                            "CC",
                            egui::FontId::proportional(14.0),
                            egui::Color32::from_white_alpha(sa),
                        );
                        if current.is_some() {
                            // Underline: subtitles are on.
                            let y = sbtn.center().y + 9.0;
                            sp.line_segment(
                                [egui::pos2(sbtn.center().x - 9.0, y), egui::pos2(sbtn.center().x + 9.0, y)],
                                egui::Stroke::new(2.0, egui::Color32::from_white_alpha(sa)),
                            );
                        }
                        if s_resp.on_hover_text("Subtitles").clicked() {
                            toggle_sub = true;
                        }
                        if self.sub_menu {
                            let follow = std::mem::take(&mut self.sub_follow);
                            let area = egui::Area::new(egui::Id::unique("sub_menu"))
                                .order(egui::Order::Foreground)
                                .pivot(egui::Align2::RIGHT_BOTTOM)
                                .fixed_pos(sbtn.right_top() - egui::vec2(0.0, 6.0))
                                .show(ui.ctx(), |ui| {
                                    // The player's look: black background, white text.
                                    egui::Frame::NONE
                                        .fill(egui::Color32::from_black_alpha(220))
                                        .corner_radius(6.0)
                                        .inner_margin(6.0)
                                        .show(ui, |ui| {
                                          // Many tracks: scroll inside the space above the button
                                          // instead of growing over the controls.
                                          let max_h = (sbtn.top() - rect.top() - 30.0).max(104.0);
                                          egui::ScrollArea::vertical().max_height(max_h).show(ui, |ui| {
                                            let labels: Vec<(Option<i64>, String)> =
                                                std::iter::once((None, "Off".to_string()))
                                                    .chain(tracks.iter().map(|t| (Some(t.id), subtitles::label(t))))
                                                    .collect();
                                            for (i, (id, text)) in labels.iter().enumerate() {
                                                let (row, resp) =
                                                    ui.allocate_exact_size(egui::vec2(210.0, 26.0), egui::Sense::click());
                                                if resp.hovered() {
                                                    self.sub_cursor = i;
                                                }
                                                if i == self.sub_cursor {
                                                    ui.painter().rect_filled(row, 4.0, egui::Color32::from_white_alpha(45));
                                                    if follow {
                                                        resp.scroll_to_me(None);
                                                    }
                                                }
                                                ui.painter().text(
                                                    row.left_center() + egui::vec2(8.0, 0.0),
                                                    egui::Align2::LEFT_CENTER,
                                                    text,
                                                    egui::FontId::proportional(14.0),
                                                    egui::Color32::WHITE,
                                                );
                                                if *id == current {
                                                    // Dot: the track in use.
                                                    ui.painter().circle_filled(
                                                        row.right_center() - egui::vec2(12.0, 0.0),
                                                        3.5,
                                                        egui::Color32::WHITE,
                                                    );
                                                }
                                                if resp.clicked() {
                                                    pick_sub = Some(*id);
                                                }
                                            }
                                            if tracks.is_empty() {
                                                ui.label(
                                                    egui::RichText::new("No subtitles in this video")
                                                        .color(egui::Color32::from_white_alpha(150)),
                                                );
                                            }
                                          });
                                        });
                                });
                            let outside = ui.ctx().input(|i| i.pointer.any_pressed())
                                && !area.response.contains_pointer()
                                && !sbtn.contains(ui.ctx().pointer_latest_pos().unwrap_or_default());
                            if outside {
                                self.sub_menu = false;
                            }
                        }
                    }
                    // Play/pause button, left of it: shows the action a click performs.
                    if chrome {
                        let pbtn = btn.translate(egui::vec2(-38.0, 0.0));
                        let pb_resp = ui.interact(pbtn, egui::Id::unique("pause_btn"), egui::Sense::click());
                        let paused = p.paused();
                        let pa = if pb_resp.hovered() { 230 } else { 140 };
                        let pp = ui.painter_at(rect);
                        pp.rect_filled(pbtn, 4.0, egui::Color32::from_black_alpha(pa / 2));
                        let white = egui::Color32::from_white_alpha(pa);
                        let c = pbtn.center();
                        if paused {
                            pp.add(egui::Shape::convex_polygon(
                                vec![c + egui::vec2(-5.0, -8.0), c + egui::vec2(-5.0, 8.0), c + egui::vec2(7.0, 0.0)],
                                white,
                                egui::Stroke::NONE,
                            ));
                        } else {
                            for dx in [-6.0, 2.0] {
                                pp.rect_filled(egui::Rect::from_min_size(c + egui::vec2(dx, -8.0), egui::vec2(4.0, 16.0)), 0.0, white);
                            }
                        }
                        if pb_resp.on_hover_text(if paused { "Play (Space)" } else { "Pause (Space)" }).clicked() {
                            toggle_pause = true;
                        }
                    }
                    // Next episode button (">>" as two triangles), left of "CC":
                    // episodes with a next one only; hidden when it would not fit
                    // inside the video rect.
                    let nbtn = btn.translate(egui::vec2(-114.0, 0.0));
                    let fits = nbtn.left() >= rect.left() + 4.0 && nbtn.top() >= rect.top() + 4.0;
                    if chrome
                        && fits
                        && !pb.is_live()
                        && self.next_cache.get(pb.library(), pb.entry_index(), &self.hidden).is_some()
                    {
                        let n_resp = ui.interact(nbtn, egui::Id::unique("next_btn"), egui::Sense::click());
                        let na = if n_resp.hovered() { 230 } else { 140 };
                        let np = ui.painter_at(rect);
                        np.rect_filled(nbtn, 4.0, egui::Color32::from_black_alpha(na / 2));
                        let white = egui::Color32::from_white_alpha(na);
                        let c = nbtn.center();
                        for dx in [-8.0, 0.0] {
                            np.add(egui::Shape::convex_polygon(
                                vec![c + egui::vec2(dx, -7.0), c + egui::vec2(dx, 7.0), c + egui::vec2(dx + 8.0, 0.0)],
                                white,
                                egui::Stroke::NONE,
                            ));
                        }
                        if n_resp.on_hover_text("Next episode (Shift+Right)").clicked() {
                            next_clicked = true;
                        }
                    }
                    if chrome {
                        let b = ui.interact(btn, egui::Id::unique("fullscreen_btn"), egui::Sense::click());
                        let fs = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
                        let alpha = if b.hovered() { 230 } else { 140 };
                        let painter = ui.painter_at(rect);
                        painter.rect_filled(btn, 4.0, egui::Color32::from_black_alpha(alpha / 2));
                        painter.rect_stroke(
                            btn.shrink2(egui::vec2(8.0, 6.0)),
                            1.0,
                            egui::Stroke::new(2.0, egui::Color32::from_white_alpha(alpha)),
                            egui::StrokeKind::Inside,
                        );
                        if b.on_hover_text(if fs { "Leave full screen (Esc)" } else { "Full screen (F11)" }).clicked() {
                            self.set_fullscreen(ui.ctx(), !fs);
                        }
                    }
                    let next_offer = self.autoplay.offering();
                    if next_offer {
                        offer = self.next_episode_offer(ui, btn);
                    }
                    if self.intro_offering() {
                        intro = self.skip_intro_offer(ui, btn, next_offer);
                    }
                }
                _ => {
                    ui.painter().rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
                    let help = match &self.player {
                        Err(e) => format!("Playback unavailable: {e}"),
                        Ok(_) => "Click a channel, movie or episode (or Enter) to play.\n\n\
                                  Up/Down change channel (live) · Enter on the playing row: full view\n\
                                  F favorite · Space pause · Esc stop / back · F5 refresh · Ctrl+F search\n\
                                  Series: Enter opens a show · Backspace/Left back to the shows"
                            .into(),
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        help,
                        egui::FontId::proportional(15.0),
                        ui.visuals().weak_text_color(),
                    );
                }
            }
            if let Some((bar, position)) = bar_job {
                self.seek_bar(ui, bar, position);
            }
            if toggle_sub {
                self.toggle_sub_menu();
            }
            if toggle_fs {
                let fs = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
                self.set_fullscreen(&ui.ctx().clone(), !fs);
            }
            if let (Some(id), Ok(p)) = (pick_sub, &mut self.player) {
                // Playback continues; picking closes the menu.
                let choice = p.select_sub(id);
                self.sub_menu = false;
                self.remember_sub(choice);
            }
            if toggle_pause {
                if let Ok(p) = &mut self.player {
                    p.toggle_pause();
                }
            }
            match offer {
                Some(true) => {
                    eprintln!("autoplay: offer accepted");
                    self.autoplay.accept();
                    self.play_next_episode(true);
                }
                Some(false) => {
                    eprintln!("autoplay: offer dismissed");
                    self.autoplay.dismiss();
                }
                None => {}
            }
            // Re-checked: the seek bar (drawn after the button) may have started a drag.
            match intro.filter(|_| self.intro_offering()) {
                Some(true) => self.accept_intro(),
                Some(false) => self.dismiss_intro(),
                None => {}
            }
            if next_clicked {
                self.skip_to_next_episode();
            }
        });
    }

    /// The "Next episode in N" button (with a countdown line) and a Cancel
    /// button over the video's bottom right, above the control buttons
    /// (`controls` = the fullscreen button). White on black, shapes and plain
    /// text only. Returns Some(true) = play now, Some(false) = cancel.
    fn next_episode_offer(&self, ui: &egui::Ui, controls: egui::Rect) -> Option<bool> {
        let left = self.autoplay.remaining(Instant::now());
        let next = egui::Rect::from_min_max(
            egui::pos2(controls.right() - 220.0, controls.top() - 52.0),
            egui::pos2(controls.right(), controls.top() - 10.0),
        );
        let cancel = egui::Rect::from_min_size(next.left_top() - egui::vec2(98.0, 0.0), egui::vec2(90.0, next.height()));
        let n_resp = ui.interact(next, egui::Id::unique("next_episode_btn"), egui::Sense::click());
        let c_resp = ui.interact(cancel, egui::Id::unique("next_episode_cancel"), egui::Sense::click());
        let painter = ui.painter();
        let font = egui::FontId::proportional(15.0);
        // Play now: a triangle, the text, and a line that runs down with the countdown.
        let a = if n_resp.hovered() { 240 } else { 210 };
        painter.rect_filled(next, 6.0, egui::Color32::from_black_alpha(a));
        painter.rect_stroke(next, 6.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(a - 60)), egui::StrokeKind::Inside);
        let c = next.left_center() + egui::vec2(20.0, 0.0);
        painter.add(egui::Shape::convex_polygon(
            vec![c + egui::vec2(-5.0, -8.0), c + egui::vec2(-5.0, 8.0), c + egui::vec2(7.0, 0.0)],
            egui::Color32::WHITE,
            egui::Stroke::NONE,
        ));
        painter.text(
            next.left_center() + egui::vec2(38.0, 0.0),
            egui::Align2::LEFT_CENTER,
            format!("Next episode in {}", left.ceil() as u64),
            font.clone(),
            egui::Color32::WHITE,
        );
        let frac = (left / OFFER_SECS).clamp(0.0, 1.0) as f32;
        let y = next.bottom() - 4.0;
        let (x0, x1) = (next.left() + 8.0, next.right() - 8.0);
        painter.line_segment(
            [egui::pos2(x0, y), egui::pos2(x0 + (x1 - x0) * frac, y)],
            egui::Stroke::new(2.0, egui::Color32::from_white_alpha(200)),
        );
        // Cancel.
        let ca = if c_resp.hovered() { 240 } else { 210 };
        painter.rect_filled(cancel, 6.0, egui::Color32::from_black_alpha(ca));
        painter.text(cancel.center(), egui::Align2::CENTER_CENTER, "Cancel", font, egui::Color32::from_white_alpha(ca - 20));
        if n_resp.hovered() || c_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if n_resp.on_hover_text("Play the next episode now (Enter)").clicked() {
            Some(true)
        } else if c_resp.on_hover_text("Keep watching (Esc)").clicked() {
            Some(false)
        } else {
            None
        }
    }

    /// The "Skip intro" button and a Cancel button over the video's bottom
    /// right, above the control buttons (`controls` = the fullscreen button),
    /// and above the "Next episode" offer when that shows too. Same look as
    /// that offer. Returns Some(true) = skip, Some(false) = cancel.
    fn skip_intro_offer(&self, ui: &egui::Ui, controls: egui::Rect, above_next: bool) -> Option<bool> {
        let lift = if above_next { 52.0 } else { 0.0 };
        let skip = egui::Rect::from_min_max(
            egui::pos2(controls.right() - 150.0, controls.top() - 52.0 - lift),
            egui::pos2(controls.right(), controls.top() - 10.0 - lift),
        );
        let cancel = egui::Rect::from_min_size(skip.left_top() - egui::vec2(98.0, 0.0), egui::vec2(90.0, skip.height()));
        let s_resp = ui.interact(skip, egui::Id::unique("skip_intro_btn"), egui::Sense::click());
        let c_resp = ui.interact(cancel, egui::Id::unique("skip_intro_cancel"), egui::Sense::click());
        let painter = ui.painter();
        let font = egui::FontId::proportional(15.0);
        // Skip: two triangles (">>" as shapes) and the text.
        let a = if s_resp.hovered() { 240 } else { 210 };
        painter.rect_filled(skip, 6.0, egui::Color32::from_black_alpha(a));
        painter.rect_stroke(skip, 6.0, egui::Stroke::new(1.0, egui::Color32::from_white_alpha(a - 60)), egui::StrokeKind::Inside);
        let c = skip.left_center() + egui::vec2(16.0, 0.0);
        for dx in [-6.0, 2.0] {
            painter.add(egui::Shape::convex_polygon(
                vec![c + egui::vec2(dx, -7.0), c + egui::vec2(dx, 7.0), c + egui::vec2(dx + 8.0, 0.0)],
                egui::Color32::WHITE,
                egui::Stroke::NONE,
            ));
        }
        painter.text(
            skip.left_center() + egui::vec2(38.0, 0.0),
            egui::Align2::LEFT_CENTER,
            "Skip intro",
            font.clone(),
            egui::Color32::WHITE,
        );
        let ca = if c_resp.hovered() { 240 } else { 210 };
        painter.rect_filled(cancel, 6.0, egui::Color32::from_black_alpha(ca));
        painter.text(cancel.center(), egui::Align2::CENTER_CENTER, "Cancel", font, egui::Color32::from_white_alpha(ca - 20));
        if s_resp.hovered() || c_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if s_resp.on_hover_text("Skip the intro (Enter)").clicked() {
            Some(true)
        } else if c_resp.on_hover_text("Watch the intro (Esc)").clicked() {
            Some(false)
        } else {
            None
        }
    }

    fn browse(&mut self, ui: &mut egui::Ui) {
        let Some(lib) = self.shown_lib.clone() else { return };
        let playing = self.playing_entry();
        let scroll_to = self.scroll_to.take();
        // Movies / episodes in groups that are not hidden (None while the group list is computed).
        let vis = self.groups.as_deref().map(|g| visible_counts(g, &self.hidden));
        match self.tab {
            Tab::Live => {
                egui::Panel::left("countries").resizable(true).default_size(190.0).show(ui, |ui| {
                    let scroll = self.country_scroll.take();
                    if let Some((c, g)) =
                        live::countries(ui, &lib, (self.live.country(), self.live.group()), &mut self.country_view, scroll)
                    {
                        self.live.select_country(&lib, c);
                        self.live.select_group(&lib, g);
                        self.live.focus_countries();
                        self.scroll_to = Some(0);
                    }
                    if self.live.countries_focused() {
                        // Keyboard focus is here: outline the panel.
                        let stroke = egui::Stroke::new(2.0, ui.visuals().selection.bg_fill);
                        ui.painter().rect_stroke(ui.max_rect(), 2.0, stroke, egui::StrokeKind::Inside);
                    }
                });
                egui::Panel::left("channels").resizable(true).default_size(330.0).show(ui, |ui| {
                    if self.live.list().is_empty() {
                        ui.weak(if self.filter.is_empty() { "No channels" } else { "No channels match the filter" });
                    }
                    let a = live::channels(
                        ui,
                        &lib,
                        self.live.list(),
                        playing,
                        &self.favs,
                        &mut self.channel_view,
                        scroll_to,
                    );
                    if let Some(RowAction::Click(i) | RowAction::DoubleClick(i)) = a {
                        self.live.list_mut().set_cursor(i);
                        self.live.focus_channels();
                        let e = self.live.list().current();
                        // A click plays at once and cancels a pending zap.
                        match e {
                            Some(e) if Some(e) != playing => self.play_from(self.tab_list(), e, Tab::Live),
                            _ => drop(self.zap.cancel()),
                        }
                        if matches!(a, Some(RowAction::DoubleClick(_))) {
                            self.set_fullscreen(&ui.ctx().clone(), true);
                        }
                    }
                });
            }
            Tab::Favorites => {
                egui::Panel::left("favorites").resizable(true).default_size(360.0).show(ui, |ui| {
                    if self.fav.rows().is_empty() {
                        ui.weak("No favorites yet: select a channel and press F.");
                    }
                    let a = live::favorites(
                        ui,
                        self.fav.rows(),
                        self.fav.cursor(),
                        playing,
                        &mut self.fav_view,
                        scroll_to,
                    );
                    if let Some(RowAction::Click(i) | RowAction::DoubleClick(i)) = a {
                        self.fav.set_cursor(i);
                        match self.fav.play_target() {
                            Some(e) if Some(e) != playing => self.play_from(self.tab_list(), e, Tab::Favorites),
                            Some(_) => drop(self.zap.cancel()),
                            None => {
                                self.zap.cancel();
                                self.message = Some("This favorite is not in the current playlist.".into())
                            }
                        }
                        if matches!(a, Some(RowAction::DoubleClick(_))) && self.playback.is_some() {
                            self.set_fullscreen(&ui.ctx().clone(), true);
                        }
                    }
                });
            }
            Tab::Movies => {
                egui::Panel::left("movies").resizable(true).default_size(420.0).show(ui, |ui| {
                    let focus = std::mem::take(&mut self.focus_search);
                    let mut toggled = None;
                    vod::search_row(ui, |ui| {
                        toggled = vod::group_menu(ui, self.groups.as_deref(), &self.hidden, &mut self.group_filter);
                        let hint = "Search movies (Ctrl+F)";
                        if let Some(t) = vod::search_box(ui, vod::movie_search_id(), self.movies.query(), hint, focus) {
                            self.set_vod_query(Tab::Movies, t);
                        }
                    });
                    let n = self.movies.list().len();
                    let hint = format!("Type to search {} movies.", vis.map_or(lib.movies().len(), |v| v.0));
                    let all_hidden = vis.is_some_and(|v| v.0 == 0) && !lib.movies().is_empty();
                    vod::result_note(ui, self.movies.query(), self.movies.searching(), n, &hint, all_hidden);
                    let a = vod::movies(
                        ui,
                        &lib,
                        self.movies.list(),
                        playing,
                        &self.favs,
                        &mut self.movie_view,
                        scroll_to,
                    );
                    if let Some(RowAction::Click(i) | RowAction::DoubleClick(i)) = a {
                        self.movies.list_mut().set_cursor(i);
                        match self.movies.list().current() {
                            Some(e) if Some(e) != playing => self.play_from(self.tab_list(), e, Tab::Movies),
                            _ => drop(self.zap.cancel()),
                        }
                        if matches!(a, Some(RowAction::DoubleClick(_))) {
                            self.set_fullscreen(&ui.ctx().clone(), true);
                        }
                    }
                    if let Some(vod::GroupAction::Set(g, h)) = toggled {
                        self.set_groups_hidden(&g, h);
                    }
                });
            }
            Tab::Series => {
                egui::Panel::left("series").resizable(true).default_size(420.0).show(ui, |ui| match self.series.open().cloned() {
                    Some(open) => {
                        ui.horizontal(|ui| {
                            if ui.button("< Shows").on_hover_text("Backspace / Left").clicked() {
                                self.series_back();
                            }
                            ui.strong(&open.name);
                        });
                        let titles = self.titles.get(&open.name);
                        let a = vod::episodes(ui, &lib, &open, playing, &self.favs, titles.as_deref(), &mut self.episode_view, scroll_to);
                        if let Some(RowAction::Click(i) | RowAction::DoubleClick(i)) = a {
                            if let Some(e) = self.series.open_mut().and_then(|o| o.set_cursor(i)) {
                                if Some(e) != playing {
                                    self.play_from(self.tab_list(), e, Tab::Series);
                                } else {
                                    self.zap.cancel();
                                }
                                if matches!(a, Some(RowAction::DoubleClick(_))) {
                                    self.set_fullscreen(&ui.ctx().clone(), true);
                                }
                            }
                        }
                    }
                    None => {
                        let focus = std::mem::take(&mut self.focus_search);
                        let mut toggled = None;
                        vod::search_row(ui, |ui| {
                            toggled = vod::group_menu(ui, self.groups.as_deref(), &self.hidden, &mut self.group_filter);
                            let q = self.series.search.query().to_string();
                            let hint = "Search series (Ctrl+F)";
                            if let Some(t) = vod::search_box(ui, vod::series_search_id(), &q, hint, focus) {
                                self.set_vod_query(Tab::Series, t);
                            }
                        });
                        let s = &self.series.search;
                        let hint = match vis {
                            Some(v) => format!("Type to search series ({} episodes).", v.1),
                            None => format!("Type to search {} shows.", lib.shows().len()),
                        };
                        let all_hidden = vis.is_some_and(|v| v.1 == 0) && !lib.shows().is_empty();
                        vod::result_note(ui, s.query(), s.searching(), s.list().len(), &hint, all_hidden);
                        if self.recent_listed() {
                            let a = vod::recent(ui, &lib, self.recent.items(), self.recent_cursor, &mut self.recent_view, scroll_to);
                            if let Some(RowAction::Click(i) | RowAction::DoubleClick(i)) = a {
                                self.recent_cursor = Some(i);
                                self.open_recent(i);
                            }
                        } else {
                            let a = vod::shows(ui, &lib, self.series.search.list(), &mut self.show_view, scroll_to);
                            if let Some(RowAction::Click(i) | RowAction::DoubleClick(i)) = a {
                                self.series.search.list_mut().set_cursor(i);
                                if self.series.open_selected(&lib, &self.hidden) {
                                    self.scroll_to = self.vod_cursor();
                                }
                            }
                        }
                        if let Some(vod::GroupAction::Set(g, h)) = toggled {
                            self.set_groups_hidden(&g, h);
                        }
                    }
                });
            }
        }
        self.video_area(ui);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let frame_start = Instant::now();
        if !self.first_frame {
            self.first_frame = true;
            eprintln!("startup: first frame after {} ms", self.t0.elapsed().as_millis());
        }
        self.poll_background();
        self.titles.poll();
        self.previews.poll(&ctx);
        let finished = match &mut self.player {
            Ok(p) => {
                p.update();
                p.status() == Status::Finished
            }
            Err(_) => false,
        };
        // A finished episode with a next one (offer not dismissed) goes on to it.
        if !self.update_autoplay(finished) && finished {
            self.on_finished();
        }
        self.update_skip_intro();
        if ctx.input(|i| i.viewport().close_requested()) {
            // Safety net: if teardown after the window closes ever blocks,
            // end the process instead of leaving it lingering.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(5));
                hard_exit();
            });
        }
        if self.positions_saved.elapsed() >= POSITION_SAVE_EVERY {
            self.save_position();
        }
        self.update_cursor(&ctx);
        self.handle_keys(&ctx);
        self.fire_zap();

        if self.view == View::Player && self.playback.is_some() {
            self.video_area(ui);
        } else {
            self.view = View::Browse;
            self.top_bar(ui);
            if self.tab != self.last_tab {
                // Opening Movies/Series focuses its search box.
                self.last_tab = self.tab;
                self.focus_search = match self.tab {
                    Tab::Movies => true,
                    Tab::Series => self.series.open().is_none(),
                    _ => false,
                };
                self.scroll_to = self.vod_cursor();
            }
            self.status_bar(ui);
            if self.shown_lib.is_some() {
                self.browse(ui);
            } else {
                self.phase_screen(ui);
            }
        }

        // After the panels, so a show opened by a click this frame is requested
        // at once (the worker's answer repaints). Idempotent: covers every
        // path that opens a show (click, Enter, rebase).
        if let Some(o) = self.series.open() {
            self.titles.request(&o.name);
        }
        // The playing episode's show too (its overlay title includes the episode name).
        let playing_show = self.playback.as_ref().and_then(|pb| {
            let e = pb.entry_index()?;
            show_of(pb.library(), e).map(|s| pb.library().shows()[s].name.clone())
        });
        if let Some(name) = playing_show {
            self.titles.request(&name);
        }

        if let Some(tab) = &mut self.help {
            if !help::show(&ctx, tab) {
                self.help = None;
            }
        }
        self.url_prompt_ui(&ctx);

        let spent = frame_start.elapsed();
        if spent > SLOW_FRAME {
            eprintln!("slow frame: {} ms building the UI", spent.as_millis());
        }

        // Keep timers (supervisor, refresh/cache polling) running.
        let busy = self.cache_rx.is_some() || self.refresher.is_some();
        let player_tick = self.player.as_ref().ok().and_then(Player::repaint_interval);
        if let Some(w) = self.zap.wait(Instant::now()) {
            // Fire the pending zap without waiting for input.
            ctx.request_repaint_after(w);
        }
        if self.autoplay.offering() {
            // The countdown of the "Next episode" offer.
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.intro_offering() {
            // The "Skip intro" prompt goes away at the end of the intro.
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if busy || player_tick.is_some() {
            ctx.request_repaint_after(player_tick.unwrap_or(Duration::from_millis(100)));
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        // Stop playback first, then free the render context (GL current).
        // The process has been seen to linger after the window closed
        // (a terminal waiting until Ctrl+C): if shutdown or teardown blocks,
        // a watchdog ends the process.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(3));
            hard_exit();
        });
        self.save_position();
        if let Ok(p) = &mut self.player {
            p.shutdown(gl);
        }
        // Nothing left to save (favorites/settings are written on change);
        // skip the remaining teardown (mpv core destroy, worker threads).
        std::process::exit(0);
    }
}
