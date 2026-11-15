# Changelog

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
