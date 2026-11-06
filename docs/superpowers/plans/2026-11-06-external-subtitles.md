# External Subtitles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Import `.srt` files for a show or movie, play them as extra tracks in the CC menu, sync them automatically from an embedded reference track, and let the user nudge the delay by hand.

**Architecture:** Pure logic lives in the `iptv` crate (`subsync` = offset search, `subimport` = file-name parsing and matching, `extsubs` = the on-disk store) and in GUI-free `iptv-player::state` modules (`autosync`); `mpv_backend` gets thin wrappers over mpv's `sub-add`, `sub-delay`, `secondary-sid`, `secondary-sub-start`; `ui::player::Player` re-adds imported tracks for every supervisor generation and feeds the auto-sync; `ui::mod` and `ui::vod` get the Import button, dialog, marks, keys and notices.

**Tech Stack:** Rust (workspace `iptv` + `iptv-player`), eframe/egui 0.37, libmpv2-sys, `serde_json`; no new dependencies (the Windows file dialog is an `extern "system"` call, like the existing `GetDiskFreeSpaceExW`).

**Spec:** `docs/superpowers/specs/2026-11-06-external-subtitles-design.md` (read it first; `AGENTS.md` holds the repo rules).

## Global Constraints

- No new crates. No ffmpeg dependency and no second provider connection for sync (spec: "Non-goals").
- Only `.srt`; UTF-8, UTF-8 with BOM, or Windows-1252; max file size 2 MiB (`extsubs::MAX_FILE_BYTES`).
- Never print or store provider URLs/credentials; errors name files, not URLs. Examples in docs/tests use imaginary names ("Example Show", `S02E06`).
- Sync constants (spec): search ±90 s step 0.05 s, match tolerance 0.5 s, `MIN_LINES` 25, recheck at 60 lines, apply only when best score >= 1.5 x best rival (rival = a shift more than 3 s away) and >= 50 % of reference lines match, ignore shifts < 0.15 s, a manual nudge always wins.
- Imported mpv tracks use the title `"Imported"` (`subtitles::IMPORTED_TITLE`) so the remembered `SubChoice` can tell them from embedded tracks of the same language.
- New JSON store follows `positions.rs`: `version: 1`, atomic write (`fsutil::atomic_write`), unreadable file moved aside with `fsutil::backup_path`, never overwritten.
- Keep each existing file's line endings (check with `grep -c $'\r' <file>` before editing; do not use `sed -i` on CRLF files). New source files: `git add`. Add key bindings to `state/help.rs` KEYS and README.
- Commit on branch `feature/external-subtitles` (this worktree); end commit messages with the two attribution lines from the session instructions.

## Review Focus

1. Empty, cue-less, oversized (> 2 MiB) or binary `.srt` files: import refuses them with a message, stores nothing (Task 2 test).
2. Two files for the same episode and language in one folder, or a file whose name has no episode number: reported as "duplicate"/"not matched", never silently dropped or overwritten (Task 2 tests).
3. Reconnect/retry or a new episode mid-session: imported tracks are added again once per generation, the chosen track and its delay come back, and a delay never leaks onto an embedded track (Task 5 test + `ext_sub_demo`).
4. Auto-sync with fewer than 25 lines, unrelated reference timing, or after a manual nudge: never applies a shift (Task 4 tests).
5. A stored copy deleted from the data dir, or an episode whose key left the playlist: the track is skipped with a log line (no URL), nothing crashes (Task 2 and Task 5 tests).

---

### Task 1: Offset search (`iptv::subsync`)

**Files:**
- Create: `crates/iptv/src/subsync.rs`; Modify: `crates/iptv/src/lib.rs` (`pub mod subsync;`)
- Test: in-module `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub const MIN_LINES: usize = 25; pub const RECHECK_LINES: usize = 60; pub const MIN_APPLY_SECS: f64 = 0.15;`
  - `pub fn decode_srt(bytes: &[u8]) -> String` (strip BOM; UTF-8 else Windows-1252; CRLF/CR to LF)
  - `pub fn cue_starts(srt: &str) -> Vec<f64>` (sorted start seconds of every `HH:MM:SS,mmm --> ...` line; `.` also accepted)
  - `pub struct Match { pub shift: f64, pub matched: usize, pub total: usize, pub rival: usize }` and `impl Match { pub fn confident(&self) -> bool }`
  - `pub fn best_shift(reference: &[f64], file: &[f64]) -> Option<Match>` (`shift` is what mpv's `sub-delay` needs: `reference ~ file + shift`; `None` when either list is empty)

- [ ] **Step 1: Write failing tests** `decode_handles_bom_utf8_and_cp1252` (BOM bytes `EF BB BF` stripped; bytes `0xE9` decode to `é`, `0x92` to `'`), `cue_starts_reads_comma_and_dot_and_sorts`, `finds_a_constant_shift` (file starts = 40 pseudo-random times over 20 min, reference = file + 11.9 -> `shift` within 0.1 of 11.9, `confident()`), `finds_a_small_negative_shift` (-0.45), `tolerates_missing_and_extra_lines` (drop 25 % of reference lines, add 20 % random extras: still within 0.15 and confident), `too_few_lines_is_not_confident` (20 lines -> `confident()` false), `unrelated_timing_is_not_confident` (two independent random lists), `empty_lists_give_none`.
- [ ] **Step 2: Run** `cargo test -p iptv subsync` -> FAIL (module missing).
- [ ] **Step 3: Implement** the module. `best_shift`: score(s) = number of reference starts that have a `file + s` within 0.5 s (sorted list + binary search); evaluate s in -90..=90 step 0.05; `rival` = best score at a shift more than 3 s from the best; final `shift` = median of the shifts within 3 s of the best that reach the top score. `confident()` = `total >= MIN_LINES && matched * 2 >= total && matched * 2 >= rival * 3`. Pseudo-random test data: a small LCG in the test module (no `rand` crate).
- [ ] **Step 4: Run** `cargo test -p iptv subsync` -> PASS.
- [ ] **Step 5: Commit** `git add crates/iptv/src/subsync.rs crates/iptv/src/lib.rs` ... `feat: subtitle offset search (subsync)`.

### Task 2: Import naming, matching and the store (`iptv::subimport`, `iptv::extsubs`)

**Files:**
- Create: `crates/iptv/src/subimport.rs`, `crates/iptv/src/extsubs.rs`; Modify: `crates/iptv/src/lib.rs`, `crates/iptv/src/subtitles.rs`
- Test: in-module tests (tempdir)

**Interfaces:**
- Consumes: `subsync::{decode_srt, cue_starts}`, `fsutil::{atomic_write, backup_path}`, the `positions.rs` load/quarantine pattern.
- Produces:
  - in `subtitles.rs`: `pub const IMPORTED_TITLE: &str = "Imported"`; `pub fn normalize_lang(code: &str) -> Option<&'static str>` (2- or 3-letter code of any language `language_name` knows -> its 3-letter code, e.g. `"en"` -> `"eng"`, `"is"|"ice"` -> `"isl"`); `label()` returns `"<Language> (imported)"` for a track whose title is `IMPORTED_TITLE`.
  - `subimport.rs`: `pub struct NameInfo { pub lang: Option<&'static str>, pub season_episode: Option<(u32, u32)> }`; `pub fn parse_name(file_name: &str) -> NameInfo` (dot/space/dash separated tokens: language token via `normalize_lang`; `S02E06`, `s02e06`, `2x06` case-insensitive); `pub struct Target { pub key: String, pub season: u32, pub episode: u32 }`; `pub struct Plan { pub matched: Vec<(PathBuf, String)>, pub unmatched: Vec<PathBuf>, pub duplicates: Vec<PathBuf> }`; `pub fn plan_for_show(files: &[PathBuf], targets: &[Target]) -> Plan` (files sorted by name first; for one key the first file per language wins, later ones go to `duplicates`; a file matching no target goes to `unmatched`); `pub fn srt_files_in(dir: &Path) -> Vec<PathBuf>` (non-recursive, `.srt` case-insensitive, sorted); `pub struct Report { pub imported: usize, pub message: String, pub need_language: Vec<PathBuf> }`; `pub fn import_show(store: &mut ExtSubs, files: &[PathBuf], targets: &[Target], default_lang: Option<&str>) -> Report`; `pub fn import_movie(store: &mut ExtSubs, key: &str, file: &Path, default_lang: Option<&str>) -> Report`. A file with no language in its name and no `default_lang` is put in `need_language` (not imported); the UI asks once and calls again. `message` reads like `Imported 28 of 30. Not matched (2): a.srt, b.srt. Duplicates (1): c.srt.` (names only, max 5 listed then `...`).
  - `extsubs.rs`: `pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024; pub const MAX_FILES: usize = 2000;` `pub struct ExtSub { pub lang: String, pub file: String, pub delay: f64, pub manual: bool }` (serde, `delay`/`manual` default 0/false); `pub struct ExtSubs` with `load(dir: &Path) -> ExtSubs`, `for_key(&self, key: &str) -> &[ExtSub]`, `add(&mut self, key: &str, lang: &str, bytes: &[u8]) -> anyhow::Result<()>` (refuses > `MAX_FILE_BYTES`, bytes that decode to no cue, and > `MAX_FILES`; writes `<dir>/subtitles/<id>.srt` as normalised UTF-8; same key+lang replaces the entry and deletes the old copy, delay reset), `remove(&mut self, key, lang)`, `set_delay(&mut self, key, lang, delay: f64, manual: bool)`, `path_of(&self, s: &ExtSub) -> PathBuf`, `cue_starts_of(&self, s: &ExtSub) -> Vec<f64>` (empty if the copy is missing), `save_if_dirty(&mut self) -> anyhow::Result<()>`; index file `subtitles.json` = `{"version":1,"next_id":N,"items":{key:[ExtSub]}}`.

- [ ] **Step 1: Write failing tests.** `subimport`: `parse_name_reads_episode_and_language` (`"Example.Show.S02E06.en.srt"` -> `(2,6)`, `eng`; `"example show 2x06 Icelandic.srt"` -> `(2,6)`, `isl`; `"movie.srt"` -> both `None`), `plan_matches_by_episode_and_reports_the_rest` (3 targets, 4 files incl. one unknown name and one second `S02E01` -> 2 matched, 1 unmatched, 1 duplicate), `import_show_needs_a_language_when_the_name_has_none`, `import_movie_attaches_any_name`, `message_lists_at_most_five_names`. `extsubs`: `add_roundtrips_and_survives_reload`, `same_key_and_language_replaces_and_deletes_the_old_copy`, `refuses_empty_no_cue_and_oversized_files` (nothing written, index unchanged), `unreadable_index_is_moved_aside_and_save_refuses`, `missing_copy_gives_no_cues_and_no_panic`, `set_delay_and_manual_flag_persist`. `subtitles`: `normalize_lang_maps_two_and_three_letter_codes`, `imported_track_label_has_suffix`.
- [ ] **Step 2: Run** `cargo test -p iptv subimport extsubs subtitles` -> FAIL.
- [ ] **Step 3: Implement** the three files as specified (store mirrors `Positions`: `dir`, `items`, `dirty`, `blocked`).
- [ ] **Step 4: Run** the same tests -> PASS; `cargo test -p iptv` -> PASS.
- [ ] **Step 5: Commit** (`git add` both new files) `feat: subtitle import planning and the imported-subtitle store`.

### Task 3: mpv wrappers and the headless demo

**Files:**
- Modify: `crates/iptv-player/src/mpv_backend.rs` (add after `set_sub`); Create: `crates/iptv-player/examples/ext_sub_demo.rs`

**Interfaces:**
- Produces on `MpvBackend`: `pub fn sub_add(&mut self, path: &Path, lang: &str) -> bool` (mpv command `sub-add <path> auto Imported <lang>`; true on success), `pub fn set_sub_delay(&mut self, secs: f64)`, `pub fn sub_delay(&self) -> f64`, `pub fn set_secondary_sub(&mut self, id: Option<i64>)` (sets `secondary-sid`; always sets `secondary-sub-visibility=no`), `pub fn secondary_sub_start(&self) -> Option<f64>` (property `secondary-sub-start`, `None` when unavailable). Also `pub fn has_secondary_sub_start(&self) -> bool` (true when mpv knows the property, i.e. reading it without a file fails with "unavailable" and not "not found"); the demo asserts it is true for the bundled libmpv.

- [ ] **Step 1: Write the demo** `ext_sub_demo.rs` in the style of `retry_demo.rs`/`record_demo.rs` (read one of them first): with ffmpeg (skip with a message if missing) make a 20 s test video with one embedded subrip track whose invented lines start at 2.0, 6.0, 11.0, 16.5 s, and an external `.srt` whose lines start 2.5 s later; play it headless through `MpvBackend`; assert in order: after `Playing`, `sub_tracks()` gains exactly one track with title `Imported` and the given language after `sub_add`; `sub_delay()` reads back what `set_sub_delay(-2.5)` set; with `set_secondary_sub(Some(embedded id))` the values of `secondary_sub_start()` seen while playing include 2.0, 6.0 and 11.0 (within 0.05) and never draw anything (no panic with `vo=null`); after a second `load` of the same file the imported track is gone (proves it must be re-added per generation). Print `RESULT: ALL CHECKS OK`.
- [ ] **Step 2: Run** `cargo run -p iptv-player --example ext_sub_demo` -> fails to compile (functions missing).
- [ ] **Step 3: Implement** the five wrappers next to `set_sub` using the same `mpv_set_property_string`/`get_opt_string`/`mpv_command` style.
- [ ] **Step 4: Run** the demo -> `RESULT: ALL CHECKS OK`; `cargo test --workspace` -> PASS.
- [ ] **Step 5: Commit** (`git add` the example) `feat: mpv wrappers for external subtitles, delay and secondary track`.

### Task 4: Auto-sync controller (`state::autosync`)

**Files:**
- Create: `crates/iptv-player/src/state/autosync.rs`; Modify: `crates/iptv-player/src/state/mod.rs` (`pub mod autosync;`), `crates/iptv/src/subtitles.rs` (add `pick_reference`)
- Test: in-module

**Interfaces:**
- Consumes: `iptv::subsync::{best_shift, MIN_LINES, RECHECK_LINES, MIN_APPLY_SECS}`.
- Produces:
  - `pub enum Step { Nothing, Apply(f64), Unsure }`
  - `pub struct AutoSync`; `AutoSync::new(file_starts: Vec<f64>, current_delay: f64) -> AutoSync`; `observe(&mut self, start: Option<f64>)` (a start not yet seen, rounded to ms, counts as one new reference line; `None` and repeats are ignored); `lines_seen(&self) -> usize`; `step(&mut self) -> Step`; `manual(&mut self)` (user nudged: every later `step` is `Nothing`).
  - `iptv::subtitles::pick_reference(tracks: &[SubTrack], prefer_lang: Option<&str>) -> Option<i64>`: first track that is not imported (title != `IMPORTED_TITLE`) and not forced; one whose language equals `prefer_lang` wins; `None` when there is none.
- Behaviour of `step`: `Nothing` below `MIN_LINES` lines; at `MIN_LINES` compute once: confident and `|shift - current_delay| >= MIN_APPLY_SECS` -> `Apply(shift)` (and remember it as the current delay); confident but smaller -> `Nothing`; not confident -> `Unsure` (once); at `RECHECK_LINES` compute once more with all lines: `Apply` only if confident and it differs from the current delay by at least `MIN_APPLY_SECS`, otherwise `Nothing`.

- [ ] **Step 1: Write failing tests:** `waits_for_enough_lines` (24 lines -> `Nothing`), `applies_the_shift_at_25_lines` (file starts + 0.45 = reference; feed 25 -> `Apply` within 0.1 of 0.45, then `Nothing` on the next call), `tiny_shift_is_ignored` (0.1), `unrelated_timing_gives_unsure_once`, `repeats_and_seeks_do_not_inflate_the_count` (feed the same start 50 times -> `lines_seen() == 1`), `recheck_at_60_lines_can_correct`, `manual_nudge_stops_everything`, `few_lines_with_a_wrong_peak_never_apply` (13-line data that matches a false +80 s peak: no `Apply`), `pick_reference_prefers_language_then_first_embedded`, `pick_reference_ignores_imported_and_forced`.
- [ ] **Step 2: Run** `cargo test -p iptv-player autosync` and `cargo test -p iptv pick_reference` -> FAIL.
- [ ] **Step 3: Implement** both.
- [ ] **Step 4: Run** -> PASS.
- [ ] **Step 5: Commit** (`git add` the new file) `feat: auto-sync controller and reference track choice`.

### Task 5: Player integration (`ui::player::Player`)

**Files:**
- Modify: `crates/iptv-player/src/ui/player.rs` (struct, `set_want_sub` neighbourhood, `select_sub`, `update`, `stop`), `crates/iptv-player/src/ui/mod.rs` (`App::start_with` passes the imported tracks)
- Test: unit tests for the pure parts; the headless demo extended in Task 3's file

**Interfaces:**
- Consumes: Task 2 (`ExtSubs`, `ExtSub`), Task 3 wrappers, Task 4 (`AutoSync`, `Step`, `pick_reference`).
- Produces:
  - `pub struct ImportedTrack { pub lang: String, pub path: PathBuf, pub delay: f64, pub manual: bool, pub starts: Vec<f64> }`
  - `Player::set_imported(&mut self, tracks: Vec<ImportedTrack>)` (called by `start_with` right after `set_want_sub`; resets per-generation state)
  - `pub enum SyncEvent { Applied { lang: String, delay: f64 }, Unsure { lang: String }, Nudged { lang: String, delay: f64 } }`
  - `Player::poll_sync(&mut self) -> Vec<SyncEvent>`; `Player::nudge(&mut self, step: f64) -> bool` (adds `step` to the delay of the currently selected imported track, applies it to mpv, marks it manual, stops its `AutoSync`; false if none is selected); `Player::current_imported(&self) -> Option<&ImportedTrack>`.
- Behaviour: when the status is `Playing` and the generation changed, for each imported track whose file exists call `sub_add` (log `imported subtitle missing: <file name>` otherwise), then apply the wanted subtitle (`pick` already ranks the `Imported` title), then `apply_delay()`: delay of the selected imported track, else 0. `select_sub` also calls `apply_delay`. While an imported track is selected and an `AutoSync` exists for it: set the secondary track once per generation to `pick_reference(embedded tracks, prefer)` (skip if none), feed `secondary_sub_start()` into `observe` every `update`, and turn a `Step::Apply` into `set_sub_delay` + store + `SyncEvent::Applied`. When the selected track changes away from imported, or on `stop`, clear the secondary track (`set_secondary_sub(None)`). No `AutoSync` for a track with `manual == true`. No `AutoSync` at all (log `subtitle auto-sync off: mpv lacks secondary-sub-start`, once) when `has_secondary_sub_start()` is false.

- [ ] **Step 1: Write failing tests** for the pure helper `fn delay_for(selected_lang_title: Option<(&str, &str)>, tracks: &[ImportedTrack]) -> f64` (imported lang+title match -> its delay; embedded track or none -> 0.0): `delay_resets_for_embedded_tracks`, `delay_follows_the_selected_imported_track`, `manual_track_gets_no_autosync`.
- [ ] **Step 2: Run** `cargo test -p iptv-player player` -> FAIL.
- [ ] **Step 3: Implement** the Player changes; in `App::start_with` build `ImportedTrack`s from `self.extsubs.for_key(pb.key())` (add `extsubs: ExtSubs` to `App`, loaded in `App::new` from the data dir). Local recordings and live get none.
- [ ] **Step 4: Extend `ext_sub_demo`** with the reconnect check (second `load`: after `Playing` again, tracks + delay are back when the app-side logic re-adds them is out of scope for the demo; assert only what Task 3 asserts) and run `cargo test --workspace` -> PASS.
- [ ] **Step 5: Commit** `feat: play imported subtitles and auto-sync them`.

### Task 6: Import flow in the UI

**Files:**
- Create: `crates/iptv-player/src/ui/filedialog.rs`; Modify: `crates/iptv-player/src/ui/mod.rs` (`pub mod filedialog;`, `App` fields, Series header, Movies panel, dialog), `crates/iptv-player/src/ui/vod.rs` (`episodes` gets a mark callback), `crates/iptv-player/src/state/export.rs` only if `FolderTree` needs a `pub` item
- Test: unit tests for the pure parts; manual GUI check (Task 8)

**Interfaces:**
- Consumes: Task 2 (`import_show`, `import_movie`, `srt_files_in`, `Report`, `Target`), `FolderTree` (`state::export`) for choosing a folder.
- Produces:
  - `filedialog::parse_multiselect(buf: &[u16]) -> Vec<PathBuf>` (the `OFN_ALLOWMULTISELECT` buffer: a lone path = one file; otherwise directory then names, NUL separated, double-NUL end)
  - `filedialog::pick_srt_files() -> std::sync::mpsc::Receiver<Vec<PathBuf>>` (thread; Win32 `GetOpenFileNameW` from comdlg32 via `extern "system"`, filter `*.srt`, multi-select, explorer style; empty vec on cancel)
  - `vod::episodes(..., has_subs: &dyn Fn(usize) -> Option<String>)` where the string (for example `"+ en"`) is appended to the row text after two spaces.
- UI: Series open-show header row gets a button "Import subtitles..." after the show name; Movies panel gets the same button under the result note, enabled when a movie is selected or playing. The click opens an `egui::Modal` (same look as the export dialog; Esc closes) with "Choose files..." (runs `pick_srt_files`, polled each frame), "Choose folder..." (opens the existing `FolderTree` picker; its choice goes through `srt_files_in`), and the `Report.message` afterwards. When `need_language` is non-empty the modal shows a language dropdown (default English) and "Import" re-runs with `default_lang`. Series imports use `Target`s built from the open show's episodes (`iptv::series::parse_episode` season/episode + `favorite_key`); Movies imports use `import_movie` with the selected/playing movie's key (first file only; say so in the message if more were chosen). After a successful import: `extsubs.save_if_dirty()` (error -> `self.message`), and if the playing entry got a track, the next `start_with` or a new `Player::set_imported` call picks it up (call it right away for the playing entry).

- [ ] **Step 1: Write failing tests** in `filedialog.rs`: `multiselect_single_path`, `multiselect_directory_plus_names`, `multiselect_empty_is_empty`.
- [ ] **Step 2: Run** `cargo test -p iptv-player filedialog` -> FAIL.
- [ ] **Step 3: Implement** `filedialog.rs` (+ `Cargo`-free FFI), then the UI wiring and the episode marks.
- [ ] **Step 4: Run** `cargo test --workspace` -> PASS; `cargo build -p iptv-player` -> OK.
- [ ] **Step 5: Commit** (`git add` the new file) `feat: import subtitles dialog for shows and movies`.

### Task 7: Nudge keys, notices and track removal

**Files:**
- Create: `crates/iptv-player/src/state/subkeys.rs`; Modify: `state/mod.rs`, `state/help.rs`, `ui/mod.rs` (`handle_keys`, CC menu, update loop), `README.md` (keys list only)
- Test: in-module

**Interfaces:**
- Produces: `pub fn nudge(typing: bool, tab_focused: bool, sub_menu: bool, help_open: bool, vod_playing: bool, earlier: bool, later: bool) -> Option<f64>` returning `Some(-0.1)` for **Z**, `Some(0.1)` for **X**, only when a movie/episode plays, nothing is typed, no tab button has focus, the subtitle menu and help are closed (modelled on `state::next::shift_right`; Z/X are free in `handle_keys` today, verify).
- UI: `App` consumes the keys, calls `Player::nudge`, stores the result (`extsubs.set_delay(key, lang, delay, true)` + save) and shows the notice `Subtitles +0.5 s` / `Subtitles -0.2 s` via the existing status `message`. Each frame `Player::poll_sync` events become notices (`Subtitles shifted +0.4 s (automatic)` and store with `manual = false`; `Unsure` -> `Could not match the subtitles automatically; adjust with Z / X`). Imported rows in the CC menu get a small `x` shape at the row end (like the Recordings delete x) that calls `extsubs.remove`, saves, and re-adds nothing until the next start (the track stays loaded in mpv for the current playback; the notice says `Removed; gone after this video`).
- Help: KEYS gets a `Z  X` row under the player/subtitles section: "Subtitles earlier / later by 0.1 s (imported subtitles)".

- [ ] **Step 1: Write failing tests:** `z_and_x_only_while_a_video_plays`, `blocked_while_typing_menu_help_or_tab_focus`, `help_table_lists_the_new_keys` (the existing help tests check shape; add an assertion for the `Z  X` row).
- [ ] **Step 2: Run** `cargo test -p iptv-player subkeys help` -> FAIL.
- [ ] **Step 3: Implement** `subkeys.rs`, the key handling, notices, removal `x`, help row and README key line.
- [ ] **Step 4: Run** `cargo test --workspace` -> PASS.
- [ ] **Step 5: Commit** (`git add` the new file) `feat: subtitle nudge keys, sync notices and track removal`.

### Task 8: Docs, manual check and deploy

**Files:**
- Modify: `README.md` (Using it: "Import subtitles", sync behaviour, limits), `AGENTS.md` (one block: modules, constants, the spike result, `ext_sub_demo`), `docs/follow-ups.md` (manual checks to do + ideas: export embedding the imported track, `.vtt`/`.ass`), `docs/superpowers/specs/2026-11-06-external-subtitles-design.md` (status line: built)
- Test: whole workspace + a manual run

- [ ] **Step 1: Write the docs** with imaginary names only.
- [ ] **Step 2: Run** `cargo test --workspace` and `cargo run -p iptv-player --example ext_sub_demo` -> all PASS / `RESULT: ALL CHECKS OK`.
- [ ] **Step 3: Manual check with local files** (no provider): local ffmpeg server and the `--playlist-file` scratch data dir from `AGENTS.md`, an episode file with a Danish-like embedded track and an external `.srt` 3.0 s late: import by file and by folder; the CC menu lists "English (imported)"; after ~2 minutes the notice `Subtitles shifted -3.0 s (automatic)` appears and the line timing matches; Z/X change it and the value survives a restart; reconnect keeps the track; the `x` removes it; an unmatched file is listed in the message. Note the outcomes in `docs/follow-ups.md`.
- [ ] **Step 4: Commit** `docs: external subtitles`; then run the project skill `.claude/skills/build-and-deploy/SKILL.md` (checks the app is not running) and report.
