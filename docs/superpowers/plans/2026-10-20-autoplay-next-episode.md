# Autoplay Next Episode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Offer "Next episode" (countdown, auto-start) during the closing credits of a series episode, never skipping post-credit content, and continue from a season's last episode into the next season.

**Architecture:** Pure, unit-tested pieces (`next_episode`, credits classifier + decision, an autoplay state machine) in `iptv-player::state`; the existing `ThumbService` worker (one extra provider connection, shared with seek previews) gets a "tail scan" command that returns classified samples; `ui/mod.rs` wires the state machine to playback and draws the offer.

**Tech Stack:** Rust, egui/eframe 0.37, libmpv (`thumb_mpv`), serde (settings).

**Spec:** `docs/superpowers/specs/2026-10-20-autoplay-next-episode-design.md`

## Global Constraints
- Tail scan: last 8 min (`SCAN_TAIL_SECS = 480.0`), one frame per `thumbs::BUCKET_SECS` (10 s); countdown `OFFER_SECS = 8.0`.
- Never skip content: scan failure / refused second connection / unknown duration / incomplete scan => offer only at `Status::Finished`.
- No URL, host or credential in logs/errors/Debug (manual Debug rules in AGENTS.md).
- Settings: `autoplay_next` optional field, `settings.json` version stays 1, absent = on.
- Movies, live, `(Unsorted)` episodes: no offer; `on_finished` behaves as today.
- Keep each file's line endings (see AGENTS.md); `git add` new files; commits end with the Co-Authored-By/Claude-Session lines from the session reminder.
- Branch `feature/autoplay-next-episode` (already created); update AGENTS.md, README.md, `docs/follow-ups.md` when done.

## Review Focus
- Credits roll, then a stinger (content), then black, then EOF: offer must come after the stinger, not at the credits start (test in Task 3).
- Black frames only / content only in the tail: no early offer (Task 3).
- One failed frame in the tail: no early offer (Task 3).
- Last episode of the last season, or `(Unsorted)`, or next season fully hidden by group filter: no offer, finish returns to list (Task 2, Task 5).
- User seeks back out of the tail or dismisses the offer: offer must not reappear for this episode; seeking backwards cancels a running countdown only if the position leaves the credits (Task 4).

---

### Task 1: `autoplay_next` setting

**Files:** Modify `crates/iptv/src/settings.rs`; tests in the same file.

**Interfaces:**
- Produces: `Settings::autoplay_next(&self) -> bool` (default true). Field stored as `Option<bool>` in `SettingsFile`/`Settings` (`#[serde(default, skip_serializing_if = "Option::is_none")]`), preserved across `save`/`load` and quarantine rules unchanged. No setter needed (hand-edited).

- [ ] **Step 1:** Write tests `autoplay_next_defaults_to_true`, `autoplay_next_false_roundtrips_and_is_kept_when_saving_other_changes` (write file with `"autoplay_next":false`, load, `set_sub_pref`, save, reload => still false; a file without the key stays without it).
- [ ] **Step 2:** `cargo test -p iptv settings` fails to compile/fails.
- [ ] **Step 3:** Add the field to `SettingsFile` and `Settings`, thread it through `parse` (return type becomes a small struct instead of a tuple), `load_with`, `save`; implement the getter.
- [ ] **Step 4:** `cargo test -p iptv settings` passes.
- [ ] **Step 5:** Commit `feat: autoplay_next setting`.

### Task 2: Next-episode resolution

**Files:** Create `crates/iptv-player/src/state/next.rs`; modify `state/mod.rs` (`pub mod next;`).

**Interfaces:**
- Consumes: `vod::episode_rows(lib, show, hidden) -> Vec<EpisodeRow>`, `Library::shows()`, `iptv::series::UNSORTED`.
- Produces: `pub fn next_episode(lib: &Library, entry: usize, hidden: &HashSet<String>) -> Option<usize>`: the entry of the `EpisodeRow::Episode` following `entry` in its show's rows (crossing season headers); `None` if `entry` is in no show, in `(Unsorted)`, hidden, or last.

- [ ] **Step 1:** Tests (build a `Library` the way `vod.rs` tests do): `next_within_season`, `last_of_season_goes_to_first_of_next_season`, `last_of_show_is_none`, `unsorted_is_none`, `skips_hidden_group_episodes`, `movie_entry_is_none`.
- [ ] **Step 2:** Run `cargo test -p iptv-player state::next`; fails.
- [ ] **Step 3:** Implement: find the show whose seasons contain `entry` (linear scan; called once per episode start), skip `UNSORTED`, scan `episode_rows`.
- [ ] **Step 4:** Tests pass.
- [ ] **Step 5:** Commit `feat: next episode resolution`.

### Task 3: Credits classifier and offer decision

**Files:** Create `crates/iptv-player/src/state/credits.rs`; modify `state/mod.rs`.

**Interfaces:**
- Produces:
  - `pub enum FrameKind { Credits, Black, Content }`
  - `pub fn classify(rgba: &[u8], width: u32, height: u32) -> FrameKind` (RGBA, row-major, as `thumb_mpv::Thumb`). Black = nearly uniform dark (e.g. mean luma < 20 and low variance). Credits = dark/plain background and >= 3 separate horizontal bands of bright, high-contrast, small-run pixels (text layout) covering a small fraction of the frame. Otherwise Content. Thresholds are tuned on synthetic frames first, then on real episodes (Task 6 manual check).
  - `pub fn scan_times(duration: f64) -> Vec<f64>`: times `duration - SCAN_TAIL_SECS` (min 0) stepping `BUCKET_SECS`, last one `duration - 3.0`; empty if `duration < 30.0`.
  - `pub fn offer_time(samples: &[(f64, Option<FrameKind>)], duration: f64) -> f64`: `None` kind = frame failed. Returns the earliest sample time T such that every sample from T on is Credits/Black and at least one is Credits; if any sample is `None`, or no such T, or the last sample is Content, returns `duration` (= offer at the real end).
  - `pub const SCAN_TAIL_SECS: f64 = 480.0;`

- [ ] **Step 1:** Classifier tests with synthetic images (helper builds frames): `black_frame_is_black`, `white_text_rows_on_dark_is_credits`, `noisy_gradient_scene_is_content`, `bright_flat_frame_is_content`, `single_title_line_is_not_credits` (one band only => Content).
- [ ] **Step 2:** Decision tests: `offer_at_start_of_final_credits`, `stinger_after_credits_moves_offer_to_the_end` (credits, content, black => `duration`), `all_black_tail_offers_at_end`, `all_content_tail_offers_at_end`, `failed_frame_offers_at_end`, `black_gap_inside_credits_is_neutral`, plus `scan_times` tests (short video empty; last time is duration-3).
- [ ] **Step 3:** Run `cargo test -p iptv-player state::credits`; fails.
- [ ] **Step 4:** Implement all four items; classifier works on a downsampled luma plane, no new dependencies.
- [ ] **Step 5:** Tests pass. Commit `feat: credits classifier and offer decision`.

### Task 4: Tail scan through the thumbnail worker

**Files:** Modify `crates/iptv-player/src/thumbs.rs`, `crates/iptv-player/src/ui/previews.rs`.

**Interfaces:**
- Consumes: `credits::{classify, FrameKind}`, `ThumbMpv::frame_at(secs, timeout) -> Result<Thumb>`.
- Produces:
  - `ThumbService::scan(&self, gen: u64, times: Vec<f64>)` -> `Cmd::Scan`; worker takes each frame sequentially (no textures, classification on the worker thread), aborts when a newer command (Open/Close/Scan/Want) arrives in the queue, replies once: `Reply::Scanned { gen, samples: Vec<(f64, Option<FrameKind>)> }`. Previously-queued `Want`s are answered after the scan finishes.
  - `Previews::start_scan(&mut self, url: &str, times: Vec<f64>)` (opens the second source if not yet opened; no-op if `unavailable`; marks scan started) and `Previews::scan_result(&mut self) -> ScanState` with `enum ScanState { NotStarted, Running, Done(Vec<(f64, Option<FrameKind>)>), Unavailable }`. `reset` clears it.

- [ ] **Step 1:** Unit-test the pure part: factor the worker's scan loop as `fn run_scan(times, take: impl FnMut(f64) -> Option<FrameKind>, aborted: impl Fn() -> bool) -> Option<Vec<..>>` and test `scan_collects_in_order`, `scan_aborts_when_told`.
- [ ] **Step 2:** Run, fail. Step 3: implement `run_scan`, the command, the reply, the `Previews` methods (`OpenFailed` => `Unavailable`).
- [ ] **Step 4:** `cargo test -p iptv-player` passes; `cargo build -p iptv-player`.
- [ ] **Step 5:** Commit `feat: tail scan via the thumbnail worker`.

### Task 5: Autoplay state machine

**Files:** Create `crates/iptv-player/src/state/autoplay.rs`; modify `state/mod.rs`.

**Interfaces:**
- Consumes: `credits::{scan_times, offer_time, FrameKind, SCAN_TAIL_SECS}`, `ScanState` shape from Task 4 (take the samples slice, keep this module free of egui/previews types).
- Produces:
  - `pub const OFFER_SECS: f64 = 8.0;`
  - `pub struct Autoplay` with `new() `, `arm(&mut self, next: Option<usize>, enabled: bool)` (called when an episode starts; `None`/disabled => Off), `disarm(&mut self)`.
  - `pub enum Step { Nothing, StartScan(Vec<f64>), Offer, PlayNext }` and `pub fn update(&mut self, now: Instant, position: Option<(f64, Option<f64>)>, scan: Option<Option<Vec<(f64, Option<FrameKind>)>>>, finished: bool) -> Step` where `scan` = `None` not available yet, `Some(None)` = unavailable/failed, `Some(Some(samples))` = done.
  - Queries: `offering(&self) -> bool`, `remaining(&self, now: Instant) -> f64`, `next(&self) -> Option<usize>`, `dismiss(&mut self)` (no offer again for this episode), `accept(&mut self)`.
  - Behaviour: scan starts once position >= duration - SCAN_TAIL_SECS (duration known); offer time = `offer_time(..)` or `duration` when scan unavailable; `Offer` once position >= offer time; countdown `OFFER_SECS` then `PlayNext`; `finished` while armed and not dismissed => `PlayNext` at once; position dropping below offer time - 5 s while offering (user sought back) cancels the offer (back to waiting).

- [ ] **Step 1:** Tests: `no_scan_before_the_tail`, `scan_starts_in_the_tail`, `offer_at_credits_start`, `stinger_delays_offer`, `scan_unavailable_offers_at_end_only`, `countdown_plays_next_after_8s`, `dismiss_stops_offer_and_autoplay_at_finish` (dismissed + finished => Nothing, so `on_finished` stops as before), `finished_while_waiting_plays_next`, `seek_back_cancels_offer`, `disarmed_does_nothing`, `no_next_episode_is_off`.
- [ ] **Step 2:** Run, fail. Step 3: implement. Step 4: pass.
- [ ] **Step 5:** Commit `feat: autoplay state machine`.

### Task 6: Wire into the app, draw the offer, docs

**Files:** Modify `crates/iptv-player/src/ui/mod.rs` (`App::start`, `stop`, `on_finished`, `video_area`, `update` loop, key handling), `crates/iptv-player/src/ui/player.rs` only if `position()` needs nothing new; docs: `AGENTS.md`, `README.md`, `docs/follow-ups.md`.

**Interfaces:**
- Consumes: Tasks 1-5. `Playback::tab`, `OpenShow::select_entry`, `App::start(Playback)`, `App::hidden` set (use the same source `episode_rows` callers use).

- [ ] **Step 1:** `App` gets `autoplay: Autoplay`. In `start`: after `previews.reset()`, `autoplay.arm(next_episode(lib, entry, &hidden), settings.autoplay_next())` for non-live `Series` kind entries, else `disarm`. `stop`: `disarm`.
- [ ] **Step 2:** Per frame (after `p.update()`): map `previews.scan_result()` into `update(...)`; on `StartScan(times)` call `previews.start_scan(url, times)`; on `PlayNext` call a new `App::play_next_episode()` which builds `Playback::new(lib, <show episodes list from episode_rows>, next, Tab::Series)`, calls `start`, selects the entry in the open show if its `show` matches (`o.select_entry`) and sets `scroll_to`, and forgets the finished episode's position. In `on_finished`, if `autoplay` yields `PlayNext`, do that instead of stopping.
- [ ] **Step 3:** Draw the offer in `video_area` over the video's bottom right (shapes, white-on-black like the CC menu): "Next episode in N" plus a Cancel label; click on the button = `accept` + play now; click Cancel = `dismiss`. Keys while offering (not typing, no subtitle menu): Enter = play now, Esc = dismiss (Esc must not also stop playback in that frame). Request repaint every 100 ms while offering.
- [ ] **Step 4:** `cargo test --workspace` passes; `cargo clippy` clean if the project uses it. Manual check with a local ffmpeg-served two-episode fake show (see AGENTS.md "Manual GUI test"): a lavfi video whose last minute is scrolling text on black => offer at the text start; then one with a bright testsrc segment after the text => offer only at the end. Tune classifier thresholds if needed (note the real-episode check on the Wheel of Time S1 finale as a manual to-do in `docs/follow-ups.md` if no sample is available).
- [ ] **Step 5:** Docs: AGENTS.md (autoplay, tail scan, setting, keys), README keys and setting, follow-ups (tuning, known limits: credits over footage). Commit `feat: autoplay next episode`; then build+deploy per the project skill.

---

## Self-review
- Spec coverage: lookahead scan (T3/T4), classifier (T3), decision/fallbacks (T3/T5), offer UI and keys (T6), next episode across seasons and hidden/unsorted (T2), start through `App::start` (T6), setting (T1), finish with next => play (T5/T6), docs/checks (T6).
- Types: `FrameKind`, `Option<FrameKind>` samples, `scan_times`, `offer_time` identical across T3-T5.
