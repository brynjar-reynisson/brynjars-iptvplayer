//! The help card: a centered overlay with the key and settings tables from
//! `state::help`. Playback keeps running underneath.

use eframe::egui;

use crate::state::help::HelpTab;

fn focus_id() -> egui::Id {
    egui::Id::unique("help_focus")
}

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
            // Keyboard focus lives on the card and keeps Tab: egui would otherwise
            // move focus between the widgets behind it. `App::handle_keys` reads Tab.
            let anchor = ui.interact(
                egui::Rect::from_min_size(ui.min_rect().min, egui::vec2(1.0, 1.0)),
                focus_id(),
                egui::Sense::focusable_noninteractive(),
            );
            ctx.memory_mut(|m| {
                m.request_focus(anchor.id);
                m.set_focus_lock_filter(
                    anchor.id,
                    egui::EventFilter { tab: true, horizontal_arrows: false, vertical_arrows: false, escape: false },
                );
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tab_event(shift: bool) -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: if shift { egui::Modifiers::SHIFT } else { egui::Modifiers::NONE },
        }
    }

    /// Tab must not move keyboard focus to the widgets behind the card.
    #[test]
    fn tab_keeps_focus_on_the_card() {
        let ctx = egui::Context::default();
        let mut tab = HelpTab::Keys;
        let behind = std::cell::Cell::new(None);
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput { events, ..Default::default() };
            let mut out = ctx.run_pass(input, |ctx| {
                egui::Area::new(egui::Id::unique("behind_area")).show(ctx, |ui| {
                    behind.set(Some(ui.button("behind").id));
                });
                show(ctx, &mut tab);
            });
            out.textures_delta.clear();
        };
        frame(vec![]);
        frame(vec![]);
        assert!(ctx.memory(|m| m.has_focus(focus_id())));
        for shift in [false, true, false] {
            frame(vec![tab_event(shift)]);
            frame(vec![]);
            assert!(ctx.memory(|m| m.has_focus(focus_id())), "focus left the card");
            assert!(!ctx.memory(|m| m.has_focus(behind.get().unwrap())));
        }
    }
}
