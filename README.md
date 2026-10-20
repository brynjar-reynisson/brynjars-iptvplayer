# IPTV Player

A Windows desktop IPTV player written in Rust: browse an Xtream-style M3U playlist
(live channels by country, favorites, movies, series) and play it in the same window
with libmpv. Streams that drop are reconnected automatically, without any user action,
until you pick something else.

## Setup

1. **Toolchain**: Rust stable with MSVC (`rustup default stable-x86_64-pc-windows-msvc`).
   You also need Visual Studio Build Tools with the C++ workload.
2. **libmpv**: download the x86_64 dev package (`mpv-dev-x86_64-<date>-git-<hash>.7z`) from
   <https://github.com/shinchiro/mpv-winbuild-cmake/releases>, extract it with 7-Zip and put
   `libmpv-2.dll` in `./libmpv/`. Generate the MSVC import library `libmpv/mpv.lib` from the
   DLL (see `AGENTS.md`, section *libmpv*, for the `dumpbin` / `lib /def` steps). Both files
   are gitignored. You need mpv 0.38 or later.
3. **Build**: `cargo build --release`. The build script copies `libmpv-2.dll` next to
   `target/release/iptv-player.exe`.
4. **Config**: copy `config.example.toml` to `config.toml` beside the exe (or in the data
   dir, see below) and set `playlist_url` to your provider's M3U URL. `config.toml` holds
   your credentials. It is gitignored, so never commit it.
   - Optional: `user_agent = "..."` sets the User-Agent sent with the playlist download
     and every stream request. If it is missing or empty, the app sends
     `VLC/3.0.20 LibVLC/3.0.20`. Some providers block requests that have no known
     player User-Agent. If a refresh fails with "server returned HTTP 454 (blocked?
     check user_agent in config.toml)", try another value.
   - Optional: `top_countries = "Iceland, United Kingdom"` lists those live countries first, in that
     order (comma-separated). The rest are alphabetical, with "Other" last. Applies after
     the next refresh (F5) or restart. The old single `top_country` key still works.
   - The first download can take a long time while the provider generates the file.
   - A changed `user_agent` applies to the next refresh (F5). Streams pick it up after
     a restart.
5. **Run**: `target\release\iptv-player.exe`, or `cargo run --release -p iptv-player`.

### Release folder

`powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1 [-Out <dir>]` builds
the release binary and writes a self-contained folder (default `dist\iptv-player\`, which
is gitignored) with `iptv-player.exe`, `libmpv-2.dll` and `config.example.toml`. Copy the
example to `config.toml` there and the folder runs on its own. The exe and the DLL are the
only files needed at runtime.

### Data directory

`config.toml`, `cache/` (parsed playlist), `favorites.json`, `positions.json` (resume points), `episode-titles.json` (cached episode titles) and `settings.json` (hidden
groups, subtitle choices, autoplay) live in the data directory. The app looks for it in this order:
`--data-dir <path>`, then the `IPTV_DATA_DIR` environment variable, then the exe's folder.

## Using it

- Tabs: **Live** (countries A-Z, then channels), **Favorites**, **Movies**, **Series**
  (show, then seasons, then episodes).
- Click or press **Enter** to play the selected entry. **Enter** or double-click on the
  playing row goes full screen. **Enter** or double-clicking the video toggles full screen
  (back to the previous window layout).
- **Up/Down** move the selection. While live TV plays, they also switch channel after a
  short pause (300 ms), so holding the key opens only one stream.
- Live TV: **Right** works like Enter on the channel list (plays the selected channel). **Left**
  moves the keyboard to the country list (outlined); Up/Down then walk the rows, and **Right**
  or **Enter** go back into the channels and play the first one. **Esc** also leaves the country
  list. The selected country is opened to show its groups (Sport, Kids, News, ...) indented
  under it: pick the country row for all its channels or a group row for just that group.
- **F** toggles a favorite. **Space** pauses. The **[ ]** button at the video's
  bottom right (or **F11** / **Alt+Enter**) makes the player fill the whole screen, without
  the channel list. **Esc** goes back to how it was before. Otherwise **Esc** leaves the filter box, or stops
  playback and leaves the player view. **F5** refreshes the playlist.
- **Ctrl+F** or **/** focuses the channel filter (on Movies/Series, the search box).
- A play/pause button sits left of the full-screen button and shows the action a click
  performs (**Space** does the same).
- For movies and episodes, moving the mouse over the video or pressing a key shows a seek
  bar below the video for 3 seconds: the played and total time with a dot at the current
  position, and a small preview frame of the video above the mouse (or above the dot after
  an arrow-key step). Click or drag on the bar to jump (the jump happens when you let go),
  or press **Left**/**Right** to step 10 seconds back/forward. Previews use a second
  connection to the provider, opened when you first hover the bar; if the provider refuses
  it, the bar works without previews.
- Movies and episodes have a **CC** button (left of play/pause) that picks the subtitles while
  playback continues (also the **C** key; Up/Down + Enter or a click choose, Esc closes). For series the choice is remembered per show and matched in later
  episodes (by language); stored in `settings.json`.
- Next episode: during the closing credits of an episode a **Next episode in 8** button
  appears at the video's bottom right and counts down; at 0 the next episode starts (the
  last episode of a season continues with the next season). A click on it plays it now
  (also **Enter** in the full player view or full screen; in the list view Enter keeps its
  usual meaning); **Esc** or **Cancel** keeps watching (no offer again for that episode).
  The countdown waits while the video is paused, buffering or reconnecting. The credits
  are found by looking ahead at the last 8 minutes over the preview connection; if real
  scenes follow the credits (or the check is not possible), the next episode starts only
  when the file ends. Turn it off with `"autoplay_next": false` in `settings.json`.
  Movies, live TV and unsorted episodes are not affected.
- A **>>** button (left of **CC**, also **Shift+Right**) skips to the next episode at any time
  while an episode with a next one plays, whether autoplay is on or not. The skipped episode
  keeps its place, so you can resume it later. It is hidden on the last episode and when the
  video panel is too narrow.
- Episode titles: when you open a show, titles (`S01E02 - Title`) are looked up on TVmaze
  (needs internet; only the show name is sent) and cached in `episode-titles.json` beside the
  other data files (kept 30 days; not-found results 1 day). Obscure or Nordic shows may not be
  found; those rows just show `S01E02`. Pressing Refresh (F5) also retries shows whose lookup
  failed or found nothing.
- Movies and episodes resume where you stopped (closing the app, stopping, or picking
  something else). Positions are saved every 15 s in `positions.json`. Anything watched to
  within the last minute (or 95 %) counts as finished and starts over, as does anything
  stopped in the first 15 s. Live channels are never remembered.
- Movies/Series: results update as you type. The **Groups** menu hides whole groups
  from the results.
- The status overlay shows what is happening while a stream is not playing:
  - "Buffering…"
  - "Reconnecting… (attempt N)" for network drops (retries after 0.5 s, 1 s, 2 s,
    then every 3 s).
  - "Account rejected or too many connections" for HTTP 401/403/458/459 and
    connection-limit replies (retries after 3 s, 6 s, 12 s, then every 15 s).
  - "Channel unavailable (off-air?)" for 404/407/410, which is typical for event
    channels between events (retries after 3 s, 6 s, then every 10 s).

  It never gives up on its own.
- A refresh that would shrink the playlist to less than half its cached size is
  refused. A "Force refresh" button overrides that.

## Developer flags

- The release build is a windowed app and does not open a console window. If you start
  it from a terminal, its status and timing lines still go to that terminal. The prompt
  comes back right away and the lines appear after it. Debug builds keep a console.
- `--console` attaches the release build to the terminal it was started from. It also
  saves the frames that the "Next episode" credits check looked at, as PNG files under
  `scan-frames/<episode>-<time>/` in the data directory (named `NN_<seconds>s_<L>.png`,
  L = C credits, B black, K card such as an end logo, X content), and prints that folder. Useful for tuning the check
  on real shows; delete the folder when done.
- `--playlist-file <path.m3u>` parses a local M3U file instead of downloading
  `playlist_url` (no `config.toml` needed). F5 reads the file again.
- Startup timings and slow frames (> 25 ms) are printed to stderr.
- Favorites are keyed by the provider's numeric stream id. Channels without one (for
  example third-party `.../master.m3u8` links) are keyed by name and group instead, so
  renaming such a channel in the playlist drops its favorite.
- `cargo test --workspace` runs the unit tests. With `IPTV_SAMPLE=<path to a real
  .m3u>`, `cargo test --release --workspace -- --ignored --nocapture` runs the parse,
  library-build, cache and search performance tests.
- `cargo run -p iptv-player --example retry_demo` runs a headless end-to-end retry check
  against local ffmpeg/HTTP servers (needs `ffmpeg` on PATH and ports 8097-8099).
  `retry_demo --url-from-playlist <file.m3u> <group-title> <index> <secs>` plays one
  real entry headlessly. It prints only masked log lines and the status timeline.

## Known limitations

- Search folds case and diacritics. It does not unify Arabic spelling variants
  (alef forms, tashkeel), and results are not ranked by relevance.
- mpv opens http(s) through its own libcurl backend, so the `stream-lavf-o` reconnect
  options do nothing. The app's retry supervisor does the reconnecting.
- After a quick channel switch, the first retry of the new channel can come sooner than
  the backoff says. Log lines that arrive within 500 ms of the switch are ignored.
- Video is always decoded in software: hardware decoding showed corrupt video (black
  dashes) on the developer's graphics card. 4K streams get cheaper scaling and decoder
  settings so they keep up.
- Next episode: credits are recognised from small preview frames (bright text lines on a
  dark plain background). Credits over moving pictures are not recognised, so such
  episodes offer the next one only at the very end; unusual title cards on black near the
  end could be taken for credits. Not skipping content after the credits is best effort:
  frames are checked every 10 s, plain logo cards (dark or white) count as neither content
  nor credits, and a single frame that looks like content between credits is treated as noise, so a scene after the credits shorter than
  about 10 s (or in the last 3 s) can be missed, and frames come from the nearest keyframe, so the offer
  can appear a few seconds early. The offer always counts down 8 s and can be cancelled.
  The extra connection used for the check is closed when it is done (hovering the seek
  bar opens it again).
- Credentials: URLs are redacted in every log line and error message. The playlist
  cache contains the raw URLs, so keep the data directory private.
