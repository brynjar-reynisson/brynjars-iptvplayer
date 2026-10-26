//! EPG spike panel: what is on now and next for the selected live channel.
//! Shows the richer of the XMLTV and `get_short_epg` answers (`iptv::epg::richer`).
//! Thin renderer over `state::epg::EpgLoader`.

use eframe::egui;
use iptv::epg::{clock, now_and_next, richer, Programme, Source};

use crate::state::epg::{EpgLoader, GuideState};

/// How many programmes are listed.
const SHOWN: usize = 6;

pub fn panel(ui: &mut egui::Ui, epg: &EpgLoader, channel: &str, stream_id: &str, now: i64, offset: i64) {
    // Not `strong()`: the black theme makes strong text black.
    let text = ui.visuals().text_color();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(channel).color(text).size(15.0));
        match epg.epg_id(stream_id) {
            Some(id) => ui.weak(format!("({id})")),
            None => ui.weak("(no EPG id from the provider)"),
        };
    });
    match epg.state() {
        GuideState::Unavailable => {
            ui.weak("No guide: needs an Xtream playlist URL in config.toml.");
            return;
        }
        GuideState::Loading => {
            ui.weak("Downloading the guide…");
        }
        GuideState::Failed(e) => {
            ui.colored_label(ui.visuals().error_fg_color, format!("Guide download failed: {e}"));
        }
        GuideState::Ready { .. } => {}
    }
    let x = epg.xmltv_for(stream_id, now);
    let short = match epg.short_for(stream_id) {
        Some(Ok(l)) => Some(now_and_next(l, now)),
        _ => None,
    };
    let (source, items) = richer(x, short, SHOWN);
    ui.add_space(2.0);
    if items.is_empty() {
        match epg.short_for(stream_id) {
            None => ui.weak("Asking the provider…"),
            Some(Err(e)) => ui.colored_label(ui.visuals().error_fg_color, e),
            Some(Ok(_)) => ui.weak("No programme information for this channel."),
        };
        return;
    }
    for (i, p) in items.iter().take(SHOWN).enumerate() {
        programme(ui, p, i, now, offset, text);
    }
    ui.add_space(2.0);
    let name = match source {
        Source::Xmltv => "XMLTV",
        Source::Short => "provider short guide",
    };
    let other = match source {
        Source::Xmltv => short.map_or("not answered".to_string(), |l| format!("short guide: {}", l.len())),
        Source::Short => format!("XMLTV: {}", x.len()),
    };
    ui.weak(format!("Source: {name} ({other})"));
}

fn programme(ui: &mut egui::Ui, p: &Programme, i: usize, now: i64, offset: i64, text: egui::Color32) {
    let live = p.start <= now;
    let head = format!("{}-{}  {}", clock(p.start, offset), clock(p.stop, offset), p.title);
    ui.horizontal_wrapped(|ui| {
        if live {
            ui.label(egui::RichText::new(head).color(text).size(15.0));
            let done = ((now - p.start) as f32 / (p.stop - p.start).max(1) as f32).clamp(0.0, 1.0);
            ui.weak(format!("{:.0}%", done * 100.0));
        } else {
            ui.label(egui::RichText::new(head).color(text));
        }
    });
    // The description only for the running programme and the next one.
    if i < 2 && !p.desc.is_empty() {
        ui.add(egui::Label::new(egui::RichText::new(&p.desc).weak().small()).wrap());
    }
}
