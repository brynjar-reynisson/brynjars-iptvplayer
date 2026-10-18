# Follow-ups

Known gaps and ideas that were found during implementation and review but not done in the first version. None of them blocks normal use. Ordered roughly by value.

## To verify by hand (could not be tested automatically)

- **Minimised window during playback.** Minimise the window for more than 30 s while a stream plays, then restore it. The supervisor ticks and mpv rendering are driven by egui frames; if eframe stops producing frames while minimised, retries and the render loop may pause. Check that playback continues or reconnects after restore.
- **Pause while reconnecting.** Press Space (pause) while the overlay says "Reconnecting…". The retry reloads the stream paused; confirm mpv's `PLAYBACK_RESTART` still arrives, otherwise the start watchdog could loop.
- **Typing in the search and filter boxes with a real keyboard.** Automated runs sometimes lost a key press in text boxes; this is believed to be a test-harness artefact.
- **Hardware decoding** (`hwdec=auto-safe`) on a wider range of real streams, and resizing / HiDPI with a non-origin video rectangle on other displays.

## Behaviour

- **Credential rotation while playing.** `Playback::rebase` updates the playback's URL after a refresh, but the `Supervisor` keeps retrying the URL it was started with. If the provider rotates credentials while something plays, retries keep failing until the channel is selected again. Fix: give the supervisor an `update_url` used on the next (re)load.
- **First retry after a quick channel switch.** For 500 ms after a channel switch the new load's own log lines are ignored (to keep a late error of the old channel from being blamed on the new one), so a fast rejection of the channel just chosen is classed "other" and retries at the short network backoff once before being classified correctly.
- **Arabic search.** Search folds Latin diacritics but does not unify Arabic spelling variants (alef/hamza forms, tashkeel). There is also no relevance ranking: results are in playlist order.
- **Series shows named by prefix.** Two-letter language/country prefixes (`XX - `, `XX:`) are stripped from show names, which can also strip a genuine two-letter title; country tags such as `(US)` and years are kept, so the same show can appear more than once.
- **Titles with an embedded `NxM`** before an `SxxExx` marker (for example a title containing `4x4`) are read as season 4 episode 4. Prefer the `SxxExx` marker over `NxM` when both are present.
- **Unsorted series episodes** are findable by their own title, but only through the single `(Unsorted)` result row, not as separate result rows.
- **User seek buffering for more than 15 s** triggers a reload at the pre-seek position.
- **Pending channel switch after a tab switch** still fires 300 ms later for the old tab's list.
- **Groups menu** draws ~600 checkboxes unvirtualised when open; fine in practice but a candidate for virtualisation.
- **Show rows' episode counts** ignore hidden groups, so the count can differ from what opening the show lists.

## Robustness

- **Refresh memory peak.** The "new playlist is much smaller than the cached one" safety check decodes the whole old cache while the UI still holds the old library and the new entries are in memory; the transient peak is probably above 500 MB. A header-only entry count in the cache would avoid this.
- **Redirect policy.** Same-host redirects are followed even when they downgrade `https` to `http` or change port; reject scheme downgrades.
- **Silent quarantine.** An unreadable `favorites.json` or `settings.json` is moved aside to `*.bad[.N]` without telling the user; show a one-time notice. A transient read error (antivirus lock) also triggers this.
- **Search worker thread.** `SearchWorker` spawn uses `.expect`, so a failing thread spawn panics; when the worker is found dead, the request that triggered the check is dropped and the user must edit the query again.
- **Atomic-write helper** `fsutil` is `pub`; it could be `pub(crate)`.
- **Windows file sharing.** `cache::load` opens the file without delete-sharing semantics, so a concurrent reader can make the writer's rename fail transiently (it is retried).
- **No App-level tests** for `ui/mod.rs` orchestration (`sync_library`, `on_finished`, player-view stepping, scroll handling); only the GUI-free `state/` pieces and the headless `retry_demo` are covered.
- **Test-only nits.** Two worker-panic tests print panic text to stderr; `fetch` tests use local TCP servers with generous timeouts (no flakiness seen).

## Build / tooling

- **`mpv.lib` generation is manual** (dumpbin → hand-written `.def` → `lib /def`, documented in `AGENTS.md`). Add a script to generate it from `libmpv-2.dll`.
- **`build.rs`** has `rerun-if-changed` on files that do not exist when libmpv is absent, so the build script reruns on every build in that case (harmless).
- **Library ready time** from cache was measured at 0.43–0.48 s in the last run (earlier runs under load showed 1.0–1.3 s, still on a background thread so the window is usable within ~0.1–0.5 s). A lazily built search index (`OnceLock`) or one concatenated string with offsets would cut build time and memory further.

## Ideas (not requested)

- Show watched/resumable marks (and the saved position) in the movie and episode lists.
- EPG / programme guide (the playlist carries `tvg-id`).
- Per-country ordering or hiding of live channels.
- Audio-track and subtitle selection UI (mpv defaults are used now).
- Optional autoplay of the next episode.
