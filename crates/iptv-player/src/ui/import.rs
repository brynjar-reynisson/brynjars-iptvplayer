//! "Import subtitles..." dialog (Series open show and Movies): one modal with
//! the states choose / folder tree / language question / result. The App runs
//! the import itself (`App::run_import`); this file holds the dialog state and
//! the drawing.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use eframe::egui;
use iptv::subimport::Target;

use crate::state::export::FolderTree;

/// What the import applies to.
pub enum Scope {
    /// The open show: the file names are matched to these episodes.
    Show { name: String, targets: Vec<Target> },
    /// One movie (favorite key).
    Movie { name: String, key: String },
}

pub enum Stage {
    Choose,
    /// The native file dialog is open on a thread.
    Picking(Receiver<Vec<PathBuf>>),
    Folder(FolderTree),
    /// Files without a language in their name: ask once.
    Language { files: Vec<PathBuf>, pick: usize },
    Result(String),
}

pub struct ImportDialog {
    pub scope: Scope,
    pub stage: Stage,
    /// Messages of earlier rounds (the language question comes after a first import).
    pub done: Vec<String>,
    /// Appended to the final message (for example "Only the first file was used.").
    pub note: String,
    /// The files came from the file dialog (not from a folder).
    pub picked_files: bool,
}

/// Where a single chosen file goes in a show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleAttach {
    /// The usual matching by the episode marker in the file names.
    Normal,
    /// Attach to this episode (favorite key), whatever the name says.
    Key(String),
    /// No episode to attach to: tell the user.
    NeedEpisode,
}

/// One file picked in the file dialog whose name has no episode marker goes
/// to the open show's cursor episode, else to the playing episode of that
/// show, else nowhere. Anything else is matched by name.
pub fn single_file_attach(
    file_count: usize,
    picked_files: bool,
    name_has_marker: bool,
    cursor_key: Option<&str>,
    playing_key: Option<&str>,
) -> SingleAttach {
    if file_count != 1 || !picked_files || name_has_marker {
        return SingleAttach::Normal;
    }
    match cursor_key.or(playing_key) {
        Some(k) => SingleAttach::Key(k.to_string()),
        None => SingleAttach::NeedEpisode,
    }
}

/// The text of the result stage: earlier rounds, then this round's message,
/// then the note.
pub fn compose_result(done: &[String], message: &str, note: &str) -> String {
    let mut all: Vec<String> = done.to_vec();
    all.push(if note.is_empty() { message.to_string() } else { format!("{message} {note}") });
    all.join("\n")
}

/// A playing item may stand in for "the selected movie" only when it is a movie.
pub fn movie_fallback_ok(kind: Option<iptv::playlist::Kind>) -> bool {
    kind == Some(iptv::playlist::Kind::Movie)
}

/// What the user did this frame.
pub enum Action {
    None,
    Close,
    /// Import these files (`default_lang` = the answer to the language question).
    Run { files: Vec<PathBuf>, default_lang: Option<String> },
}

/// The languages offered by the question (the app's canonical codes).
pub const LANGS: [&str; 20] = [
    "eng", "isl", "dan", "swe", "nor", "fin", "deu", "fra", "spa", "ita", "por", "nld", "pol", "rus", "tur", "ara", "heb",
    "jpn", "kor", "zho",
];

fn lang_label(code: &str) -> String {
    format!("{} ({code})", iptv::subtitles::language_name(code).unwrap_or(code))
}

impl ImportDialog {
    pub fn new(scope: Scope) -> ImportDialog {
        ImportDialog { scope, stage: Stage::Choose, done: Vec::new(), note: String::new(), picked_files: false }
    }

    pub fn heading(&self) -> String {
        match &self.scope {
            Scope::Show { name, .. } | Scope::Movie { name, .. } => format!("Import subtitles: {name}"),
        }
    }

    /// Shows the result text (earlier rounds first).
    pub fn finish(&mut self, message: String) {
        let done = std::mem::take(&mut self.done);
        self.stage = Stage::Result(compose_result(&done, &message, &self.note));
    }

    /// The language question for these files; `earlier` is the first round's message.
    pub fn ask_language(&mut self, files: Vec<PathBuf>, earlier: String) {
        if !earlier.is_empty() {
            self.done.push(earlier);
        }
        self.stage = Stage::Language { files, pick: 0 };
    }
}

/// Draws the modal. It reads Enter/Esc itself (`handle_keys` returns early
/// while it is open).
pub fn dialog(ctx: &egui::Context, d: &mut ImportDialog) -> Action {
    // a finished file dialog
    let mut picked: Option<Vec<PathBuf>> = None;
    let mut picking = false;
    if let Stage::Picking(rx) = &d.stage {
        match rx.try_recv() {
            Ok(v) => picked = Some(v),
            Err(TryRecvError::Empty) => {
                picking = true;
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(TryRecvError::Disconnected) => picked = Some(Vec::new()),
        }
    }
    if let Some(v) = picked {
        d.stage = Stage::Choose;
        if !v.is_empty() {
            d.picked_files = true;
            return Action::Run { files: v, default_lang: None };
        }
    }
    let in_tree = matches!(d.stage, Stage::Folder(_));
    let (enter, esc, up, down, left, right) = ctx.input_mut(|i| {
        let mut k = |key| i.consume_key(egui::Modifiers::NONE, key);
        (
            k(egui::Key::Enter),
            k(egui::Key::Escape),
            in_tree && k(egui::Key::ArrowUp),
            in_tree && k(egui::Key::ArrowDown),
            in_tree && k(egui::Key::ArrowLeft),
            in_tree && k(egui::Key::ArrowRight),
        )
    });
    let heading = d.heading();
    let width = 420.0_f32.min((ctx.content_rect().width() - 60.0).max(240.0));
    let mut action = Action::None;
    let mut next: Option<Stage> = None;
    match &mut d.stage {
        Stage::Choose | Stage::Picking(_) => {
            egui::Modal::new(egui::Id::unique("import_dialog")).show(ctx, |ui| {
                ui.set_width(width);
                ui.heading(&heading);
                ui.add_space(6.0);
                ui.label(match &d.scope {
                    Scope::Show { .. } => {
                        "Choose .srt files named like the episodes (S01E02 or 1x02). A language in the name (en, Icelandic...) is picked up."
                    }
                    Scope::Movie { .. } => "Choose an .srt file for this movie. A language in the name (en, Icelandic...) is picked up.",
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!picking, egui::Button::new("Choose files...")).clicked() {
                        next = Some(Stage::Picking(super::filedialog::pick_srt_files()));
                    }
                    if ui.add_enabled(!picking, egui::Button::new("Choose folder...")).clicked() {
                        next = Some(Stage::Folder(FolderTree::open_at(None)));
                    }
                    if ui.add_enabled(!picking, egui::Button::new("Cancel")).on_hover_text("Esc").clicked() {
                        action = Action::Close;
                    }
                });
            });
            if esc && !picking {
                action = Action::Close;
            }
        }
        Stage::Folder(tree) => {
            if up {
                tree.step(-1);
            }
            if down {
                tree.step(1);
            }
            if left {
                tree.left();
            }
            if right {
                tree.right();
            }
            let mut select_it = enter;
            let mut cancel = esc;
            let mut clicked: Option<(usize, bool)> = None;
            let can_select = tree.selected().is_some();
            egui::Modal::new(egui::Id::unique("import_dialog")).show(ctx, |ui| {
                ui.set_width(width);
                ui.heading("Choose a folder");
                ui.add_space(6.0);
                let line = tree.selected().map_or("Click a folder to select it".to_string(), |p| p.display().to_string());
                ui.label(line);
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let sel = tree.selected_row();
                    for (i, row) in tree.rows().iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::click());
                        if !ui.is_rect_visible(rect) {
                            continue;
                        }
                        let v = ui.visuals();
                        let selected = sel == Some(i);
                        if selected {
                            ui.painter().rect_filled(rect, 2.0, v.selection.bg_fill);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 2.0, v.widgets.hovered.weak_bg_fill);
                        }
                        let color = if selected { v.selection.stroke.color } else { v.text_color() };
                        let x0 = rect.left() + 4.0 + row.depth as f32 * 16.0;
                        let c = egui::pos2(x0 + 8.0, rect.center().y);
                        let pts = if row.expanded {
                            vec![c + egui::vec2(-4.0, -2.5), c + egui::vec2(4.0, -2.5), c + egui::vec2(0.0, 3.5)]
                        } else {
                            vec![c + egui::vec2(-2.5, -4.0), c + egui::vec2(3.5, 0.0), c + egui::vec2(-2.5, 4.0)]
                        };
                        ui.painter().add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
                        let text_x = x0 + 20.0;
                        let galley = ui.painter().layout_no_wrap(row.name.clone(), egui::FontId::proportional(14.0), color);
                        let clip = egui::Rect::from_min_max(egui::pos2(text_x, rect.top()), rect.right_bottom())
                            .intersect(ui.clip_rect());
                        ui.painter().with_clip_rect(clip).galley(
                            egui::pos2(text_x, rect.center().y - galley.size().y / 2.0),
                            galley,
                            color,
                        );
                        if selected && (up || down) {
                            ui.scroll_to_rect(rect, None);
                        }
                        if resp.clicked() {
                            let on_triangle = resp.interact_pointer_pos().is_some_and(|p| p.x >= x0 && p.x < x0 + 18.0);
                            clicked = Some((i, on_triangle));
                        }
                    }
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(can_select, egui::Button::new("Select")).on_hover_text("Enter").clicked() {
                        select_it = true;
                    }
                    if ui.button("Cancel").on_hover_text("Esc").clicked() {
                        cancel = true;
                    }
                });
            });
            if let Some((i, on_triangle)) = clicked {
                if on_triangle {
                    tree.toggle(i);
                } else {
                    tree.select(i);
                }
            }
            if select_it && tree.selected().is_some() {
                if let Some(dir) = tree.choose() {
                    let files = iptv::subimport::srt_files_in(&dir);
                    if files.is_empty() {
                        next = Some(Stage::Result("No .srt files in that folder.".to_string()));
                    } else {
                        d.picked_files = false;
                        action = Action::Run { files, default_lang: None };
                    }
                }
                if matches!(action, Action::None) && next.is_none() {
                    next = Some(Stage::Choose);
                }
            } else if cancel {
                next = Some(Stage::Choose);
            }
        }
        Stage::Language { files, pick } => {
            let mut run = enter;
            let mut cancel = esc;
            egui::Modal::new(egui::Id::unique("import_dialog")).show(ctx, |ui| {
                ui.set_width(width);
                ui.heading("Which language?");
                ui.add_space(6.0);
                let n = files.len();
                ui.label(format!(
                    "{n} file{} ha{} no language in {} name.",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "s" } else { "ve" },
                    if n == 1 { "its" } else { "their" }
                ));
                ui.add_space(4.0);
                egui::ComboBox::from_id_salt("import_lang").selected_text(lang_label(LANGS[*pick])).show_ui(ui, |ui| {
                    for (i, c) in LANGS.iter().enumerate() {
                        ui.selectable_value(pick, i, lang_label(c));
                    }
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Import").on_hover_text("Enter").clicked() {
                        run = true;
                    }
                    if ui.button("Cancel").on_hover_text("Esc").clicked() {
                        cancel = true;
                    }
                });
            });
            if cancel {
                action = Action::Close;
            } else if run {
                action = Action::Run { files: std::mem::take(files), default_lang: Some(LANGS[*pick].to_string()) };
            }
        }
        Stage::Result(text) => {
            let mut close = enter || esc;
            egui::Modal::new(egui::Id::unique("import_dialog")).show(ctx, |ui| {
                ui.set_width(width);
                ui.heading(&heading);
                ui.add_space(6.0);
                ui.label(text.as_str());
                ui.add_space(10.0);
                if ui.button("OK").on_hover_text("Enter").clicked() {
                    close = true;
                }
            });
            if close {
                action = Action::Close;
            }
        }
    }
    if let Some(s) = next {
        d.stage = s;
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use iptv::playlist::Kind;

    #[test]
    fn compose_result_appends_note_and_earlier_rounds() {
        assert_eq!(compose_result(&[], "Imported 1.", ""), "Imported 1.");
        assert_eq!(compose_result(&[], "Imported 1.", "Only the first file was used."), "Imported 1. Only the first file was used.");
        assert_eq!(compose_result(&["First.".to_string()], "Second.", "Note."), "First.\nSecond. Note.");
    }

    #[test]
    fn note_survives_the_language_question() {
        let mut d = ImportDialog::new(Scope::Movie { name: "Invented Film".into(), key: "movie:1".into() });
        d.note = "Only the first file was used.".into();
        d.ask_language(vec![PathBuf::from("a.srt")], String::new());
        d.finish("Imported 1 subtitle.".into());
        match d.stage {
            Stage::Result(t) => assert_eq!(t, "Imported 1 subtitle. Only the first file was used."),
            _ => panic!("expected the result stage"),
        }
    }

    #[test]
    fn single_file_without_a_marker_goes_to_the_cursor_then_the_playing_episode() {
        use SingleAttach::*;
        assert_eq!(single_file_attach(1, true, false, Some("c"), Some("p")), Key("c".into()));
        assert_eq!(single_file_attach(1, true, false, None, Some("p")), Key("p".into()));
        assert_eq!(single_file_attach(1, true, false, None, None), NeedEpisode);
    }

    #[test]
    fn marker_several_files_or_a_folder_use_the_normal_path() {
        use SingleAttach::*;
        assert_eq!(single_file_attach(1, true, true, Some("c"), None), Normal);
        assert_eq!(single_file_attach(2, true, false, Some("c"), None), Normal);
        assert_eq!(single_file_attach(1, false, false, Some("c"), None), Normal);
    }

    #[test]
    fn only_a_movie_may_stand_in_for_the_selection() {
        assert!(movie_fallback_ok(Some(Kind::Movie)));
        assert!(!movie_fallback_ok(Some(Kind::Series)));
        assert!(!movie_fallback_ok(Some(Kind::Live)));
        assert!(!movie_fallback_ok(None));
    }
}
