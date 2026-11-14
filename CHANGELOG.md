# Changelog

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
