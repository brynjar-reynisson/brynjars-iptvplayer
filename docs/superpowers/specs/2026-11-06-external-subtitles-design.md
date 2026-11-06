# External subtitles: import `.srt` files and keep them in sync

Date: 2026-11-06. Status: built (2026-11-06). Not yet verified in the real app or on the real provider; the open items are listed in `docs/follow-ups.md` ("External subtitles, manual checks still to do").

## Intent and why

The provider's movies and episodes carry subtitles only in a few languages
(for example Danish, Norwegian and Swedish, no English). The user has English
`.srt` files from other sources and wants to watch **streams in the app** with
them, without exporting each video to an MKV and embedding the file (the
`add-subtitles` skill does that for exported files; for a 30-episode show it
means 30 large files). So the app must load external subtitle files itself,
let the user choose them like any other subtitle track, and line them up with
the picture, because files made for another release are often a fixed number of
seconds off (11.9 s for one movie, up to 0.6 s for a whole show).

## Goals

1. An **Import subtitles...** button for the open show (Series tab) or the
   selected movie (Movies tab). It imports a **folder** of `.srt` files or a
   **single file**.
2. Imported files appear as extra tracks in the existing CC menu, are chosen
   and remembered like embedded tracks, and survive restarts, refreshes and
   reconnects.
3. **Automatic sync**, applied silently with a small notice: while the video
   plays, compare the start times of the lines of an embedded subtitle track
   (the reference) with the imported file and set a delay.
4. **Manual nudge** keys to correct or undo it, saved per file.

## Non-goals and anti-goals

- No subtitle search or download inside the app (finding files stays the
  user's job; the project skill covers exporting plus embedding).
- Formats other than `.srt` (UTF-8, UTF-8 with BOM, or a legacy 8-bit code page)
  are out of scope. No styling.
- No second connection to the provider for syncing: the provider cuts older
  connections when a new one is opened. The sync must run on the connection
  the player already has. No dependency on ffmpeg for this feature.
- Must never make playback worse: a wrong or unsure sync result is never
  applied silently; the user is told instead.
- No credentials or provider URLs in logs, files or messages (the existing
  rule).

## What the user sees

- **Series tab, show open:** under the show's title a button "Import
  subtitles...". It opens a chooser for a folder or a file (see "Import" below).
  Episodes that have an imported file show a small mark (for example "+ en")
  after their name.
- **Movies tab:** the same button in the movie's detail header when a movie is
  selected. A single file attaches to the selected movie.
- **CC menu:** imported tracks are listed after the embedded ones, labelled with
  their language and an "(imported)" suffix; picking one is like picking an
  embedded track. A show-level choice (existing `SubChoice` memory) applies to
  later episodes by language, so choosing "English (imported)" once makes the
  next episodes start with it.
- **Notice:** a short line in the player (same place and style as other
  transient notices) such as "Subtitles shifted +0.4 s (automatic)". Also
  shown for the manual nudge ("Subtitles +0.5 s").
- **Keys (builder's choice, suggestion):** two keys that step the current
  imported track's delay by 0.1 s; they must not clash with existing bindings
  and must be added to the help card tables (`state/help.rs`) and README.

## Import

- **Folder:** every `.srt` in it (not recursive). For a show, a file belongs to
  the episode whose season/episode numbers (`S02E06`, `s02e06`, `2x06`) appear
  in its file name and which exists in the open show; the rest are listed in a
  short result message ("Imported 28 of 30; 2 not matched: ..."), nothing
  silently dropped.
- **Single file:** attaches to the playing or selected episode/movie, whatever
  its name says.
- **Language:** taken from the file name when it has a recognisable code
  (`.en.`, `.eng.`, `.is.`, ...); otherwise asked once for the batch (a small
  dialog with a language choice, default from the file name or English).
- **Storage (the builder's call, agreed in principle):** the app copies each
  file into `<data dir>/subtitles/` under a generated name and records, per
  playlist entry (favorite key), `{language, file, delay}` in an index file
  (for example `subtitles.json`, same atomic-write and quarantine rules as the
  other JSON files). Moving or deleting the original files later changes
  nothing. Re-importing the same episode and language replaces the entry.
- Removing an imported track: a way to remove one (a row action or a CC menu
  entry) is wanted; where exactly is the builder's choice.
- Encoding: strip a BOM, decode UTF-8, else Windows-1252; normalise to UTF-8
  when copying.

## Playing an imported track

mpv loads an external file with the `sub-add` command (language and title can
be given). A `loadfile` (new episode, reconnect, retry) drops it, so the
imported tracks are added again for each supervisor generation, in the place
where the wanted subtitle is applied today (`Player::update`, once per
generation when the status is Playing). The track list, `current_sub` and
`select_sub` must treat them like embedded tracks; the remembered choice
(`SubChoice`) must be able to name an imported track (language-based matching
already exists). `sub-delay` in mpv is a single global value: it must be set
whenever the selected track changes (imported track: its saved delay,
embedded track: 0), so one track's shift never leaks into another.

## Automatic sync

### Why this approach

Spike on 2026-11-06 (libmpv v0.41 build used by the app, headless, a test file
with two subtitle tracks): with `secondary-sid=<reference track>`,
`secondary-sub-visibility=no` and no video output, the property
`secondary-sub-start` reports the **exact start time of each line** of the
reference track as it plays (2.3, 6.3, 11.3, 16.8 s in the test), and
`secondary-sub-text` the line itself. It works while the track is not shown and
needs no extra connection. The line start is the cue's own time, so a poll
every 50 ms is precise enough. (Observe the property or poll each frame.)

Offline, on 30 real episodes with ffmpeg-extracted windows, matching the
**start times** of the lines (not their text, so the reference may be in any
language) found the offset with 74-92 % of the reference lines matching and
left 0.00 s after applying it. An 11.9 s offset on a movie was found the same
way by shared names.

### Behaviour

- Applies when an imported track is the one shown (or about to be) and the
  playing entry has at least one embedded subtitle track that can serve as the
  reference (prefer the one in the user's remembered language, else the first).
  The reference is selected as the secondary subtitle track, invisible.
- While the video plays, collect the reference line starts that were seen
  (position-aware: only lines actually passed through; seeks create gaps, that is
  fine). After about 25 collected lines (a couple of minutes of dialogue) and
  again later (about 60 lines) compute the offset.
- Offset search: shift `s` in -90..+90 s (step 0.05 s) maximising the number of
  reference line starts that have an imported-file line start within 0.5 s once
  shifted; refine by the median of the shifts with the top score. Same method
  as the measured offline run, in Rust, pure and unit-tested.
- **Why a minimum number of lines:** in the offline run, 3 of 90 two-and-a-half-minute
  windows held only 9-13 lines and each matched a wrong offset of +77 to +90 s by
  chance; every window with 20 or more lines agreed within about 0.5 s. Do not
  compute with fewer than about 25 lines.
- **Confidence:** apply only when the best score is at least about 1.5 times the
  best rival that is more than 3 s away, and at least 50 % of the reference
  lines match. Otherwise do nothing and show "Could not match the subtitles
  automatically; adjust with the keys".
- Apply as the track's delay (and store it with the entry); show the notice.
  Do nothing for shifts below 0.15 s. Never overwrite a delay the user set by
  hand for that file (a manual nudge wins and stops further automatic changes
  for it).
- Sync happens once per file and entry; the stored delay is reused on later
  plays. A re-import clears it.
- No reference available (a movie without embedded tracks): no automatic
  sync, the nudge keys only.

## Constraints and risks

- Until enough lines were seen (first minutes) the imported track runs
  unshifted: acceptable, said in the README.
- The reference's own timing is assumed right.
- Property availability: `secondary-sub-start` exists in the bundled libmpv; the
  startup check that already requires mpv >= 0.38 should also require this
  property (or the feature switches itself off with a log line).
- Secondary subtitle track must never be drawn (visibility off) and must be
  restored/cleared on stop or when sync is done.
- All new state is GUI-free and unit-tested in the existing style (`state/`),
  the egui layer stays thin; follow the repo's rules in `AGENTS.md` (line endings
  per file, new files added to git, docs and help tables updated, English only
  examples, imaginary names in docs).

## Left to the builder

- Exact widget layout and wording, key choice, the result dialog.
- File names/structure of the index file and the generated copies.
- Algorithm details beyond the above (window collection data structure, when to
  recompute, tolerances), provided the confidence rules hold.
- Whether the existing Export (movie to MKV) later embeds the imported track
  (not part of this work).
