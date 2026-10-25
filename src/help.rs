//! Tooltip and help text, kept in one place so it's easy to edit.

pub const PRESET: &str = "Starting point for the settings below. “Shrink for library” turns a big remux into a 2–5 GB HEVC 10-bit file that direct-plays on Apple TV. Save your own from the menu.";

pub const CODEC: &str = "Video format of the output. HEVC is the best default for Apple TV: small files, HDR10, plays everywhere. H.264 is bigger at the same quality but plays on anything. Copy keeps the video untouched and only changes tracks and container.";
pub const CODEC_AV1: &str = "AV1 is the most efficient, but older Apple TV 4K models (before 2022) can't decode it in hardware, so the NAS may have to transcode the video. Use HEVC for direct play.";

pub const SIZE_MODE: &str = "Target size: pick the file size; the bitrate is calculated for you. Constant quality: pick a quality level; the size depends on how hard the video is to compress.";
pub const TARGET_SIZE: &str = "Wanted output size. The encoder aims about 3% under it because hardware encoding is single-pass and can overshoot. 2–3 GB is fine for 1080p, 4–5 GB for 4K.";
pub const QUALITY: &str = "Lower = better quality and bigger file. 20–28 is typical; each step of about 6 roughly halves or doubles the size. AV1 uses a 0–63 scale where 28 is a good start.";
pub const TEN_BIT: &str = "10-bit keeps HDR and reduces banding in skies and dark scenes, at almost no size cost. Needed for HDR sources. Only HEVC and AV1 support it.";
pub const DOWNSCALE: &str = "Smaller resolution = much smaller file at the same quality. 4K → 1080p is a good trade when the target size is small or the screen is under ~65″. Never upscales.";
pub const CONTAINER: &str = "MKV holds every kind of audio and subtitle track and is best for Plex. MP4 is more widely supported by phones and browsers but drops subtitles and some audio formats.";
pub const APPLE_AUDIO: &str = "Converts audio Apple TV can't play natively (TrueHD, DTS, DTS-HD MA, Opus) so the NAS never has to transcode. Compatible tracks (AAC, AC-3, E-AC-3 incl. Atmos, FLAC) are copied as they are.";
pub const DOWNMIX: &str = "Converts every multichannel track (5.1, 7.1) to stereo AAC at 224 kb/s, whatever its codec. Small and plays everywhere, good for TV speakers or headphones, but surround and Atmos are lost. Stereo tracks follow the normal rules. You can still change each track below.";
pub const FALLBACK: &str = "What multichannel TrueHD/DTS becomes. E-AC-3 5.1 at 640 kb/s is small and plays everywhere, but 7.1 is folded to 5.1 and Atmos is lost. AAC 7.1 keeps all channels (lossy). FLAC 7.1 is lossless but large.";

pub const RATE_CONTROL: &str = "How the encoder spends bits. Automatic uses VBR for target size and CQP for constant quality. CBR wastes bits on easy scenes; QVBR is VBR with a quality floor. Only modes your driver accepts are listed.";
pub const BITRATE: &str = "Average video bitrate for the bitrate modes. As a guide: 1080p HEVC 4–8 Mb/s, 4K HEVC 12–20 Mb/s.";
pub const MAXRATE: &str = "Peak bitrate the encoder may use in hard scenes. 0 = automatic (1.5 × the average). Higher peaks look better in action scenes but can stutter on slow networks.";
pub const COMPRESSION: &str = "Encoder speed/quality trade-off passed as -compression_level. Lower is usually slower and slightly better; the effect depends on the driver and some ignore it.";
pub const GOP: &str = "Frames between keyframes. Default is about 2 seconds of video, which keeps seeking fast. Longer saves a little space but makes seeking slower.";
pub const BFRAMES: &str = "B-frames improve compression a bit. Leave on Automatic unless you know your driver handles a specific value; unsupported values are ignored or fail.";
pub const METADATA: &str = "Keeps chapters and global tags (film title, release info). Turn off to drop them. Track languages and titles are always kept.";
pub const EXTRA_ARGS: &str = "Extra ffmpeg arguments, added before the output file. Quote values with spaces like in a shell (it is never run through a shell). Mistakes here can make the encode fail.";
pub const COMMAND: &str = "The exact command that will run. Copy it to try it in a terminal.";

pub const ESTIMATE: &str = "Estimated output size from short sample encodes at 20%, 50% and 80% into the file. Grainy or high-motion scenes elsewhere can make the real size differ.";
pub const ESTIMATE_BITRATE: &str = "Calculated from the bitrate and the track sizes. Hardware VBR is single-pass, so the real size can be off by a few percent.";

pub const CROP: &str = "Cuts black bars so no bits are spent on them. Detection samples several scenes and keeps the largest picture area found, so scenes with a changing aspect ratio (IMAX) are never cut.";
pub const SCAN: &str = "Automatic looks at the picture (not just the file's flags) and picks what's needed: progressive video is left alone, interlaced TV/camera video is deinterlaced, and telecined NTSC film (29.97 fps made from 23.976) gets its original film frames back. Change it only if the result looks wrong: combing lines mean it needs deinterlacing; jerky film motion means inverse telecine.";
pub const DOUBLE_RATE: &str = "Interlaced video holds 50 (or 60) half-pictures per second. Doubling keeps all of that motion (50p/60p), which looks right for sport and TV; off gives 25p/30p, which is smaller but motion is less smooth.";
pub const CROP_GPU: &str = "The crop runs on the GPU, so it costs no speed.";
pub const CROP_CPU: &str = "Your driver can't crop on the GPU, so frames make a round trip through system memory. Expect roughly 20–40% slower encoding.";
pub const CROP_DV: &str =
    "Cropping is turned off for this job: the Dolby Vision metadata describes the full frame.";

pub const DV_PRESERVE: &str = "Experimental. Extracts the Dolby Vision metadata (RPU) with dovi_tool, re-encodes the video, injects the RPU back and checks the result. Profile 7 is converted to 8.1, which Apple TV plays. Needs HEVC and no scaling.";
pub const DV_MISSING: &str = "Install dovi_tool to keep Dolby Vision. It is not packaged for Fedora; get it from https://github.com/quietvoid/dovi_tool/releases and put it on your PATH.";

pub const VMAF: &str = "Scores sample encodes against the source with Netflix's VMAF. ≥ 95 = visually transparent, 90–95 = very good, 80–90 = good with visible loss on close inspection, below 80 = poor. The worst segment matters more than the average. Runs on the CPU, so it's slow; it never blocks encoding.";
pub const VMAF_HDR: &str =
    "VMAF models are trained on SDR video, so scores for HDR sources are approximate.";
pub const VMAF_MISSING: &str =
    "Your ffmpeg has no libvmaf filter, so quality checks are unavailable.";
pub const VMAF_AFTER: &str = "After each encode, scores a few segments of the finished file against the source at the same timestamps.";

pub const OUTPUT_DIR: &str =
    "Where the new files go. Empty = next to each input file. The input is never overwritten.";
pub const SUFFIX: &str = "Added to the file name, e.g. Movie-reencoded.mkv.";
pub const DEVICE: &str =
    "The GPU used for decoding and encoding. Automatic picks the first AMD render device.";
pub const LANGUAGES: &str = "Languages to keep, in order, separated by commas, e.g. “Norwegian, English, Original”. Original = the language of the first audio track. The first kept audio track becomes the default.";

pub const PGS: &str = "Image-based PGS subtitles: Plex usually burns them into the video when they're turned on, which forces a transcode. SRT is preferred.";
