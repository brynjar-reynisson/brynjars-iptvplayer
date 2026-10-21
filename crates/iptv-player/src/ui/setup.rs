//! First-run prompt: a modal dialog asking for the playlist URL while
//! `config.toml` has none. Nothing else can be used until it is saved (or the
//! app is quit). The URL only lives in the text box and `config.toml`.

use std::path::Path;

use eframe::egui;

/// Why the playlist URL could not be read from `config.toml`.
#[derive(Clone, PartialEq, Eq)]
pub enum Problem {
    /// The file is fine but `playlist_url` is empty or still the example.
    Unset,
    /// The file cannot be read or is invalid (URL-free reason).
    Broken(String),
}

impl Problem {
    /// `None` when `config.toml` in `dir` loads.
    pub fn of(dir: &Path) -> Option<Problem> {
        match iptv::config::load(dir) {
            Ok(_) => None,
            Err(e) if e.downcast_ref::<iptv::config::UrlUnset>().is_some() => Some(Problem::Unset),
            Err(e) => Some(Problem::Broken(crate::state::safe_text(&format!("{e:#}")))),
        }
    }
}

/// What the user did this frame.
pub enum Outcome {
    None,
    /// "Save" with the (unvalidated) text.
    Save(String),
    /// Broken file: read it again.
    Reload,
    Quit,
}

pub struct UrlPrompt {
    problem: Problem,
    text: String,
    error: Option<String>,
    /// `config.toml` could not be created at startup (reason, no URL).
    note: Option<String>,
    focus: bool,
}

impl UrlPrompt {
    pub fn new(problem: Problem, note: Option<String>) -> UrlPrompt {
        UrlPrompt { problem, text: String::new(), error: None, note, focus: true }
    }

    pub fn problem(&self) -> &Problem {
        &self.problem
    }

    pub fn set_problem(&mut self, problem: Problem) {
        self.problem = problem;
    }

    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }
}

fn url_box_id() -> egui::Id {
    egui::Id::unique("playlist_url_box")
}

/// Draws the dialog over a dimmed backdrop; it blocks everything behind it.
pub fn show(ctx: &egui::Context, p: &mut UrlPrompt, config_path: &Path) -> Outcome {
    let mut out = Outcome::None;
    egui::Modal::new(egui::Id::unique("url_prompt")).show(ctx, |ui| {
        ui.set_width(460.0_f32.min((ctx.content_rect().width() - 60.0).max(240.0)));
        match p.problem.clone() {
            Problem::Unset => {
                ui.heading("Playlist URL needed");
                ui.add_space(6.0);
                ui.label("Paste the M3U playlist URL from your IPTV provider. Without it nothing can be played.");
                ui.add_space(8.0);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut p.text)
                        .id(url_box_id())
                        .hint_text("http://provider.example/get.php?username=…&password=…")
                        .desired_width(f32::INFINITY),
                );
                if std::mem::take(&mut p.focus) {
                    r.request_focus();
                }
                if p.error.is_some() && r.changed() {
                    p.error = None;
                }
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if let Some(e) = &p.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                if let Some(n) = &p.note {
                    ui.weak(n);
                }
                ui.weak(format!("It is saved in {}", config_path.display()));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let can_save = !p.text.trim().is_empty();
                    if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() || (enter && can_save) {
                        out = Outcome::Save(p.text.clone());
                    }
                    if ui.button("Quit").clicked() {
                        out = Outcome::Quit;
                    }
                });
            }
            Problem::Broken(detail) => {
                ui.heading("Cannot read the settings file");
                ui.add_space(6.0);
                ui.label(detail);
                ui.add_space(6.0);
                ui.label("Fix or delete the file (a new one is created at the next start), then reload.");
                ui.weak(format!("{}", config_path.display()));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Reload").clicked() {
                        out = Outcome::Reload;
                    }
                    if ui.button("Quit").clicked() {
                        out = Outcome::Quit;
                    }
                });
            }
        }
    });
    out
}
