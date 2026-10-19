# Episode Titles + Tab/Enter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** (1) Enter on a Tab-focused Movies/Series tab button opens that tab's search page like a click. (2) Episode rows show real titles (`S01E02 - The Visit`), looked up lazily from TVmaze and cached on disk.

**Architecture:** Pure logic (name cleaning, matching, JSON parsing, cache with TTLs, one blocking HTTP fetch) lives in a new `iptv::episode_titles` module. The player crate gets a GUI-free `state::titles::TitleLoader` (one background thread, deduped requests, results polled by the App) and `episode_text` takes an optional titles map. Rows render `S01E02` immediately and gain the title when the answer arrives.

**Tech Stack:** Rust, egui/eframe 0.37, `reqwest::blocking` (already a dep of `iptv`), serde_json, tempfile (tests).

**Spec:** approved in chat on 2026-10-19 (no spec file; the decisions are in this header and Global Constraints).

## Global Constraints

- Branches: bugfix `bugfix/tab-enter-search` (Task 1), feature `feature/episode-titles` (Tasks 2-5), both off `main`; regular branches, no worktrees. Use `git -C <path>`. `git add` every new source file. Update `AGENTS.md`, `README.md`, `docs/follow-ups.md` when committing the feature branch. Co-author trailer per the session's attribution reminder.
- Source: TVmaze only, `GET {base}/singlesearch/shows?q=<name>&embed=episodes` (base `https://api.tvmaze.com`). 404 = no such show. Response: `{ "name", "premiered": "YYYY-MM-DD"|null, "_embedded": { "episodes": [ { "season", "number", "name" } ] } }`; `number` can be null (specials): skip those.
- Only the cleaned show name is sent. Never send or log playlist URLs/credentials. Errors shown/logged must not contain URLs (`iptv::redact::redact_url` on any text that could).
- Cache file `episode-titles.json` in the data dir, `{"version":1,"shows":{key:{"at":<unix secs>,"status":"hit"|"miss"|"error","titles":{"1:2":"The Visit"}}}}`. Key = `iptv::library::normalize(cleaned name)` plus `|<year>` when a year is present. TTLs: hit 30 days, miss 1 day, error 1 hour. Atomic writes via `iptv::fsutil`; unreadable/unknown-version file is moved aside with `fsutil::backup_path` and never overwritten (same rules as `positions.rs`; if the move fails, `save` refuses). Max 500 shows (oldest `at` dropped).
- HTTP: client timeout 10 s, User-Agent `iptv::config::DEFAULT_USER_AGENT`. At most one request at a time, 300 ms pause between requests. No setting to turn the feature off.
- Failure is silent: rows stay `S01E02`. No popup. `(Unsorted)` is never looked up. A title already in the playlist entry name wins over TVmaze.
- Match rule: normalized TVmaze name must equal normalized cleaned query; if the playlist name had a year, TVmaze `premiered` year must be within ±1 of it (if TVmaze has no premiered date, accept).
- Keep each file's existing line endings (see AGENTS.md; `series.rs`, `cache.rs` are CRLF — do not use `sed -i` on them).

## Review Focus

- Show name with year and country tag, e.g. `Show (2019)` / `Show (US)`: cleaned name `Show`, year 2019 only for the year tag; country tag dropped.
- Icelandic/Nordic characters (`Ófærð`, `Å`, `Ø`): cleaning and normalisation must not mangle or panic on non-ASCII; the query is URL-encoded.
- TVmaze returns a different show (fuzzy match) → treated as miss, rows unchanged.
- Offline / timeout / HTTP 429 / 5xx / invalid JSON → `error` entry (1 h), no crash, rows unchanged.
- Opening the same show repeatedly or two shows quickly → one request per show, no duplicates, results land on the right show (keyed by show name, not by which show is open now).
- Hidden-group filtering and library refresh while titles load: titles are keyed by show name, so a refresh does not lose them.
- Enter on a focused tab button must not also play/open the selected row of the previous tab.

---

## Task 1: Tab focus + Enter opens the tab (bugfix branch)

**Files:**
- Modify: `crates/iptv-player/src/state/mod.rs` (new pure fn + test)
- Modify: `crates/iptv-player/src/ui/mod.rs` (`handle_keys` ~795-875, `top_bar` ~929-936, App struct/new)

**Interfaces:**
- Produces: `pub fn tab_enter(focused: Option<Tab>) -> Option<(Tab, bool)>` in `state/mod.rs`: `Some((tab, focus_search))` when a tab button has keyboard focus (`focus_search` is true for Movies and Series only), `None` otherwise.

Cause: `handle_keys` consumes Enter globally and calls `activate`, so a focused tab button never receives it.

- [ ] **Step 1: Branch.** `git -C <repo> checkout -b bugfix/tab-enter-search`.
- [ ] **Step 2: Failing test** in `state/mod.rs` tests: `tab_enter_opens_focused_tab` asserting `tab_enter(Some(Tab::Movies)) == Some((Tab::Movies, true))`, `Series` -> `(Series, true)`, `Live` -> `(Live, false)`, `Favorites` -> `(Favorites, false)`, `None` -> `None`. Run `cargo test -p iptv-player tab_enter` → fails to compile/FAIL.
- [ ] **Step 3: Implement `tab_enter`** (trivial match) and make the test pass.
- [ ] **Step 4: Wire it.** Add `focused_tab: Option<Tab>` to `App` (init `None`). In `top_bar`, for each tab keep the response of `selectable_label`; set `self.focused_tab = Some(t)` when `resp.has_focus()`, and reset `self.focused_tab = None` at the start of the loop (so it reflects this frame). In `handle_keys`'s `if enter {` branch, before the fullscreen check: `if let Some((tab, focus_search)) = tab_enter(self.focused_tab)` → set `self.tab = tab`, `self.view = View::Browse`, `self.focus_search = focus_search`, `self.focused_tab = None`, surrender the tab button's focus (`ctx.memory_mut(|m| m.surrender_focus(id))` — use the id of the response, store it alongside as `focused_tab_id: Option<egui::Id>`), then skip `activate`. Not while the sub menu owns Enter (already handled: `enter` is false then). Ignore when fullscreen (`self.focused_tab` is only set in Browse view anyway).
- [ ] **Step 5: Verify.** `cargo test --workspace` passes; `cargo build --release`; manual: run the app, Tab to Movies, Enter → Movies tab with search box focused; same for Series; Live/Favorites just switch.
- [ ] **Step 6: Commit** (`fix: Enter on a focused tab button opens the tab and its search box`), update `AGENTS.md` Keys paragraph with one sentence. Deploy via the `build-and-deploy` skill (memory: deploy after each bugfix/feature commit; report if the app was open). Merge to main only after the user tests the deployed build.

---

## Task 2: Titles model, name cleaning, matching, parsing, cache (pure)

**Files:**
- Create: `crates/iptv/src/episode_titles.rs`; Modify: `crates/iptv/src/lib.rs` (`pub mod episode_titles;`)
- Test: unit tests in the same file (`tempfile` for cache tests)

**Interfaces:**
- Produces:
  - `pub struct Titles(BTreeMap<(u32, u32), String>)` with `get(&self, season: u32, episode: u32) -> Option<&str>`, `len()`, `Clone`, `Debug`, `PartialEq`.
  - `pub struct Query { pub name: String, pub year: Option<u32> }`; `pub fn clean_query(show: &str) -> Query` (drops a trailing `(YYYY)` into `year`, drops a trailing 2-letter-ish country tag `(US)`/`(UK)` etc., collapses whitespace).
  - `pub fn cache_key(q: &Query) -> String`.
  - `pub fn parse_response(json: &[u8], q: &Query) -> Result<Option<Titles>>`: `Ok(None)` = show does not match (miss), `Err` = invalid JSON.
  - `pub enum Lookup { Hit(Titles), Miss, Unknown }` and `pub struct TitleCache` with `load(dir: &Path) -> TitleCache`, `get(&self, key: &str, now: u64) -> Lookup` (Unknown when absent or expired; an `error` entry within 1 h returns `Miss` so we do not retry), `put_hit/put_miss/put_error(&mut self, key, now, ...)`, `save_if_dirty(&mut self) -> Result<()>`; `now` is passed in (unix secs) for testability.

- [ ] **Step 1: Failing tests** (names and assertions):
  - `clean_query_strips_year_and_country`: `clean_query("Fake Show (2019)")` == `Query{name:"Fake Show", year:Some(2019)}`; `"Fake Show (US)"` → name `Fake Show`, year `None`; `"Ófærð"` unchanged; `"  Fake   Show "` → `Fake Show`.
  - `parse_matches_name_and_year`: JSON for show `Fake Show` premiered `2019-05-01` with episodes (1,2,"The Visit"), (1,null,"Special") → `Some` with `get(1,2)==Some("The Visit")`, `len()==1`. Same JSON with `Query{year:Some(2005)}` → `None`; with name `Other Show` → `None`; premiered null and year given → `Some`. Name comparison via `library::normalize` (case/diacritics-insensitive).
  - `parse_rejects_invalid_json`: `parse_response(b"<html>", ..)` is `Err`.
  - `cache_round_trip_and_ttl`: put_hit at t=0; `get` at 29 days → `Hit`; at 31 days → `Unknown`. put_miss at 0: `Miss` at 23 h, `Unknown` at 25 h. put_error at 0: `Miss` at 59 min, `Unknown` at 61 min. After `save_if_dirty` + `load`, the hit survives.
  - `cache_unreadable_file_is_moved_aside`: garbage `episode-titles.json` → `load` gives empty cache and `episode-titles.json.bad` exists; unknown `version` same.
  - `cache_caps_entries`: 501 puts → 500 kept, oldest dropped.
- [ ] **Step 2: Run** `cargo test -p iptv episode_titles` → FAIL.
- [ ] **Step 3: Implement** the signatures above. Model the cache file handling on `positions.rs` (`load_with` mover injection is optional; the quarantine test above is enough). Titles serialise as `"S:E"` string keys.
- [ ] **Step 4: Run tests** → PASS.
- [ ] **Step 5: Commit** (branch `feature/episode-titles` created from main in this step; `git add` the new file).

---

## Task 3: TVmaze fetch

**Files:** Modify `crates/iptv/src/episode_titles.rs` (+ tests).

**Interfaces:**
- Consumes: `Query`, `parse_response`, `Titles` from Task 2.
- Produces: `pub enum Fetched { Found(Titles), NotFound, Failed(String) }` and `pub fn fetch_titles(base_url: &str, q: &Query) -> Fetched` — blocking; 404 or a non-matching show → `NotFound`; network/HTTP≥400 (other than 404)/bad JSON → `Failed(reason)`; the reason never contains the URL (`reqwest` errors: use `e.without_url()`/redact).

- [ ] **Step 1: Failing tests** with a local `TcpListener` server (copy the pattern at the `fetch.rs` tests ~line 410): `fetch_found` (200 with the JSON fixture; also assert the request line contains `/singlesearch/shows?q=Fake%20Show&embed=episodes` and a `User-Agent` header), `fetch_404_is_not_found`, `fetch_500_is_failed`, `fetch_connection_refused_is_failed` (bind then drop the listener), `fetch_encodes_nordic_name` (query `Ófærð` → request path contains `%C3%93f%C3%A6r%C3%B0`).
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement** with `reqwest::blocking::Client::builder().timeout(10 s).user_agent(DEFAULT_USER_AGENT)`; percent-encode via `reqwest::Url::parse_with_params`/`query_pairs_mut` (no new crate), but the test above pins the encoding (`%20` for spaces: if `query_pairs_mut` yields `+`, accept either in the assertion and say so in the test comment). **Step 4:** tests PASS. **Step 5: Commit.**

---

## Task 4: Loader, row text, UI wiring

**Files:**
- Create: `crates/iptv-player/src/state/titles.rs`; Modify `state/mod.rs` (`pub mod titles;`)
- Modify: `crates/iptv-player/src/state/vod.rs` (`episode_text`), `crates/iptv-player/src/ui/vod.rs` (`episodes`), `crates/iptv-player/src/ui/mod.rs` (App field, open-show hook, poll)

**Interfaces:**
- Consumes: `iptv::episode_titles::{clean_query, cache_key, fetch_titles, Fetched, Lookup, TitleCache, Titles, Query}`, `iptv::series::{parse_episode, EpisodeRef}`.
- Produces:
  - `pub struct TitleLoader` with `TitleLoader::new(data_dir: PathBuf, base_url: String, wake: impl Fn() + Send + 'static) -> TitleLoader`, `request(&mut self, show: &str)` (ignored for `(Unsorted)`, empty, or already requested/answered this session), `poll(&mut self) -> Vec<String>` (show names whose answer arrived since last poll), `get(&self, show: &str) -> Option<Arc<Titles>>`. One worker thread with an `mpsc` channel; the worker owns the `TitleCache`: cache `Hit` → answer without network; `Miss` → answer none; `Unknown` → fetch (300 ms gap between fetches), record hit/miss/error, `save_if_dirty`, call `wake()`. `base_url` is a parameter so tests use a local server; production passes `https://api.tvmaze.com`. Worker death must not panic the UI (`request` after death is a no-op).
  - `pub fn episode_text(lib: &Library, show: usize, entry: usize, titles: Option<&Titles>) -> String` (changed signature): as today, but when the label has no title part (the entry name has none) and `titles` has `(season, episode)` from `parse_episode`, return `S01E02 - <title>`. `(Unsorted)` unchanged.

- [ ] **Step 1: Failing tests.** In `state/vod.rs` tests: `episode_text_adds_title_when_name_has_none` (library with `Fake Show S01E02`, titles `{(1,2):"The Visit"}` → `S01E02 - The Visit`), `episode_text_prefers_playlist_title` (`Fake Show S01E02 - Own Title` with a different TVmaze title → `S01E02 - Own Title`), `episode_text_without_titles_unchanged`. Update existing `episode_rows_have_season_headers_and_labels` callers for the new parameter (`None`). In `state/titles.rs` tests (local server as in Task 3, temp data dir): `loader_fetches_once_and_caches` (request twice → one HTTP request; `poll` returns the show name once; `get` returns titles; a second `TitleLoader` on the same dir answers with no HTTP request), `loader_ignores_unsorted`, `loader_failure_gives_no_titles_and_no_retry_within_session`.
- [ ] **Step 2: Run** `cargo test -p iptv-player titles` / `vod` → FAIL.
- [ ] **Step 3: Implement** `titles.rs` and the `episode_text` change.
- [ ] **Step 4: Wire the UI.** `App` owns `titles: TitleLoader`, created with the data dir and a `wake` closure that calls `ctx.request_repaint()` (clone the egui `Context` in `App::new`). Where a show is opened (`series.open_selected` success in `activate`, and the click path; also after `series.rebase`), call `self.titles.request(&open.name)`. Each frame call `self.titles.poll()` (repaint is already requested by `wake`). `ui::vod::episodes` gets `titles: Option<&Titles>` (from `self.titles.get(&open.name)`) and passes it to `episode_text`. No other behaviour changes.
- [ ] **Step 5: Verify.** `cargo test --workspace` PASS; `cargo build --release`. Manual with the real playlist (build-and-deploy skill gives the deployed build): open a show, rows show `S01E02` then gain titles within ~1-2 s; reopen is instant; a data dir with no network leaves rows as `S01E02`; `episode-titles.json` appears in the data dir; check an Icelandic show.
- [ ] **Step 6: Commit** (`git add crates/iptv-player/src/state/titles.rs`).

---

## Task 5: Docs, follow-ups, deploy

**Files:** `AGENTS.md`, `README.md`, `docs/follow-ups.md`.

- [ ] **Step 1:** `AGENTS.md`: one bullet "Episode titles" (module, TVmaze, cache file + TTLs, match rule, loader, privacy: only the cleaned show name is sent). `README.md`: note that episode titles come from TVmaze, are cached in `episode-titles.json`, need internet, and that obscure/Nordic shows may not be found. `docs/follow-ups.md`: add "manual check: Icelandic/Nordic shows' coverage", "no setting to turn the lookup off", "second source (TMDB) if TVmaze coverage is poor", "episode numbering differences between provider and TVmaze (specials/absolute numbering) can show a wrong or missing title".
- [ ] **Step 2:** `cargo test --workspace`, then deploy with the `build-and-deploy` skill (report if the app was open). Commit docs. Do not merge to main until the user has tried the deployed build; after merge delete the branch locally and remotely (user CLAUDE.md), running `gh auth status` before any push.
