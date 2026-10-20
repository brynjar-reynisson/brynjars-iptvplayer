# Autoplay next episode (design)

## Intent
When a series episode ends, offer "Next episode" Netflix-style: a button with a countdown over the video that starts the next episode by itself unless cancelled. The offer must never skip extra content after the credits (stingers, post-credit scenes), and this must work for any show without a hand-kept list.

## Goals
- Offer appears during the closing credits, at the start of the final credits stretch.
- If real content follows the credits, the offer waits until that content has played (at the real end of the file).
- Last episode of a season continues into the first episode of the next season of the same show, when it exists.
- Auto-play on by default; switch in `settings.json`.

## Non-goals / anti-goals
- No curated list of shows, no web lookup of credit timestamps.
- No real OCR engine (Windows OCR / Tesseract). Never skip content: when unsure, offer only at the real end of the file.
- Movies, live channels and `(Unsorted)` episodes get no offer (finishing behaves as today: stop, back to the list).

## Decided
- **Look ahead, not react.** The second headless mpv (`thumb_mpv` / `thumbs::ThumbService`, same cost as seek previews) samples the tail of the file: the last 8 minutes (my call), one frame per `BUCKET_SECS` (10 s), lazily once playback reaches that tail's start. Frames are the existing 240 px previews.
- **Classifier** (pure function on RGBA, unit-tested with synthetic images): a frame is *credits-like* if it is mostly dark or plain (low luminance variance outside text) with several horizontal rows of small bright high-contrast glyph runs (text layout), or *black* (near-uniform dark; neutral), or a *card* (plain flat background of any luminance with a small logo/text on it, e.g. production-company end logos on dark or white; neutral exactly like black; added after the first real-episode frames). Anything else is *content*. Language independent; no OCR. Thresholds are the builder's to tune on real episodes.
- **Decision (pure):** find the earliest sampled time T such that every sample from T to the end is credits-like or black and at least one is credits-like. Offer at T. If the final samples are content, offer only at the real end (`Status::Finished`). Black gaps between credits and a stinger never count as content, but the stinger then moves T after it.
- **Fallbacks (never skip):** second connection refused, frame failures, unknown duration, or an incomplete scan when playback reaches the tail = offer at the real end only.
- **Offer UI:** "Next episode" button with a countdown of 8 s (my call) over the video, bottom right, drawn with shapes (no symbol fonts). Enter or click = play now. Esc or a Cancel button = dismiss and keep playing (no offer again for this episode); the prompt is also dismissed by the user stopping/starting something else. Countdown at 0 starts the next episode. If the file ends while the offer is showing, the next episode starts right away.
- **Next episode:** the next `EpisodeRow::Episode` after the playing one in `vod::episode_rows(lib, show, hidden)` (respects hidden groups, crosses season headers, so last of season N -> first of season N+1). No next row or `(Unsorted)` show = no offer. Resolution happens on the UI thread from the library (cheap).
- **Starting it:** goes through `App::start(Playback)` (resume rules, subtitle preference per show, position saving, `previews.reset()` all apply). The new `Playback` keeps `tab = Series`; the open show's cursor moves to the new episode (`OpenShow::select_entry`) and the list scrolls to it.
- **Setting:** `Settings.autoplay_next: Option<bool>` (absent = true), optional field so the version stays 1.
- On `Status::Finished` with a next episode and autoplay on, `on_finished` starts the next episode instead of stopping; the finished one is forgotten from `positions`.

## Left to the builder
Where the scan state lives (suggest `state::credits`: pure sampling plan + decision; `ThumbService` extension for a "tail scan" request returning frames beyond the 300-texture UI cache, since these frames need no textures), classifier thresholds, exact button placement, countdown ticking via repaint requests, docs (AGENTS.md, README keys, follow-ups).

## Checks
Unit tests for classifier, decision, next-episode resolution (season crossing, hidden groups, last of show, unsorted), setting default. Manual: Wheel of Time S1 finale (stinger), a normal episode, an episode at a season end, second connection refused.
