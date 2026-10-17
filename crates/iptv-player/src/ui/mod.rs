//! The eframe application: tabs (Live | Favorites | Movies | Series), browse
//! lists beside the video, a full player view, keyboard handling and the
//! status line. Thin over `crate::state`, which holds all testable logic.
//!
//! Adding a tab (Task 12: Movies/Series): give it a browser struct in
//! `state`, a renderer in `ui`, and arms in `App::tab_list`, `App::step`,
//! `App::activate` and `App::selected_entry`.

pub mod live;
pub mod player;

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use iptv::favorites::Favorites;
use iptv::fetch::{self, RefreshEvent, RefreshOptions, Refresher, Source};
use iptv::library::Library;

use crate::state::data::{Action, DataState, Phase, RefreshState, SourceStatus};
use crate::state::favs::{toggle_favorite, toggle_key, FavBrowser};
use crate::state::live::LiveBrowser;
use crate::state::playback::Playback;
use crate::state::{format_bytes, safe_text, Tab};
use live::{ListView, RowAction};
use player::Player;

/// Command-line options.
pub struct Options {
    pub data_dir: PathBuf,
    /// Dev: parse this local `.m3u` instead of downloading `playlist_url`.
    pub playlist_file: Option<PathBuf>,
    /// Process start, for startup timings on stderr.
    pub t0: Instant,
}

pub fn run(opts: Options) -> anyhow::Result<()> {
    let native = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 760.0]).with_title("IPTV Player"),
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

/// UI build time above this is logged (responsiveness diagnostics).
const SLOW_FRAME: Duration = Duration::from_millis(25);

type CacheResult = Option<(Arc<Library>, SystemTime)>;

fn filter_id() -> egui::Id {
    egui::Id::unique("channel_filter")
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
    /// Transient on-screen message (favorite toggled, save error, ...).
    message: Option<String>,
    filter: String,
    focus_filter: bool,
    scroll_to: Option<usize>,
    country_view: ListView,
    channel_view: ListView,
    fav_view: ListView,
    t0: Instant,
    first_frame: bool,
    ready_logged: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, opts: Options) -> App {
        let (source, status) = resolve_source(&opts.data_dir, &opts.playlist_file);
        let (data, actions) = DataState::new(status);
        let player = Player::new(cc).map_err(|e| safe_text(&format!("{e:#}")));
        if let Err(e) = &player {
            eprintln!("player unavailable: {e}");
        }
        let favs = Favorites::load(&opts.data_dir);
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            data_dir: opts.data_dir,
            playlist_file: opts.playlist_file,
            source,
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
            message: None,
            filter: String::new(),
            focus_filter: false,
            scroll_to: None,
            country_view: ListView::default(),
            channel_view: ListView::default(),
            fav_view: ListView::default(),
            t0: opts.t0,
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
                    let (tx, rx) = channel();
                    let dir = self.data_dir.clone();
                    let ctx = self.ctx.clone();
                    std::thread::spawn(move || {
                        let r = fetch::load_cached(&dir);
                        let _ = tx.send(r);
                        ctx.request_repaint();
                    });
                    self.cache_rx = Some(rx);
                }
                Action::StartRefresh { force } => {
                    if let Some(src) = &self.source {
                        self.refresher =
                            Some(Refresher::spawn_source(src.clone(), self.data_dir.clone(), RefreshOptions { force }));
                    }
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
                if let RefreshEvent::Failed(m) = &ev {
                    eprintln!("refresh failed: {}", safe_text(m));
                }
                self.data.on_refresh_event(ev);
            }
            if done {
                self.refresher = None;
            }
        }
        self.sync_library();
    }

    /// Adopt a new library (cache loaded or refresh done) atomically.
    fn sync_library(&mut self) {
        let Some(new) = self.data.library().cloned() else { return };
        if self.shown_lib.as_ref().is_some_and(|l| Arc::ptr_eq(l, &new)) {
            return;
        }
        self.live.set_library(self.shown_lib.as_deref(), &new);
        self.live.set_filter(&new, &self.filter);
        self.fav.reload(Some(&new), &self.favs);
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

    fn start(&mut self, pb: Playback) {
        match &mut self.player {
            Ok(p) => p.play(&pb.entry().url, pb.is_live()),
            Err(e) => self.message = Some(format!("Playback unavailable: {e}")),
        }
        self.playback = Some(pb);
    }

    fn play_from(&mut self, list: Vec<usize>, entry: usize, tab: Tab) {
        if let Some(lib) = self.shown_lib.clone() {
            self.start(Playback::new(lib, list, entry, tab));
        }
    }

    fn stop(&mut self) {
        if let Ok(p) = &mut self.player {
            p.stop();
        }
        self.playback = None;
        self.view = View::Browse;
    }

    fn playing_entry(&self) -> Option<usize> {
        let lib = self.shown_lib.as_ref()?;
        self.playback.as_ref().filter(|p| p.same_library(lib)).map(Playback::entry_index)
    }

    /// The Up/Down list of the visible tab.
    fn tab_list(&self) -> Vec<usize> {
        match self.tab {
            Tab::Live => self.live.list().items().to_vec(),
            Tab::Favorites => self.fav.playable_entries(),
            Tab::Movies | Tab::Series => Vec::new(),
        }
    }

    /// Up/Down: in the player view walk the playback list; in the browse
    /// view move the cursor, and switch channel too if something is playing.
    fn step(&mut self, delta: i32) {
        if self.view == View::Player {
            if let Some(pb) = &mut self.playback {
                let e = pb.step(delta);
                let (url, live) = (pb.entry().url.clone(), pb.is_live());
                if let Ok(p) = &mut self.player {
                    p.play(&url, live);
                }
                if pb.tab == Tab::Live && self.shown_lib.as_ref().is_some_and(|l| pb.same_library(l)) {
                    self.live.list_mut().select_entry(e);
                    self.scroll_to = self.live.list().cursor();
                }
            }
            return;
        }
        let playing = self.playing_entry();
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
        if let (Some(e), true) = (picked, self.playback.is_some()) {
            self.play_from(self.tab_list(), e, self.tab);
        }
    }

    /// Enter: play the selected row; on the row already playing, open the
    /// full player view.
    fn activate(&mut self) {
        match self.selected_entry() {
            Some(e) if self.playing_entry() == Some(e) => self.view = View::Player,
            Some(e) => self.play_from(self.tab_list(), e, self.tab),
            None if self.tab == Tab::Favorites && self.fav.cursor().is_some() => {
                self.message = Some("This favorite is not in the current playlist.".into())
            }
            None => {}
        }
    }

    fn selected_entry(&self) -> Option<usize> {
        match self.tab {
            Tab::Live => self.live.list().current(),
            Tab::Favorites => self.fav.play_target(),
            Tab::Movies | Tab::Series => None,
        }
    }

    /// `F`: toggle favorite on the playing channel, else the selected row.
    fn toggle_favorite(&mut self) {
        let res = if let Some(pb) = &self.playback {
            let name = pb.entry().name.clone();
            toggle_favorite(&mut self.favs, pb.library(), pb.entry_index()).map(|on| (on, name))
        } else if let (Some(lib), Some(e)) = (self.shown_lib.clone(), self.selected_entry()) {
            let name = lib.entry(e).name.clone();
            toggle_favorite(&mut self.favs, &lib, e).map(|on| (on, name))
        } else if let Some(row) = (self.tab == Tab::Favorites).then(|| self.fav.cursor()).flatten() {
            let row = self.fav.rows()[row].clone();
            toggle_key(&mut self.favs, &row.key, &row.label).map(|on| (on, row.label))
        } else {
            return;
        };
        self.message = Some(match res {
            Ok((true, n)) => format!("Added to favorites: {n}"),
            Ok((false, n)) => format!("Removed from favorites: {n}"),
            Err(e) => safe_text(&e),
        });
        self.fav.reload(self.shown_lib.as_deref(), &self.favs);
    }

    // ---- input -------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers as M};
        let filter_focused = ctx.memory(|m| m.has_focus(filter_id()));
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
        let typing = ctx.egui_wants_keyboard_input();
        let (fav, space, slash) = if typing {
            (false, false, false)
        } else {
            ctx.input_mut(|i| {
                (
                    i.consume_key(M::NONE, Key::F),
                    i.consume_key(M::NONE, Key::Space),
                    i.consume_key(M::NONE, Key::Slash),
                )
            })
        };
        if up {
            self.step(-1);
        }
        if down {
            self.step(1);
        }
        if enter {
            if filter_focused {
                ctx.memory_mut(|m| m.surrender_focus(filter_id()));
            }
            self.activate();
        }
        if esc {
            if filter_focused {
                ctx.memory_mut(|m| m.surrender_focus(filter_id()));
            } else if self.view == View::Player || self.playback.is_some() {
                self.stop();
            } else if !self.filter.is_empty() {
                self.set_filter(String::new());
            }
        }
        if f5 {
            self.request_refresh(false);
        }
        if ctrl_f || slash {
            self.tab = Tab::Live;
            self.view = View::Browse;
            self.focus_filter = true;
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
                for t in Tab::ALL {
                    if ui.selectable_label(self.tab == t, t.label()).clicked() {
                        self.tab = t;
                    }
                }
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
            });
        });
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
                                "{} entries · {} live channels in {} countries · {} favorites",
                                lib.entry_count(),
                                live,
                                lib.countries().len(),
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
                Phase::NoConfig { config_path, detail } => {
                    ui.heading("No playlist configured");
                    ui.add_space(8.0);
                    ui.label(format!("Create the file\n{}\nwith this content:", config_path.display()));
                    ui.code("playlist_url = \"http://<your provider>/get.php?...\"");
                    ui.label("(see config.example.toml), then press Refresh (F5).");
                    ui.add_space(8.0);
                    ui.weak(detail);
                    if ui.button("Refresh (F5)").clicked() {
                        self.request_refresh(false);
                    }
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

    fn video_area(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let rect = ui.max_rect();
            match (&self.player, &self.playback) {
                (Ok(p), Some(pb)) => {
                    let fav = if self.favs.contains(&pb.key()) { "  · Favorite" } else { "" };
                    p.show(ui, rect, &format!("{}{fav}", pb.entry().name));
                    let r = ui.interact(rect, egui::Id::unique("video"), egui::Sense::click());
                    if r.double_clicked() {
                        self.view = if self.view == View::Player { View::Browse } else { View::Player };
                    }
                    r.on_hover_text("Double-click: full view");
                }
                _ => {
                    ui.painter().rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
                    let help = match &self.player {
                        Err(e) => format!("Playback unavailable: {e}"),
                        Ok(_) => "Click a channel (or Enter) to play.\n\n\
                                  Up/Down change channel · Enter on the playing channel: full view\n\
                                  F favorite · Space pause · Esc stop / back · F5 refresh · Ctrl+F filter"
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
        });
    }

    fn browse(&mut self, ui: &mut egui::Ui) {
        let Some(lib) = self.shown_lib.clone() else { return };
        let playing = self.playing_entry();
        let scroll_to = self.scroll_to.take();
        match self.tab {
            Tab::Live => {
                egui::Panel::left("countries").resizable(true).default_size(190.0).show(ui, |ui| {
                    if let Some(c) = live::countries(ui, &lib, self.live.country(), &mut self.country_view) {
                        self.live.select_country(&lib, c);
                        self.scroll_to = Some(0);
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
                        let e = self.live.list().current();
                        if let Some(e) = e.filter(|e| Some(*e) != playing) {
                            self.play_from(self.tab_list(), e, Tab::Live);
                        }
                        if matches!(a, Some(RowAction::DoubleClick(_))) {
                            self.view = View::Player;
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
                            Some(_) => {}
                            None => {
                                self.message = Some("This favorite is not in the current playlist.".into())
                            }
                        }
                        if matches!(a, Some(RowAction::DoubleClick(_))) && self.playback.is_some() {
                            self.view = View::Player;
                        }
                    }
                });
            }
            Tab::Movies | Tab::Series => {
                egui::Panel::left("placeholder").default_size(330.0).show(ui, |ui| {
                    ui.add_space(20.0);
                    ui.weak(format!("{}: coming next.", self.tab.label()));
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
        if let Ok(p) = &mut self.player {
            p.update();
        }
        self.handle_keys(&ctx);

        if self.view == View::Player && self.playback.is_some() {
            self.video_area(ui);
        } else {
            self.view = View::Browse;
            self.top_bar(ui);
            self.status_bar(ui);
            if self.shown_lib.is_some() {
                self.browse(ui);
            } else {
                self.phase_screen(ui);
            }
        }

        let spent = frame_start.elapsed();
        if spent > SLOW_FRAME {
            eprintln!("slow frame: {} ms building the UI", spent.as_millis());
        }

        // Keep timers (supervisor, refresh/cache polling) running.
        let busy = self.cache_rx.is_some() || self.refresher.is_some();
        let player_tick = self.player.as_ref().ok().and_then(Player::repaint_interval);
        if busy || player_tick.is_some() {
            ctx.request_repaint_after(player_tick.unwrap_or(Duration::from_millis(100)));
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        // Stop playback first, then free the render context (GL current).
        if let Ok(p) = &mut self.player {
            p.shutdown(gl);
        }
    }
}
