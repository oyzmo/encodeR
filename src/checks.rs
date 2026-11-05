//! Input-vs-output sanity checks: pure `(probe, settings, selection) → Vec<Issue>`,
//! each with a severity and, where it makes sense, a one-click fix.

use crate::ffmpeg::estimate::{self, BitrateVerdict, SizeRange};
use crate::ffmpeg::probe::ProbeInfo;
use crate::scan::{Scan, ScanType};
use crate::settings::{Container, Downscale, EncodeSettings, SizeMode, VideoCodec};
use crate::tracks::{self, AudioAction, Selection};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Strong,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fix {
    TenBit,
    Codec(VideoCodec),
    Mkv,
    ConvertIncompatibleAudio,
    ConvertLosslessAudio,
    DropImageSubtitles,
    Target(f64),
    Downscale(Downscale),
}

impl Fix {
    pub fn label(self) -> String {
        match self {
            Fix::TenBit => "Switch to 10-bit".into(),
            Fix::Codec(VideoCodec::Copy) => "Copy (remux only)".into(),
            Fix::Codec(c) => format!("Switch to {}", c.label()),
            Fix::Mkv => "Switch to MKV".into(),
            Fix::ConvertIncompatibleAudio => "Convert audio".into(),
            Fix::ConvertLosslessAudio => "Convert lossless tracks".into(),
            Fix::DropImageSubtitles => "Drop PGS".into(),
            Fix::Target(gb) => format!("Use {gb:.1} GB"),
            Fix::Downscale(d) => format!("Downscale to {}", d.label()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub fix: Option<Fix>,
}

fn issue(
    id: &'static str,
    severity: Severity,
    message: impl Into<String>,
    fix: Option<Fix>,
) -> Option<Issue> {
    Some(Issue {
        id,
        severity,
        message: message.into(),
        fix,
    })
}

pub struct CheckInput<'a> {
    pub probe: &'a ProbeInfo,
    pub settings: &'a EncodeSettings,
    pub selection: &'a Selection,
    /// Video bitrate from the target-size maths (target mode only).
    pub video_kbps: Option<u32>,
    pub estimate: Option<SizeRange>,
    pub vmaf: Option<f64>,
    pub dv_preserve: bool,
    pub crop: bool,
    /// The crop that will be applied, for the output size.
    pub active_crop: Option<crate::crop::Crop>,
    /// Frame sizes the hardware encoder accepts (from the capability check).
    pub encoder_limits: Option<crate::ffmpeg::capabilities::SizeLimits>,
    /// What idet found, and what the encode will do about it.
    pub scan_detected: Option<ScanType>,
    pub scan: Scan,
}

impl CheckInput<'_> {
    fn encoding(&self) -> bool {
        self.settings.codec != VideoCodec::Copy
    }
    fn source_ten_bit(&self) -> bool {
        self.probe.video.as_ref().is_some_and(|v| v.bit_depth > 8)
    }
    /// HDR (PQ, HLG, Dolby Vision) or BT.2020 colour: 8-bit output washes it out.
    fn source_wide_gamut(&self) -> bool {
        self.probe.hdr.is_hdr() || self.probe.hdr.bt2020
    }
}

fn source_codec_class(name: &str) -> VideoCodec {
    match name {
        "hevc" => VideoCodec::Hevc,
        "av1" | "vp9" => VideoCodec::Av1,
        _ => VideoCodec::H264,
    }
}

pub fn eight_to_ten(c: &CheckInput) -> Option<Issue> {
    (c.encoding() && !c.source_ten_bit() && c.settings.output_ten_bit()).then_some(())?;
    issue(
        "8to10",
        Severity::Info,
        "8-bit source to 10-bit output: no extra colour detail is gained. It can reduce banding slightly and sometimes compresses a bit better, but very old players may not handle it.",
        None,
    )
}

pub fn hdr_to_eight(c: &CheckInput) -> Option<Issue> {
    let wide = c.source_wide_gamut();
    (c.encoding() && (wide || c.source_ten_bit()) && !c.settings.output_ten_bit()).then_some(())?;
    let h264 = c.settings.codec == VideoCodec::H264;
    if wide {
        // `hdr264` covers HDR to H.264 with a better fix.
        (!h264).then_some(())?;
        return issue(
            "hdr8",
            Severity::Strong,
            "HDR source to 8-bit output: HDR will be lost and colours will look washed out (there is no tone mapping).",
            Some(Fix::TenBit),
        );
    }
    // A 10-bit SDR source keeps its colours in 8 bits; only gradients suffer.
    issue(
        "hdr8",
        Severity::Warning,
        "10-bit source to 8-bit output: smooth gradients (skies, shadows) may show more banding. Colours stay the same, since the source isn't HDR.",
        (!h264).then_some(Fix::TenBit),
    )
}

pub fn hdr_to_h264(c: &CheckInput) -> Option<Issue> {
    (c.source_wide_gamut() && c.settings.codec == VideoCodec::H264).then_some(())?;
    issue(
        "hdr264",
        Severity::Strong,
        "HDR source to H.264: HDR is lost and colours will look washed out. Use HEVC 10-bit or AV1 10-bit instead.",
        Some(Fix::Codec(VideoCodec::Hevc)),
    )
}

pub fn same_codec(c: &CheckInput) -> Option<Issue> {
    let v = c.probe.video.as_ref()?;
    (c.settings.codec.probe_name()? == v.codec).then_some(())?;
    let src = c.probe.video_bit_rate()? as f64;
    let out = c.video_kbps.map(|k| k as f64 * 1000.0).or_else(|| {
        let e = c.estimate?;
        let audio = (c.selection.audio_bytes(c.probe) + c.selection.subtitle_bytes(c.probe)) as f64;
        Some((((e.lo + e.hi) as f64 / 2.0 - audio).max(0.0)) * 8.0 / c.probe.duration)
    })?;
    (out >= src * 0.8).then_some(())?;
    issue(
        "samecodec",
        Severity::Warning,
        format!(
            "The source is already {} at a similar bitrate. Re-encoding only loses quality and time; copy the video or pick a smaller size.",
            c.settings.codec.label()
        ),
        Some(Fix::Codec(VideoCodec::Copy)),
    )
}

pub fn output_not_smaller(c: &CheckInput) -> Option<Issue> {
    let input = c.probe.size_bytes;
    let est = match (c.settings.size_mode, c.encoding()) {
        (SizeMode::TargetSize, true) if c.settings.bitrate_from_target() => {
            (c.settings.target_gb * estimate::GB) as u64
        }
        _ => c.estimate?.lo,
    };
    (input > 0 && est >= input).then_some(())?;
    issue(
        "bigger",
        Severity::Warning,
        "The estimated output is not smaller than the input, so this won't save space.",
        None,
    )
}

pub fn low_bitrate_source(c: &CheckInput) -> Option<Issue> {
    let v = c.probe.video.as_ref()?;
    c.encoding().then_some(())?;
    let kbps = c.probe.video_bit_rate()? as f64 / 1000.0;
    let min = estimate::min_bpp(source_codec_class(&v.codec));
    (estimate::bpp(kbps, v.width, v.height, v.fps) < min * 1.3).then_some(())?;
    issue(
        "lowsrc",
        Severity::Warning,
        format!(
            "The source is already low-bitrate ({:.1} Mb/s). Re-encoding will mainly add artefacts.",
            kbps / 1000.0
        ),
        Some(Fix::Codec(VideoCodec::Copy)),
    )
}

pub fn older_codec(c: &CheckInput) -> Option<Issue> {
    let v = c.probe.video.as_ref()?;
    let out = c.settings.codec.probe_name()?;
    (VideoCodec::generation(&v.codec) > VideoCodec::generation(out)).then_some(())?;
    issue(
        "oldercodec",
        Severity::Info,
        format!(
            "{} is less efficient than the source's {}, so the file will likely grow at the same quality. Fine if you need compatibility.",
            c.settings.codec.label(),
            v.codec.to_uppercase()
        ),
        None,
    )
}

/// Bitrate a larger target would need to look good, rounded up to 0.5 GB.
fn suggested_target(c: &CheckInput, w: u32, h: u32, fps: f64) -> f64 {
    let kbps = estimate::min_bpp(c.settings.codec) * 1.25 * w as f64 * h as f64 * fps / 1000.0;
    let bytes = kbps * 1000.0 * c.probe.duration / 8.0
        + (c.selection.audio_bytes(c.probe) + c.selection.subtitle_bytes(c.probe)) as f64;
    let gb =
        bytes / estimate::TARGET_FRACTION / (1.0 - estimate::CONTAINER_OVERHEAD) / estimate::GB;
    (gb * 2.0).ceil() / 2.0
}

/// Encoded picture size and frame rate, after cropping, downscaling and deinterlacing.
fn scaled_size(c: &CheckInput) -> Option<(u32, u32, f64)> {
    let v = c.probe.video.as_ref()?;
    let (w, h) = crate::ffmpeg::command::output_dimensions(c.probe, c.settings, c.active_crop)?;
    Some((w, h, v.fps * c.scan.rate_factor()))
}

pub fn target_bitrate(c: &CheckInput) -> Option<Issue> {
    (c.encoding() && c.settings.bitrate_from_target()).then_some(())?;
    let (w, h, fps) = scaled_size(c)?;
    let Some(kbps) = c.video_kbps else {
        return issue(
            "targetimpossible",
            Severity::Strong,
            "The selected audio and subtitles alone are bigger than the target size.",
            Some(Fix::ConvertLosslessAudio),
        );
    };
    match estimate::bitrate_verdict(kbps, c.settings.codec, w, h, fps) {
        BitrateVerdict::Fine => None,
        BitrateVerdict::TooLow { suggest_height } => {
            let fix = match suggest_height {
                Some(1080) => Some(Fix::Downscale(Downscale::P1080)),
                Some(720) => Some(Fix::Downscale(Downscale::P720)),
                _ => Some(Fix::Target(suggested_target(c, w, h, fps))),
            };
            issue(
                "lowtarget",
                Severity::Warning,
                format!(
                    "{:.1} Mb/s is too low for {h}p {} to look good. Downscale or pick a larger target.",
                    kbps as f64 / 1000.0,
                    c.settings.codec.label()
                ),
                fix,
            )
        }
    }
}

pub fn audio_share(c: &CheckInput) -> Option<Issue> {
    (c.settings.size_mode == SizeMode::TargetSize).then_some(())?;
    let target = c.settings.target_gb * estimate::GB;
    let audio = c.selection.audio_bytes(c.probe) as f64;
    let lossless = c.selection.selected_audio().any(|ch| {
        ch.action == AudioAction::Copy && c.probe.audio_by_index(ch.index).is_some_and(is_lossless)
    });
    (target > 0.0 && audio / target > 0.25 && lossless).then_some(())?;
    issue(
        "audioshare",
        Severity::Warning,
        format!(
            "Audio takes {:.0}% of the target size. Converting the lossless tracks (E-AC-3 5.1 at 640 kb/s) leaves more room for the video.",
            audio / target * 100.0
        ),
        Some(Fix::ConvertLosslessAudio),
    )
}

fn is_lossless(a: &crate::ffmpeg::probe::AudioStream) -> bool {
    matches!(a.codec.as_str(), "truehd" | "mlp" | "flac" | "alac")
        || a.codec.starts_with("pcm_")
        || (a.codec == "dts" && a.profile.contains("MA"))
}

fn output_audio_codec(a: &crate::ffmpeg::probe::AudioStream, action: AudioAction) -> String {
    action
        .target_codec()
        .map_or_else(|| a.codec.clone(), str::to_string)
}

pub fn container_conflicts(c: &CheckInput) -> Vec<Issue> {
    let mut v = Vec::new();
    if c.settings.container != Container::Mp4 {
        return v;
    }
    if c.selection.selected_subs().next().is_some() {
        v.extend(issue(
            "mp4subs",
            Severity::Warning,
            "MP4 output drops the selected subtitles. Use MKV to keep them.",
            Some(Fix::Mkv),
        ));
    }
    let bad: Vec<String> = c
        .selection
        .selected_audio()
        .filter_map(|ch| {
            let a = c.probe.audio_by_index(ch.index)?;
            let codec = output_audio_codec(a, ch.action);
            (matches!(codec.as_str(), "truehd" | "mlp" | "vorbis")
                || codec.starts_with("pcm_")
                || codec.starts_with("wma"))
            .then(|| tracks::codec_name(&codec, ""))
        })
        .collect();
    if !bad.is_empty() {
        v.extend(issue(
            "mp4audio",
            Severity::Strong,
            format!(
                "MP4 can't hold {} audio. Switch to MKV or convert those tracks.",
                bad.join(", ")
            ),
            Some(Fix::Mkv),
        ));
    }
    v
}

pub fn apple_tv(c: &CheckInput) -> Vec<Issue> {
    let mut v = Vec::new();
    let bad: Vec<String> = c
        .selection
        .selected_audio()
        .filter_map(|ch| {
            let a = c.probe.audio_by_index(ch.index)?;
            (!tracks::apple_compatible(&output_audio_codec(a, ch.action)))
                .then(|| tracks::audio_label(a))
        })
        .collect();
    if !bad.is_empty() {
        v.extend(issue(
            "atvaudio",
            Severity::Warning,
            format!(
                "Apple TV can't play {} natively; the NAS would have to transcode.",
                bad.join(", ")
            ),
            Some(Fix::ConvertIncompatibleAudio),
        ));
    }
    let pgs = c
        .selection
        .selected_subs()
        .filter(|s| {
            c.probe
                .subtitle_by_index(s.index)
                .is_some_and(|s| !tracks::is_text_subtitle(&s.codec))
        })
        .count();
    if pgs > 0 && c.settings.container == Container::Mkv {
        v.extend(issue(
            "atvpgs",
            Severity::Warning,
            format!("{pgs} PGS subtitle track(s) kept. PGS may force Plex to transcode (burn in) when turned on."),
            Some(Fix::DropImageSubtitles),
        ));
    }
    if c.settings.codec == VideoCodec::Av1
        || (c.settings.codec == VideoCodec::Copy
            && c.probe.video.as_ref().is_some_and(|v| v.codec == "av1"))
    {
        v.extend(issue(
            "atvav1",
            Severity::Warning,
            "Older Apple TV 4K models have no AV1 hardware decoding, so the NAS may have to transcode the video.",
            Some(Fix::Codec(VideoCodec::Hevc)),
        ));
    }
    v
}

/// Codecs the RDNA4 VCN decodes on the GPU (8/10-bit 4:2:0).
pub fn hw_decodable(codec: &str, pix_fmt: &str, bit_depth: u32) -> bool {
    let is_420 = pix_fmt.is_empty()
        || pix_fmt.contains("420")
        || pix_fmt == "nv12"
        || pix_fmt.starts_with("p010");
    match codec {
        "h264" => is_420 && bit_depth == 8,
        "hevc" | "vp9" | "av1" => is_420 && bit_depth <= 10,
        _ => false,
    }
}

pub fn software_decode(c: &CheckInput) -> Option<Issue> {
    let v = c.probe.video.as_ref()?;
    (c.encoding() && !hw_decodable(&v.codec, &v.pix_fmt, v.bit_depth)).then_some(())?;
    issue(
        "swdecode",
        Severity::Info,
        format!(
            "The GPU can't decode {} {}, so this job uses the slower software-decode path.",
            v.codec.to_uppercase(),
            v.pix_fmt
        ),
        None,
    )
}

pub fn dolby_vision(c: &CheckInput) -> Vec<Issue> {
    let mut v = Vec::new();
    let Some(d) = c.probe.hdr.dovi else {
        return v;
    };
    if c.settings.codec == VideoCodec::Copy {
        return v;
    }
    if c.dv_preserve {
        v.extend(issue("dvexp", Severity::Info, "Dolby Vision preservation is experimental. The output's DV profile is checked after muxing.", None));
        if c.crop {
            v.extend(issue("dvcrop", Severity::Info, "Cropping is turned off for this job because the Dolby Vision metadata describes the full frame.", None));
        }
    } else if d.profile == 5 {
        v.extend(issue(
            "dv5",
            Severity::Strong,
            "Dolby Vision profile 5 has no HDR10 base layer: without Dolby Vision the colours will be wrong (green/purple).",
            None,
        ));
    } else {
        v.extend(issue("dv", Severity::Strong, "Dolby Vision will be removed; the output will be HDR10. Apple TV will still play it in HDR10.", None));
    }
    if c.probe.hdr.hdr10plus {
        v.extend(issue(
            "hdr10plus",
            Severity::Warning,
            "HDR10+ dynamic metadata will be lost; static HDR10 remains.",
            None,
        ));
    }
    v
}

pub fn hdr10plus(c: &CheckInput) -> Option<Issue> {
    (c.encoding() && c.probe.hdr.hdr10plus && c.probe.hdr.dovi.is_none()).then_some(())?;
    issue(
        "hdr10plus",
        Severity::Warning,
        "HDR10+ dynamic metadata will be lost; static HDR10 remains.",
        None,
    )
}

pub fn vmaf_low(c: &CheckInput) -> Option<Issue> {
    let score = c.vmaf?;
    (c.settings.size_mode == SizeMode::TargetSize && score < crate::ffmpeg::vmaf::SUGGEST_BELOW)
        .then_some(())?;
    let (w, h, fps) = scaled_size(c)?;
    issue(
        "vmaflow",
        Severity::Warning,
        format!(
            "Sample quality is VMAF {score:.0} at this size. A larger target or downscaling would look better."
        ),
        Some(Fix::Target(
            suggested_target(c, w, h, fps).max(c.settings.target_gb + 1.0),
        )),
    )
}

pub fn encoder_size(c: &CheckInput) -> Option<Issue> {
    let limits = c.encoder_limits.filter(|_| c.encoding())?;
    let (w, h) = crate::ffmpeg::command::output_dimensions(c.probe, c.settings, c.active_crop)?;
    if limits.fits(w, h) {
        return None;
    }
    let fix = if c.settings.codec == VideoCodec::H264 {
        VideoCodec::Copy
    } else {
        VideoCodec::H264
    };
    issue(
        "encsize",
        Severity::Strong,
        format!(
            "{w}×{h} is outside what the {} hardware encoder accepts ({}–{} × {}–{} pixels).",
            c.settings.codec.label(),
            limits.min_w,
            limits.max_w,
            limits.min_h,
            limits.max_h
        ),
        Some(Fix::Codec(fix)),
    )
}

pub fn interlacing(c: &CheckInput) -> Option<Issue> {
    let v = c.probe.video.as_ref()?;
    let flagged = matches!(v.field_order.as_deref(), Some("tt" | "bb" | "tb" | "bt"));
    if !c.encoding() {
        let interlaced = matches!(
            c.scan_detected,
            Some(ScanType::Interlaced { .. } | ScanType::Telecine { .. })
        );
        return interlaced.then_some(()).and_then(|_| {
            issue(
                "scancopy",
                Severity::Info,
                "The video is copied as it is, so it stays interlaced. Apple TV deinterlaces it while playing.",
                None,
            )
        });
    }
    match c.scan {
        Scan::Progressive if flagged && c.scan_detected == Some(ScanType::Progressive) => issue(
            "scanflag",
            Severity::Info,
            "The video is flagged as interlaced, but the picture is progressive, so it isn't deinterlaced.",
            None,
        ),
        Scan::Progressive => None,
        Scan::Deinterlace { .. } => issue(
            "deinterlace",
            Severity::Info,
            format!(
                "Interlaced video: {} {}.",
                c.scan.describe(v.fps),
                "on the CPU (bwdif)"
            ),
            None,
        ),
        Scan::Telecine { .. } => issue(
            "telecine",
            Severity::Info,
            format!("Telecined film: {} on the CPU.", c.scan.describe(v.fps)),
            None,
        ),
    }
}

/// All checks, most severe first.
pub fn run(c: &CheckInput) -> Vec<Issue> {
    let mut v: Vec<Issue> = [
        eight_to_ten(c),
        hdr_to_eight(c),
        hdr_to_h264(c),
        same_codec(c),
        output_not_smaller(c),
        low_bitrate_source(c),
        older_codec(c),
        target_bitrate(c),
        audio_share(c),
        software_decode(c),
        hdr10plus(c),
        vmaf_low(c),
        encoder_size(c),
        interlacing(c),
    ]
    .into_iter()
    .flatten()
    .chain(container_conflicts(c))
    .chain(apple_tv(c))
    .chain(dolby_vision(c))
    .collect();
    v.sort_by_key(|i| std::cmp::Reverse(i.severity));
    v
}

/// Apply a one-click fix.
pub fn apply_fix(fix: Fix, p: &ProbeInfo, s: &mut EncodeSettings, sel: &mut Selection) {
    match fix {
        Fix::TenBit => {
            if !s.codec.supports_10bit() {
                s.codec = VideoCodec::Hevc;
            }
            s.ten_bit = true;
        }
        Fix::Codec(c) => {
            if c != s.codec && c != VideoCodec::Copy {
                s.qp = c.default_qp();
            }
            s.codec = c;
        }
        Fix::Mkv => s.container = Container::Mkv,
        Fix::ConvertIncompatibleAudio => {
            let forced = EncodeSettings {
                apple_tv_audio: true,
                ..s.clone()
            };
            for ch in sel.audio.iter_mut().filter(|ch| ch.selected) {
                if let Some(a) = p.audio_by_index(ch.index)
                    && !tracks::apple_compatible(&output_audio_codec(a, ch.action))
                {
                    ch.action = tracks::default_action(a, &forced);
                }
            }
        }
        Fix::ConvertLosslessAudio => {
            for ch in sel
                .audio
                .iter_mut()
                .filter(|ch| ch.selected && ch.action == AudioAction::Copy)
            {
                if let Some(a) = p.audio_by_index(ch.index)
                    && is_lossless(a)
                {
                    ch.action = if a.channels > 2 {
                        AudioAction::Eac3 { kbps: 640 }
                    } else {
                        AudioAction::Aac {
                            kbps: 224,
                            stereo: true,
                        }
                    };
                }
            }
        }
        Fix::DropImageSubtitles => {
            for ch in &mut sel.subs {
                if p.subtitle_by_index(ch.index)
                    .is_some_and(|s| !tracks::is_text_subtitle(&s.codec))
                {
                    ch.selected = false;
                }
            }
        }
        Fix::Target(gb) => s.target_gb = gb,
        Fix::Downscale(d) => s.downscale = d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe::{self, tests::sample_4k};
    use crate::tracks::auto_select;

    fn sdr_1080(codec: &str, bitrate: u64) -> ProbeInfo {
        probe::parse(&format!(
            r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"{codec}","width":1920,"height":1080,
            "pix_fmt":"yuv420p","avg_frame_rate":"24/1","bit_rate":"{bitrate}"}},
            {{"index":1,"codec_type":"audio","codec_name":"ac3","channels":6,"bit_rate":"448000","tags":{{"language":"eng"}}}}],
            "format":{{"duration":"6000","size":"{}"}}}}"#,
            bitrate * 6000 / 8 + 336_000_000
        ))
        .unwrap()
    }

    struct Fx {
        p: ProbeInfo,
        s: EncodeSettings,
        sel: Selection,
    }

    impl Fx {
        fn new(p: ProbeInfo, s: EncodeSettings) -> Self {
            let (sel, _, _) = auto_select(&p, &["eng".into()], &s);
            Self { p, s, sel }
        }
        fn input(&self) -> CheckInput<'_> {
            let audio = self.sel.audio_bytes(&self.p);
            let subs = self.sel.subtitle_bytes(&self.p);
            CheckInput {
                probe: &self.p,
                settings: &self.s,
                selection: &self.sel,
                video_kbps: estimate::target_video_kbps(
                    self.s.target_gb,
                    self.p.duration,
                    audio,
                    subs,
                ),
                estimate: None,
                vmaf: None,
                dv_preserve: false,
                crop: false,
                active_crop: None,
                encoder_limits: None,
                scan_detected: None,
                scan: Scan::Progressive,
            }
        }
    }

    #[test]
    fn bitrate_checks_use_the_encoded_picture() {
        let f = Fx::new(sdr_1080("h264", 20_000_000), EncodeSettings::default());
        let mut c = f.input();
        let fps = f.p.video.as_ref().unwrap().fps;
        c.active_crop = Some(crate::crop::Crop {
            w: 1920,
            h: 800,
            x: 0,
            y: 140,
        });
        c.scan = Scan::Deinterlace {
            tff: true,
            double_rate: true,
        };
        assert_eq!(scaled_size(&c), Some((1920, 800, fps * 2.0)));
    }

    fn ids(v: &[Issue]) -> Vec<&'static str> {
        v.iter().map(|i| i.id).collect()
    }

    #[test]
    fn eight_bit_to_ten_is_info() {
        let f = Fx::new(sdr_1080("h264", 20_000_000), EncodeSettings::default());
        let i = eight_to_ten(&f.input()).unwrap();
        assert_eq!(i.severity, Severity::Info);
        let f = Fx::new(
            sdr_1080("h264", 20_000_000),
            EncodeSettings {
                ten_bit: false,
                ..Default::default()
            },
        );
        assert!(eight_to_ten(&f.input()).is_none());
    }

    #[test]
    fn hdr_to_eight_bit_is_strong_with_fix() {
        let mut f = Fx::new(
            sample_4k(),
            EncodeSettings {
                ten_bit: false,
                ..Default::default()
            },
        );
        let i = hdr_to_eight(&f.input()).unwrap();
        assert_eq!((i.severity, i.fix), (Severity::Strong, Some(Fix::TenBit)));
        apply_fix(Fix::TenBit, &f.p, &mut f.s, &mut f.sel);
        assert!(hdr_to_eight(&f.input()).is_none());
    }

    #[test]
    fn hdr_to_h264() {
        let f = Fx::new(
            sample_4k(),
            EncodeSettings {
                codec: VideoCodec::H264,
                ..Default::default()
            },
        );
        let i = super::hdr_to_h264(&f.input()).unwrap();
        assert_eq!(i.fix, Some(Fix::Codec(VideoCodec::Hevc)));
        // Not reported twice.
        assert!(hdr_to_eight(&f.input()).is_none());
    }

    #[test]
    fn ten_bit_sdr_is_not_treated_as_hdr() {
        let mut p = sdr_1080("hevc", 20_000_000);
        p.video.as_mut().unwrap().bit_depth = 10;
        let eight = EncodeSettings {
            ten_bit: false,
            ..Default::default()
        };
        let f = Fx::new(p.clone(), eight);
        let i = hdr_to_eight(&f.input()).unwrap();
        assert_eq!((i.severity, i.fix), (Severity::Warning, Some(Fix::TenBit)));
        let f = Fx::new(
            p,
            EncodeSettings {
                codec: VideoCodec::H264,
                ..Default::default()
            },
        );
        assert!(super::hdr_to_h264(&f.input()).is_none());
        let i = hdr_to_eight(&f.input()).unwrap();
        assert_eq!((i.severity, i.fix), (Severity::Warning, None));
    }

    #[test]
    fn same_codec_similar_bitrate() {
        // HEVC 1080p at 3 Mb/s, target size gives more than that.
        let f = Fx::new(sdr_1080("hevc", 3_000_000), EncodeSettings::default());
        assert_eq!(
            same_codec(&f.input()).unwrap().fix,
            Some(Fix::Codec(VideoCodec::Copy))
        );
        let f = Fx::new(sdr_1080("hevc", 30_000_000), EncodeSettings::default());
        assert!(same_codec(&f.input()).is_none());
        let f = Fx::new(sdr_1080("h264", 3_000_000), EncodeSettings::default());
        assert!(same_codec(&f.input()).is_none());
    }

    #[test]
    fn output_not_smaller_than_input() {
        // ~2.6 GB input, 4 GB target.
        let f = Fx::new(sdr_1080("h264", 3_000_000), EncodeSettings::default());
        assert!(output_not_smaller(&f.input()).is_some());
        let f = Fx::new(sdr_1080("h264", 30_000_000), EncodeSettings::default());
        assert!(output_not_smaller(&f.input()).is_none());
        // Quality mode uses the estimate.
        let f = Fx::new(
            sdr_1080("h264", 30_000_000),
            EncodeSettings {
                size_mode: SizeMode::ConstantQuality,
                ..Default::default()
            },
        );
        let mut c = f.input();
        assert!(output_not_smaller(&c).is_none());
        c.estimate = Some(SizeRange {
            lo: 30_000_000_000,
            hi: 31_000_000_000,
        });
        assert!(output_not_smaller(&c).is_some());
    }

    #[test]
    fn low_bitrate_source_warns() {
        let f = Fx::new(sdr_1080("h264", 2_000_000), EncodeSettings::default());
        assert!(low_bitrate_source(&f.input()).is_some());
        let f = Fx::new(sdr_1080("h264", 15_000_000), EncodeSettings::default());
        assert!(low_bitrate_source(&f.input()).is_none());
    }

    #[test]
    fn older_codec_is_info() {
        let f = Fx::new(
            sdr_1080("hevc", 15_000_000),
            EncodeSettings {
                codec: VideoCodec::H264,
                ten_bit: false,
                ..Default::default()
            },
        );
        assert_eq!(older_codec(&f.input()).unwrap().severity, Severity::Info);
        let f = Fx::new(sdr_1080("h264", 15_000_000), EncodeSettings::default());
        assert!(older_codec(&f.input()).is_none());
    }

    #[test]
    fn low_target_suggests_downscale_or_bigger_target() {
        // 4K, 1 GB over 2 h with E-AC-3 audio ≈ 0.3 Mb/s video: too low even at 720p.
        let mut f = Fx::new(
            sample_4k(),
            EncodeSettings {
                target_gb: 1.0,
                ..Default::default()
            },
        );
        let i = target_bitrate(&f.input()).unwrap();
        assert!(matches!(i.fix, Some(Fix::Target(gb)) if gb > 1.0));
        // 2 GB ≈ 1.4 Mb/s: enough for 720p only.
        f.s.target_gb = 2.0;
        assert_eq!(
            target_bitrate(&f.input()).unwrap().fix,
            Some(Fix::Downscale(Downscale::P720))
        );
        f.s.target_gb = 4.5;
        let i = target_bitrate(&f.input()).unwrap();
        assert_eq!(i.fix, Some(Fix::Downscale(Downscale::P1080)));
        f.s.downscale = Downscale::P1080;
        assert!(target_bitrate(&f.input()).is_none());
    }

    #[test]
    fn audio_too_big_for_target() {
        let p = sample_4k();
        let s = EncodeSettings {
            target_gb: 3.0,
            apple_tv_audio: false,
            ..Default::default()
        };
        let (mut sel, _, _) = auto_select(&p, &["eng".into()], &s);
        for c in &mut sel.audio {
            c.selected = c.index == 1;
        }
        let mut f = Fx { p, s, sel };
        let c = f.input();
        assert_eq!(
            audio_share(&c).unwrap().fix,
            Some(Fix::ConvertLosslessAudio)
        );
        // 4.05 GB of TrueHD alone is bigger than 3 GB.
        assert_eq!(target_bitrate(&c).unwrap().id, "targetimpossible");
        apply_fix(Fix::ConvertLosslessAudio, &f.p, &mut f.s, &mut f.sel);
        assert!(audio_share(&f.input()).is_none());
    }

    #[test]
    fn mp4_conflicts() {
        let p = sample_4k();
        let s = EncodeSettings {
            container: Container::Mp4,
            apple_tv_audio: false,
            ..Default::default()
        };
        let (mut sel, _, _) = auto_select(&p, &["eng".into()], &s);
        sel.audio.iter_mut().for_each(|c| c.selected = c.index == 1);
        let mut f = Fx { p, s, sel };
        assert_eq!(
            ids(&container_conflicts(&f.input())),
            vec!["mp4subs", "mp4audio"]
        );
        apply_fix(Fix::Mkv, &f.p, &mut f.s, &mut f.sel);
        assert!(container_conflicts(&f.input()).is_empty());
    }

    #[test]
    fn apple_tv_compatibility() {
        let p = sample_4k();
        let s = EncodeSettings {
            codec: VideoCodec::Av1,
            apple_tv_audio: false,
            ..Default::default()
        };
        let (mut sel, _, _) = auto_select(&p, &["eng".into(), "nor".into()], &s);
        sel.subs.iter_mut().for_each(|c| c.selected |= c.index == 8);
        let mut f = Fx { p, s, sel };
        assert_eq!(
            ids(&apple_tv(&f.input())),
            vec!["atvaudio", "atvpgs", "atvav1"]
        );
        apply_fix(Fix::ConvertIncompatibleAudio, &f.p, &mut f.s, &mut f.sel);
        apply_fix(Fix::DropImageSubtitles, &f.p, &mut f.s, &mut f.sel);
        apply_fix(Fix::Codec(VideoCodec::Hevc), &f.p, &mut f.s, &mut f.sel);
        assert!(apple_tv(&f.input()).is_empty());
        assert_eq!(f.s.qp, VideoCodec::Hevc.default_qp());
    }

    #[test]
    fn software_decode_info() {
        let f = Fx::new(
            sdr_1080("mpeg2video", 15_000_000),
            EncodeSettings::default(),
        );
        assert!(software_decode(&f.input()).is_some());
        let f = Fx::new(sdr_1080("h264", 15_000_000), EncodeSettings::default());
        assert!(software_decode(&f.input()).is_none());
        assert!(!hw_decodable("h264", "yuv420p10le", 10));
        assert!(!hw_decodable("hevc", "yuv422p10le", 10));
        assert!(hw_decodable("av1", "yuv420p10le", 10));
    }

    #[test]
    fn dolby_vision_warnings() {
        let f = Fx::new(sample_4k(), EncodeSettings::default());
        let mut c = f.input();
        assert_eq!(ids(&dolby_vision(&c)), vec!["dv"]);
        assert_eq!(dolby_vision(&c)[0].severity, Severity::Strong);
        c.dv_preserve = true;
        c.crop = true;
        assert_eq!(ids(&dolby_vision(&c)), vec!["dvexp", "dvcrop"]);
        let mut p5 = sample_4k();
        p5.hdr.dovi.as_mut().unwrap().profile = 5;
        let f5 = Fx::new(p5, EncodeSettings::default());
        assert_eq!(ids(&dolby_vision(&f5.input())), vec!["dv5"]);
    }

    #[test]
    fn vmaf_low_suggests_bigger_target() {
        let f = Fx::new(sample_4k(), EncodeSettings::default());
        let mut c = f.input();
        c.vmaf = Some(85.0);
        assert!(matches!(vmaf_low(&c).unwrap().fix, Some(Fix::Target(gb)) if gb >= 5.0));
        c.vmaf = Some(94.0);
        assert!(vmaf_low(&c).is_none());
    }

    #[test]
    fn encoder_size_limits() {
        use crate::ffmpeg::capabilities::SizeLimits;
        let limits = Some(SizeLimits {
            min_w: 384,
            max_w: 8192,
            min_h: 128,
            max_h: 4352,
        });
        let small = probe::parse(r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":320,"height":240}],"format":{"duration":"10"}}"#).unwrap();
        let f = Fx::new(small, EncodeSettings::default());
        let mut c = f.input();
        assert!(encoder_size(&c).is_none());
        c.encoder_limits = limits;
        let i = encoder_size(&c).unwrap();
        assert_eq!(
            (i.severity, i.fix),
            (Severity::Strong, Some(Fix::Codec(VideoCodec::H264)))
        );
        let f = Fx::new(sample_4k(), EncodeSettings::default());
        let mut c = f.input();
        c.encoder_limits = limits;
        assert!(encoder_size(&c).is_none());
    }

    #[test]
    fn interlacing_notes() {
        let p = probe::parse(r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mpeg2video","width":720,"height":576,"field_order":"tt","avg_frame_rate":"25/1"}],"format":{"duration":"10"}}"#).unwrap();
        let f = Fx::new(p, EncodeSettings::default());
        let mut c = f.input();
        c.scan_detected = Some(ScanType::Progressive);
        assert_eq!(interlacing(&c).unwrap().id, "scanflag");
        c.scan_detected = Some(ScanType::Interlaced { tff: true });
        c.scan = Scan::Deinterlace {
            tff: true,
            double_rate: true,
        };
        let i = interlacing(&c).unwrap();
        assert_eq!(
            i.message,
            "Interlaced video: Deinterlacing to 50p on the CPU (bwdif)."
        );
        let copy = EncodeSettings {
            codec: VideoCodec::Copy,
            ..Default::default()
        };
        c.settings = &copy;
        assert_eq!(interlacing(&c).unwrap().id, "scancopy");
    }

    #[test]
    fn run_sorts_by_severity() {
        let f = Fx::new(
            sample_4k(),
            EncodeSettings {
                ten_bit: false,
                codec: VideoCodec::Av1,
                ..Default::default()
            },
        );
        let v = run(&f.input());
        assert_eq!(v[0].severity, Severity::Strong);
        assert!(v.windows(2).all(|w| w[0].severity >= w[1].severity));
    }
}
