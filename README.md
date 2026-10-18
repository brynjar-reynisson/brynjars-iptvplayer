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
5. **Run**: `target\release\iptv-player.exe`, or `cargo run --release -p iptv-player`.

### Release folder

`powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1 [-Out <dir>]` builds
the release binary and writes a self-contained folder (default `dist\iptv-player\`, which
is gitignored) with `iptv-player.exe`, `libmpv-2.dll` and `config.example.toml`. Copy the
example to `config.toml` there and the folder runs on its own. The exe and the DLL are the
only files needed at runtime.

### Data directory

`config.toml`, `cache/` (parsed playlist), `favorites.json` and `settings.json` (hidden
groups) live in the data directory. The app looks for it in this order:
`--data-dir <path>`, then the `IPTV_DATA_DIR` environment variable, then the exe's folder.

## Using it

- Tabs: **Live** (countries, then channels), **Favorites**, **Movies**, **Series**
  (show, then seasons, then episodes).
- Click or press **Enter** to play the selected entry. **Enter** or double-click on the
  playing row opens the full player view. Double-clicking the video toggles it.
- **Up/Down** move the selection. While live TV plays, they also switch channel after a
  short pause (300 ms), so holding the key opens only one stream.
- **F** toggles a favorite. **Space** pauses. **Esc** leaves the filter box, or stops
  playback and leaves the player view. **F5** refreshes the playlist.
- **Ctrl+F** or **/** focuses the channel filter (on Movies/Series, the search box).
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
- Hardware decoding (`hwdec=auto-safe`) has been tested with the bundled test streams
  only.
- Credentials: URLs are redacted in every log line and error message. The playlist
  cache contains the raw URLs, so keep the data directory private.
