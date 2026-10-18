# IPTV Player Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Windows Rust desktop app that browses an Xtream-style M3U playlist (live by country, favorites, movie/series search) and plays it in the same window with endless stream retry.

**Architecture:** A library crate `iptv` holds all logic that can be tested without a window or mpv (config, parsing, library/search, cache, favorites, retry state machine). A thin binary `iptv-player` adds the egui UI and a libmpv backend that is driven by the retry state machine through a `Backend` trait.

**Tech Stack:** Rust stable (MSVC toolchain), eframe/egui (glow backend), libmpv via the `libmpv2` crate + `libmpv-2.dll`, `reqwest` (blocking, rustls) for download, `serde` + `bincode` for cache, `serde_json` for favorites, `toml` for config, `anyhow`.

**Spec:** `docs/superpowers/specs/2026-10-17-iptv-player-design.md` (read it first, including "Assistant's calls").

## Global Constraints

- Target: Windows 11; distribution is a folder with `iptv-player.exe` + `libmpv-2.dll`.
- Playlist URL/credentials only in gitignored `config.toml`; commit `config.example.toml` with placeholders. Never commit `*.m3u`, `cache/`, `*.dll`.
- Credentials never appear in logs, panics or on-screen errors: every URL printed goes through `redact::redact_url`.
- Stream kind from URL path: contains `/movie/` → Movie, `/series/` → Series, anything else → Live. Separator entries (`===== X ======`) are dropped.
- Live country = text before `" - "` in `group-title`; no `" - "` → `"Other"`. Channels in file order.
- Favorite key = `"{kind}:{id}"` where id is the last URL path segment without extension; fallback `"{kind}:name:{normalized name}|{group}"`.
- Retry: endless, backoff capped at 3 s; auth/connection-limit failures (HTTP 401/403/458/459-style or "too many connections") use a 15 s cap and a distinct message. VOD within 5 s of duration at end = natural finish (no autoplay).
- Performance (this machine): usable UI from cache < ~1 s, search < ~50 ms per keystroke, memory < ~500 MB with the full sample (282k entries).
- Config, `cache/`, `favorites.json` live beside the exe; overridable via `--data-dir <path>` or env `IPTV_DATA_DIR`.
- Repo conventions: `AGENTS.md` (not CLAUDE.md); `git -C <path>`; `git add` new files; branches `feature/…`; update `AGENTS.md` + docs when committing a feature branch.
- Sample playlist for tests/perf (outside repo, never copy into it): `<samples dir>\<playlist>.m3u` (a samples folder outside the repo; the file name contains a credential segment, never quote it). Unit tests use small inline fixtures with fake hosts/credentials.

## Review Focus

1. Entry names containing commas, quotes, `#`, or non-ASCII (Arabic, Français): `#EXTINF` name is everything after the last unquoted comma; attributes may contain commas inside quotes. (Task 2)
2. Truncated/garbage M3U (EXTINF with no URL line, blank lines, CRLF, BOM, UTF-8 errors): skip bad entries, never panic. (Task 2)
3. Failed or interrupted refresh download: old cache and in-memory library must remain untouched. (Tasks 6, 7)
4. Stale events: an end-of-file/error event from the previous channel arriving after the user switched channel must not trigger a retry of the old one. (Task 9)
5. Episode names that don't match `SxxExx`, or shows whose names differ only by case/year: appear under a catch-all, never vanish. (Task 4)

---

## File Structure

```
Cargo.toml                    workspace root; members: crates/iptv, crates/iptv-player
config.example.toml
crates/iptv/src/lib.rs        module declarations
crates/iptv/src/redact.rs     credential redaction
crates/iptv/src/config.rs     config + data-dir resolution
crates/iptv/src/playlist.rs   M3U parser, Entry, Kind
crates/iptv/src/library.rs    Library: live by country, movies, series tree, search
crates/iptv/src/series.rs     episode-name parsing
crates/iptv/src/cache.rs      binary cache save/load
crates/iptv/src/fetch.rs      download + background refresh + atomic swap
crates/iptv/src/favorites.rs  favorites store
crates/iptv/src/supervisor.rs retry state machine (pure) + Backend trait
crates/iptv-player/src/main.rs        eframe entry
crates/iptv-player/src/mpv_backend.rs libmpv implementation of Backend + render
crates/iptv-player/src/ui/{mod,live,vod,player}.rs
```

---

### Task 1: Toolchain, scaffold, config, redaction

**Files:** Create workspace files above, `config.example.toml`, `crates/iptv/src/{lib,redact,config}.rs`; modify `AGENTS.md` (build/run instructions).

**Interfaces:**
- Produces: `redact::redact_url(s: &str) -> String` (replaces user/pass path segments and any `username=`/`password=` query values with `***`); `config::Config { playlist_url: String }`; `config::data_dir(cli: Option<PathBuf>) -> PathBuf` (order: CLI, `IPTV_DATA_DIR`, exe's directory); `config::load(dir: &Path) -> anyhow::Result<Config>` (error message names the missing file/field, never the URL).

- [ ] **Step 1:** Install Rust if `cargo --version` fails: `winget install Rustlang.Rustup`, then `rustup default stable-x86_64-pc-windows-msvc`. If `link.exe` is missing, install VS Build Tools (C++ workload) and tell the user. Verify `cargo --version`.
- [ ] **Step 2:** Create the workspace/crates and pin deps listed in Tech Stack (use latest stable at install time). Verify `cargo build` passes.
- [ ] **Step 3: Failing tests** in `redact.rs`: `redacts_xtream_live_path` (`http://h:8080/USER/PASS/123` → `http://h:8080/***/***/123`), `redacts_movie_and_series_paths` (`/movie/USER/PASS/9.mkv` → `/movie/***/***/9.mkv`), `redacts_query_credentials` (`?username=a&password=b` → `?username=***&password=***`), `leaves_non_credential_url_unchanged`. In `config.rs`: `cli_overrides_env_overrides_exe_dir`, `load_reports_missing_file_without_url`. Run → FAIL.
- [ ] **Step 4:** Implement to pass. `cargo test -p iptv` → PASS.
- [ ] **Step 5:** Add `config.example.toml` (`playlist_url = "http://example.invalid/get.php?username=USER&password=PASS&type=m3u_plus"`). Obtain `libmpv-2.dll` (x86_64 build from the mpv-winbuild releases) into `./libmpv/` (gitignored via `*.dll`; add `/libmpv/` too) and document in `AGENTS.md` how to get it and what `libmpv2` needs to link (import lib generated from the DLL). Document `cargo test`, `cargo run -p iptv-player`.
- [ ] **Step 6:** Commit `feat: scaffold workspace, config, redaction`.

---

### Task 2: M3U parser

**Files:** Create `crates/iptv/src/playlist.rs`; modify `lib.rs`.

**Interfaces:**
- Produces: `enum Kind { Live, Movie, Series }` (Copy, Serialize/Deserialize, `as_str()`); `struct Entry { kind: Kind, id: String, name: String, group: String, logo: String, url: String }`; `playlist::parse<R: BufRead>(r: R) -> Vec<Entry>` (never panics; invalid UTF-8 lossily decoded); `playlist::stream_id(url: &str) -> Option<String>` (last path segment without extension); `playlist::favorite_key(e: &Entry) -> String` per Global Constraints.

- [ ] **Step 1: Failing tests** with inline fixtures: `parses_live_movie_series_kinds`; `name_with_comma_and_quotes` (`group-title="A, B"` and name `Foo, Bar`); `drops_separator_entries`; `extinf_without_url_is_skipped`; `handles_crlf_and_bom`; `non_ascii_names_preserved`; `stream_id_strips_extension`; `favorite_key_falls_back_to_name_group_when_no_id`. Run → FAIL.
- [ ] **Step 2:** Implement `parse` as a single pass over lines (attribute scan that honours quotes). → PASS.
- [ ] **Step 3:** Ignored test `parses_real_sample` (`#[ignore]`, path from env `IPTV_SAMPLE`) asserting 250k–300k entries with ≥25k Live, ≥50k Movie, ≥150k Series and finishing under 3 s in release. Run with `cargo test --release -- --ignored` against the sample; record count in the commit message.
- [ ] **Step 4:** Commit `feat: M3U parser`.

---

### Task 3: Library — live grouping and movie list

**Files:** Create `crates/iptv/src/library.rs`; modify `lib.rs`.

**Interfaces:**
- Consumes: `Entry`, `Kind`.
- Produces: `struct Library`; `Library::build(entries: Vec<Entry>) -> Library`; `Library::countries(&self) -> &[Country]` where `Country { name: String, channels: Vec<usize> }` (indices into entries, file order, countries in first-appearance order, `"Other"` last); `Library::entry(&self, idx: usize) -> &Entry`; `Library::movies(&self) -> &[usize]`; `Library::find_by_key(&self, key: &str) -> Option<usize>`.

- [ ] **Step 1: Failing tests:** `merges_groups_with_same_country_prefix` (`Sweden - Nationella` + `Sweden - Sport` → one `Sweden` in file order), `group_without_dash_goes_to_other_last`, `find_by_key_roundtrips_favorite_key`. Run → FAIL.
- [ ] **Step 2:** Implement; → PASS. Commit `feat: library live grouping`.

---

### Task 4: Series parsing and tree

**Files:** Create `crates/iptv/src/series.rs`; modify `library.rs`, `lib.rs`.

**Interfaces:**
- Produces: `series::parse_episode(name: &str) -> Option<EpisodeRef { show: String, season: u32, episode: u32 }>` (recognise `S01E02`, `s1e2`, `1x02`; strip a leading `"Series: "`/language prefix such as `SE: ` only if the sample shows it — check 20 sample names first and record findings in a code comment); `Library::shows(&self) -> &[Show]`, `Show { name: String, seasons: Vec<Season> }`, `Season { number: Option<u32>, episodes: Vec<usize> }` (episodes sorted by episode number; unparsed entries go to a show named `"(Unsorted)"` with `number: None`). Show identity = case-insensitive trimmed name.

- [ ] **Step 1: Failing tests:** `parses_sxxexx_variants`, `unparseable_name_goes_to_unsorted`, `same_show_different_case_merges`, `episodes_sorted_within_season`. Run → FAIL.
- [ ] **Step 2:** Implement; → PASS. Commit `feat: series grouping`.

---

### Task 5: Search

**Files:** Modify `library.rs`.

**Interfaces:**
- Produces: `enum SearchScope { Live, Movies, Series }`; `struct SearchHit { entry: Option<usize>, show: Option<usize> }`; `Library::search(&self, scope: SearchScope, query: &str, limit: usize) -> Vec<SearchHit>` (case- and diacritic-insensitive substring over whitespace-split terms, all terms must match; Series hits are shows). Normalized lowercase names precomputed in `build`.

- [ ] **Step 1: Failing tests:** `all_terms_must_match`, `case_and_diacritics_insensitive` (`francais` finds `Français`), `empty_query_returns_empty`, `limit_respected`.
- [ ] **Step 2:** Implement → PASS. Add `#[ignore]` perf test on the sample: build ≤ 3 s release, 200 representative queries each < 50 ms. Run it; if over budget, add a trigram or first-letter index and re-run.
- [ ] **Step 3:** Commit `feat: library search`.

---

### Task 6: Cache

**Files:** Create `crates/iptv/src/cache.rs`.

**Interfaces:**
- Produces: `cache::save(dir: &Path, entries: &[Entry]) -> anyhow::Result<()>` (write `cache/playlist.bin.tmp`, fsync, rename over `playlist.bin`; header with format version + unix timestamp); `cache::load(dir: &Path) -> Option<CacheData { entries: Vec<Entry>, saved_at: SystemTime }>` (None on missing/corrupt/version mismatch); `CacheData::is_stale(&self, max_age: Duration) -> bool`.

- [ ] **Step 1: Failing tests:** `roundtrip`, `corrupt_file_returns_none`, `failed_save_leaves_previous_cache_intact` (simulate by saving to a dir where the tmp rename target is a read-only/locked file or by truncating the tmp mid-way via an injectable writer), `stale_detection`.
- [ ] **Step 2:** Implement → PASS. Ignored perf test: load of the sample's cache < 500 ms release. Commit `feat: playlist cache`.

---

### Task 7: Fetch and background refresh

**Files:** Create `crates/iptv/src/fetch.rs`.

**Interfaces:**
- Consumes: `playlist::parse`, `cache`, `Library::build`.
- Produces: `fetch::download(url: &str, dest: &Path) -> anyhow::Result<()>` (streams to a temp file; errors are passed through `redact_url`); `struct Refresher` with `Refresher::spawn(url: String, dir: PathBuf) -> Refresher` and `Refresher::poll(&self) -> Option<RefreshEvent>` where `RefreshEvent::{Progress(u64 /*bytes*/), Done(Arc<Library>), Failed(String /*redacted*/)}`. On any failure the cache file is untouched.

- [ ] **Step 1: Failing tests** using a local `tiny_http`-style test server (dev-dependency): `refresh_success_writes_cache_and_emits_done`, `interrupted_download_emits_failed_and_keeps_old_cache` (server closes mid-body), `error_message_contains_no_credentials`.
- [ ] **Step 2:** Implement → PASS. Commit `feat: background refresh`.

---

### Task 8: Favorites

**Files:** Create `crates/iptv/src/favorites.rs`.

**Interfaces:**
- Produces: `Favorites::load(dir: &Path) -> Favorites` (missing/corrupt → empty, corrupt file is renamed `.bad`); `toggle(&mut self, key: &str, label: &str)`; `contains(&self, key: &str) -> bool`; `list(&self) -> &[FavoriteItem { key: String, label: String }]` (insertion order); `save(&self) -> anyhow::Result<()>` (atomic write). Labels are kept so entries missing from a refreshed playlist can still be listed as unavailable (UI checks `Library::find_by_key`).

- [ ] **Step 1: Failing tests:** `toggle_adds_then_removes`, `roundtrip_through_disk`, `corrupt_file_yields_empty_and_keeps_backup`. → implement → PASS. Commit `feat: favorites`.

---

### Task 9: Retry supervisor (pure state machine)

**Files:** Create `crates/iptv/src/supervisor.rs`.

**Interfaces:**
- Produces:
  - `trait Backend { fn load(&mut self, url: &str, start_at: Option<f64>); fn stop(&mut self); }`
  - `enum PlayerEvent { Started, Position(f64), Duration(f64), Buffering(bool), Ended, Error(ErrorClass) }` with `enum ErrorClass { Network, Auth, Other }` and `ErrorClass::from_message(&str) -> ErrorClass` (401/403/458/459, "too many connections", "unauthorized" → Auth; other http/network failures → Network).
  - `struct Supervisor<B: Backend>`; `Supervisor::new(b: B)`; `play(&mut self, url: String, is_live: bool, now: Instant)` (increments an internal generation counter); `on_event(&mut self, generation: u32, ev: PlayerEvent, now: Instant)` (events with a stale generation are ignored); `tick(&mut self, now: Instant)` (fires a scheduled retry when due); `status(&self) -> Status`, `enum Status { Idle, Playing, Buffering, Reconnecting { attempt: u32, auth: bool }, Finished }`; `stop(&mut self)`; `generation(&self) -> u32`.
  - Backoff: 0.5, 1, 2, then 3 s cap (15 s cap when `auth`); reset to start after `Started` followed by ≥10 s of continued playing. Live `Ended` → treated as drop. VOD `Ended` with `position >= duration - 5` → `Finished`; otherwise reconnect with `start_at = last position`. VOD with live=false and no duration known + `Ended` → reconnect.

- [ ] **Step 1: Failing tests** with a fake `Backend` and manually advanced `Instant`: `live_error_retries_with_backoff_sequence`, `backoff_caps_at_3s_network_15s_auth`, `auth_error_sets_auth_flag`, `stale_generation_events_ignored` (play A, play B, then A's Error must not schedule a retry), `live_ended_reconnects`, `vod_end_near_duration_finishes`, `vod_drop_resumes_at_last_position`, `backoff_resets_after_ten_seconds_stable`, `retries_forever` (1000 failures, still Reconnecting), `stop_cancels_pending_retry`. Run → FAIL.
- [ ] **Step 2:** Implement → PASS. Commit `feat: retry supervisor`.

---

### Task 10: libmpv backend and rendering

**Files:** Create `crates/iptv-player/src/mpv_backend.rs`, `crates/iptv-player/src/main.rs` (minimal window).

**Interfaces:**
- Consumes: `supervisor::{Backend, PlayerEvent, ErrorClass}`.
- Produces: `MpvBackend::new(gl: &glow::Context-getter) -> anyhow::Result<MpvBackend>` implementing `Backend`; `MpvBackend::poll_events(&mut self) -> Vec<(u32 /*generation*/, PlayerEvent)>` (maps mpv end-file reason/error, `time-pos`, `duration`, `paused-for-cache` to `PlayerEvent`, tagging each with the generation current at `load` time); `MpvBackend::paint(&mut self, w: u32, h: u32, fbo: i32)` for use in an `egui::PaintCallback`.
- Options set at init: `reconnect`, `reconnect_streamed`, `reconnect_delay_max=3` via `stream-lavf-o`; `network-timeout=10`; `hwdec=auto-safe`; `vo=libmpv`; credentials never logged (disable mpv's own URL logging: `msg-level=all=warn`, and redact any mpv message before showing it).

- [ ] **Step 1 (spike, throwaway acceptance):** a window that plays a local file with mpv's OpenGL render API inside an egui `PaintCallback`. If the render-API route cannot be made to work with eframe's glow backend within reason, fall back to mpv `wid` rendering into a borrowed child window and note the decision in `AGENTS.md`.
- [ ] **Step 2:** Implement the backend; add test `error_message_mapping` (unit-testing the pure mapping function `map_end_file(reason, error_text) -> PlayerEvent` for 401, connection refused, timeout, normal EOF).
- [ ] **Step 3: Manual verification with a local stream** (ffmpeg is on PATH): run `ffmpeg -re -f lavfi -i testsrc=size=640x360:rate=25 -f mpegts -listen 1 http://127.0.0.1:8099/live` and play it; kill ffmpeg, confirm the supervisor status goes `Reconnecting`; restart ffmpeg, confirm playback resumes without user action. Record the observation in the commit message.
- [ ] **Step 4:** Commit `feat: libmpv backend`.

---

### Task 11: UI shell, live browsing, favorites

**Files:** Create `crates/iptv-player/src/ui/{mod,live,player}.rs`; modify `main.rs`.

**Interfaces:**
- Consumes: `Library`, `Refresher`, `Favorites`, `Supervisor<MpvBackend>`.
- Produces: `App` (eframe) with tabs Live / Favorites / Movies / Series; left country list, right channel list (virtualised with `ScrollArea::show_rows`), channel logos optional (skip in v1 unless cheap), click or Enter plays; Up/Down keys change channel while playing; `F` toggles favorite on the current/selected channel; `Esc` leaves player view; "Refresh" button; status line shows refresh progress/errors (redacted) and player `Status` text ("Reconnecting… (attempt N)" / "Account rejected or too many connections — retrying"). Startup: show cached library immediately (loaded on a background thread), refresh in background if stale (>24 h) or no cache; no `config.toml` → on-screen instructions naming the expected path.

- [ ] **Step 1:** Build the UI; launch with the real sample via a cache built from it (`IPTV_DATA_DIR` pointing at a scratch dir; use the `run` skill to launch and screenshot).
- [ ] **Step 2: Manual acceptance checklist** (record results): window usable < ~1 s from cache; scrolling 31k live channels is smooth; Up/Down switches channel and the previous stream's late error never restarts it; `F` persists across restart; killing network shows Reconnecting and recovers on restore.
- [ ] **Step 3:** Commit `feat: live and favorites UI`.

---

### Task 12: Movies and series UI

**Files:** Create `crates/iptv-player/src/ui/vod.rs`; modify `ui/mod.rs`.

- [ ] **Step 1:** Movies tab: search box (focused on tab open), results list virtualised, limit 500, group filter drop-down (multi-select hide-list persisted in `favorites.json`'s sibling `settings.json`). Series tab: search → show list → seasons → episodes; selecting an episode plays it; `(Unsorted)` visible. Natural finish returns to the episode list (no autoplay).
- [ ] **Step 2: Manual acceptance:** search "familjen" style Swedish and `français` diacritics return results as you type with no visible lag; a show with unparsed episodes appears under `(Unsorted)`; closing a movie mid-way and reselecting works (v1 does not remember position).
- [ ] **Step 3:** Commit `feat: movies and series UI`.

---

### Task 13: Verification, performance, docs

**Files:** Modify `AGENTS.md`, the spec (if reality diverged), add `README.md` (setup: config, DLL, run).

- [ ] **Step 1:** Run `cargo test` (all pass) and the ignored sample tests in release; run `cargo clippy -- -D warnings`.
- [ ] **Step 2:** Measure and record in `AGENTS.md`: cold start from cache, search latency, peak memory (Task Manager) against Global Constraints; fix or note any miss.
- [ ] **Step 3:** Grep tracked files and a sample log run for the real credential segments/host (use values from the sample file at run time; do not write them anywhere): must find none.
- [ ] **Step 4:** Update `AGENTS.md` and docs; commit `docs: usage and measurements`.
