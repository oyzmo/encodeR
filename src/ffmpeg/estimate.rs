//! Output size: target-size bitrate maths, bitrate-mode and remux estimates,
//! and sample encoding with extrapolation for CQP/QVBR.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::command::{self, DecodeMode, EncodeJob};
use super::probe::ProbeInfo;
use super::runner::{self, Cancel};
use crate::settings::{Container, VideoCodec, cache_dir};
use crate::tracks::Selection;

/// Aim under the target: hardware VBR is single-pass and can overshoot.
pub const TARGET_FRACTION: f64 = 0.97;
pub const CONTAINER_OVERHEAD: f64 = 0.01;
pub const GB: f64 = 1e9;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SizeRange {
    pub lo: u64,
    pub hi: u64,
}

/// Video bitrate (kb/s) that hits `target_gb` with the given audio/subtitle sizes.
pub fn target_video_kbps(
    target_gb: f64,
    duration: f64,
    audio_bytes: u64,
    sub_bytes: u64,
) -> Option<u32> {
    if duration <= 0.0 {
        return None;
    }
    let usable_bits = target_gb * GB * 8.0 * TARGET_FRACTION * (1.0 - CONTAINER_OVERHEAD);
    let video_bits = usable_bits - (audio_bytes + sub_bytes) as f64 * 8.0;
    (video_bits > 0.0).then(|| (video_bits / duration / 1000.0) as u32)
}

/// Bits per pixel per frame below which the codec starts to look poor.
pub fn min_bpp(codec: VideoCodec) -> f64 {
    match codec {
        VideoCodec::H264 => 0.055,
        VideoCodec::Hevc => 0.032,
        VideoCodec::Av1 => 0.024,
        VideoCodec::Copy => 0.0,
    }
}

pub fn bpp(kbps: f64, width: u32, height: u32, fps: f64) -> f64 {
    let px = width as f64 * height as f64 * fps;
    if px <= 0.0 { 0.0 } else { kbps * 1000.0 / px }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BitrateVerdict {
    Fine,
    /// Too low at this resolution; `suggest_height` would look fine.
    TooLow {
        suggest_height: Option<u32>,
    },
}

pub fn bitrate_verdict(
    kbps: u32,
    codec: VideoCodec,
    width: u32,
    height: u32,
    fps: f64,
) -> BitrateVerdict {
    let min = min_bpp(codec);
    if codec == VideoCodec::Copy || bpp(kbps as f64, width, height, fps) >= min {
        return BitrateVerdict::Fine;
    }
    let suggest_height = [1080u32, 720]
        .into_iter()
        .filter(|h| *h < height)
        .find(|h| {
            let w = (width as f64 * *h as f64 / height as f64).round() as u32;
            bpp(kbps as f64, w, *h, fps) >= min
        });
    BitrateVerdict::TooLow { suggest_height }
}

/// ~3 segments at 20/50/80 % (whole file if short).
pub fn sample_segments(duration: f64) -> Vec<(f64, f64)> {
    const LEN: f64 = 8.0;
    if duration < 60.0 {
        return vec![(0.0, duration)];
    }
    [0.2, 0.5, 0.8]
        .iter()
        .map(|f| ((duration * f).min(duration - LEN), LEN))
        .collect()
}

/// Extrapolate video bytes from `(bytes, seconds)` samples. The range spans the
/// sample rates, widened to at least ±10 % around the mean.
pub fn extrapolate(samples: &[(u64, f64)], duration: f64) -> Option<SizeRange> {
    let total_s: f64 = samples.iter().map(|s| s.1).sum();
    if samples.is_empty() || total_s <= 0.0 || duration <= 0.0 {
        return None;
    }
    let mean = samples.iter().map(|s| s.0 as f64).sum::<f64>() / total_s;
    let rates = samples
        .iter()
        .filter(|s| s.1 > 0.0)
        .map(|s| s.0 as f64 / s.1);
    let (min, max) = rates.fold((f64::MAX, 0.0f64), |(lo, hi), r| (lo.min(r), hi.max(r)));
    Some(SizeRange {
        lo: (min.min(mean * 0.9) * duration) as u64,
        hi: (max.max(mean * 1.1) * duration) as u64,
    })
}

/// Add audio, subtitles and container overhead to a video range.
pub fn with_streams(video: SizeRange, audio: u64, subs: u64) -> SizeRange {
    let f = |v: u64| ((v + audio + subs) as f64 * (1.0 + CONTAINER_OVERHEAD)) as u64;
    SizeRange {
        lo: f(video.lo),
        hi: f(video.hi),
    }
}

/// Bitrate modes: the size follows from the bitrate; ±5 % for single-pass VBR.
pub fn bitrate_range(kbps: u32, duration: f64, audio: u64, subs: u64, exact: bool) -> SizeRange {
    let v = (kbps as f64 * 1000.0 * duration / 8.0) as u64;
    let spread = if exact { 0.0 } else { 0.05 };
    with_streams(
        SizeRange {
            lo: (v as f64 * (1.0 - spread)) as u64,
            hi: (v as f64 * (1.0 + spread)) as u64,
        },
        audio,
        subs,
    )
}

/// Remux: the kept streams' sizes.
pub fn copy_size(p: &ProbeInfo, sel: &Selection) -> u64 {
    let video = p
        .video
        .as_ref()
        .and_then(|v| v.size_bytes)
        .or_else(|| {
            p.video_bit_rate()
                .map(|b| (b as f64 * p.duration / 8.0) as u64)
        })
        .unwrap_or(0);
    ((video + sel.audio_bytes(p) + sel.subtitle_bytes(p)) as f64 * (1.0 + CONTAINER_OVERHEAD))
        as u64
}

pub fn format_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} kB", b / 1e3)
    }
}

/// "≈ 1.2–1.5 GB, about 45% of the original".
pub fn describe(r: SizeRange, input: u64) -> String {
    let span = if r.hi as f64 >= GB {
        if (r.hi - r.lo) as f64 / GB < 0.05 {
            format!("{:.1} GB", r.lo as f64 / GB)
        } else {
            format!("{:.1}–{:.1} GB", r.lo as f64 / GB, r.hi as f64 / GB)
        }
    } else if (r.hi - r.lo) as f64 / 1e6 < 1.0 {
        format!("{:.0} MB", r.lo as f64 / 1e6)
    } else {
        format!("{:.0}–{:.0} MB", r.lo as f64 / 1e6, r.hi as f64 / 1e6)
    };
    if input > 0 {
        let pct = (r.lo + r.hi) as f64 / 2.0 / input as f64 * 100.0;
        format!("≈ {span}, about {pct:.0}% of the original")
    } else {
        format!("≈ {span}")
    }
}

static SAMPLE_RUN: AtomicU64 = AtomicU64::new(0);

pub struct Sample {
    pub path: PathBuf,
    pub start: f64,
    pub secs: f64,
    pub bytes: u64,
}

/// Encode the sample segments video-only. Files are deleted unless `keep`.
pub fn encode_samples(
    ffmpeg: &str,
    job: &EncodeJob,
    cancel: &Cancel,
    keep: bool,
) -> Result<Vec<Sample>, String> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut settings = job.settings.clone();
    settings.container = Container::Mkv;
    settings.extra_args.clear();
    let mut out = Vec::new();
    for (n, (start, secs)) in sample_segments(job.probe.duration).into_iter().enumerate() {
        // Unique per call: estimates for several jobs and VMAF run at the same time.
        let path = dir.join(format!(
            "sample-{}-{}-{n}.mkv",
            std::process::id(),
            SAMPLE_RUN.fetch_add(1, Ordering::Relaxed)
        ));
        let mut decode = job.decode;
        let result = loop {
            let mut j = EncodeJob {
                settings: &settings,
                output: &path,
                decode,
                ..job.clone()
            };
            j.segment = Some((start, secs));
            j.video_only = true;
            j.progress = false;
            let args: Vec<OsString> = command::build(&j)?;
            let r = runner::run_capture(ffmpeg, &args, cancel)?;
            if !r.success
                && !r.cancelled
                && decode == DecodeMode::Hardware
                && command::is_hw_decode_failure(&r.stderr)
            {
                decode = DecodeMode::Software;
                continue;
            }
            break r;
        };
        if result.cancelled {
            let _ = std::fs::remove_file(&path);
            return Err("Cancelled".into());
        }
        if !result.success {
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "Sample encode failed: {}",
                runner::error_summary(&result.stderr)
            ));
        }
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if !keep {
            let _ = std::fs::remove_file(&path);
        }
        out.push(Sample {
            path,
            start,
            secs,
            bytes,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_size_maths() {
        // 4 GB over 2 h with 576 MB audio.
        let kbps = target_video_kbps(4.0, 7200.0, 576_000_000, 0).unwrap();
        let expect = (4e9 * 8.0 * 0.97 * 0.99 - 576e6 * 8.0) / 7200.0 / 1000.0;
        assert_eq!(kbps, expect as u32);
        assert!((3600..3700).contains(&kbps));
        // Audio larger than the target: impossible.
        assert_eq!(target_video_kbps(2.0, 7200.0, 4_050_000_000, 0), None);
        assert_eq!(target_video_kbps(2.0, 0.0, 0, 0), None);
    }

    #[test]
    fn low_bitrate_suggests_downscale() {
        // 3.6 Mb/s HEVC at 4K24 is too thin, but fine at 1080p.
        assert_eq!(
            bitrate_verdict(3600, VideoCodec::Hevc, 3840, 2160, 23.976),
            BitrateVerdict::TooLow {
                suggest_height: Some(1080)
            }
        );
        assert_eq!(
            bitrate_verdict(12000, VideoCodec::Hevc, 3840, 2160, 23.976),
            BitrateVerdict::Fine
        );
        assert_eq!(
            bitrate_verdict(300, VideoCodec::H264, 1920, 1080, 24.0),
            BitrateVerdict::TooLow {
                suggest_height: None
            }
        );
        assert_eq!(
            bitrate_verdict(1, VideoCodec::Copy, 1920, 1080, 24.0),
            BitrateVerdict::Fine
        );
    }

    #[test]
    fn segments() {
        assert_eq!(sample_segments(30.0), vec![(0.0, 30.0)]);
        let s = sample_segments(1000.0);
        assert_eq!(s, vec![(200.0, 8.0), (500.0, 8.0), (800.0, 8.0)]);
    }

    #[test]
    fn extrapolation() {
        // Equal samples: ±10 % around 1 MB/s.
        let r = extrapolate(
            &[(8_000_000, 8.0), (8_000_000, 8.0), (8_000_000, 8.0)],
            1000.0,
        )
        .unwrap();
        assert_eq!((r.lo, r.hi), (900_000_000, 1_100_000_000));
        // Spread samples: the range follows the extremes.
        let r = extrapolate(
            &[(4_000_000, 8.0), (8_000_000, 8.0), (16_000_000, 8.0)],
            1000.0,
        )
        .unwrap();
        assert_eq!((r.lo, r.hi), (500_000_000, 2_000_000_000));
        assert!(extrapolate(&[], 10.0).is_none());
        assert!(extrapolate(&[(1, 0.0)], 10.0).is_none());
    }

    #[test]
    fn ranges_and_text() {
        let r = with_streams(
            SizeRange {
                lo: 1_000_000_000,
                hi: 1_000_000_000,
            },
            100_000_000,
            0,
        );
        assert_eq!(r.lo, 1_111_000_000);
        let b = bitrate_range(8000, 1000.0, 0, 0, true);
        assert_eq!(b.lo, b.hi);
        assert_eq!(b.lo, 1_010_000_000);
        assert!(bitrate_range(8000, 1000.0, 0, 0, false).hi > b.hi);
        let t = describe(
            SizeRange {
                lo: 1_200_000_000,
                hi: 1_500_000_000,
            },
            3_000_000_000,
        );
        assert_eq!(t, "≈ 1.2–1.5 GB, about 45% of the original");
        assert_eq!(
            describe(
                SizeRange {
                    lo: 2_000_000_000,
                    hi: 2_010_000_000
                },
                0
            ),
            "≈ 2.0 GB"
        );
        assert_eq!(format_bytes(4_050_000_000), "4.05 GB");
        assert_eq!(format_bytes(80_000), "80 kB");
        assert_eq!(
            describe(
                SizeRange {
                    lo: 12_000_000,
                    hi: 12_000_000
                },
                0
            ),
            "≈ 12 MB"
        );
    }

    #[test]
    fn copy_size_uses_stream_sizes() {
        let p = crate::ffmpeg::probe::tests::sample_4k();
        let (sel, _, _) = crate::tracks::auto_select(
            &p,
            &["eng".to_string()],
            &crate::settings::EncodeSettings::default(),
        );
        let size = copy_size(&p, &sel);
        assert!(size > 52_200_000_000);
    }
}
