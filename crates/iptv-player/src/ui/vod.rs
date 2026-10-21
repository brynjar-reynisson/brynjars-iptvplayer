//! Movies and Series panels: search box, group filter menu, virtualised
//! result lists (`live::list`, fixed rows) and the opened show's seasons and
//! episodes. Thin renderers: they return what the user did; `ui::App`
//! applies it through `crate::state::vod`.

use std::collections::HashSet;

use eframe::egui;
use iptv::favorites::Favorites;
use iptv::library::Library;
use iptv::playlist::favorite_key;
use iptv::recent_series::Recent;
use iptv::series::episode_label;

use super::live::{list, row, ListView, RowAction, RowLook};
use crate::state::nav::ChannelList;
use crate::state::vod::{
    episode_text, filter_choices, group_choices, season_label, EpisodeRow, OpenShow, VodGroup, SEARCH_LIMIT,
};

pub fn movie_search_id() -> egui::Id {
    egui::Id::unique("movie_search")
}

pub fn series_search_id() -> egui::Id {
    egui::Id::unique("series_search")
}

/// One line laid out right to left: add the Groups button first, then the
/// search box, which takes the remaining width.
pub fn search_row<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), f).inner).inner
}

/// Search box filling the remaining width (lay it out last, in a
/// right-to-left row, or a resizable panel grows every frame); returns the
/// new text when it changed.
pub fn search_box(ui: &mut egui::Ui, id: egui::Id, text: &str, hint: &str, focus: bool) -> Option<String> {
    let mut t = text.to_string();
    let r = ui.add(egui::TextEdit::singleline(&mut t).id(id).hint_text(hint).desired_width(ui.available_width()));
    if focus {
        r.request_focus();
    }
    r.changed().then_some(t)
}

/// What the user did in the Groups menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupAction {
    /// Hide (`true`) or show these groups.
    Set(Vec<String>, bool),
}

/// "Groups" menu: a filter box, "Show all" (the listed groups), and a
/// checkbox per movie/series group (checked = shown). Hidden groups that
/// left the playlist stay listed so they can be unhidden. `groups` is `None`
/// while the list is still being computed.
pub fn group_menu(
    ui: &mut egui::Ui,
    groups: Option<&[VodGroup]>,
    hidden: &HashSet<String>,
    filter: &mut String,
) -> Option<GroupAction> {
    use egui::containers::menu::{MenuButton, MenuConfig};
    let label = if hidden.is_empty() { "Groups".to_string() } else { format!("Groups ({} hidden)", hidden.len()) };
    let mut out = None;
    MenuButton::new(label)
        .config(MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
        .ui(ui, |ui| {
            let Some(groups) = groups else {
                ui.spinner();
                return;
            };
            let choices = group_choices(groups, hidden);
            ui.weak("Uncheck a group to hide it from Movies and Series results.");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(filter).hint_text("Filter groups").desired_width(200.0));
                let listed = filter_choices(&choices, filter);
                let to_show: Vec<String> = listed.iter().filter(|c| c.hidden).map(|c| c.name.clone()).collect();
                let what = if filter.trim().is_empty() { "Show all" } else { "Show all listed" };
                if ui.add_enabled(!to_show.is_empty(), egui::Button::new(what)).clicked() {
                    out = Some(GroupAction::Set(to_show, false));
                }
            });
            let listed = filter_choices(&choices, filter);
            if listed.is_empty() {
                ui.weak("No groups match the filter.");
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                for c in listed {
                    let mut shown = !c.hidden;
                    let name = if c.name.trim().is_empty() { "(no group)" } else { c.name.as_str() };
                    let text = if c.present {
                        format!("{name}  ({})", c.count)
                    } else {
                        format!("{name}  (not in playlist)")
                    };
                    if ui.checkbox(&mut shown, text).changed() {
                        out = Some(GroupAction::Set(vec![c.name.clone()], !shown));
                    }
                }
            });
        });
    out
}

/// One line under the search box: hint, "Searching…", or the result count.
/// `all_hidden`: every group of this tab is hidden, so nothing can match.
pub fn result_note(ui: &mut egui::Ui, query: &str, searching: bool, n: usize, hint: &str, all_hidden: bool) {
    if all_hidden {
        ui.weak("All groups hidden: use Groups > Show all.");
    } else if query.trim().is_empty() {
        ui.weak(hint);
    } else if searching && n == 0 {
        ui.weak("Searching…");
    } else if n == 0 {
        ui.weak("No matches.");
    } else if n >= SEARCH_LIMIT {
        ui.weak(format!("First {SEARCH_LIMIT} matches: type more to narrow down."));
    } else {
        ui.weak(if n == 1 { "1 match".to_string() } else { format!("{n} matches") });
    }
}

/// Movie results.
pub fn movies(
    ui: &mut egui::Ui,
    lib: &Library,
    l: &ChannelList,
    playing: Option<usize>,
    favs: &Favorites,
    view: &mut ListView,
    scroll_to: Option<usize>,
) -> Option<RowAction> {
    let items = l.items();
    list(ui, "movies", items.len(), view, scroll_to, |ui, i| {
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
        .on_hover_text(&e.group)
    })
}

/// Show results: name + episode count.
pub fn shows(ui: &mut egui::Ui, lib: &Library, l: &ChannelList, view: &mut ListView, scroll_to: Option<usize>) -> Option<RowAction> {
    let items = l.items();
    list(ui, "shows", items.len(), view, scroll_to, |ui, i| {
        let s = &lib.shows()[items[i]];
        let n: usize = s.seasons.iter().map(|x| x.episodes.len()).sum();
        let text = format!("{}  ({n} episode{})", s.name, if n == 1 { "" } else { "s" });
        row(ui, RowLook { text: &text, selected: l.cursor() == Some(i), playing: false, star: false, greyed: false })
    })
}

/// Recently watched shows (newest first) under an empty search box: show name
/// and the episode it stopped at. Shown rows only need names, so a show that
/// left the playlist is still listed (opening it says so).
pub fn recent(
    ui: &mut egui::Ui,
    lib: &Library,
    items: &[Recent],
    cursor: Option<usize>,
    view: &mut ListView,
    scroll_to: Option<usize>,
) -> Option<RowAction> {
    ui.strong("Recently watched");
    list(ui, "recent", items.len(), view, scroll_to, |ui, i| {
        let r = &items[i];
        let label = lib
            .find_by_key(&r.key)
            .and_then(|e| episode_label(&lib.entry(e).name))
            .and_then(|l| l.split(" - ").next().map(str::to_string));
        let text = match label {
            Some(l) => format!("{}  ({l})", r.show),
            None => r.show.clone(),
        };
        row(ui, RowLook { text: &text, selected: cursor == Some(i), playing: false, star: false, greyed: false })
    })
}

/// The opened show: season headers (greyed, not selectable) and episodes.
pub fn episodes(
    ui: &mut egui::Ui,
    lib: &Library,
    open: &OpenShow,
    playing: Option<usize>,
    favs: &Favorites,
    titles: Option<&iptv::episode_titles::Titles>,
    view: &mut ListView,
    scroll_to: Option<usize>,
) -> Option<RowAction> {
    let rows = open.rows();
    list(ui, "episodes", rows.len(), view, scroll_to, |ui, i| match rows[i] {
        EpisodeRow::Season(n) => {
            let text = season_label(n);
            row(ui, RowLook { text: &text, selected: false, playing: false, star: false, greyed: true })
        }
        EpisodeRow::Episode(e) => {
            let text = episode_text(lib, open.show, e, titles);
            row(
                ui,
                RowLook {
                    text: &format!("    {text}"),
                    selected: open.cursor() == Some(i),
                    playing: playing == Some(e),
                    star: favs.contains(&favorite_key(lib.entry(e))),
                    greyed: false,
                },
            )
        }
    })
}
