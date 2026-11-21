//! "Match source quality": the target size that keeps about the source's
//! quality. Two steps:
//!
//! 1. An instant estimate: the source's video bitrate times how many more
//!    bits the output encoder needs than the source's codec for the same
//!    quality ([`estimate_kbps`]).
//! 2. An optional VMAF check: encode the sample segments at a few bitrates
//!    and keep the lowest one that still scores as transparent
//!    ([`Search`]). Pure here; the worker runs the encodes.

use crate::ffmpeg::estimate::{self, CONTAINER_OVERHEAD, GB, TARGET_FRACTION};
use crate::ffmpeg::probe::ProbeInfo;
use crate::ffmpeg::vmaf::VmafResult;
use crate::settings::{EncodeSettings, VideoCodec};

/// Bits a codec needs for a given quality, relative to a good software
/// H.264 encode. Release encodes are mostly careful software encodes.
fn source_cost(codec: &str) -> Option<f64> {
    Some(match codec {
        "mpeg2video" => 2.0,
        "mpeg4" | "msmpeg4v3" => 1.4,
        "vc1" | "wmv3" => 1.2,
        "h264" => 1.0,
        "vp9" => 0.65,
        "hevc" => 0.6,
        "av1" => 0.5,
        _ => return None,
    })
}

/// The same for the VA-API encoders: single-pass hardware encodes need more
/// bits than careful software ones. Rough figures, calibrated by eye;
/// the VMAF check is the accurate answer.
fn output_cost(codec: VideoCodec) -> Option<f64> {
    Some(match codec {
        VideoCodec::H264 => 1.15,
        VideoCodec::Hevc => 0.75,
        VideoCodec::Av1 => 0.62,
        VideoCodec::Copy => return None,
    })
}

/// How many times the source's video bitrate the output needs.
pub fn factor(source_codec: &str, out: VideoCodec) -> Option<f64> {
    Some(output_cost(out)? / source_cost(source_codec)?)
}

/// Instant estimate of the video bitrate (kb/s) that keeps the source's quality.
/// Downscaling lowers it: bitrate scales with roughly pixels^0.75.
pub fn estimate_kbps(p: &ProbeInfo, s: &EncodeSettings) -> Option<u32> {
    let v = p.video.as_ref()?;
    let src = p.video_bit_rate()? as f64 / 1000.0;
    let mut kbps = src * factor(&v.codec, s.codec)?;
    if let Some(h) = s.downscale.height().filter(|h| *h < v.height)
        && v.height > 0
    {
        kbps *= (h as f64 / v.height as f64).powf(1.5);
    }
    (kbps >= estimate::MIN_VIDEO_KBPS as f64).then_some(kbps.round() as u32)
}

/// Target size (GB, rounded up to 0.1) at which the target-size maths give
/// `video_kbps`. Inverse of [`estimate::target_video_kbps`].
pub fn target_gb(video_kbps: u32, duration: f64, audio: u64, subs: u64) -> f64 {
    let bits = video_kbps as f64 * 1000.0 * duration + (audio + subs) as f64 * 8.0;
    let gb = bits / 8.0 / (GB * TARGET_FRACTION * (1.0 - CONTAINER_OVERHEAD));
    ((gb * 10.0).ceil() / 10.0).max(0.5)
}

/// Transparent enough to call it "the source's quality": the mean in the
/// "visually transparent" band and no segment far below it.
pub const PASS_MEAN: f64 = 95.0;
pub const PASS_WORST: f64 = 93.0;

pub fn passes(r: &VmafResult) -> bool {
    r.mean >= PASS_MEAN && r.worst >= PASS_WORST
}

/// Each step encodes and scores the samples (VMAF on the CPU is slow).
pub const MAX_STEPS: u32 = 5;

/// Bisection over bitrates between half and twice the instant estimate:
/// test the estimate; if it fails, test the top; then narrow down to the
/// lowest bitrate that passes, in log steps, until within ~10 %.
#[derive(Clone, Debug)]
pub struct Search {
    lo: f64,
    hi: f64,
    max: f64,
    start: u32,
    steps: u32,
    best: Option<(u32, VmafResult)>,
    last_fail: Option<(u32, VmafResult)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Found {
    /// Lowest bitrate that passed; `None` if even the top one didn't.
    pub kbps: Option<u32>,
    /// Score at that bitrate, or at the highest one tried.
    pub score: VmafResult,
    /// Highest bitrate tried (for "not reached at X").
    pub tried_max: u32,
}

impl Search {
    pub fn new(estimate_kbps: u32) -> Self {
        let k = estimate_kbps.max(estimate::MIN_VIDEO_KBPS) as f64;
        Search {
            lo: k / 2.0,
            hi: k * 2.0,
            max: k * 2.0,
            start: k as u32,
            steps: 0,
            best: None,
            last_fail: None,
        }
    }

    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// The next bitrate to try, or `None` when done.
    pub fn next(&self) -> Option<u32> {
        if self.steps == 0 {
            return Some(self.start);
        }
        if self.steps >= MAX_STEPS {
            return None;
        }
        if self.best.is_none() {
            // The estimate failed: try the top once; if that fails too, stop.
            return (self.lo < self.max).then_some(self.max as u32);
        }
        (self.hi / self.lo >= 1.1).then(|| (self.lo * self.hi).sqrt().round() as u32)
    }

    pub fn record(&mut self, kbps: u32, r: VmafResult) {
        self.steps += 1;
        if passes(&r) {
            self.hi = kbps as f64;
            if self.best.is_none_or(|(b, _)| kbps < b) {
                self.best = Some((kbps, r));
            }
        } else {
            self.lo = self.lo.max(kbps as f64);
            if self.last_fail.is_none_or(|(b, _)| kbps > b) {
                self.last_fail = Some((kbps, r));
            }
        }
    }

    pub fn result(&self) -> Option<Found> {
        let tried_max = self
            .best
            .iter()
            .chain(&self.last_fail)
            .map(|(k, _)| *k)
            .max()?;
        Some(match (self.best, self.last_fail) {
            (Some((k, r)), _) => Found {
                kbps: Some(k),
                score: r,
                tried_max,
            },
            (None, Some((_, r))) => Found {
                kbps: None,
                score: r,
                tried_max,
            },
            (None, None) => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe;
    use crate::settings::Downscale;

    fn probe(codec: &str, kbps: u64, h: u32) -> ProbeInfo {
        let w = h * 16 / 9;
        probe::parse(&format!(
            r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"{codec}","width":{w},"height":{h},
            "pix_fmt":"yuv420p10le","avg_frame_rate":"24/1","bit_rate":"{}"}}],
            "format":{{"duration":"6000","size":"2000000000"}}}}"#,
            kbps * 1000
        ))
        .unwrap()
    }

    fn hevc() -> EncodeSettings {
        EncodeSettings {
            codec: VideoCodec::Hevc,
            ..Default::default()
        }
    }

    #[test]
    fn factors_follow_codec_efficiency() {
        let f = |src, out| factor(src, out).unwrap();
        assert!((f("av1", VideoCodec::Hevc) - 1.5).abs() < 1e-9);
        assert!(f("av1", VideoCodec::H264) > 2.0);
        assert!(f("hevc", VideoCodec::Hevc) > 1.0, "re-encode costs bits");
        assert!(f("h264", VideoCodec::Hevc) < 1.0);
        assert!(f("mpeg2video", VideoCodec::Hevc) < 0.5);
        assert_eq!(factor("prores", VideoCodec::Hevc), None);
        assert_eq!(factor("av1", VideoCodec::Copy), None);
    }

    #[test]
    fn estimate_scales_bitrate_and_downscale() {
        let p = probe("av1", 2000, 1080);
        assert_eq!(estimate_kbps(&p, &hevc()), Some(3000));
        let small = EncodeSettings {
            downscale: Downscale::P720,
            ..hevc()
        };
        let k = estimate_kbps(&p, &small).unwrap();
        assert!((1600..1700).contains(&k), "{k}");
        // Downscale above the source height is ignored.
        let up = EncodeSettings {
            downscale: Downscale::P2160,
            ..hevc()
        };
        assert_eq!(estimate_kbps(&p, &up), Some(3000));
        assert_eq!(estimate_kbps(&probe("prores", 2000, 1080), &hevc()), None);
    }

    #[test]
    fn target_gb_inverts_target_maths() {
        let gb = target_gb(3000, 6000.0, 300_000_000, 1_000_000);
        // The target-size maths at that size give at least the bitrate asked for.
        let k = estimate::target_video_kbps(gb, 6000.0, 300_000_000, 1_000_000).unwrap();
        assert!((3000..3060).contains(&k), "{gb} GB → {k}");
        assert_eq!(gb, (gb * 10.0).round() / 10.0);
        assert_eq!(target_gb(100, 10.0, 0, 0), 0.5);
    }

    fn score(m: f64) -> VmafResult {
        VmafResult {
            mean: m,
            worst: m - 1.0,
        }
    }

    /// Run the search against a fake encoder whose score rises with bitrate.
    fn run(start: u32, threshold: u32) -> (Search, Vec<u32>) {
        let mut s = Search::new(start);
        let mut tried = Vec::new();
        while let Some(k) = s.next() {
            tried.push(k);
            s.record(k, score(if k >= threshold { 96.0 } else { 90.0 }));
        }
        (s, tried)
    }

    #[test]
    fn search_goes_down_when_estimate_passes() {
        let (s, tried) = run(3000, 2000);
        assert_eq!(tried[0], 3000);
        assert!(tried.len() as u32 <= MAX_STEPS);
        let f = s.result().unwrap();
        let k = f.kbps.unwrap();
        assert!((2000..2400).contains(&k), "{k} from {tried:?}");
        assert!(passes(&f.score));
    }

    #[test]
    fn search_goes_up_when_estimate_fails() {
        let (s, tried) = run(3000, 4500);
        assert_eq!(&tried[..2], &[3000, 6000]);
        let k = s.result().unwrap().kbps.unwrap();
        assert!((4500..5300).contains(&k), "{k} from {tried:?}");
    }

    #[test]
    fn search_reports_not_reached() {
        let (s, tried) = run(3000, 10_000);
        assert_eq!(tried, vec![3000, 6000]);
        let f = s.result().unwrap();
        assert_eq!(f.kbps, None);
        assert_eq!(f.tried_max, 6000);
        assert!(!passes(&f.score));
    }

    #[test]
    fn worst_segment_must_pass_too() {
        assert!(passes(&VmafResult {
            mean: 95.5,
            worst: 93.0
        }));
        assert!(!passes(&VmafResult {
            mean: 97.0,
            worst: 90.0
        }));
        assert!(!passes(&VmafResult {
            mean: 94.9,
            worst: 94.0
        }));
    }
}
