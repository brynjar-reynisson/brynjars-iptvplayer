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
    // Enter is the dialog's shortcut only while the text box (or nothing) has
    // focus; a Tab-focused button or list row handles its own Enter.
    let shortcut = ctx.memory(|m| m.focused()).is_none_or(|id| id == name_id());
    let (enter, esc) = ctx.input_mut(|i| {
        (
            shortcut && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
        )
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
        return folder_tree(ctx, d, enter, esc);
    }
    // the tree is closed: the next one scrolls to its selection again
    ctx.data_mut(|m| m.remove::<Option<std::path::PathBuf>>(tree_seen_id()));
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

/// Height of one tree row.
const TREE_ROW_H: f32 = 22.0;
/// Indent per tree level, and the width of the triangle's click area.
const TREE_INDENT: f32 = 16.0;

fn tree_seen_id() -> egui::Id {
    egui::Id::unique("export_tree_seen")
}

/// The folder tree state of the dialog: Select / Cancel.
fn folder_tree(ctx: &egui::Context, d: &mut ExportDialog, enter: bool, esc: bool) -> DialogAction {
    // Keys: the tree owns the arrows while it is shown.
    let (up, down, left, right) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
        )
    });
    if let Some(t) = &mut d.picker {
        if up {
            t.step(-1);
        }
        if down {
            t.step(1);
        }
        if left {
            t.left();
        }
        if right {
            t.right();
        }
    }
    let mut select_it = enter;
    let mut cancel = esc;
    let mut clicked: Option<(usize, bool)> = None;
    let mut can_select = false;
    let seen = ctx.data(|m| m.get_temp::<Option<std::path::PathBuf>>(tree_seen_id()));
    let now_sel = d.picker.as_ref().and_then(|t| t.selected().map(|p| p.to_path_buf()));
    let scroll_to = seen.as_ref() != Some(&now_sel);
    egui::Modal::new(egui::Id::unique("export_dialog")).show(ctx, |ui| {
        ui.set_width(420.0_f32.min((ctx.content_rect().width() - 60.0).max(240.0)));
        ui.heading("Choose a folder");
        ui.add_space(6.0);
        if let Some(t) = &d.picker {
            can_select = t.selected().is_some();
            let line = t.selected().map_or("Click a folder to select it".to_string(), |p| p.display().to_string());
            ui.label(line);
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let sel = t.selected_row();
                for (i, row) in t.rows().iter().enumerate() {
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(ui.available_width(), TREE_ROW_H), egui::Sense::click());
                    if scroll_to && sel == Some(i) {
                        ui.scroll_to_rect(rect, None);
                    }
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
                    let x0 = rect.left() + 4.0 + row.depth as f32 * TREE_INDENT;
                    let c = egui::pos2(x0 + TREE_INDENT * 0.5, rect.center().y);
                    let pts = if row.expanded {
                        vec![c + egui::vec2(-4.0, -2.5), c + egui::vec2(4.0, -2.5), c + egui::vec2(0.0, 3.5)]
                    } else {
                        vec![c + egui::vec2(-2.5, -4.0), c + egui::vec2(3.5, 0.0), c + egui::vec2(-2.5, 4.0)]
                    };
                    ui.painter().add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
                    let text_x = x0 + TREE_INDENT + 4.0;
                    let galley = ui.painter().layout_no_wrap(row.name.clone(), egui::FontId::proportional(14.0), color);
                    let clip = egui::Rect::from_min_max(egui::pos2(text_x, rect.top()), rect.right_bottom())
                        .intersect(ui.clip_rect());
                    ui.painter().with_clip_rect(clip).galley(
                        egui::pos2(text_x, rect.center().y - galley.size().y / 2.0),
                        galley,
                        color,
                    );
                    if resp.clicked() {
                        let on_triangle = resp
                            .interact_pointer_pos()
                            .is_some_and(|p| p.x >= x0 && p.x < x0 + TREE_INDENT + 2.0);
                        clicked = Some((i, on_triangle));
                    }
                }
            });
        }
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
    if let Some(t) = &mut d.picker {
        if let Some((i, on_triangle)) = clicked {
            if on_triangle {
                t.toggle(i);
            } else {
                t.select(i);
            }
        }
        let now_sel = t.selected().map(|p| p.to_path_buf());
        ctx.data_mut(|m| m.insert_temp(tree_seen_id(), now_sel));
    }
    if select_it && can_select {
        d.use_picked();
        d.replace_question = None;
        // a stale error (e.g. not enough free space) belonged to the old folder
        return DialogAction::Edited;
    }
    if cancel {
        d.close_picker();
    }
    DialogAction::None
}
