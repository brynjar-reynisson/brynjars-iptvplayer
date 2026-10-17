//! Browse lists (countries, channels, favorites). Thin renderers: they draw
//! virtualised rows (`ScrollArea::show_rows`) and return what was clicked;
//! all state changes happen in `ui::App` via `crate::state`.

use eframe::egui;
use iptv::favorites::Favorites;
use iptv::library::Library;
use iptv::playlist::favorite_key;

use crate::state::favs::FavRow;
use crate::state::nav::ChannelList;

pub const ROW_H: f32 = 22.0;

/// Something the user did to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// Single click: select the row (and play it, for channel lists).
    Click(usize),
    /// Double click (channel lists: play and open the full player view).
    DoubleClick(usize),
}

/// Scroll position of one list, remembered between frames so the keyboard
/// cursor can be kept visible.
#[derive(Debug, Default, Clone, Copy)]
pub struct ListView {
    offset: f32,
    view_h: f32,
}

impl ListView {
    /// Scroll offset that makes `row` visible (minimal scroll), if needed.
    fn offset_for(&self, row: usize) -> Option<f32> {
        let top = row as f32 * ROW_H;
        if top < self.offset {
            Some(top)
        } else if top + ROW_H > self.offset + self.view_h {
            Some(top + ROW_H - self.view_h)
        } else {
            None
        }
    }
}

struct RowLook<'a> {
    text: &'a str,
    selected: bool,
    playing: bool,
    star: bool,
    greyed: bool,
}

fn row(ui: &mut egui::Ui, look: RowLook<'_>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let v = ui.visuals();
    let bg = if look.selected {
        Some(v.selection.bg_fill)
    } else if resp.hovered() {
        Some(v.widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 2.0, bg);
    }
    let color = if look.greyed {
        v.weak_text_color()
    } else if look.selected {
        v.selection.stroke.color
    } else {
        v.text_color()
    };
    let mut x = rect.left() + 6.0;
    let font = egui::FontId::proportional(14.0);
    // Markers are drawn as shapes: the default fonts lack the symbol glyphs.
    let c = egui::pos2(x + 6.0, rect.center().y);
    if look.playing {
        let pts = vec![c + egui::vec2(-4.0, -6.0), c + egui::vec2(6.0, 0.0), c + egui::vec2(-4.0, 6.0)];
        ui.painter().add(egui::Shape::convex_polygon(pts, egui::Color32::from_rgb(40, 170, 90), egui::Stroke::NONE));
    }
    if look.star {
        ui.painter().add(star(egui::pos2(rect.right() - 12.0, rect.center().y), 6.5, egui::Color32::GOLD));
    }
    x += 18.0;
    // Single line, clipped: rows must stay exactly ROW_H tall.
    let galley = ui.painter().layout_no_wrap(look.text.to_string(), font, color);
    let text_area = egui::Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(rect.right() - 22.0, rect.bottom()));
    let clip = text_area.intersect(ui.clip_rect());
    ui.painter()
        .with_clip_rect(clip)
        .galley(egui::pos2(x, rect.center().y - galley.size().y.min(ROW_H) / 2.0), galley, color);
    resp
}

/// Five-pointed star filled with `color`, centred on `c` (radius `r`).
/// Not convex, so it is drawn as a fan of triangles from the centre.
fn star(c: egui::Pos2, r: f32, color: egui::Color32) -> egui::Shape {
    let pt = |i: u32| {
        let rr = if i.is_multiple_of(2) { r } else { r * 0.45 };
        let a = std::f32::consts::PI * (i as f32 / 5.0) - std::f32::consts::FRAC_PI_2;
        c + egui::vec2(a.cos() * rr, a.sin() * rr)
    };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(c, color);
    for i in 0..10 {
        mesh.colored_vertex(pt(i), color);
    }
    for i in 0..10u32 {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % 10);
    }
    egui::Shape::mesh(mesh)
}

/// Virtualised list scaffold: `n` rows, keeps `scroll_to` visible.
fn list<F>(ui: &mut egui::Ui, id: &str, n: usize, view: &mut ListView, scroll_to: Option<usize>, mut draw: F) -> Option<RowAction>
where
    F: FnMut(&mut egui::Ui, usize) -> egui::Response,
{
    let mut area = egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]);
    if let Some(off) = scroll_to.and_then(|r| view.offset_for(r)) {
        area = area.vertical_scroll_offset(off);
    }
    let mut action = None;
    // Zero vertical spacing (show_rows derives the row pitch from the ui it
    // is called on), so row i starts exactly at i * ROW_H.
    let out = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            area.show_rows(ui, ROW_H, n, |ui, range| {
                for i in range {
                    let r = draw(ui, i);
                    if r.double_clicked() {
                        action = Some(RowAction::DoubleClick(i));
                    } else if r.clicked() {
                        action = Some(RowAction::Click(i));
                    }
                }
            })
        })
        .inner;
    view.offset = out.state.clamped_offset().y;
    view.view_h = out.inner_rect.height();
    action
}

/// Country list (left panel). Returns a clicked country index.
pub fn countries(ui: &mut egui::Ui, lib: &Library, selected: Option<usize>, view: &mut ListView) -> Option<usize> {
    let cs = lib.countries();
    let a = list(ui, "countries", cs.len(), view, None, |ui, i| {
        let label = format!("{}  ({})", cs[i].name, cs[i].channels.len());
        row(ui, RowLook { text: &label, selected: selected == Some(i), playing: false, star: false, greyed: false })
    });
    a.map(|a| match a {
        RowAction::Click(i) | RowAction::DoubleClick(i) => i,
    })
}

/// Channel list of the selected country (filtered).
pub fn channels(
    ui: &mut egui::Ui,
    lib: &Library,
    l: &ChannelList,
    playing: Option<usize>,
    favs: &Favorites,
    view: &mut ListView,
    scroll_to: Option<usize>,
) -> Option<RowAction> {
    let items = l.items();
    list(ui, "channels", items.len(), view, scroll_to, |ui, i| {
        let e = lib.entry(items[i]);
        row(
            ui,
            RowLook {
                text: &e.name,
                selected: l.cursor() == Some(i),
                playing: playing == Some(items[i]),
                star: favs.contains(&favorite_key(e)),
                greyed: false,
            },
        )
    })
}

/// Favorites list; unavailable rows are greyed and marked.
pub fn favorites(
    ui: &mut egui::Ui,
    rows: &[FavRow],
    cursor: Option<usize>,
    playing: Option<usize>,
    view: &mut ListView,
    scroll_to: Option<usize>,
) -> Option<RowAction> {
    list(ui, "favorites", rows.len(), view, scroll_to, |ui, i| {
        let r = &rows[i];
        let text = if r.playable() { r.label.clone() } else { format!("{}  (unavailable)", r.label) };
        let resp = row(
            ui,
            RowLook {
                text: &text,
                selected: cursor == Some(i),
                playing: r.entry.is_some() && r.entry == playing,
                star: r.playable(),
                greyed: !r.playable(),
            },
        );
        if r.playable() {
            resp
        } else {
            resp.on_hover_text("Not in the current playlist")
        }
    })
}
