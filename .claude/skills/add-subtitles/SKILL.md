---
name: add-subtitles
description: Use when the user wants to add an .srt subtitle file to a movie/episode .mkv (typically one exported from the player) as a new subtitle track in a given language, make it the default, or fix its timing. Handles track counting, language tagging, default flag, sync measurement against an embedded track, verification, and replacing the original only after the user agrees.
---

# Add an .srt to an MKV

Adds a subtitle file as a new track in an existing `.mkv` without re-encoding (video and sound are copied). Needs `ffmpeg` and `ffprobe` (the same ones the app's Export uses: on the PATH or beside `iptv-player.exe`). Matroska can not gain a track in place, so the file is rewritten to a temporary copy next to the original, checked, and only then swapped in.

Inputs to get from the user: the `.mkv`, the `.srt`, the language (an ISO 639-2 three-letter code such as `eng`, `isl`, `dan`, plus a display title such as `English`), and whether it should be the default track.

All examples below use an imaginary movie: `D:\Movies\Example Movie (1990).mkv`, `D:\Subs\Example.Movie.1990.en.srt`, language `eng`. Never write a user's real file names into tracked files or commit messages.

## Steps

1. **Look at the movie.**
   ```
   ffprobe -v error -show_entries stream=index,codec_type,codec_name:stream_tags=language,title:stream_disposition=default -of compact "Example Movie (1990).mkv"
   ```
   Count the existing subtitle streams: that count is `N`, the new track's index among subtitle streams (`s:N`). Note whether the language is already present (then ask if they want a second one) and which tracks are default. Only `.mkv`: for `.mp4` the subtitle codec would be `mov_text`, a different job; say so.
2. **Check the `.srt` encoding.** Most are UTF-8. If decoding as UTF-8 fails (Icelandic and other files are often Windows-1252 / ISO-8859-1), pass `-sub_charenc CP1252` instead of `UTF-8` below. Check a line with accented letters afterwards.
3. **Measure the timing** (skip only if the user says it is already in sync). A subtitle made for another cut is often a constant number of seconds off (a longer intro, for instance). If the movie already has a subtitle track, extract the first one to a scratch folder (not the repo, not the movie's folder) and compare:
   ```
   ffmpeg -v error -y -i "Example Movie (1990).mkv" -map 0:s:0 "<scratch>\ref.srt"
   python -I .claude\skills\add-subtitles\measure_offset.py "<scratch>\ref.srt" "D:\Subs\Example.Movie.1990.en.srt"
   ```
   It matches proper nouns shared by both files and prints the shift to apply (`-itsoffset`, negative = show earlier). Exit 0 = a constant offset with a clear peak: use it. Exit 2 (too few shared names, rivals as strong, first and second half differ) = do not guess: tell the user, and ask them to name a distinctive line and when it is spoken in the movie (compare with the `.srt` time), or give up the shift. A reference in another language is fine; a reference that is itself out of sync with the video is not detectable here, so say the result assumes the embedded track is right. Without any embedded track, ask for the one-line comparison.
4. **Build the temporary copy** next to the original (same drive; needs free space of about the movie's size plus 1 GiB, check first). Example: one existing subtitle track would make `N` 1; default flag wanted; shift `-11.9`:
   ```
   ffmpeg -v error -y -i "Example Movie (1990).mkv" -sub_charenc UTF-8 -itsoffset -11.9 -i "Example.Movie.1990.en.srt" ^
     -map 0 -map 1:0 -c copy ^
     -metadata:s:s:N language=eng -metadata:s:s:N title=English ^
     -disposition:s:0 0 -disposition:s:N default ^
     "Example Movie (1990).with-en.tmp.mkv"
   ```
   - `-map 0` keeps everything (all audio, subtitles, chapters, attachments); `-map 1:0` adds the `.srt`.
   - Leave out `-itsoffset` when no shift is needed. When making the new track the default, clear the flag on each existing subtitle track too (`-disposition:s:0 0 -disposition:s:1 0 ...` for every index below `N`); when not default, drop the `-disposition` options.
   - Do not add the `forced` flag unless asked: it shows the track always, even with subtitles switched off.
5. **Verify before touching the original.**
   - `ffprobe` the copy: the original streams are all there, one more subtitle stream with the right language/title, the intended track is the only default subtitle, and `format=duration` equals the original's.
   - Re-extract the new track (`-map 0:s:N`) and run `measure_offset.py` again against the reference: the shift should now be about 0.
   - Report the result and file sizes (about the original's size plus the `.srt`'s).
6. **Ask before replacing.** The swap is the only destructive step: delete the original, rename the copy to the original's name. Offer the choice to keep both until they have watched it. After the swap, `ffprobe` the final file again. Delete scratch files; never commit them.

## Notes

- Players differ: VLC, this app (mpv), Plex, Jellyfin and most TVs start with the default-flagged track; Windows Media Player ignores the flag (subtitles stay off until picked, tracks listed in file order). Putting the new track first (`-map 1:0` before `-map 0:s`, with `-map 0:v -map 0:a` first) can help such players; not guaranteed.
- A player that has remembered a subtitle choice for the show (this app does, for episodes) uses that choice, not the flag.
- If ffmpeg reports an error about the subtitle codec, the container is probably not Matroska or the `.srt` is malformed; show the user the last lines of ffmpeg's message.
- This skill is the whole tool: the app has no subtitle-embedding feature on purpose.
