# Skip intro: design

## Intent
Episodes of a show repeat the same title sequence. A "Skip intro" button, like "Next episode", jumps over it. The intro is found by comparing the opening of the playing episode with another episode of the same show, not by classifying frames (the intro is ordinary pictures and music, so the credits classifier cannot see it).

## Evidence (2026-10-22, The Man in the High Castle S02 E04/E05, `head_frames` example)
- 0-10 s black / studio card, intro 11-88 s (title card 80-88 s), then a near-black card with tiny "written by" text (5 s in E04, ~10 s in E05), then the first scene (94 s / ~99 s).
- Capture takes ~0.65 s per frame on the headless connection (keyframe seeks).
- A 16x9 grayscale fingerprint of the same intro frame in two episodes differs by 0; outside the intro by 18-70; shifting the comparison by 3 s drops matches from 94 to ~39. The match is sharp and finds the offset.

## Goals
- A "Skip intro" button at the video's bottom right, shown while the position is inside the intro; Enter (player view, same rules as the Next-episode offer) or a click seeks to the skip target. Esc/Cancel dismisses it for this episode.
- Works for any show with a repeating intro, no per-show tuning, no audio analysis.
- Costs extra provider connections (a pool of up to 4) only while scanning; silent when anything fails.

## Non-goals
- Auto-skip (a `settings.json` option later), intros that differ per episode, cold-open intros later than ~4 min, movies, live, `(Unsorted)` episodes.
- No Shift+S or other new key: Enter is enough.

## How it works
1. **Scan** (its own worker `intro_scan::IntroScanner`, separate from the preview/tail-scan worker; each job runs on a pool of up to 4 headless `ThumbMpv` sources in parallel, one connection each, pulling times from a shared queue, because a frame costs ~0.7 s of seek latency per connection: 61 frames in ~11.5 s): fingerprint (16x9 gray) every `INTRO_STEP_SECS` = 4 s over the first `INTRO_SCAN_SECS` = 240 s of the episode. Near-black/flat frames are ignored by the matcher (they match anything dark).
2. **Reference** per show, cached in `intros.json` (data dir, atomic, quarantine rules as `settings.json`, max 200 shows): only the reference's fingerprints inside its intro span (from `find_intro_pair`) plus 8 s on both sides. Storing the whole 0-240 s scan made that episode, when played later, match itself far beyond its intro. First time for a show: scan a neighbouring episode (next, else previous in the show) the same way and match the two scans (my call). Later episodes only need their own scan, matched against the stored reference.
3. **Match** (pure, unit-tested `iptv::intro`): for each offset, the longest run of consecutive matching non-dark frames; the best run of at least `MIN_INTRO_SECS` (20 s, my call) is the intro `[start, end]` in the playing episode.
4. **End refinement and skip target:** 1 s frames around `end`; the skip target is `end` extended through Black/Card/Credits-classified frames (the dark text card) to the first Content frame, capped at +20 s (my call).
5. **UI:** `state::skip_intro` (pure state machine like `autoplay`: Waiting -> Offering when position is in `[start, end)` -> Done/Dismissed). Position past `end` or a resume beyond it never offers it. `--console` saves every intro scan frame (no separate capture); the `intro_check` example scans two episodes headlessly and prints the decision (`head_frames` only saves frames).

## Failure behaviour
No match, scan failure, refused second connection, short video: no button, one line on stderr (no URL). Never blocks the Next-episode scan (separate worker and connections).

## Left to the builder
File layout, fingerprint thresholds (tune with the two real episodes and a third show), button placement details.
