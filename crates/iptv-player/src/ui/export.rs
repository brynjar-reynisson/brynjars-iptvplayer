//! Thin egui layer of "Export recording as MKV": the dialog (with its folder
//! picker and replace question), the progress line and the done toast. All
//! state lives in `crate::state::export`.

use eframe::egui;
use iptv::export::Progress;

use crate::state::export::ExportDialog;
use crate::state::format_bytes;

/// What the user did in the dialog this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    None,
    /// Export pressed (or Enter): the caller runs `ExportDialog::submit`.
    Export,
    /// The file name text changed: the caller clears its error line.
    Edited,
    /// Replace pressed in the replace question.
    Replace,
    Cancel,
}

/// The Recordings list's red (the only hard-coded colour).
const RED: egui::Color32 = egui::Color32::from_rgb(230, 70, 70);

fn name_id() -> egui::Id {
    egui::Id::unique("export_file_name")
}

/// The modal dialog. It reads Enter/Esc itself (the app's `handle_keys`
/// returns early while it is open).
pub fn dialog(ctx: &egui::Context, d: &mut ExportDialog, error: Option<&str>) -> DialogAction {
    let (enter, esc) = ctx.input_mut(|i| {
        (i.consume_key(egui::Modifiers::NONE, egui::Key::Enter), i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    });
    let mut action = DialogAction::None;
    // Three states of one window: replace question, folder picker, form.
    if let Some(req) = d.replace_question.clone() {
        let name = req.dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        egui::Modal::new(egui::Id::unique("export_dialog")).show(ctx, |ui| {
            ui.set_width(420.0_f32.min((ctx.content_rect().width() - 60.0).max(240.0)));
            ui.heading("Replace file?");
            ui.add_space(6.0);
            ui.label(format!("{name} already exists. Replace it?"));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Replace").on_hover_text("Enter").clicked() || enter {
                    action = DialogAction::Replace;
                }
                if ui.button("Keep").on_hover_text("Esc").clicked() || esc {
                    d.replace_question = None;
                }
            });
        });
        return action;
    }
    if d.picker.is_some() {
        let mut up = false;
        let mut entered = None;
        let mut use_it = enter;
        let mut back = esc;
        let mut can_use = false;
        egui::Modal::new(egui::Id::unique("export_dialog")).show(ctx, |ui| {
            ui.set_width(420.0_f32.min((ctx.content_rect().width() - 60.0).max(240.0)));
            ui.heading("Choose a folder");
            ui.add_space(6.0);
            if let Some(p) = &d.picker {
                can_use = p.location().is_some();
                let here = p.location().map_or("Drives".to_string(), |l| l.display().to_string());
                ui.label(here);
                ui.add_space(4.0);
                ui.add_enabled_ui(can_use, |ui| up = ui.button("Up").clicked());
                egui::ScrollArea::vertical().max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (i, name) in p.rows().iter().enumerate() {
                        if ui.selectable_label(false, name).clicked() {
                            entered = Some(i);
                        }
                    }
                });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(can_use, egui::Button::new("Use this folder")).on_hover_text("Enter").clicked() {
                    use_it = true;
                }
                if ui.button("Back").on_hover_text("Esc").clicked() {
                    back = true;
                }
            });
        });
        if let Some(p) = &mut d.picker {
            if up {
                p.up();
            }
            if let Some(i) = entered {
                p.enter(i);
            }
        }
        if use_it && can_use {
            d.use_picked();
            d.replace_question = None;
        } else if back {
            d.close_picker();
        }
        return action;
    }
    let mut change = false;
    let mut run = enter;
    let mut cancel = esc;
    egui::Modal::new(egui::Id::unique("export_dialog")).show(ctx, |ui| {
        ui.set_width(420.0_f32.min((ctx.content_rect().width() - 60.0).max(240.0)));
        ui.heading("Export recording as MKV");
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let folder = d.folder().map_or("(none)".to_string(), |f| f.display().to_string());
            ui.label(format!("Folder: {folder}"));
            if ui.button("Change folder").clicked() {
                change = true;
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("File name:");
            let r = ui.add(egui::TextEdit::singleline(&mut d.name).id(name_id()).desired_width(f32::INFINITY));
            if r.changed() {
                action = DialogAction::Edited;
            }
            if ctx.memory(|m| m.focused().is_none()) {
                r.request_focus();
            }
        });
        if let Some(e) = error {
            ui.add_space(4.0);
            ui.colored_label(RED, e);
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button("Export").on_hover_text("Enter").clicked() {
                run = true;
            }
            if ui.button("Cancel").on_hover_text("Esc").clicked() {
                cancel = true;
            }
        });
    });
    if action == DialogAction::Edited {
        d.replace_question = None;
    }
    if change {
        d.open_picker();
        d.replace_question = None;
    } else if cancel {
        action = DialogAction::Cancel;
    } else if run {
        action = DialogAction::Export;
    }
    action
}

/// `Exporting <name>  mm:ss  <size>  NN %`, a thin bar when the fraction is
/// known, and a Cancel button. True = Cancel was clicked.
pub fn progress_line(ui: &mut egui::Ui, name: &str, p: Option<Progress>) -> bool {
    let mut cancel = false;
    let p = p.unwrap_or(Progress { out_secs: 0.0, bytes: 0, fraction: None });
    let pct = p.fraction.map_or(String::new(), |f| format!("  {:.0} %", (f * 100.0).clamp(0.0, 100.0)));
    ui.horizontal(|ui| {
        ui.label(format!("Exporting {name}  {}  {}{pct}", super::clock(p.out_secs), format_bytes(p.bytes)));
        cancel = ui.button("Cancel").clicked();
    });
    if let Some(f) = p.fraction {
        ui.add(egui::ProgressBar::new(f.clamp(0.0, 1.0)).desired_height(4.0));
    }
    cancel
}

/// A short message at the bottom centre; does not block input.
pub fn toast(ctx: &egui::Context, text: &str) {
    egui::Area::new(egui::Id::unique("export_toast"))
        .order(egui::Order::Tooltip)
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -40.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(text);
            });
        });
}
