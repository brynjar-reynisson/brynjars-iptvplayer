//! The help card: a centered overlay with the key and settings tables from
//! `state::help`. Playback keeps running underneath.

use eframe::egui;

use crate::state::help::HelpTab;

/// Draws the card; returns false when it should close (click outside, X).
pub fn show(ctx: &egui::Context, tab: &mut HelpTab) -> bool {
    let mut open = true;
    // Dimmed backdrop; a click on it closes the card.
    let screen = ctx.content_rect();
    let backdrop = egui::Area::new(egui::Id::unique("help_backdrop"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_black_alpha(140));
            resp
        });
    let backdrop_clicked = backdrop.inner.clicked();

    let width = (screen.width() - 40.0).clamp(280.0, 640.0);
    let height = (screen.height() - 80.0).clamp(160.0, 520.0);
    let win = egui::Window::new("Help")
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .fixed_size([width, height])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                for t in HelpTab::ALL {
                    if ui.selectable_label(*tab == t, t.label()).clicked() {
                        *tab = t;
                    }
                }
                ui.weak("   F1 / Esc closes");
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for s in tab.sections() {
                    ui.add_space(4.0);
                    ui.strong(s.title);
                    egui::Grid::new(("help_grid", s.title)).num_columns(2).spacing([14.0, 3.0]).show(ui, |ui| {
                        for r in s.rows {
                            ui.label(egui::RichText::new(r.name).monospace());
                            ui.add(egui::Label::new(r.what).wrap());
                            ui.end_row();
                        }
                    });
                }
            });
        });
    let _ = win;
    open && !backdrop_clicked
}
