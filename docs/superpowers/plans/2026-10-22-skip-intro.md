# Skip intro Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A "Skip intro" button that jumps over a show's repeating title sequence, found by comparing the opening of the playing episode with another episode of the same show.

**Architecture:** Pure matching/persistence in the `iptv` crate (`intro`, `intros`); a headless scan worker (`intro_scan`, its own thread and `ThumbMpv`) in `iptv-player`; a pure flow state machine (`state::skip_intro`) driven per frame by the `App`, which draws the button next to the existing "Next episode" offer.

**Tech Stack:** Rust, egui/eframe 0.37, libmpv2-sys (`ThumbMpv`), serde_json. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-22-skip-intro-design.md`

## Global Constraints

- Branch `feature/skip-intro` (already checked out). Use `git -C <path>`. `git add` every new source file. Commit messages end with the two attribution lines already used on this branch (`Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_01KxMkSGmiV3rZ3o8hiQdY9K`).
- Never print or log a URL (use `iptv::redact::redact_url` if one must appear; prefer not). New types holding URLs need a manual `Debug` that omits them.
- Keep each file's existing line endings (all files touched here are LF; `mpv_backend.rs`, `cache.rs`, `redact.rs`, `series.rs` are CRLF and are not touched).
- Intro scan: `INTRO_STEP_SECS = 4.0`, `INTRO_SCAN_SECS = 240.0`, `MIN_INTRO_SECS = 20.0`, refinement `1 s` steps from the last matched time to `+24 s`, skip target capped at `+20 s` past the intro end (spec).
- Fingerprint = 16x9 grayscale; "same frame" = mean absolute difference `< MATCH_DIST = 12.0` (measured: identical intro frames differ by 0, others by 18-70).
- Enter (player view only, same gating as the Next-episode offer) or a click on the button skips; Esc/"Cancel" dismisses. No other key. No auto-skip.
- Only non-live `Kind::Series` episodes that have a show (`recent_id`) take part; movies, live, `(Unsorted)` never.
- `cargo test --workspace` must stay green after every task.

## Review Focus

1. Two episodes that both start with black frames must not "match" on the black (non-informative frames are ignored; test in Task 1).
2. A reference where the intro does not repeat (run shorter than 20 s) gives no button (Task 1, Task 4).
3. A scan reply that belongs to the previous episode must be ignored after the user switches episodes (Task 4 job ids; Task 5 cancel on start).
4. Resuming an episode past the intro (position >= target) never shows the button (Task 4).
5. `intros.json` that is corrupt or of an unknown version is moved aside, never deleted, and the app runs without it (Task 2).

---

### Task 1: Fingerprints and matching (`iptv::intro`)

**Files:**
- Create: `crates/iptv/src/intro.rs`
- Modify: `crates/iptv/src/lib.rs` (add `pub mod intro;`)

**Interfaces:**
- Produces (all `pub`, pure):
  - `pub const INTRO_STEP_SECS: f64 = 4.0; INTRO_SCAN_SECS: f64 = 240.0; MIN_INTRO_SECS: f64 = 20.0; MATCH_DIST: f64 = 12.0;`
  - `#[derive(Clone, PartialEq)] pub struct Fingerprint(pub [u8; 144])` with a manual `Debug` printing only `Fingerprint(mean N)`; `pub fn to_hex(&self) -> String`, `pub fn from_hex(s: &str) -> Option<Fingerprint>`.
  - `pub fn fingerprint(rgba: &[u8], width: u32, height: u32) -> Fingerprint` (RGBA input, luma = `0.299R+0.587G+0.114B`, area average into 16 columns x 9 rows).
  - `pub fn is_informative(fp: &Fingerprint) -> bool` (false when mean luma < 20 or max-min < 24: black/flat frames).
  - `pub fn distance(a: &Fingerprint, b: &Fingerprint) -> f64` (mean absolute difference).
  - `pub type Prints = Vec<(f64, Option<Fingerprint>)>` (time-sorted, `None` = frame could not be taken).
  - `#[derive(Debug, Clone, Copy, PartialEq)] pub struct Span { pub start: f64, pub end: f64 }`.
  - `pub fn scan_times(duration: f64) -> Vec<f64>`: `0, 4, 8, ...` up to `min(INTRO_SCAN_SECS, duration - 3)`; empty for non-finite or `< 30` durations.
  - `pub fn find_intro(own: &[(f64, Option<Fingerprint>)], reference: &[(f64, Option<Fingerprint>)]) -> Option<Span>`: the intro span in `own`'s timeline.

- [ ] **Step 1: Write failing tests** in `intro.rs` `#[cfg(test)]` (build synthetic fingerprints with a helper `fp(seed: u8)` whose bytes vary by position so different seeds differ by > 12 and the same seed differs by 0; a flat/dark one for black):
  - `hex_round_trips`; `from_hex` rejects wrong length/non-hex.
  - `fingerprint_of_black_is_not_informative`, `fingerprint_of_a_gradient_is_informative`, `fingerprint_ignores_alpha`.
  - `scan_times_cover_four_minutes`: `scan_times(2700.0)` starts `[0.0, 4.0]`, ends `240.0`, len 61; `scan_times(100.0)` ends `96.0`; `scan_times(29.0)` and NaN are empty.
  - `finds_the_intro_at_the_same_time`: own and reference each = black x3, then seeds 1..=20 (4 s apart, from t=12), then seeds 100+i unique scenes; result `Some(Span{start:12.0, end:12.0+19*4+2})`.
  - `finds_the_intro_with_an_offset`: own starts the intro 20 s later than the reference.
  - `black_frames_alone_do_not_match`: both all-black for 60 samples -> `None`.
  - `a_short_common_run_is_not_an_intro`: only 4 common frames (16 s) -> `None`.
  - `a_dark_frame_inside_the_intro_does_not_break_it`: one `None`/black sample in the middle of the 20 matching ones still gives the full span.
  - `different_episodes_without_a_common_run_give_none`.
- [ ] **Step 2:** `cargo test -p iptv intro` -> fails to compile/FAIL.
- [ ] **Step 3: Implement.** `find_intro`: for each distinct offset `d = ref_t - own_t` over all pairs (times are on the same 4 s grid, compare with `abs < 0.5`), walk `own` in time order; a sample matches when it and the reference sample at `t + d` are both informative and `distance < MATCH_DIST`; a run may skip one non-matching sample that is non-informative/`None` on either side (a gap), but any informative mismatch ends it. Run seconds = `last_matched_t - first_matched_t + INTRO_STEP_SECS`; take the longest run over all offsets (ties: smallest offset); return it only if `>= MIN_INTRO_SECS` as `Span { start: first_matched_t, end: last_matched_t + INTRO_STEP_SECS / 2.0 }`.
- [ ] **Step 4:** `cargo test -p iptv intro` -> PASS.
- [ ] **Step 5: Commit** (`git add crates/iptv/src/intro.rs crates/iptv/src/lib.rs`): `feat: intro fingerprints and cross-episode matching`.

---

### Task 2: Per-show reference store (`iptv::intros`)

**Files:**
- Create: `crates/iptv/src/intros.rs`
- Modify: `crates/iptv/src/lib.rs` (add `pub mod intros;`)

**Interfaces:**
- Consumes: `iptv::intro::{Fingerprint, Prints}` (Task 1).
- Produces:
  - `pub struct Intros` (manual `Debug`: show count only).
  - `pub fn Intros::load(dir: &Path) -> Intros` (file `intros.json`, `{"version":1,"items":[{"show":"<lowercased>","prints":[[secs,"<hex>"|null],...]}]}`; same quarantine/atomic-write/`fsutil::backup_path` rules as `crates/iptv/src/recent_series.rs`, which is the pattern to copy; an unreadable or unknown-version file is moved aside to `intros.json.bad[.N]`, never deleted; if that move fails `save_if_dirty` refuses).
  - `pub fn Intros::get(&self, show: &str) -> Option<&Prints>` (key = lowercased show name).
  - `pub fn Intros::put(&mut self, show: &str, prints: Prints)` (replaces; max `MAX_SHOWS = 200`, oldest insertion dropped).
  - `pub fn Intros::save_if_dirty(&mut self) -> anyhow::Result<()>`.

- [ ] **Step 1: Write failing tests** (use `tempfile`, as `recent_series.rs` tests do): `round_trips_prints_including_none`, `show_key_is_case_insensitive`, `put_replaces_and_keeps_at_most_200`, `corrupt_file_is_moved_aside_and_load_is_empty`, `unknown_version_is_moved_aside`, `failed_move_aside_blocks_save`, `missing_file_loads_empty_and_save_if_dirty_writes_nothing_when_clean`.
- [ ] **Step 2:** `cargo test -p iptv intros` -> FAIL.
- [ ] **Step 3: Implement** by following `recent_series.rs` for structure and error handling.
- [ ] **Step 4:** `cargo test -p iptv` -> PASS.
- [ ] **Step 5: Commit** (add new file): `feat: per-show intro reference store (intros.json)`.

---

### Task 3: Headless scan worker (`intro_scan`) and a check example

**Files:**
- Create: `crates/iptv-player/src/intro_scan.rs`, `crates/iptv-player/examples/intro_check.rs`
- Modify: `crates/iptv-player/src/lib.rs` (add `pub mod intro_scan;`)

**Interfaces:**
- Consumes: `iptv::intro::{fingerprint, Fingerprint}`, `crate::state::credits::{classify, FrameKind}`, `crate::thumb_mpv::ThumbMpv` (`new(ua, 240)`, `open(url, timeout)`, `frame_at(secs, timeout) -> Result<Thumb>`; `Thumb { width, height, rgba }`).
- Produces:
  - `pub struct JobResult { pub job: u64, pub prints: Vec<(f64, Option<Fingerprint>)>, pub kinds: Vec<(f64, Option<FrameKind>)>, pub error: Option<String> }` (`error` = the source could not be opened; text without URL).
  - `pub struct IntroScanner` with `pub fn new(user_agent: String, wake: impl Fn() + Send + 'static) -> IntroScanner`, `pub fn scan(&self, job: u64, url: &str, times: Vec<f64>)` (opens a fresh `ThumbMpv` on `url`, 20 s open timeout, 15 s per frame; a newer `scan` or `cancel` replaces/aborts the running job at the next frame and its result is **not** sent; the source is closed when the job ends), `pub fn cancel(&self)`, `pub fn poll(&self) -> Vec<JobResult>`.
  - Pure helper `fn run_job(times: &[f64], take: impl FnMut(f64) -> Option<(Option<Fingerprint>, Option<FrameKind>)>, aborted: impl FnMut() -> bool) -> Option<(Prints, Vec<(f64, Option<FrameKind>)>)>` (`None` when aborted; checks `aborted` before every frame).

- [ ] **Step 1: Write failing tests** for `run_job`: `takes_every_time_in_order`, `aborted_stops_and_returns_none`, `a_failed_frame_is_none_in_both_lists`.
- [ ] **Step 2:** `cargo test -p iptv-player intro_scan` -> FAIL.
- [ ] **Step 3: Implement** the worker thread (name `intro-scan`) with a channel of `Cmd::{Scan{job,url,times}, Cancel}`; between frames `try_recv` to see a newer command. Log `intro scan: open failed: <err>` (no URL) on stderr.
- [ ] **Step 4: Example `intro_check`**: `cargo run --release -p iptv-player --example intro_check -- --data-dir <dir> --name <entry-name-substring A> --ref <entry-name-substring B>`; loads the cache (as `examples/head_frames.rs` does), scans A and B with `scan_times(duration)` where duration is taken as `INTRO_SCAN_SECS + 3.0`, runs `find_intro`, prints the span and the per-sample letters (`kind_letter`), never a URL. Verify manually (real provider, app closed): `--name "The Man in the High Castle S02 E04" --ref "The Man in the High Castle S02 E05"` prints a span with start 11 and end about 90 (intro 11-88 s).
- [ ] **Step 5:** `cargo test --workspace` -> PASS. Commit (add new files): `feat: headless intro scan worker and intro_check example`.

---

### Task 4: Flow state machine (`state::skip_intro`)

**Files:**
- Create: `crates/iptv-player/src/state/skip_intro.rs`
- Modify: `crates/iptv-player/src/state/mod.rs` (add `pub mod skip_intro;`)

**Interfaces:**
- Consumes: `iptv::intro::{scan_times, find_intro, Prints, Span, INTRO_STEP_SECS}`, `crate::state::credits::FrameKind`.
- Produces:
  - `pub const REFINE_SECS: f64 = 24.0; pub const MAX_EXTEND_SECS: f64 = 20.0;`
  - `#[derive(Debug, Clone, Copy, PartialEq)] pub enum Which { Own, Reference, Refine }`
  - `#[derive(Debug, Clone, PartialEq)] pub enum Action { None, Scan { job: u64, which: Which, times: Vec<f64> }, SaveReference(Prints) }` (`Prints` = the reference's fingerprints, to be stored by the App).
  - `pub fn refine_times(span_end: f64, duration: f64) -> Vec<f64>`: 1 s steps from `floor(span_end - INTRO_STEP_SECS/2)` to `+ REFINE_SECS`, never beyond `duration - 3`.
  - `pub fn skip_target(span_end: f64, kinds: &[(f64, Option<FrameKind>)]) -> f64`: walking frames with `t >= span_end - INTRO_STEP_SECS/2` in time order: leading `Content` frames are skipped; the first `Black|Card|Credits` frame starts a "card"; the first `Content` frame after a card is the target if it is within `MAX_EXTEND_SECS` of `span_end`; with no card, or no Content within the cap, the target is `span_end`.
  - `pub struct SkipIntro` with:
    - `pub fn new() -> SkipIntro` (also `Default`); `pub fn arm(&mut self, stored: Option<Prints>, has_neighbour: bool)` (starts `Waiting`; with neither a stored reference nor a neighbour -> `Off`); `pub fn disarm(&mut self)`.
    - `pub fn update(&mut self, duration: Option<f64>) -> Action`: in `Waiting` with a duration where `scan_times(d)` is non-empty -> `Action::Scan { Own }` with a fresh job id; empty -> `Off`.
    - `pub fn on_result(&mut self, job: u64, prints: Prints, kinds: Vec<(f64, Option<FrameKind>)>, duration: f64) -> Action` (stale `job` ids -> `Action::None`): after `Own`: stored reference -> `find_intro(own, stored)`; found -> `Scan{Refine}` over `refine_times`, none -> `Off`; no stored reference and a neighbour -> `Scan{Reference}` with the same times (own prints kept); after `Reference`: `find_intro(own, ref)`; found -> `SaveReference(ref prints)` then (next call to `update`, which returns it) the `Refine` scan; none -> `Off`; after `Refine`: `Ready { start: span.start, target: skip_target(..) }`.
    - `pub fn on_failed(&mut self, job: u64)` -> `Off` (current job only).
    - `pub fn offering(&self, position: f64) -> bool` (`Ready`, not dismissed/accepted, `start <= position < target - 1.0`); `pub fn target(&self) -> Option<f64>`; `pub fn dismiss(&mut self)`; `pub fn accept(&mut self)`; `pub fn state_name(&self) -> &'static str`.
  - To return `SaveReference` and `Scan{Refine}` in sequence, `on_result` returns `SaveReference` and the following `update(duration)` returns the pending `Scan`.

- [ ] **Step 1: Write failing tests**: `arm_without_reference_or_neighbour_is_off`; `update_requests_the_own_scan_once_the_duration_is_known`; `update_with_short_duration_turns_off`; `own_scan_with_stored_reference_that_matches_asks_for_refinement`; `own_scan_without_match_turns_off`; `first_time_flow_scans_the_neighbour_then_saves_the_reference_then_refines`; `no_common_run_in_the_reference_turns_off`; `stale_job_results_are_ignored`; `refine_result_makes_it_ready_and_offering_inside_the_intro`; `not_offering_past_the_target_or_when_resumed_after_it`; `dismiss_and_accept_stop_the_offer`; `skip_target_*` (card then content gives that content time; no card gives `span_end`; content beyond the cap gives `span_end`; leading intro Content frames before the card are skipped); `refine_times_are_one_second_steps_and_respect_duration`.
- [ ] **Step 2:** `cargo test -p iptv-player skip_intro` -> FAIL.
- [ ] **Step 3: Implement** as specified (pure; job ids from an internal counter).
- [ ] **Step 4:** `cargo test --workspace` -> PASS.
- [ ] **Step 5: Commit** (add new file): `feat: skip-intro flow state machine`.

---

### Task 5: App integration, button, docs

**Files:**
- Modify: `crates/iptv-player/src/ui/mod.rs`, `crates/iptv-player/src/state/next.rs` (neighbour helper), `crates/iptv-player/src/state/help.rs`, `README.md`, `AGENTS.md`, `docs/follow-ups.md`
- Test: `state/next.rs` unit tests, `state/help.rs` existing table tests

**Interfaces:**
- Consumes: Tasks 1-4; existing `App::start`, `recent_id(pb)`, `Player::position()`, `Player::seek_to`, `App::next_episode_offer` (copy its drawing/Esc/Enter gating), `state::next::next_episode`, `show_episodes`.
- Produces: `pub fn neighbour_episode(lib: &Library, entry: usize, hidden: &HashSet<String>) -> Option<usize>` in `state/next.rs` (the next episode, else the previous one in the same show's `episode_rows` order, crossing seasons; `None` for non-series/`(Unsorted)`/hidden).

- [ ] **Step 1: Failing tests** in `state/next.rs`: `neighbour_is_the_next_episode`, `neighbour_of_the_last_episode_is_the_previous_one`, `neighbour_of_a_single_episode_show_is_none`, `neighbour_of_a_movie_is_none`.
- [ ] **Step 2:** implement `neighbour_episode`; tests pass.
- [ ] **Step 3: Wire the App.** Fields `intro_scanner: IntroScanner` (created next to `Previews` with the same user agent and an egui-repaint wake), `intros: Intros` (loaded from the data dir), `skip_intro: SkipIntro`, `intro_urls: (String, Option<String>)` (playing URL, neighbour URL). In `start`: `intro_scanner.cancel()`; for a non-live `Kind::Series` episode with `recent_id` -> `skip_intro.arm(self.intros.get(&show).cloned(), neighbour.is_some())` and remember URLs; otherwise `disarm`. Each frame (next to `update_autoplay`): poll `intro_scanner`; feed results/errors to `on_result`/`on_failed`; `update(duration)`; execute `Action::Scan` (`Which::Own`/`Refine` -> playing URL, `Reference` -> neighbour URL; stderr `intro: scanning ... (N frames)`), `Action::SaveReference` -> `intros.put(show, prints)` + save (log errors like `save_recent`). Log the decision once: `intro: span S-E s, skip to T s` or `intro: none`. Never the URL.
- [ ] **Step 4: Button.** `App::skip_intro_offer` draws "Skip intro" (plain text + shape like `next_episode_offer`) above the control buttons at the video's bottom right while `skip_intro.offering(position)`, and a Cancel; stacked above the Next-episode offer if both show. Click or Enter (player view/fullscreen only, no tab button focused, not typing, subtitle menu closed, Next-episode offer not showing) -> `Player::seek_to(target)` + `accept()`; Esc or Cancel -> `dismiss()` (Esc does nothing else that press); not shown while a seek drag is active. Repaint every 200 ms while offering.
- [ ] **Step 4b: Wire `ui/mod.rs` tests/pure bits**: keep logic in `skip_intro.rs`; add only what is needed to compile.
- [ ] **Step 5: Docs.** `state/help.rs` KEYS: Enter row mentions "Skip intro" while it shows (keep the table test green); `README.md` keys/behaviour + "intros.json"; `AGENTS.md` new "Skip intro" bullet (module names, constants, intro_scan worker, `intros.json`, `head_frames`/`intro_check` examples, `--console` head capture); `docs/follow-ups.md`: auto-skip setting (`settings.json`), up to 3 provider connections while scanning with a hover preview open, manual checks still to do (a show without a dark card after the intro; a third show).
- [ ] **Step 6: Verify.** `cargo test --workspace`; `cargo build --release -p iptv-player`; manual (app closed, real provider, user plays `The Man in the High Castle S02 E04` then E05 with the deployed build): button appears at about 0:11, Enter skips to about 1:34 (E04); second episode needs no neighbour scan (stored reference).
- [ ] **Step 7: Commit** (add new files): `feat: skip intro button`. Then deploy with the project skill `build-and-deploy`.
