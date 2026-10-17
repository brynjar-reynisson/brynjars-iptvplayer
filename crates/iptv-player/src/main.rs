//! Spike harness (Task 10): plays one URL/file in an eframe (glow) window
//! through `Supervisor<MpvBackend>`, with a status overlay. Task 11 replaces
//! this with the real UI.
//!
//! Usage: `iptv-player <url-or-file> [--vod]` (live by default). The URL is
//! never printed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use iptv::supervisor::{Status, Supervisor};
use iptv_player::mpv_backend::{GlInit, MpvBackend, MpvOptions, MpvRenderer};

struct App {
    sup: Supervisor<MpvBackend>,
    renderer: Arc<MpvRenderer>,
    last_status: Option<Status>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, url: String, vod: bool) -> anyhow::Result<App> {
        let get_proc_address = cc
            .get_proc_address
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no OpenGL context (glow renderer required)"))?;
        let (c1, c2) = (cc.egui_ctx.clone(), cc.egui_ctx.clone());
        let backend = MpvBackend::new(MpvOptions {
            gl: Some(GlInit { get_proc_address, on_frame: Box::new(move || c1.request_repaint()) }),
            on_wakeup: Some(Box::new(move || c2.request_repaint())),
        })?;
        eprintln!("libmpv: {}", backend.mpv_version());
        let renderer = backend.renderer().expect("renderer in GL mode");
        let mut sup = Supervisor::new(backend);
        sup.play(url, !vod, Instant::now());
        Ok(App { sup, renderer, last_status: None })
    }
}

fn status_text(s: Status, retry_in: Option<Duration>) -> String {
    let wait = retry_in.map(|d| format!(" (next try in {:.1} s)", d.as_secs_f32())).unwrap_or_default();
    match s {
        Status::Idle => "Idle".into(),
        Status::Playing => String::new(),
        Status::Buffering => "Buffering…".into(),
        Status::Reconnecting { attempt, auth: false } => format!("Reconnecting… attempt {attempt}{wait}"),
        Status::Reconnecting { attempt, auth: true } => {
            format!("Account rejected / too many connections — retrying, attempt {attempt}{wait}")
        }
        Status::Finished => "Finished".into(),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        for (generation, ev) in self.sup.backend_mut().poll_events() {
            self.sup.on_event(generation, ev, now);
        }
        self.sup.tick(now);
        for line in self.sup.backend_mut().take_log() {
            eprintln!("mpv {line}"); // already scrubbed
        }
        let status = self.sup.status();
        if self.last_status != Some(status) {
            eprintln!("status: {status:?}");
            self.last_status = Some(status);
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ui, |ui| {
            let rect = ui.max_rect();
            let renderer = self.renderer.clone();
            ui.painter().add(egui::PaintCallback {
                rect,
                callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |info, painter| {
                    renderer.paint_callback(&info, painter);
                })),
            });
            let text = status_text(status, self.sup.next_retry_in(now));
            if !text.is_empty() {
                let pos = rect.left_top() + egui::vec2(12.0, 12.0);
                let galley = ui.painter().layout_no_wrap(text, egui::FontId::proportional(18.0), egui::Color32::WHITE);
                let bg = egui::Rect::from_min_size(pos, galley.size()).expand(6.0);
                ui.painter().rect_filled(bg, 4.0, egui::Color32::from_black_alpha(180));
                ui.painter().galley(pos, galley, egui::Color32::WHITE);
            }
        });
        // Keep the supervisor ticking even when mpv is quiet.
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        // Stop playback first, then free the render context (GL current).
        self.sup.stop();
        self.renderer.destroy(gl);
    }
}

fn main() -> anyhow::Result<()> {
    let mut url = None;
    let mut vod = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--vod" => vod = true,
            _ if url.is_none() => url = Some(a),
            _ => anyhow::bail!("usage: iptv-player <url-or-file> [--vod]"),
        }
    }
    let Some(url) = url else {
        anyhow::bail!("usage: iptv-player <url-or-file> [--vod]");
    };
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 600.0]).with_title("IPTV Player (spike)"),
        ..Default::default()
    };
    eframe::run_native(
        "IPTV Player",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, url, vod)?))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
