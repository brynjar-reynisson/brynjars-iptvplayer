# Phone Share Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A "Share" switch in the player that serves the live channel that is playing to a phone browser, through a loopback web server the user routes to `iptv.breynisson.org` themselves.

**Architecture:** mpv's `stream-record` property is pointed at a local `tcp://127.0.0.1:<tap port>/tap.ts` URL (no second provider connection). One `ffmpeg -c copy` per tap listens there and writes rolling HLS segments into `<data dir>/share/`. A small `std::net` HTTP server (loopback, token in the path) serves a self-contained page (vendored hls.js), the playlist, the segments and a status JSON. A pure state machine (`Sharer`) decides when to start the pipe and the tap; `App` wires it to the Share button / key `S`.

**Tech Stack:** Rust (existing workspace), libmpv, ffmpeg (already a soft dependency), hls.js 1.5.17 (Apache-2.0, vendored file).

**Spec:** `docs/superpowers/specs/2026-11-21-phone-share-design.md`

**Spike result (2026-11-21, done):** mpv's `stream-record` accepts a `tcp://127.0.0.1:8482/tap.ts` URL; `ffmpeg -f mpegts -i "tcp://127.0.0.1:8482?listen=1" -c copy -f hls -hls_time 2 -hls_list_size 6 -hls_flags delete_segments+omit_endlist <dir>/live.m3u8` produced a valid rolling playlist (segments decode with no errors, h264+aac). So the tap needs no growing file and no file rotation. `MpvBackend::load` already clears `stream-record`, so every retry/channel change ends the tap and ffmpeg sees EOF.

## Global Constraints

- Bind `127.0.0.1` only; default port `8480` (`share_port` in `config.toml`, valid 1024-65535); tap port = `share_port + 1`, loopback only.
- Viewing only, live channels only, no transcoding (`-c copy`).
- Token: 128-bit random, 32 lowercase hex characters, stored in `settings.json` (`share_token`, optional field, version stays 1). Wrong token or unknown path = `404` with an empty body. Never log or print the token or any provider URL.
- No second provider connection. No new crates (std::net server, `extern "system"` for Windows calls, like the rest of the project).
- Recording and sharing both use `stream-record`: refuse the second one while the first runs, with a clear message.
- Sharing blocks sleep while on (`SetThreadExecutionState`, as the recorder does). Off at every app start.
- ffmpeg missing = a clear message and sharing stays off (use `iptv::rec_merge::find_ffmpeg`).
- Keep each file's line endings (`.gitattributes`; `ui/mod.rs` is LF-with-UTF-8: edit with the Edit tool, never `sed -i` on CRLF files, and when scripting use `newline=''`). Add new files to git. Update `AGENTS.md`, README, `state/help.rs` and `docs/follow-ups.md` (Task 7).

## Review Focus

- A channel switch (or reconnect) while sharing: the phone's player keeps going or reconnects by itself, no stale segments/playlist, one ffmpeg at a time (Task 4 demo step, Task 5 tests).
- A phone connecting when nothing plays / the PC is reconnecting: the page says "waiting", `/status` reports `live:false`, no crash, no 5xx (Task 3 tests).
- ffmpeg missing, dying mid-game, or the tap never connecting: message, automatic restart, the player's own stream undisturbed (Task 4, Task 5 tests).
- The port is already taken when sharing is switched on: a clear message, sharing stays off, no panic (Task 3 test).
- Hostile or sloppy requests (`..`, `%2e%2e`, backslashes, absurdly long request line, headers never finished, POST): `404`/close within seconds, no file outside `share/` readable (Task 2 and 3 tests).
- App exit / Esc: no ffmpeg process, listener or `seg-*.ts` left behind (Task 6, Task 7 manual check).

---

### Task 1: Config keys and settings token

**Files:**
- Modify: `crates/iptv/src/config.rs` (struct `Config`, `load`, tests, manual `Debug`), `config.example.toml` (commented examples), `crates/iptv/src/settings.rs`

**Interfaces:**
- Produces: `pub const DEFAULT_SHARE_PORT: u16 = 8480;` `Config.share_port: u16`, `Config.share_host: Option<String>` (trimmed host only: letters, digits, `.`, `-`, optionally `:port`; no scheme, no `/`, no whitespace); `Settings::share_token(&self) -> Option<&str>`, `Settings::set_share_token(&mut self, token: &str)` (marks dirty; saved by the existing `save_if_dirty`).

- [ ] **Step 1: Write failing tests** in `config.rs`: `share_port_defaults_and_validates` (missing = 8480, `share_port = 9000` = 9000, `80`, `70000`, `"x"` are errors naming the file and key), `share_host_trims_and_rejects_scheme` (`" iptv.breynisson.org "` -> `Some("iptv.breynisson.org")`, `"https://x.org"` and `"a b"` rejected, blank = `None`). In `settings.rs`: `share_token_round_trips_and_keeps_version_1`.
- [ ] **Step 2:** Run `cargo test -p iptv config settings` - the new tests fail to compile/fail.
- [ ] **Step 3:** Implement. `load` reads the optional keys like `user_agent` does (`parse_share_port`, `parse_share_host`); update the struct literal in the existing config tests; the manual `Debug` for `Config` prints them (host is not a secret). Settings: optional `share_token` field next to `export_dir`, same parse/save paths.
- [ ] **Step 4:** Add commented `# share_port = 8480` and `# share_host = "iptv.example.org"` lines with a one-line explanation to `config.example.toml` (it is also the first-run template: run the whole `config` test module).
- [ ] **Step 5:** `cargo test -p iptv` passes. Commit `feat: share_port, share_host and share_token settings`.

### Task 2: `iptv::share` (pure: token, routes, link, status, viewers, ffmpeg args)

**Files:**
- Create: `crates/iptv/src/share.rs`; Modify: `crates/iptv/src/lib.rs` (`pub mod share;`)

**Interfaces:**
- Produces:
  - `pub fn new_token() -> anyhow::Result<String>` (16 bytes from `BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG` (2) via `extern "system"`, linked to `bcrypt`; hex), `pub fn is_valid_token(s: &str) -> bool` (exactly 32 lowercase hex).
  - `pub enum Route { Page, HlsJs, Playlist, Segment(String), Status, NotFound }` (`Debug, PartialEq`), `pub fn route(method: &str, target: &str, token: &str) -> Route`: GET and HEAD only; the target's query/fragment is ignored; paths are `/<token>/`, `/<token>/hls.js`, `/<token>/live.m3u8`, `/<token>/status`, `/<token>/seg-<digits>.ts` (1-9 digits); the token compare is constant-time; anything else, including `..`, `%` escapes, `\`, empty segments, other methods = `NotFound`.
  - `pub fn link(host: Option<&str>, port: u16, token: &str) -> String` (`https://<host>/<token>/`, or `http://localhost:<port>/<token>/` without a host).
  - `pub fn status_json(channel: &str, live: bool) -> String` (`{"channel":"...","live":true}`, JSON-escaped, channel cut at 100 chars).
  - `pub struct Viewers` with `record(&mut self, who: &str, now: Instant)`, `count(&mut self, now: Instant) -> usize` (distinct `who` seen within `VIEWER_WINDOW` = 15 s, expired ones dropped, at most 64 kept).
  - `pub const SEG_SECS: u32 = 2; pub const LIST_SIZE: u32 = 8;` `pub fn ffmpeg_args(tap_port: u16, dir: &Path, restart: bool) -> Vec<String>`: `-hide_banner -loglevel warning -f mpegts -i tcp://127.0.0.1:<tap_port>?listen=1&listen_timeout=30000 -c copy -f hls -hls_time 2 -hls_list_size 8 -hls_segment_filename <dir>/seg-%d.ts -hls_flags delete_segments+omit_endlist` plus `+append_list+discont_start` when `restart` (continues the playlist after a channel change; the demo in Task 7 proves it), output `<dir>/live.m3u8`. `pub fn tap_url(tap_port: u16) -> String` = `tcp://127.0.0.1:<port>/tap.ts`.

- [ ] **Step 1: Write failing tests** for each function; `route` tests include `/<token>/seg-12.ts` -> `Segment("seg-12.ts")`, `/<token>/seg-../x`, `/<token>/%2e%2e/`, `/<token>/seg-1.ts/`, `/<token>/seg-1234567890.ts`, wrong token, missing token, `POST`, `//`, backslash, a 10 000-char target: all `NotFound`; `HEAD /<token>/` = `Page`. `viewers_count_distinct_within_window`; `status_json_escapes_quotes_and_controls`; `ffmpeg_args_first_and_restart`.
- [ ] **Step 2:** `cargo test -p iptv share` fails (module missing).
- [ ] **Step 3:** Implement in `share.rs`.
- [ ] **Step 4:** Tests pass. Commit `feat: iptv::share (routes, token, link, ffmpeg args)`.

### Task 3: `iptv::share_server` (HTTP server) and the page

**Files:**
- Create: `crates/iptv/src/share_server.rs`, `crates/iptv/assets/share.html`, `crates/iptv/assets/hls.min.js`, `crates/iptv/assets/HLSJS-LICENSE.txt`; Modify: `crates/iptv/src/lib.rs`

**Interfaces:**
- Consumes: `share::{route, Route, status_json, Viewers}`.
- Produces: `pub struct Server` with `pub fn start(port: u16, token: String, dir: PathBuf) -> anyhow::Result<Server>` (binds `127.0.0.1:port`; a taken port = `Err` whose message says the port is in use), `pub fn set_status(&self, channel: &str, live: bool)`, `pub fn viewers(&self) -> usize`, `pub fn stop(self)` (also on `Drop`; joins the accept thread within 2 s). Behaviour: one thread per connection, at most 32 at a time (more get an immediate close), read timeout 10 s, write timeout 30 s, request line + headers capped at 8 KiB, GET/HEAD only, no keep-alive (`Connection: close`). Responses: page (`text/html; charset=utf-8`), hls.js (`application/javascript`), playlist (`application/vnd.apple.mpegurl`, read from `<dir>/live.m3u8`; missing file = `404`), segments (`video/mp2t`, read from `<dir>/<name>`, name already validated by `route`), status JSON. All with `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`; 404 has an empty body. The viewer id is the first `X-Forwarded-For` entry, else the peer IP; recorded on playlist requests only.
- The page (`share.html`, self-contained, phone-width, black theme): channel name from `/status`, a `<video playsinline controls>`; native HLS where `canPlayType('application/vnd.apple.mpegurl')` is true, else hls.js (`hls.js` route, relative URLs so the token path works behind the proxy) with `liveSyncDurationCount: 3`; a "Tap to play" overlay (autoplay with sound is blocked), "Waiting for the stream..." when `/status` says `live:false` or the player errors, automatic retry every 3 s, a fullscreen button. No external requests. `hls.min.js` = hls.js 1.5.17 `dist/hls.min.js` downloaded once with curl from `https://cdn.jsdelivr.net/npm/hls.js@1.5.17/dist/hls.min.js` (untrusted download: save to a scratch folder first, check it is JavaScript of a few hundred KB, record its SHA-256 in the commit message), plus the Apache-2.0 text.

- [ ] **Step 1: Write failing tests** (real sockets on port 0 is not supported by `start(port)`, so pick a free port by binding and dropping a `TcpListener` in the test): `serves_page_playlist_segment_and_status` (200s, content types, `Cache-Control`), `wrong_token_and_unknown_path_are_empty_404s`, `traversal_attempts_are_404` (`GET /<token>/seg-1.ts/../../x`, raw `%2e%2e`), `missing_playlist_is_404_not_5xx`, `post_is_404`, `oversized_request_line_is_closed`, `slow_client_is_dropped` (connects, sends nothing; closed within ~12 s: use a shortened timeout constant under `cfg(test)`), `port_in_use_is_a_clear_error`, `viewers_count_from_forwarded_for`, `stop_releases_the_port`.
- [ ] **Step 2:** Run `cargo test -p iptv share_server`: fails.
- [ ] **Step 3:** Implement `Server` (accept loop with a stop flag; stop wakes it by connecting to itself).
- [ ] **Step 4:** Write `share.html`; add a test that the page contains no `http://`/`https://` string other than inside the `canPlayType` MIME text (no external requests).
- [ ] **Step 5:** Tests pass; commit `feat: iptv::share_server and the share page (hls.js 1.5.17 vendored)`.

### Task 4: `iptv::share_pipe` (ffmpeg process) and shared `KeepAwake`

**Files:**
- Create: `crates/iptv/src/share_pipe.rs`, `crates/iptv/src/awake.rs`; Modify: `crates/iptv/src/rec_job.rs` (use `awake::KeepAwake`, delete its private copy), `lib.rs`

**Interfaces:**
- Produces: `pub struct KeepAwake` (`KeepAwake::new()`, releases on `Drop`; moved unchanged from `rec_job.rs`); `pub struct Pipe` with `pub fn start(ffmpeg: &Path, tap_port: u16, dir: &Path, restart: bool) -> anyhow::Result<Pipe>` (creates `dir`, spawns `ffmpeg` with `share::ffmpeg_args`, `CREATE_NO_WINDOW`, stdin/stdout null, stderr to `<dir>/ffmpeg.log` truncated, never containing the URL), `pub fn alive(&mut self) -> bool`, `pub fn started_at(&self) -> Instant`, `pub fn stop(&mut self)` (kill + wait), `Drop` stops; `pub fn has_segments(dir: &Path) -> bool` (`live.m3u8` exists and lists at least one segment); `pub fn sweep(dir: &Path)` (deletes only `seg-*.ts`, `live.m3u8`, `ffmpeg.log` directly in `dir`); `pub const PIPE_WAIT: Duration` (800 ms: time for ffmpeg to start listening before mpv connects).

- [ ] **Step 1: Failing tests:** `sweep_removes_only_share_files` (a stray `keep.txt` and a subfolder survive), `has_segments_needs_a_listed_segment`, `pipe_start_fails_cleanly_for_a_missing_ffmpeg`, `pipe_stop_kills_the_child` (ignored-if-no-ffmpeg: uses `find_ffmpeg`; starts a pipe, asserts `alive()`, `stop()`, then `!alive()`).
- [ ] **Step 2:** Run them: fail. **Step 3:** Implement. **Step 4:** `cargo test --workspace` still passes (recorder tests unchanged). Commit `feat: iptv::share_pipe and shared KeepAwake`.

### Task 5: `state::share::Sharer` (pure state machine)

**Files:**
- Create: `crates/iptv-player/src/state/share.rs`; Modify: `crates/iptv-player/src/state/mod.rs`

**Interfaces:**
- Produces: `pub const TAP_TIMEOUT: Duration = 15 s` (no segment this long after the tap started = restart), `pub const RESTART_GAP: Duration = 2 s`; `pub struct ShareInput { pub live_playing: bool, pub generation: u32, pub pipe_alive: bool, pub has_segments: bool }`; `pub enum ShareStep { Nothing, StartPipe { restart: bool }, StartTap, StopPipe, Stop(String) }` (`Stop` = give up with a message: the playback is no longer a live channel); `pub struct Sharer` with `Sharer::new() `, `poll(&mut self, now: Instant, i: ShareInput) -> ShareStep`, `pub fn state_name(&self) -> &'static str` (`"waiting"|"starting"|"tapping"|"live"`), `pub fn is_live(&self) -> bool` (segments exist). Rules: while `live_playing` is false nothing starts (a reconnect pauses sharing, it does not end it); on a new `generation` or a dead pipe or `TAP_TIMEOUT` without segments: `StopPipe` then, after `RESTART_GAP`, `StartPipe { restart: true }` (the first pipe of a session is `restart: false`); `StartTap` is returned `PIPE_WAIT` after `StartPipe`; never two pipes at once.

- [ ] **Step 1: Write failing tests:** `first_pipe_is_not_a_restart`, `tap_starts_after_the_pipe_wait`, `generation_change_restarts_with_append`, `dead_pipe_restarts_after_the_gap`, `no_segments_for_15s_restarts`, `reconnect_pauses_without_stopping` (live_playing false for 60 s: `Nothing`, then playing again with the same generation resumes `StartTap` only if the pipe is still alive), `never_two_pipes` (property over a scripted input sequence: count of `StartPipe` minus `StopPipe` stays 0..=1), `is_live_follows_segments`.
- [ ] **Step 2:** `cargo test -p iptv-player share` fails. **Step 3:** Implement. **Step 4:** pass. Commit `feat: Sharer state machine`.

### Task 6: App integration (button, key S, link, badge, cleanup)

**Files:**
- Modify: `crates/iptv-player/src/ui/mod.rs` (struct `App`, `handle_keys` next to the R key block ~line 2845, `video_area` button slots near `toggle_rec`, `rec_badge` neighbour, `on_exit` ~line 4167, `start_with`/`stop` for cleanup), `crates/iptv-player/src/ui/player.rs` (`set_stream_record` already exists), `crates/iptv-player/src/state/help.rs`

**Interfaces:**
- Consumes: Tasks 1-5. Produces: `App::toggle_share(&mut self)`, `App::update_share(&mut self)` (every frame, next to `update_recording`), `App::stop_share(&mut self, msg: Option<String>)`, `fn share_badge(&self, ui: &egui::Ui, rect: egui::Rect)`.

- [ ] **Step 1: Behaviour (write as `state` tests where pure, else follow the Recording code):** `toggle_share` on: refuses with `self.message` when not a live channel playing ("Wait until the channel plays, then share."), when `self.recorder.is_some()` ("Stop the recording first: recording and sharing cannot run together."), when `rec_merge::find_ffmpeg()` is `None` ("Sharing needs ffmpeg."); reads `share_port`/`share_host` from `config::load` (defaults when the config is unreadable), ensures the token (`share::new_token`, `settings.set_share_token`, save), starts `Server::start` (error = message and stay off), creates `KeepAwake`, `Sharer::new()`, copies `share::link(..)` to the clipboard (`ctx.copy_text`) and shows it in `self.message` ("Sharing: <link> (link copied)"). `toggle_record` refuses while sharing ("Stop sharing first..."). Off: `stop_share` stops the pipe and the server, `set_stream_record(None)`, `pipe::sweep`, drops `KeepAwake`.
- [ ] **Step 2: `update_share` per frame:** build `ShareInput` (`live_playing` = playing live channel with `Status::Playing`, `generation` = `p.generation()`, `pipe_alive`, `has_segments`); `StartPipe{restart}` -> `Pipe::start(.., tap_port = share_port + 1, ..)` (failure = `stop_share` with the message); `StartTap` -> `p.set_stream_record(Some(Path::new(&share::tap_url(tap_port))))` (the method takes a `Path`; `to_str` keeps the URL intact); `StopPipe` -> stop the pipe and `set_stream_record(None)`; keep `server.set_status(channel name, sharer.is_live())`; sharing stops by itself (message "Sharing stopped.") when playback stops, is not live, or is replaced by non-live content. A channel change while sharing keeps sharing on (the supervisor's new generation drives the restart).
- [ ] **Step 3: UI:** a Share button drawn with shapes in `video_area` at slot `-152` (record is the CC slot; shift export/next-episode slots only if they are drawn for live: they are not, live shows record only), same style as the record button (outlined; filled white when on), hidden when it does not fit; `S` key in `handle_keys` for live playback (not while typing, sub menu closed) calls `toggle_share`; `share_badge` draws "SHARING  N viewers" (or "SHARING  waiting for the stream") under `rec_badge`'s position (top right, 12 pt lower than the REC badge). Esc (`stop`) and `on_exit` call `stop_share` before the process ends.
- [ ] **Step 4: Help card:** add a `S` row to the Live keys table in `state/help.rs` ("Share the playing channel with your phone (see README); Esc or switching to a non-live item stops it") and a `share_port`/`share_host` row to the Settings table; the existing help tests (`tab_keeps_focus_on_the_card` etc.) must pass.
- [ ] **Step 5:** `cargo test --workspace` passes; `cargo build --release -p iptv-player`. Commit `feat: Share button and S key for the playing live channel`.

### Task 7: Headless demo, docs, manual checks

**Files:**
- Create: `crates/iptv-player/examples/share_demo.rs`; Modify: `README.md`, `AGENTS.md`, `docs/follow-ups.md`, the spec (Status line: implemented)

- [ ] **Step 1: `share_demo` (like `rec_job_demo`, needs ffmpeg, uses ports 8483 source, 8484 share, 8485 tap, 127.0.0.1 only; prints `RESULT: ALL CHECKS OK`):** local ffmpeg MPEG-TS source (h264+aac) -> `MpvBackend` -> `Sharer` + `Pipe` + `Server` driven exactly like `App::update_share`; asserts: (1) within 20 s `GET /<token>/live.m3u8` is 200 and lists a segment; (2) that segment starts with TS sync byte `0x47` and `ffprobe` finds h264+aac; (3) wrong token = 404, `/..%2f` = 404; (4) **channel switch**: `load` a second local source, assert a new pipe started with `restart: true`, the playlist still ends with growing sequence numbers and contains `#EXT-X-DISCONTINUITY`, the old pipe is gone (one ffmpeg process); (5) kill the pipe's ffmpeg mid-run: `Sharer` restarts it and segments resume; (6) after `stop` the port is free, no `seg-*.ts` remain and no ffmpeg of ours is running.
- [ ] **Step 2:** Run `cargo run --release -p iptv-player --example share_demo`; fix until `RESULT: ALL CHECKS OK`.
- [ ] **Step 3: Docs.** README: a "Watching on your phone" section (what it does, the one-connection reason, set `share_host`, route the name to `localhost:8480`, TLS is the proxy's job, the link contains the secret: do not post it, "S" key, limits: live only, viewing only, H.264/AAC phones, needs ffmpeg, PC must stay on, upstream bandwidth) and an Acknowledgements/License line for hls.js (Apache-2.0). `AGENTS.md`: one bullet "Phone share" (modules, ports, token, Sharer rules, the spike finding, demo command) and `iptv-player [..]` unchanged. `docs/follow-ups.md`: manual checks still to do: real provider channel on the phone over mobile data (delay, stutter, recording/sharing exclusion), iOS Safari and Android Chrome, HEVC channel (expected: no picture), proxy `X-Forwarded-For` viewer count, closing the app while sharing (no ffmpeg left), a 3 h game (disk: none used beyond 8 segments).
- [ ] **Step 4:** `cargo test --workspace` and the demo pass. Commit `docs: phone share (README, AGENTS.md, follow-ups, demo)`; leave the branch unmerged for the user's manual test (project rule: they test the deployed build before merging); then run the project skill `.claude/skills/build-and-deploy/SKILL.md` and report.
