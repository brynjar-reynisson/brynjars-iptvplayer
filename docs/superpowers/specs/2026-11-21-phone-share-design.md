# Phone share (watch the playing live channel in a phone browser)

Status: implemented (2026-11-21), branch `feature/phone-share`; plan: `docs/superpowers/plans/2026-11-21-phone-share.md`.

## Intent
The user wants to leave the computer while an important live game plays and keep watching it on a phone, in the
phone's browser, through a name they route themselves (`iptv.example.org` -> `localhost:<port>`). The PC keeps
playing in the player; the phone shows the same channel. A delay of 6-10 s is fine. Used rarely (one match), by the
user alone.

## Goals
- A "Share" switch in the player. While on, a small web server on this machine serves a page that plays the live
  channel that is playing now.
- Works in a normal phone browser with no app installed (iOS Safari natively; Android/Chrome via a bundled
  hls.js).
- No second connection to the provider (measured 2026-10-23: a second connection makes the provider cut older ones
  every 5-10 s). The phone gets the SAME bytes the player already receives.
- Safe to expose through a reverse proxy/tunnel: nothing but the stream and one page, behind a secret token.
- Leaves every other service on the machine alone.

## Non-goals
- Controlling the player from the phone (no channel switching, pause or seek). Viewing only.
- Movies/series/recordings. Live channels only (first version).
- Transcoding (no re-encode): a channel whose codec the phone cannot decode just does not play. A transcoding
  fallback is a later idea.
- Running while the app is closed or the PC sleeps. The app is the source; sharing blocks sleep while on.
- HTTPS or DNS inside the app: the user's proxy/tunnel terminates TLS.

## Anti-goals (a failure even if it technically works)
- A way into the playlist, its URLs or credentials from outside.
- Opening a second provider connection or disturbing the playing stream (stutter, reconnects, a lost recording).
- Listening on anything but loopback, or on a port another service uses.

## Constraints
- One provider connection per account; the recorder's `stream-record` (mpv writes the playing connection's bytes to
  a file, no extra connection) is the tap.
- ffmpeg is already a soft dependency (recordings, export); sharing needs it and says so when it is missing.
- No new heavy dependencies: a small blocking HTTP server on `std::net` (like the hand-written parsers elsewhere), or
  one tiny crate if clearly simpler.
- Port: default `8480` (checked 2026-11-21: free; the project's other dev ports are 8091, 8097-8099, 8188-8191).
  Configurable (`share_port` in `config.toml`). Bind `127.0.0.1` only.

## Approaches considered
1. **stream-record tap + ffmpeg HLS (chosen).** mpv records the playing connection to a file/pipe; one `ffmpeg -c copy`
   turns it into short HLS segments in a temp folder; the embedded server serves the playlist, segments and a player
   page. Cheap on CPU, no extra connection, 6-10 s delay. Trade-off: the tap file must not grow without bound (a
   3 h game at ~6 Mbps is ~8 GB).
2. **App-level tee proxy.** The app downloads the provider stream itself and feeds both mpv (loopback URL) and the
   web side. Cleanest data path but rewrites playback and the retry/supervisor logic: rejected as far too invasive.
3. **Transcode everything to H.264/AAC.** Plays on any phone, but heavy CPU for 4K/HEVC and competes with playback:
   kept only as a possible later fallback.

## Design (what is decided)
- **Switch:** a "Share" button on the live video (next to record, shapes only) and a key (proposal: `S`), off at
  every start. On = starts the tap, ffmpeg and the server; Off, Esc, channel change to a non-live item, or app exit
  stops them and deletes the temp segments. A channel switch while on restarts the pipeline for the new channel; the
  page reconnects by itself.
- **Link:** when switched on, a popup/status line shows the link `https://<share_host>/<token>/` (`share_host` in
  `config.toml`, default none: then it shows `http://localhost:<port>/<token>/` and says to set it) and the link is
  copied to the clipboard. The token is 128-bit random, created on first use, kept in `settings.json`; a "new link"
  action regenerates it.
- **Server:** loopback only. Routes: `/<token>/` (page), `/<token>/live.m3u8`, `/<token>/seg-N.ts`, `/<token>/status`
  (JSON: channel name, playing/offline). Anything else = 404 with no body detail; wrong token = 404 (not 403). Rate/size
  limits and a small cap on simultaneous requests. Never logs the token or any provider URL.
- **Page:** one self-contained HTML page with the vendored hls.js (Apache-2.0, noted in the README), native HLS on
  iOS Safari. Shows the channel name, a "waiting for the stream" message when the PC is reconnecting, and
  reconnects automatically. Phone-width layout, fullscreen button.
- **Pipeline:** ffmpeg reads the tap and writes `-c copy` HLS (2 s segments, rolling window of ~10, old segments
  deleted) into `<data dir>/share/`. The tap must be bounded: first step of the build is a short spike to test
  whether mpv can write `stream-record` to a Windows named pipe / local socket; if not, rotate the tap file every
  ~10 min (new connection-free file, ffmpeg continues) and delete old ones.
- **Sleep:** `KeepAwake` while sharing (reuse the recorder's).
- **Status in the app:** a "SHARING" badge with the number of recent viewers (requests in the last 15 s).
- **Docs:** README section, help card entry (`state/help.rs`), `AGENTS.md`, `docs/follow-ups.md` (manual checks:
  real provider, phone on mobile data, HEVC channel).

## Left to the builder
Exact HTTP crate vs hand-written; segment length/window; how the tap spike turns out; page styling; handling of a
manual recording running at the same time (it uses `stream-record` too: only one tap can be active, so starting one
while the other runs must be refused with a clear message, or the recording file be the shared tap); tests (pure
parts: token/route handling, segment window, status JSON; a headless demo like `rec_job_demo` with local ffmpeg).

## Open points / my calls (please check)
- Port 8480 and loopback-only binding.
- Viewing only, live only, no transcoding in version 1.
- Key `S`, token in `settings.json`, link copied to the clipboard.
- Recording and sharing cannot both use `stream-record`: needs a decision once the spike shows what is possible.
