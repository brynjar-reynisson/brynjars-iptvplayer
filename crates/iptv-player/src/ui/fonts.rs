//! Fallback fonts for scripts egui's bundled fonts lack (subtitle track names
//! such as Hebrew, Arabic, Hindi, Thai, Chinese, Japanese). They are the
//! Windows system fonts, read on a background thread (they are tens of MB) and
//! appended after the defaults, so Latin text is unchanged and a missing file
//! is simply skipped.

use eframe::egui;

/// `(name, file)` in the Windows fonts folder, in lookup order after egui's
/// own fonts: Latin-adjacent scripts and Hebrew/Arabic, Thai, Indic, then
/// Chinese before Japanese (shared Han characters get the Chinese shapes),
/// Korean, symbols.
const FALLBACKS: [(&str, &str); 8] = [
    ("sys-segoe", "segoeui.ttf"),
    ("sys-thai", "LeelawUI.ttf"),
    ("sys-indic", "Nirmala.ttc"),
    ("sys-zh", "msyh.ttc"),
    ("sys-ja", "YuGothR.ttc"),
    ("sys-ko", "malgun.ttf"),
    ("sys-symbols", "seguisym.ttf"),
    ("sys-ethiopic", "ebrima.ttf"),
];

/// Load the fallbacks in the background and install them when ready.
pub fn install_fallbacks(ctx: &egui::Context) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let dir = std::env::var_os("WINDIR").map_or_else(|| "C:\\Windows".into(), std::path::PathBuf::from).join("Fonts");
        let mut defs = egui::FontDefinitions::default();
        let mut added = 0;
        for (name, file) in FALLBACKS {
            let Ok(bytes) = std::fs::read(dir.join(file)) else { continue };
            defs.font_data.insert(name.to_string(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                defs.families.entry(family).or_default().push(name.to_string());
            }
            added += 1;
        }
        if added > 0 {
            ctx.set_fonts(defs);
            ctx.request_repaint();
        }
    });
}
