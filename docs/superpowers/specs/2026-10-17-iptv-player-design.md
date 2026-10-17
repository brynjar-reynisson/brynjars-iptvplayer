# IPTV Player for Windows — Design

## Intent and why
The user has a paid IPTV subscription with an IPTV provider. The reference app is an Android app (licensed separately) that works as a subscription manager: on startup it lets you select a subscription, then browses and plays it well. The user has only one subscription, so a subscription-selection step is unnecessary here. On Windows the alternatives split the job: a selector app that launches VLC, and VLC gives up on streams that stall or glitch. The goal is a single Windows desktop app, written in Rust, that browses the subscription's playlist and plays it in the same window, and that **keeps retrying a broken stream until the user changes channel**.

## What was learned from the Android APK (recon, 2026-10-17)
- The APK is a thin Android TV shell: WebView + ExoPlayer 2 (HLS/DASH/SmoothStreaming/RTSP, plus FFmpeg audio decoder extension). The browsing UI is remote JavaScript downloaded from the app's server at launch, so **there is nothing to port from the APK**; the UI is designed fresh. the Android app's subscription selection, login, SignalR and native secrets are out of scope.
- The Android "spinner, then big '!' and no retry until reselect" behaviour is something we deliberately do better.

## The input: a single Xtream-style M3U playlist
- User supplies one M3U file URL (credentials embedded) in a local config. Sample (77 MB, ~282,000 entries, 605 groups) is at `<samples dir>\<playlist>.m3u` (outside this repo; contains credentials; never commit it).
- Each entry: `#EXTINF:-1 tvg-ID="" tvg-name="..." tvg-logo="..." group-title="...",Display name` then the stream URL line.
- Kind is determined by URL path: `http://host:port/<user>/<pass>/<id>` = live (~31k); `/movie/<user>/<pass>/<id>.<ext>` = movie (~57k); `/series/<user>/<pass>/<id>.<ext>` = series episode (~195k, one flat entry per episode). Extensions seen: mkv, mp4, ts, avi. A few live entries use other hosts (Pluto TV, Amagi); treat any URL without /movie/ or /series/ as live.
- Group titles: live like `Sweden - Nationella` (country prefix before " - "); `Movies: <lang>`; `Series: <lang>`. Group-less header entries like `===== Sweden ======` exist and are separators to ignore or skip.

## Goals
1. One window: browse + play, no external player.
2. **Persistent playback**: on stall, error, or end of a live stream, show a "reconnecting" indicator and retry indefinitely (with backoff capped to a few seconds) until the user picks something else. Never settle on a dead error screen. Applies to live; for movies/series, retry/resume at the last position.
3. Live: flick through channels quickly (keyboard up/down, scroll, click); channel switch feels instant. Channels grouped by country, shown as one flat list per country, in file order.
4. Favorites: one-key add/remove, a favorites view, persisted locally. Favorites are keyed by something stable across playlist refreshes (stream id or normalized name+group — builder's call; stream id preferred).
5. Movies and series: search by name, results update as you type over ~250k entries. Series are presented as show → season → episode, derived by parsing episode names (e.g. `S01E02`); entries that fail to parse still appear under a catch-all so nothing is hidden.
6. Groups (605) are optional filters (e.g. hide languages), not the primary navigation.
7. Fast startup: parse the playlist once, store a compact local cache, load from cache on launch; refresh on demand (button/key) and optionally when stale.

## Non-goals
- the Android app's subscription management/login; obtaining the the provider playlist URL (the user supplies it).
- EPG/TV guide, recording, casting, Android/TV/remote-control UI, mobile.
- Xtream `player_api.php` integration (playlist file only).
- Cross-platform polish (Windows 11 is the target; don't gratuitously break Linux/macOS).

## Anti-goals (failure even if it "works")
- Playback that stops on a transient stream error and needs manual reselect.
- UI that freezes while loading/parsing the big playlist (Notepad-style).
- Credentials ending up in git, logs, or error messages shown on screen.

## Constraints and decisions made by the user
- Desktop use with mouse and keyboard.
- Rust. Distribution as a folder with the exe plus one DLL (libmpv) is acceptable; single-exe is not required.
- Playlist URL/credentials live in a local, gitignored config file.
- Project docs: `AGENTS.md` (not CLAUDE.md) in the repo root.

## Builder's-call defaults (assistant's choices, not user-approved details)
- UI: egui/eframe. Playback: libmpv (video rendered into the app window, via mpv's render API or an embedded child window — builder decides), with the app itself supervising end-of-file/error events and reloading; plus mpv reconnect options (`reconnect`, `reconnect_streamed`, `reconnect_delay_max` via `stream-lavf-o`) as a first line of defence.
- Data: parse on a background thread; cache in a local binary/SQLite format under a gitignored `cache/` dir; in-memory search index.
- Config: `config.toml` (gitignored) holds playlist URL; favorites and cache stored separately from it. Commit a `config.example.toml` with placeholders.
- Logging must redact URLs' credential segments.

## Left to the builder
Crate choices, module layout, cache format, search algorithm, exact keybindings, window layout, reconnect backoff numbers, episode-name parsing heuristics, how to obtain/bundle libmpv-2.dll.

## Open for later (not required for v1)
Per-country ordering/hiding, volume/audio-track/subtitle selection UI (basic mpv defaults are enough for v1), playback-position memory for movies/series.

## Assistant's calls (added after builder review; please check)
1. **Live grouping:** a country list merges all groups sharing the prefix before " - " (e.g. every `Sweden - *`), flat, in file order. Live groups without " - " (Pluto TV, Amagi, etc.) go under "Other". Separator entries (`===== X ======`) are dropped.
2. **Favorites key:** `kind + numeric stream id` (last URL path segment, ignoring host/user/pass so credential rotation doesn't lose favorites), falling back to normalized name + group when no id parses. Ids are not assumed unique across kinds.
3. **Performance targets (reference: this machine):** usable UI from cache in under ~1 s; search results under ~50 ms per keystroke; memory under ~500 MB with the full playlist loaded.
4. **Retry on dead streams:** retry forever for all errors, with backoff capped at a few seconds. For HTTP 401/403/connection-limit-style failures, show a distinct message ("account rejected / too many connections") and use a longer capped backoff (~15 s) to avoid hammering the server. The user can always switch channel.
5. **VOD end:** if position is within a few seconds of duration it is a natural finish: stop, no autoplay of the next episode in v1. Otherwise treat it as a drop and resume at the last position.
6. **Refresh:** downloads and parses on a background thread; the UI stays usable on the old data and swaps to the new index atomically. Favorites whose entries disappeared remain listed, marked unavailable.
7. **File locations:** `config.toml`, `cache/` and favorites sit beside the exe by default, with the directory overridable (flag/env var). All gitignored.
