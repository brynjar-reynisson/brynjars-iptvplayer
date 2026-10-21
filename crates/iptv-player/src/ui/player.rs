//! Video playback: `Supervisor<MpvBackend>` + the libmpv GL renderer, drawn
//! into an arbitrary rect with a status overlay. Never shows the URL.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use iptv::subtitles::{pick, SubChoice, SubTrack};
use iptv::supervisor::{Status, Supervisor};

use crate::mpv_backend::{GlInit, MpvBackend, MpvOptions, MpvRenderer};
use crate::state::status_text;

/// Streams taller than this (1080p) get [`TALL_TUNING`].
pub const TALL_MIN_HEIGHT: i64 = 1080;

/// Tuning for streams above 1080p, applied when one plays and undone for the
/// next stream (the previous values are read from mpv at startup):
/// - decoder: all cores, cheaper deblocking of non-reference frames;
/// - renderer: cheap bilinear scaling and no linear-light/sigmoid/dither work.
///   With mpv's default high-quality scalers the video output dropped about
///   half of the frames of a 50 fps 4K stream (decoder drops were 0, cache full).
pub const TALL_TUNING: [(&str, &str); 10] = [
    ("vd-lavc-threads", "0"),
    ("vd-lavc-skiploopfilter", "nonref"),
    ("vd-lavc-fast", "yes"),
    ("scale", "bilinear"),
    ("dscale", "bilinear"),
    ("cscale", "bilinear"),
    ("correct-downscaling", "no"),
    ("linear-downscaling", "no"),
    ("sigmoid-upscaling", "no"),
    ("dither-depth", "no"),
];

/// How often the playback stats of a tall stream are logged.
const STATS_EVERY: Duration = Duration::from_secs(5);

pub struct Player {
    sup: Supervisor<MpvBackend>,
    renderer: Arc<MpvRenderer>,
    paused: bool,
    last_status: Status,
    /// Subtitle choice to (re)apply on every load of the current video.
    want_sub: Option<SubChoice>,
    /// Supervisor generation `want_sub` was last applied for.
    sub_gen: u32,
    /// Supervisor generation whose video size was checked for tuning.
    tune_gen: u32,
    /// Tall-stream tuning is applied to the current stream.
    tuned: bool,
    /// mpv's values of the `TALL_TUNING` options before any tuning.
    tall_defaults: Vec<(&'static str, String)>,
    /// When the stats were last logged.
    stats_at: Instant,
    /// For log timestamps.
    t0: Instant,
}

impl Player {
    /// `user_agent`: sent on every stream request (blank = default). Video is
    /// decoded in software; streams above 1080p get [`TALL_TUNING`].
    pub fn new(cc: &eframe::CreationContext<'_>, user_agent: &str) -> anyhow::Result<Player> {
        let get_proc_address = cc
            .get_proc_address
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no OpenGL context (glow renderer required)"))?;
        let (c1, c2) = (cc.egui_ctx.clone(), cc.egui_ctx.clone());
        let backend = MpvBackend::new(MpvOptions {
            user_agent: user_agent.to_string(),
            gl: Some(GlInit { get_proc_address, on_frame: Box::new(move || c1.request_repaint()) }),
            on_wakeup: Some(Box::new(move || c2.request_repaint())),
        })?;
        eprintln!("libmpv: {}", backend.mpv_version());
        let renderer = backend.renderer().ok_or_else(|| anyhow::anyhow!("libmpv render context missing"))?;
        let tall_defaults = TALL_TUNING
            .iter()
            .filter_map(|(k, _)| backend.property(k).map(|v| (*k, v)))
            .collect::<Vec<_>>();
        Ok(Player { sup: Supervisor::new(backend), renderer, paused: false, last_status: Status::Idle, want_sub: None, sub_gen: 0, tune_gen: 0, tuned: false, tall_defaults, stats_at: Instant::now(), t0: Instant::now() })
    }

    /// Start (or switch to) a stream. Clears a user pause.
    pub fn play(&mut self, url: &str, live: bool) {
        self.play_from(url, live, None);
    }

    /// Like [`play`](Self::play), starting a movie/episode at `start_at` seconds.
    pub fn play_from(&mut self, url: &str, live: bool, start_at: Option<f64>) {
        if self.paused {
            self.sup.backend_mut().set_pause(false);
            self.paused = false;
        }
        self.want_sub = None;
        if self.tuned {
            // A new stream starts untuned again; it is tuned when its size is known.
            let b = self.sup.backend_mut();
            for (k, v) in &self.tall_defaults {
                b.set_property(k, v);
            }
            self.tuned = false;
        }
        eprintln!("{:>9.3}s play (generation {})", self.t0.elapsed().as_secs_f64(), self.sup.generation() + 1);
        self.sup.play_from(url.to_string(), live, start_at, Instant::now());
    }

    pub fn stop(&mut self) {
        if self.paused {
            self.sup.backend_mut().set_pause(false);
            self.paused = false;
        }
        self.sup.stop();
    }

    /// User pause: mpv `pause` property only (the backend never reports it
    /// as Buffering, so the stall watchdog is not triggered).
    pub fn toggle_pause(&mut self) {
        if self.sup.status() == Status::Idle {
            return;
        }
        self.paused = !self.paused;
        self.sup.backend_mut().set_pause(self.paused);
    }

    /// Current movie/episode position and duration in seconds (never live).
    pub fn position(&self) -> Option<(f64, Option<f64>)> {
        self.sup.position()
    }

    /// Seek the playing movie/episode to `secs` (ignored when idle).
    pub fn seek_to(&mut self, secs: f64) {
        if self.sup.status() != Status::Idle {
            self.sup.backend_mut().seek(secs, true);
        }
    }

    /// Subtitle choice to apply once the video plays (and again after a
    /// reconnect, which resets mpv's own selection). `None` = mpv's default.
    pub fn set_want_sub(&mut self, want: Option<SubChoice>) {
        self.want_sub = want;
        self.sub_gen = 0;
    }

    pub fn sub_tracks(&self) -> Vec<SubTrack> {
        self.sup.backend().sub_tracks()
    }

    /// Selected subtitle track id (`None` = off).
    pub fn current_sub(&self) -> Option<i64> {
        self.sup.backend().current_sub()
    }

    /// The user picked a track (`None` = off): switch now, playback continues.
    /// Returns what to remember for the next episodes.
    pub fn select_sub(&mut self, id: Option<i64>) -> SubChoice {
        let choice = id
            .and_then(|id| self.sub_tracks().into_iter().find(|t| t.id == id))
            .map_or(SubChoice::Off, |t| SubChoice::of(&t));
        self.sup.backend_mut().set_sub(id);
        self.want_sub = Some(choice.clone());
        self.sub_gen = self.sup.generation();
        choice
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn status(&self) -> Status {
        self.sup.status()
    }

    pub fn is_active(&self) -> bool {
        self.sup.status() != Status::Idle
    }

    /// Pump mpv events into the supervisor and run its timers. Every frame.
    pub fn update(&mut self) {
        let now = Instant::now();
        for (generation, ev) in self.sup.backend_mut().poll_events() {
            self.sup.on_event(generation, ev, now);
        }
        self.sup.tick(now);
        for line in self.sup.backend_mut().take_log() {
            eprintln!("mpv {line}"); // already scrubbed by the backend
        }
        // Apply the wanted subtitles once per load, as soon as it plays.
        if self.sup.status() == Status::Playing && self.sub_gen != self.sup.generation() {
            self.sub_gen = self.sup.generation();
            if let Some(want) = &self.want_sub {
                if let Some(sel) = pick(&self.sup.backend().sub_tracks(), want) {
                    self.sup.backend_mut().set_sub(sel);
                }
            }
        }
        // Tall (above 1080p) video: tune the decoder and renderer once per
        // stream when its size is known. A reconnect (new generation, same
        // stream) keeps it.
        if self.sup.status() == Status::Playing && self.tune_gen != self.sup.generation() {
            if let Some(h) = self.sup.backend().video_height() {
                self.tune_gen = self.sup.generation();
                if h > TALL_MIN_HEIGHT && !self.tuned {
                    let b = self.sup.backend_mut();
                    for (k, v) in TALL_TUNING {
                        b.set_property(k, v);
                    }
                    self.tuned = true;
                    eprintln!("tuning: {h}p video, decoder tuned");
                }
            }
        }
        if self.tuned && self.sup.status() == Status::Playing && self.stats_at.elapsed() >= STATS_EVERY {
            self.stats_at = Instant::now();
            eprintln!("{:>9.3}s stats: {}", self.t0.elapsed().as_secs_f64(), self.sup.backend().stats());
        }
        let s = self.sup.status();
        if s != self.last_status {
            eprintln!("{:>9.3}s status: {s:?}", self.t0.elapsed().as_secs_f64());
            self.last_status = s;
        }
    }

    /// Draw the video into `rect` with the overlay (channel name + status; the
    /// name only when `show_title`).
    pub fn show(&self, ui: &egui::Ui, rect: egui::Rect, title: &str, show_title: bool) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::BLACK);
        let renderer = self.renderer.clone();
        painter.add(egui::PaintCallback {
            rect,
            callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |info, p| {
                renderer.paint_callback(&info, p);
            })),
        });
        let mut lines = Vec::new();
        if show_title {
            lines.push(title.to_string());
        }
        let status = status_text(self.sup.status(), self.sup.next_retry_in(Instant::now()));
        if !status.is_empty() {
            lines.push(status);
        }
        if self.paused {
            lines.push("Paused (Space)".into());
        }
        let mut pos = rect.left_top() + egui::vec2(12.0, 12.0);
        for (i, text) in lines.into_iter().enumerate() {
            let size = if show_title && i == 0 { 16.0 } else { 18.0 };
            let galley = painter.layout_no_wrap(text, egui::FontId::proportional(size), egui::Color32::WHITE);
            let bg = egui::Rect::from_min_size(pos, galley.size()).expand(6.0);
            painter.rect_filled(bg, 4.0, egui::Color32::from_black_alpha(180));
            let h = galley.size().y;
            painter.galley(pos, galley, egui::Color32::WHITE);
            pos.y += h + 14.0;
        }
    }

    /// How soon the UI must repaint to keep the supervisor ticking.
    pub fn repaint_interval(&self) -> Option<Duration> {
        self.is_active().then_some(Duration::from_millis(100))
    }

    /// App exit: stop playback, then free the render context (GL current).
    pub fn shutdown(&mut self, gl: Option<&eframe::glow::Context>) {
        self.sup.stop();
        self.renderer.destroy(gl);
    }
}
