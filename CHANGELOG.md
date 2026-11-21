# Changelog

## 0.14.0

- **Match source quality** row under Target size: the size that keeps about the source's quality. It starts as an instant estimate from the source's video bitrate and how much more bitrate the chosen codec needs than the source's codec (AV1 → HEVC about 1.5×, H.264 → HEVC about 0.75×), and accounts for downscaling. *Check* measures it instead: it encodes the sample segments at up to five bitrates and scores them with VMAF, keeping the smallest size where the average is ≥ 95 and the worst segment ≥ 93. *Use* sets the target to the measured size if there is one, otherwise the estimate.
- `encoder --match FILE` runs the same estimate and search from the terminal, and prints the measured bitrate multiplier so the estimates can be calibrated.

## 0.13.2

- The "won't save space" warning now has fixes: *Copy (remux only)*, and in target-size mode a target about ¾ of the source. This happens with efficient sources, e.g. a 2.2 GB AV1 file and the default 2.5 GB target.

## 0.13.1

- Progress no longer sits at 0% on files with many subtitle tracks. ffmpeg holds its time counter back until every subtitle track has had a cue, so progress, ETA and speed now come from the video frame count.

## 0.13.0

- New **Output file** row at the top of each queued job shows exactly where the result will be written. *Save As…* picks another name or folder for that job only; the undo button goes back to the output folder and suffix from Preferences. The extension follows the container, the input file is never chosen, and a name another job already uses gets a number. If you agreed to replace an existing file in the save dialog, you aren't asked again at Start.

## 0.12.0

- **Damaged and incomplete files**: each file is checked for missing data after it's read. The job row gets an *Incomplete* (end cut off) or *Damaged* (holes in the middle, with the times) badge, and a warning explains what to expect. Damaged files are decoded in software with errors ignored, so the encode gets past the holes with a short glitch instead of failing. For a file that's only cut off at the end, *Copy (remux only)* is offered, since it rebuilds the index in a minute. With Copy chosen on a file with holes, re-encoding is offered instead.
- A cut-off file's real length is used for the progress bar, the target-size bitrate, sample points and the remux length check, which used to reject it.
- A cut-off MP4 that can't be read at all now says why, instead of showing ffmpeg's error.

## 0.11.0

- **Tune in mpv** in the audio delay popover: opens the film in mpv on that track. Ctrl + and Ctrl − shift the sound 50 ms at a time, and the delay in the app follows live, so you don't have to type the value in. Close mpv when the lips match. Needs mpv (`sudo dnf install mpv`).

## 0.10.0

- New **Media info** row at the top of every job: expand it to see everything ffprobe found in the file — container, duration, size and bitrate, the video's codec, resolution, frame rate, bit depth, colour and HDR/Dolby Vision metadata, and each audio and subtitle track with language, format, bitrate, size and flags. The copy button puts it on the clipboard as text.

## 0.9.0

- New **Cover Art** group on every job: keep the source's cover, choose a new picture (*Choose…*, or drop an image on the window), or remove it. A new picture is converted to a JPEG of at most 1000 pixels on the long side and is never enlarged.
- *Crop to poster shape* cuts a wide picture to 2:3, keeping the middle.
- *Also save as poster* writes the picture next to the result as `Movie.jpg`, which Plex uses as the poster when local assets are on. Plex ignores embedded cover art for movies. A poster that's already there is kept as `Movie.jpg.orig`.
- Fix tracks only can change just the cover art, or just save a poster, without touching anything else.
- Re-encodes keep the source's cover art. Before, it was dropped.

## 0.8.2

- Fix tracks only keeps cover art as cover art in MKV files. Before, the remux turned it into an ordinary one-frame video track. The picture is copied out unchanged and put back as an attachment with its file name and type. MP4 covers already stayed covers.

## 0.8.1

- Fix tracks only works on MP4/MOV files with a timecode or chapter track. Before, ffmpeg refused them. Data tracks are no longer copied: the muxer writes timecode and chapters afresh, so they don't pile up as stray tracks on each run.
- With an audio delay, every stream keeps its place in the file, so cover art no longer jumps ahead of the audio and odd stream orders are left alone.
- *Write a new file* jobs whose output already exists are asked about up front with the other replacements, so the queue no longer stops half-way.
- A new `.mp4` from a `.mov` source is a real MP4.
- The file check only lets a positive delay make the result longer and a negative delay make it shorter. An audio-only file with a negative delay no longer fails, and a cut-short file no longer passes.
- Switching a job to Fix tracks only because of a delay no longer changes your remembered *Result* choice.
- Delays can be reset while *Subtitles next to the video* is picked. The messages for delay-only jobs no longer talk about subtitles.

## 0.8.0

- "Add subtitles only" is now **Fix tracks only**: it also shifts out-of-sync audio without re-encoding anything. Its *Audio Timing* group has a delay button for every audio track, and every stream is copied as it is (Atmos is kept).
- New result choice, *Write a new file*: it remuxes into the output folder and leaves the original alone. *Replace the original file* and *Subtitles next to the video* work as before. An AVI or TS source written as a new file becomes an MKV.
- A normal job that has an audio delay set but would re-encode the video now says so, with one-click *Copy (remux only)* and *Fix tracks only* buttons.

## 0.7.2

- The audio delay is set in milliseconds, 50 ms per arrow click.

## 0.7.1

- A negative audio delay in MKV no longer starts the video late. Before, chapters landed too early and the after-encode VMAF check compared the wrong frames. The audio that would land before the start is dropped instead.
- Changing the preferred languages keeps the audio delays you set on queued jobs.

## 0.7.0

- Lip-sync fix: each audio track has a clock button in the track picker that sets an audio delay in seconds, for sources that are out of sync. Positive = the sound comes later. It uses the same sign as mpv and VLC, so you can find the value while watching and type it in.
- Copied tracks stay copies (E-AC-3 Atmos keeps Atmos). The shift is a timestamp offset, not a re-encode. Tracks with different delays each get their own.
- External subtitles synced against the speech in a delayed track move with it.

## 0.6.2

- Subtitle zips are unpacked in the background, so a zip on a sleeping NAS no longer freezes the window. Oversized archives are refused (at most 200 SRT files, 64 MB in total).
- A zip with one folder per language (`English/Movie.srt`, `Norwegian/Movie.srt`) gives `Movie.English.srt` and `Movie.Norwegian.srt` instead of two "Unknown" files. Two files with the same name keep their names, so the language in the name is still read.
- Adding the same zip twice no longer adds every subtitle twice. `encoder --add-subs` cleans up its unpacked files however the run ends.
- An alass download that isn't executable no longer counts as installed. A download cut short by quitting is cleaned up on the next install.
- The *Install alass* button says "Installing…" while it works, and the sync tooltip only mentions the button where it exists.

## 0.6.1

- The Flatpak bundles alass, so subtitles are synced automatically there too.
- Native installs get an *Install alass* button on the "alass isn't installed" check. It downloads the official build (checked against a pinned checksum) into `~/.local/share/encoder/bin`, with no root needed. A **?** next to it explains how to install alass by hand, for both native and Flatpak.
- alass in `~/.local/bin` or `~/.cargo/bin` is found even when the app is started from GNOME without those folders on PATH.

## 0.6.0

- Subtitle zips (as subtitle sites ship them) can be dropped on the window, picked in *Add Subtitle Files…* and *Add Subtitles to Videos…*, or passed to `encoder --add-subs`. The SRT files inside are used; anything else (`.nfo` and so on) is ignored.

## 0.5.1

- Subtitles-only jobs always ask before replacing a file, including jobs added while the queue is running and jobs edited after you approved them.
- Saving subtitles next to a video is all or nothing, and any existing subtitle file that gets replaced is kept as `.orig` (also with `encoder --add-subs`). Retrying a failed job no longer shifts the timing twice.
- Subtitles for `Blade.Runner.2049.mkv` are no longer picked up by `Blade.Runner.mkv` in the same folder. A subtitle file dropped while its video is encoding is no longer handed to another job.
- Subtitle files you add yourself stay selected after the video is read. Only files found next to the video are limited to your preferred languages.
- Embedding subtitles into `.m4v` files works (HEVC and files with chapters failed before).
- Titles like "Dr. No" or "The Day It…" are no longer read as a language code.
- SRT text after a blank line inside a subtitle is kept. UTF-16 files without a byte-order mark are read correctly, and the report says when a file was read as Windows-1252.
- The "only PGS" question comes back when you switch a job from subtitles-only to re-encode, or remove the SRT that covered that language.
- Folders are scanned for subtitle files in the background, so a slow NAS no longer freezes the window. No VMAF run after subtitles-only jobs.

## 0.5.0

- External SRT subtitles: files next to the video are found automatically (`Movie.no.srt`, `Movie.en.forced.srt`, `Movie.en.sdh.srt`), and more can be dropped on the window or added per job. Language and kind come from the file name and can be changed.
- Subtitle sync with alass (optional): each file is lined up against an embedded text subtitle, or the speech in the audio, before use. A manual offset is available too. The finished job reports what changed, e.g. "shifted −1.80 s" or "framerate corrected".
- Subtitles only, no re-encode: save the synced subtitles next to the video for Plex, or embed them into the MKV/MP4. An embed writes a new copy, checks it, and only then replaces the original. *Add Subtitles to Videos…* in the menu, or `encoder --add-subs`.
- Windows-1252 subtitle files are converted to UTF-8, and an external SRT replaces an embedded PGS track of the same language.

## 0.4.1

- The finished-job summary shows the target size in MB or GB with full precision ("target 100 MB", not "target 0.1 GB").
- The command-line encode prints its sanity checks after the size estimate, so the "won't save space" and "same codec" warnings show up there the same way they do in the app.

## 0.4.0

- The input can no longer be overwritten through a symlink, a case-insensitive share or another spelling of its path; outputs also avoid other queued jobs' files.
- Encodes write to a `.part` file and rename it when done, and ffmpeg stops if the app dies.
- An impossible target size or an unknown duration no longer encodes at a hidden 8 Mb/s.
- Commentary-only languages and commentary subtitles are no longer picked as the main track.
- Existing outputs are asked about once when you press Start, not mid-queue.
- "Apply to all" and presets keep hand-picked audio conversions.
- A broken settings file is set aside (presets recovered) instead of being overwritten.
- VMAF after encoding runs in the background and no longer holds up the queue.
- New checks: rate control overriding the size mode, cropping with PGS subtitles, copied video Apple TV can't play; a second fix button where useful.
- Dolby Vision: needs 10-bit output, refuses unverifiable frame counts, ignores extra arguments in the mux step.
- GPU cropping always goes through `scale_vaapi`, and the startup check verifies the cropped size.
- Single instance: opening files again adds them to the open window.
- Non-UTF-8 text in ffmpeg's output no longer drops the log, crop detection or VMAF.
- Fewer background jobs at once, debounced settings saves, and leftover cache files are cleaned up.
- Release scripts: the bump needs a changelog entry, and published commit messages are checked for private strings.

## 0.3.10

- About dialog: a Changelog page next to Details and Legal.

## 0.3.9

- No double launch when the app is started twice.
- "Apply to all" no longer breaks Dolby Vision jobs.
- Stream maps skip embedded cover art.
- Sturdier GitHub connection check in the release scripts.

## 0.3.8

- Release scripts: Ctrl+D cancels every prompt; sturdier GitHub SSH check.

## 0.3.7

- Installer menus quit on Ctrl+D; the GitHub check no longer reads the keyboard.

## 0.3.6

- GitHub pushes give up after 10 seconds when SSH can't connect.

## 0.3.5

- install.sh: a looping menu, with a Release section for the author.

## 0.3.4

- No prebuilt Flatpak: releases are source-only, the installer fetches flatpak-builder, and the README explains why.

## 0.3.3

- Licensed under GPL-3.0.

## 0.3.2

- Release script tags and publishes releases.

## 0.3.1

- README: hardware support notes.

## 0.3.0

- Renamed to encodeR, with the app ID no.oyzmo.EncodeR.

## 0.2.0

- Stereo AAC downmix switch.
- Subtitles sit behind a collapsible row.

## 0.1.10

- Background ffmpeg processes stop before the app quits.

## 0.1.9

- 10-bit crop detection fixed.
- Fixed queue stalls, sample file clashes and stale Dolby Vision/VMAF state.

## 0.1.8

- Stream statistics tags are checked per stream.
- Fixed a subtitle bitrate overflow.

## 0.1.7

- Stale statistics tags are ignored.
- Subtitles are counted in the video size estimate.

## 0.1.6

- Analysis works in the Flatpak; errors are never silent.

## 0.1.5

- REENCODER_CONFIG_DIR override, so hardware verification works in the Flatpak.

## 0.1.4

- Flatpak build fix.

## 0.1.3

- Flatpak bundles dovi_tool.

## 0.1.2

- Installer can build and install a Flatpak.

## 0.1.1

- Deinterlacing runs on the CPU only.
- The command view shows the real command.

## 0.1.0

- Automatic deinterlacing and inverse telecine.

## 0.0.3

- Handles encoder size limits and reports the real ffmpeg error.

## 0.0.2

- Capability check test-encodes at 640×360.

## 0.0.1

- First version: VA-API re-encoding for Apple TV direct play.
