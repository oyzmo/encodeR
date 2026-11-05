# Changelog

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
