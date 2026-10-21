//! Look of the app: black background, white text (matches the video area).

use eframe::egui::{self, Color32, Stroke};

const BLACK: Color32 = Color32::BLACK;
const WHITE: Color32 = Color32::WHITE;
const GREY: Color32 = Color32::from_gray(48);
const DIM: Color32 = Color32::from_gray(150);

pub fn apply(ctx: &egui::Context, theme: iptv::theme::Theme) {
    if theme == iptv::theme::Theme::Standard {
        return; // egui's defaults
    }
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(WHITE);
    v.panel_fill = BLACK;
    v.window_fill = BLACK;
    v.extreme_bg_color = BLACK; // text boxes
    v.faint_bg_color = Color32::from_gray(14);
    v.window_stroke = Stroke::new(1.0, WHITE);
    v.hyperlink_color = WHITE;
    v.selection.bg_fill = Color32::from_gray(70);
    v.selection.stroke = Stroke::new(1.0, WHITE);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = BLACK;
    w.noninteractive.weak_bg_fill = BLACK;
    w.noninteractive.bg_stroke = Stroke::new(1.0, GREY);
    w.noninteractive.fg_stroke = Stroke::new(1.0, WHITE);
    // Buttons at rest: black with a white outline.
    w.inactive.bg_fill = BLACK;
    w.inactive.weak_bg_fill = BLACK;
    w.inactive.bg_stroke = Stroke::new(1.0, DIM);
    w.inactive.fg_stroke = Stroke::new(1.0, WHITE);
    w.hovered.bg_fill = GREY;
    w.hovered.weak_bg_fill = GREY;
    w.hovered.bg_stroke = Stroke::new(1.0, WHITE);
    w.hovered.fg_stroke = Stroke::new(1.5, WHITE);
    w.active.bg_fill = WHITE;
    w.active.weak_bg_fill = WHITE;
    w.active.bg_stroke = Stroke::new(1.0, WHITE);
    w.active.fg_stroke = Stroke::new(1.5, BLACK);
    w.open.bg_fill = GREY;
    w.open.weak_bg_fill = GREY;
    w.open.bg_stroke = Stroke::new(1.0, WHITE);
    w.open.fg_stroke = Stroke::new(1.0, WHITE);
    // Same look for both OS themes, and no forced window theme: forcing it left the
    // title bar buttons white on a light title bar (invisible) at times.
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);
}
