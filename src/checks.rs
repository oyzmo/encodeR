//! Input-vs-output sanity checks: pure `(probe, settings, selection) → Vec<Issue>`,
//! each with a severity and, where it makes sense, a one-click fix.

use crate::ffmpeg::estimate::{self, BitrateVerdict, SizeRange};
use crate::ffmpeg::probe::ProbeInfo;
use crate::scan::{Scan, ScanType};
use crate::settings::{Container, Downscale, EncodeSettings, RateControl, SizeMode, VideoCodec};
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
    ConstantQuality,
    AutoRateControl,
    /// HEVC with 10-bit on, in one click.
    HevcTenBit,
    /// Convert the tracks MP4 can't hold.
    ConvertMp4Audio,
    /// Turn cropping off (job state, handled by the window).
    NoCrop,
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
            Fix::ConstantQuality => "Use constant quality".into(),
            Fix::HevcTenBit => "Switch to HEVC 10-bit".into(),
            Fix::AutoRateControl => "Use automatic".into(),
            Fix::ConvertMp4Audio => "Convert those tracks".into(),
            Fix::NoCrop => "Turn off crop".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    pub fix: Option<Fix>,
    /// A second one-click fix, shown next to the first.
    pub alt_fix: Option<Fix>,
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
        alt_fix: None,
    })
}

fn issue2(
    id: &'static str,
    severity: Severity,
    message: impl Into<String>,
    fix: Fix,
    alt_fix: Fix,
) -> Option<Issue> {
    issue(id, severity, message, Some(fix)).map(|i| Issue {
        alt_fix: Some(alt_fix),
        ..i
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
        Some(Fix::HevcTenBit),
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
        if c.probe.duration <= 0.0 {
            return issue(
                "targetimpossible",
                Severity::Strong,
                "The file's duration is unknown, so the target size can't be turned into a bitrate. Use constant quality.",
                Some(Fix::ConstantQuality),
            );
        }
        let fix = if lossless_copied(c) {
            Fix::ConvertLosslessAudio
        } else {
            Fix::Target(suggested_target(c, w, h, fps))
        };
        return issue(
            "targetimpossible",
            Severity::Strong,
            "The selected audio and subtitles leave no room for the video in this target size. The job won't start until this is fixed.",
            Some(fix),
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
    (target > 0.0 && audio / target > 0.25 && lossless_copied(c)).then_some(())?;
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

/// A selected track is a lossless copy (what `Fix::ConvertLosslessAudio` changes).
fn lossless_copied(c: &CheckInput) -> bool {
    c.selection.selected_audio().any(|ch| {
        ch.action == AudioAction::Copy
            && c.probe
                .audio_by_index(ch.index)
                .is_some_and(tracks::is_lossless)
    })
}

fn output_audio_codec(a: &crate::ffmpeg::probe::AudioStream, action: AudioAction) -> String {
    action
        .target_codec()
        .map_or_else(|| a.codec.clone(), str::to_string)
}

fn mp4_rejects(codec: &str) -> bool {
    matches!(codec, "truehd" | "mlp" | "vorbis")
        || codec.starts_with("pcm_")
        || codec.starts_with("wma")
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
            mp4_rejects(&codec).then(|| tracks::codec_name(&codec, ""))
        })
        .collect();
    if !bad.is_empty() {
        v.extend(issue2(
            "mp4audio",
            Severity::Strong,
            format!(
                "MP4 can't hold {} audio. Switch to MKV or convert those tracks.",
                bad.join(", ")
            ),
            Fix::Mkv,
            Fix::ConvertMp4Audio,
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
    let copied = (c.settings.codec == VideoCodec::Copy)
        .then_some(c.probe.video.as_ref())
        .flatten();
    if let Some(src) = copied.filter(|s| !matches!(s.codec.as_str(), "av1" | "hevc"))
        && !(src.codec == "h264" && src.bit_depth <= 8)
    {
        let what = if src.codec == "h264" {
            "10-bit H.264".to_string()
        } else {
            src.codec.to_uppercase()
        };
        v.extend(issue(
            "atvvideo",
            Severity::Warning,
            format!("Apple TV can't play {what} video natively, so the NAS would have to transcode it. Re-encode to HEVC."),
            Some(Fix::Codec(VideoCodec::Hevc)),
        ));
    }
    if copied.is_some() && c.probe.hdr.dovi.is_some_and(|d| d.profile == 7) {
        v.extend(issue(
            "atvdv7",
            Severity::Info,
            "Dolby Vision profile 7 is copied as it is: Apple TV plays it as HDR10, not Dolby Vision.",
            None,
        ));
    }
    if c.settings.codec == VideoCodec::Av1 || copied.is_some_and(|v| v.codec == "av1") {
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
/// Size mode and an explicit rate-control mode that ignore each other.
pub fn rate_control_override(c: &CheckInput) -> Option<Issue> {
    c.encoding().then_some(())?;
    let s = c.settings;
    let msg = match (s.size_mode, s.rate_control) {
        (_, RateControl::Auto) => return None,
        (SizeMode::TargetSize, rc) if !s.bitrate_from_target() => format!(
            "Rate control {} (Advanced) overrides the target size: the size picks do nothing and the file can end up much bigger.",
            rc.label()
        ),
        (SizeMode::ConstantQuality, rc @ (RateControl::Vbr | RateControl::Cbr)) => format!(
            "Rate control {} (Advanced) overrides constant quality: the quality slider does nothing; the manual bitrate is used.",
            rc.label()
        ),
        _ => return None,
    };
    issue(
        "rcoverride",
        Severity::Warning,
        msg,
        Some(Fix::AutoRateControl),
    )
}

/// Image subtitles are positioned on the uncropped frame.
pub fn crop_image_subs(c: &CheckInput) -> Option<Issue> {
    (c.encoding() && c.active_crop.is_some()).then_some(())?;
    c.selection
        .selected_subs()
        .any(|ch| {
            c.probe
                .subtitle_by_index(ch.index)
                .is_some_and(|s| !tracks::is_text_subtitle(&s.codec))
        })
        .then_some(())?;
    issue2(
        "cropimgsubs",
        Severity::Warning,
        "Cropping with PGS/VobSub subtitles kept: they are drawn for the uncropped picture and often sit in the black bars, so players may place them off the picture or squeeze them.",
        Fix::DropImageSubtitles,
        Fix::NoCrop,
    )
}

/// A downscale that the crop already made pointless.
pub fn downscale_after_crop(c: &CheckInput) -> Option<Issue> {
    let target = c.settings.downscale.height()?;
    let crop = c.active_crop?;
    (c.encoding() && target >= crop.h).then_some(())?;
    issue(
        "cropscale",
        Severity::Info,
        format!(
            "The cropped picture is already {}px high, so the {} downscale is skipped.",
            crop.h,
            c.settings.downscale.label()
        ),
        None,
    )
}

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
        rate_control_override(c),
        crop_image_subs(c),
        downscale_after_crop(c),
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
                    && tracks::is_lossless(a)
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
        Fix::ConstantQuality => {
            s.size_mode = SizeMode::ConstantQuality;
            s.rate_control = RateControl::Auto;
        }
        Fix::AutoRateControl => s.rate_control = RateControl::Auto,
        Fix::HevcTenBit => {
            if s.codec != VideoCodec::Hevc {
                s.qp = VideoCodec::Hevc.default_qp();
            }
            s.codec = VideoCodec::Hevc;
            s.ten_bit = true;
        }
        Fix::ConvertMp4Audio => {
            for ch in sel.audio.iter_mut().filter(|ch| ch.selected) {
                if let Some(a) = p.audio_by_index(ch.index)
                    && mp4_rejects(&output_audio_codec(a, ch.action))
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
        // Job state, not settings: the window turns cropping off.
        Fix::NoCrop => {}
    }
}

/// Checks for external subtitle files and subtitles-only jobs.
pub fn external_issues(
    video: &std::path::Path,
    settings: &EncodeSettings,
    mode: crate::subs::JobMode,
    externals: &[crate::subs::ExternalSub],
    alass: bool,
) -> Vec<Issue> {
    use crate::subs::{self, JobMode, SubsTarget};
    let selected: Vec<_> = externals.iter().filter(|e| e.selected).collect();
    let mut v: Vec<Option<Issue>> = Vec::new();
    if let JobMode::Subtitles(target) = mode {
        if selected.is_empty() {
            v.push(issue(
                "nosubs",
                Severity::Strong,
                "No subtitle files to add. Use “Add Subtitle File…” or put files like Movie.no.srt next to the video.",
                None,
            ));
        }
        match target {
            SubsTarget::Sidecar => {
                if let Err(e) = subs::sidecar_outputs(video, externals) {
                    v.push(issue("sidecarclash", Severity::Strong, e, None));
                }
            }
            SubsTarget::Embed => match subs::embed_container(video) {
                Err(e) => v.push(issue("embedcontainer", Severity::Strong, e, None)),
                Ok(_) => {
                    if selected.iter().any(|e| e.is_sidecar_of(video)) {
                        v.push(issue(
                            "sidecarstays",
                            Severity::Info,
                            "The .srt files next to the video stay where they are after embedding; Plex will list those subtitles twice until you remove them.",
                            None,
                        ));
                    }
                }
            },
        }
    } else if !selected.is_empty() && settings.container == Container::Mp4 {
        v.push(issue(
            "mp4movtext",
            Severity::Info,
            "External subtitles are stored as MP4 text subtitles (mov_text), which Apple TV plays; styling is lost.",
            None,
        ));
    }
    if !alass && selected.iter().any(|e| e.sync) {
        v.push(issue(
            "noalass",
            Severity::Info,
            "alass isn't installed, so subtitles can't be synced automatically; only the manual offset is applied.",
            None,
        ));
    }
    let unknown: Vec<String> = selected
        .iter()
        .filter(|e| e.language.is_none())
        .map(|e| e.file_name())
        .collect();
    if !unknown.is_empty() {
        v.push(issue(
            "sublang",
            Severity::Warning,
            format!(
                "Language unknown for {}. Pick one, or Plex shows the subtitles as “Unknown”.",
                unknown.join(", ")
            ),
            None,
        ));
    }
    let mut out: Vec<Issue> = v.into_iter().flatten().collect();
    out.sort_by_key(|i| std::cmp::Reverse(i.severity));
    out
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
        assert_eq!(i.fix, Some(Fix::HevcTenBit));
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

    #[test]
    fn explicit_rate_control_overriding_the_size_mode() {
        let mut f = Fx::new(sample_4k(), EncodeSettings::default());
        assert!(rate_control_override(&f.input()).is_none());
        f.s.rate_control = RateControl::Cqp;
        let i = rate_control_override(&f.input()).unwrap();
        assert_eq!(i.fix, Some(Fix::AutoRateControl));
        apply_fix(Fix::AutoRateControl, &f.p, &mut f.s, &mut f.sel);
        assert!(f.s.bitrate_from_target());
        f.s.size_mode = SizeMode::ConstantQuality;
        f.s.rate_control = RateControl::Vbr;
        assert!(rate_control_override(&f.input()).is_some());
        f.s.rate_control = RateControl::Qvbr;
        assert!(rate_control_override(&f.input()).is_none());
    }

    #[test]
    fn crop_with_image_subtitles() {
        let mut f = Fx::new(sample_4k(), EncodeSettings::default());
        for c in &mut f.sel.subs {
            c.selected = c.index == 7; // PGS
        }
        let crop = crate::crop::Crop {
            w: 3840,
            h: 1608,
            x: 0,
            y: 276,
        };
        let mut c = f.input();
        assert!(crop_image_subs(&c).is_none());
        c.active_crop = Some(crop);
        let i = crop_image_subs(&c).unwrap();
        assert_eq!(
            (i.fix, i.alt_fix),
            (Some(Fix::DropImageSubtitles), Some(Fix::NoCrop))
        );
    }

    #[test]
    fn impossible_target_fixes() {
        // Huge lossless audio copied: convert it.
        let mut f = Fx::new(sample_4k(), EncodeSettings::default());
        for c in &mut f.sel.audio {
            c.selected = c.index == 1; // TrueHD 7.1, copied
            c.action = AudioAction::Copy;
        }
        f.s.target_gb = 2.0;
        let i = target_bitrate(&f.input()).unwrap();
        assert_eq!(
            (i.id, i.fix),
            ("targetimpossible", Some(Fix::ConvertLosslessAudio))
        );
        // No lossless copy left to convert: a larger target instead.
        apply_fix(Fix::ConvertLosslessAudio, &f.p, &mut f.s, &mut f.sel);
        f.s.target_gb = 0.3;
        let i = target_bitrate(&f.input()).unwrap();
        assert!(matches!(i.fix, Some(Fix::Target(gb)) if gb > 0.3), "{i:?}");
        // Unknown duration: constant quality.
        f.p.duration = 0.0;
        let i = target_bitrate(&f.input()).unwrap();
        assert_eq!(i.fix, Some(Fix::ConstantQuality));
        apply_fix(Fix::ConstantQuality, &f.p, &mut f.s, &mut f.sel);
        assert!(!f.s.bitrate_from_target());
    }

    #[test]
    fn hdr_to_h264_fix_is_one_click() {
        let mut f = Fx::new(
            sample_4k(),
            EncodeSettings {
                codec: VideoCodec::H264,
                ten_bit: false,
                ..Default::default()
            },
        );
        let i = super::hdr_to_h264(&f.input()).unwrap();
        apply_fix(i.fix.unwrap(), &f.p, &mut f.s, &mut f.sel);
        let c = f.input();
        assert!(super::hdr_to_h264(&c).is_none() && hdr_to_eight(&c).is_none());
    }

    #[test]
    fn copied_video_apple_tv_cannot_play() {
        let s = EncodeSettings {
            codec: VideoCodec::Copy,
            ..Default::default()
        };
        let f = Fx::new(sdr_1080("vc1", 20_000_000), s.clone());
        assert!(ids(&apple_tv(&f.input())).contains(&"atvvideo"));
        let f = Fx::new(sdr_1080("h264", 20_000_000), s);
        assert!(!ids(&apple_tv(&f.input())).contains(&"atvvideo"));
    }

    #[test]
    fn mp4_audio_can_be_converted_instead() {
        let mut f = Fx::new(
            sample_4k(),
            EncodeSettings {
                container: Container::Mp4,
                apple_tv_audio: false,
                ..Default::default()
            },
        );
        let i = container_conflicts(&f.input())
            .into_iter()
            .find(|i| i.id == "mp4audio")
            .unwrap();
        assert_eq!(i.alt_fix, Some(Fix::ConvertMp4Audio));
        apply_fix(Fix::ConvertMp4Audio, &f.p, &mut f.s, &mut f.sel);
        assert!(!ids(&container_conflicts(&f.input())).contains(&"mp4audio"));
    }

    #[test]
    fn external_subtitle_checks() {
        use crate::subs::{ExtKind, ExternalSub, JobMode, SubsTarget};
        use std::path::Path;
        let v = Path::new("/nonexistent/Film.mkv");
        let ids = |mode, ext: &[ExternalSub], alass, s: &EncodeSettings| -> Vec<&'static str> {
            external_issues(v, s, mode, ext, alass)
                .iter()
                .map(|i| i.id)
                .collect()
        };
        let s = EncodeSettings::default();
        let sidecar = JobMode::Subtitles(SubsTarget::Sidecar);
        assert_eq!(ids(sidecar, &[], true, &s), vec!["nosubs"]);
        let no = ExternalSub::from_path(v, "/nonexistent/Film.no.srt".into(), true);
        assert!(ids(sidecar, std::slice::from_ref(&no), true, &s).is_empty());
        assert_eq!(
            ids(sidecar, std::slice::from_ref(&no), false, &s),
            vec!["noalass"]
        );
        let mut twin = no.clone();
        twin.path = "/x/other.srt".into();
        assert_eq!(
            ids(sidecar, &[no.clone(), twin.clone()], true, &s),
            vec!["sidecarclash"]
        );
        twin.kind = ExtKind::Forced;
        twin.language = None;
        assert_eq!(ids(sidecar, &[no.clone(), twin], true, &s), vec!["sublang"]);
        let avi = Path::new("/x/Film.avi");
        let embed = external_issues(
            avi,
            &s,
            JobMode::Subtitles(SubsTarget::Embed),
            std::slice::from_ref(&no),
            true,
        );
        assert_eq!(embed[0].id, "embedcontainer");
        // Embedding the video's own sidecar: it stays on disk (decided from
        // the names, without reading the folder).
        let embed = JobMode::Subtitles(SubsTarget::Embed);
        assert_eq!(
            ids(embed, std::slice::from_ref(&no), true, &s),
            vec!["sidecarstays"]
        );
        let elsewhere = ExternalSub::from_path(v, "/downloads/Film.no.srt".into(), true);
        assert!(ids(embed, &[elsewhere], true, &s).is_empty());
        let mp4 = EncodeSettings {
            container: Container::Mp4,
            ..Default::default()
        };
        assert_eq!(
            ids(JobMode::Encode, std::slice::from_ref(&no), true, &mp4),
            vec!["mp4movtext"]
        );
        assert!(ids(JobMode::Encode, &[no], true, &s).is_empty());
    }
}
