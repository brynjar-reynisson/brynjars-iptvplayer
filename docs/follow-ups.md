# Follow-ups

Known gaps and ideas that were found during implementation and review but not done in the first version. None of them blocks normal use. Ordered roughly by value.

## To verify by hand (could not be tested automatically)

- **Minimised window during playback.** Minimise the window for more than 30 s while a stream plays, then restore it. The supervisor ticks and mpv rendering are driven by egui frames; if eframe stops producing frames while minimised, retries and the render loop may pause. Check that playback continues or reconnects after restore.
- **Pause while reconnecting.** Press Space (pause) while the overlay says "Reconnecting…". The retry reloads the stream paused; confirm mpv's `PLAYBACK_RESTART` still arrives, otherwise the start watchdog could loop.
- **Typing in the search and filter boxes with a real keyboard.** Automated runs sometimes lost a key press in text boxes; this is believed to be a test-harness artefact.

- **Autoplay next episode, real episodes.** Verified only with generated local files (testsrc + scrolling drawtext credits on black). Still to check by hand on the real provider: the Wheel of Time S1 finale (stinger after the credits: the offer must wait for the real end), a normal episode with credits on black (offer at the credits start), the last episode of a season (continues into the next season), and an episode whose provider refuses the second connection (offer only at the end). Watch the `tail scan: ...` stderr line (`--console`) to tune `state::credits` thresholds; with `--console` the sampled frames are also saved under `<data dir>/scan-frames/` (one PNG per sample, letter in the file name) for tuning the classifier with real shows. Marvel-style credits (names over non-black backgrounds or footage) are expected to need classifier work. Also check a provider that drops the oldest connection when a second one opens: the scan's second connection (now closed right after the scan) must not cut the playing stream.

- **Episode titles, real show.** Open a real show: titles should appear within ~2 s and reopening it should be instant (cached). Also check how well Icelandic/Nordic shows are covered by TVmaze.

## Behaviour

- **Credential rotation while playing.** `Playback::rebase` updates the playback's URL after a refresh, but the `Supervisor` keeps retrying the URL it was started with. If the provider rotates credentials while something plays, retries keep failing until the channel is selected again. Fix: give the supervisor an `update_url` used on the next (re)load.
- **First retry after a quick channel switch.** For 500 ms after a channel switch the new load's own log lines are ignored (to keep a late error of the old channel from being blamed on the new one), so a fast rejection of the channel just chosen is classed "other" and retries at the short network backoff once before being classified correctly.
- **Arabic search.** Search folds Latin diacritics but does not unify Arabic spelling variants (alef/hamza forms, tashkeel). There is also no relevance ranking: results are in playlist order.
- **Series shows named by prefix.** Two-letter language/country prefixes (`XX - `, `XX:`) are stripped from show names, which can also strip a genuine two-letter title; country tags such as `(US)` and years are kept, so the same show can appear more than once.
- **Titles with an embedded `NxM`** before an `SxxExx` marker (for example a title containing `4x4`) are read as season 4 episode 4. Prefer the `SxxExx` marker over `NxM` when both are present.
- **Unsorted series episodes** are findable by their own title, but only through the single `(Unsorted)` result row, not as separate result rows.
- **Next-episode offer limits.** "Never skip content" is best effort: samples are 10 s apart, so a stinger shorter than ~10 s that falls between two samples (or in the last 3 s) is not seen; a single Content sample with no Content neighbour is treated as noise (logo card, misread credits frame; first seen on a real episode where lone X samples inside the credits pushed the offer to the end), and the refinement frames at +-5 s only turn a stinger into two Content samples when it lasts roughly 10 s or more, so a shorter one can still be missed; and keyframe seeks mean a classified frame can be up to one GOP away from its sample time, so the offer can come a few seconds early. The offer is behind an 8 s countdown the user can cancel. A pending Open/Close for the preview worker can wait for one scan frame (each scan resume takes at least one new frame). Credits over footage (split screen, scenes behind the names) read as content, so the offer comes only at the end. Known classifier false positives: dark show scenes (faces or figures on near-black, e.g. 5 of 38 scene frames of the tuning episode) read as Credits; harmless while Content follows them (an isolated Credits sample between Content samples is smoothed to Content), but two in a row just before real credits would start the offer early. A plain scene that is mostly one flat colour with a small object can read as Card (neutral). When a show is misclassified, run it with `--console`, open the `scan-frames/<episode>-<secs>/` folder it prints (letter in each file name) and check the frames with `cargo run -p iptv-player --example scan_frames_check -- <folder>` (classifies them in place; never commit the frames). Title cards or dark text-on-black scenes in the last 8 minutes followed only by credits/black could start the offer early. The offer is placed above the control buttons and can overlap a seek-bar preview frame. Whether an episode gets an offer is decided when it starts; hiding groups while it plays only changes which episode is next (recomputed when it plays), so hiding the next episode's group can leave an offer that then plays the one after it or, at the end, stops.
- **User seek buffering for more than 15 s** triggers a reload at the pre-seek position.
- **Pending channel switch after a tab switch** still fires 300 ms later for the old tab's list.
- **Groups menu** draws ~600 checkboxes unvirtualised when open; fine in practice but a candidate for virtualisation.
- **Show rows' episode counts** ignore hidden groups, so the count can differ from what opening the show lists.
- **No setting to turn the episode title lookup off** (it contacts TVmaze whenever a show is opened and not cached).
- **Second title source (TMDB)** if TVmaze coverage of the playlist's shows turns out to be poor.
- **Episode numbering differences** between the provider and TVmaze (specials, absolute numbering) can show a wrong or missing title.
- **Regional tags** `(UK)`/`(US)` are stripped from the show name, so `The Office (UK)` can match the US show (both share one cache key).
- **Title lookups are serial with a 10 s timeout**, so browsing offline queues delays (a failed lookup is cached for 1 h and, within a session, only retried by Refresh/F5 or after a restart).
- **Expired titles can be replaced by an error.** When a 30-day hit expires and the refetch fails (or returns not-found), the old titles are overwritten; keeping them on failure would be nicer.
- **`clean_query` strips any 2-3 letter bracket tag** (`(New)`, `(TV)`), not only country tags like `(UK)`/`(US)`.
- **Loader tests** use 200 ms sleeps for negative assertions (slightly slow, could be flaky under load).

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
- **Narrow video panel.** The **>>** next-episode button hides itself when it does not fit, but the fullscreen, play/pause and CC buttons are always drawn and get clipped at the panel's left edge when it is very narrow (seen at ~100 px wide).

- **Recent series, manual checks still to do** (2026-10-21): the continue button/Shift+P/list were only unit-tested (record, resolve, quarantine); try it in the real app: button next to the tabs, Shift+P, list under the search box, continue after credits, a show that left the playlist. Credits are only flagged while `autoplay_next` is on (the offer/scan decides); with it off only a finished episode counts. A focused continue button plus Enter is not specially handled.

## Ideas (not requested)

- Show watched/resumable marks (and the saved position) in the movie and episode lists.
- EPG / programme guide (the playlist carries `tvg-id`).
- Per-country ordering or hiding of live channels.
- Audio-track selection UI (mpv defaults are used now).
- A UI switch for `autoplay_next` (settings.json only for now).
- A "previous episode" button to match the **>>** next-episode button (Shift+Right); Shift+Left would be its natural key, but it currently seeks back 10 s like Left.
