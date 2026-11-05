//! Pure ffmpeg argument builder: (probe info, settings, selection) → argv.
//! No I/O here, so every shape can be unit-tested.

use std::ffi::OsString;
use std::path::Path;

use crate::crop::Crop;
use crate::ffmpeg::probe::ProbeInfo;
use crate::scan::Scan;
use crate::settings::{Container, EncodeSettings, RateControl, VideoCodec, av1_qindex};
use crate::tracks::{
    AudioAction, Selection, SubKind, is_audio_description, is_commentary, sub_kind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeMode {
    /// `-hwaccel vaapi` with frames kept on the GPU.
    Hardware,
    /// CPU decode, then `hwupload` to the encoder.
    Software,
}

impl DecodeMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hardware => "hardware decode",
            Self::Software => "software decode",
        }
    }
}

/// Everything one ffmpeg run needs.
#[derive(Clone, Debug)]
pub struct EncodeJob<'a> {
    pub probe: &'a ProbeInfo,
    pub settings: &'a EncodeSettings,
    pub selection: &'a Selection,
    pub output: &'a Path,
    pub device: &'a str,
    pub decode: DecodeMode,
    pub crop: Option<Crop>,
    /// The capability check found that `crop` works on VA-API frames.
    pub crop_on_gpu: bool,
    /// Deinterlace / inverse telecine plan.
    pub scan: Scan,
    /// Video bitrate from the target-size maths.
    pub video_kbps: Option<u32>,
    /// Encode only `(start, length)` seconds (size estimate / VMAF samples).
    pub segment: Option<(f64, f64)>,
    /// Video only, no audio/subtitles/metadata.
    pub video_only: bool,
    /// Write a raw Annex B HEVC stream (Dolby Vision pipeline).
    pub raw_hevc: bool,
    pub progress: bool,
}

impl<'a> EncodeJob<'a> {
    pub fn new(
        probe: &'a ProbeInfo,
        settings: &'a EncodeSettings,
        selection: &'a Selection,
        output: &'a Path,
        device: &'a str,
    ) -> Self {
        Self {
            probe,
            settings,
            selection,
            output,
            device,
            decode: DecodeMode::Hardware,
            crop: None,
            crop_on_gpu: false,
            scan: Scan::Progressive,
            video_kbps: None,
            segment: None,
            video_only: false,
            raw_hevc: false,
            progress: true,
        }
    }
}

struct Args(Vec<OsString>);

impl Args {
    fn add(&mut self, items: &[&str]) {
        self.0.extend(items.iter().map(OsString::from));
    }
    fn kv(&mut self, k: impl Into<OsString>, v: impl Into<OsString>) {
        self.0.push(k.into());
        self.0.push(v.into());
    }
}

/// Split the free-text "extra arguments" with shell quoting rules (no shell involved).
pub fn split_extra(text: &str) -> Result<Vec<String>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    shlex::split(text).ok_or_else(|| "Unbalanced quotes in extra ffmpeg arguments".to_string())
}

/// Copyable, shell-quoted rendering of a command. Display only; never executed.
pub fn display_command(program: &str, args: &[OsString]) -> String {
    let mut parts = vec![program.to_string()];
    for a in args {
        let s = a.to_string_lossy();
        parts.push(
            shlex::try_quote(&s)
                .map(|q| q.into_owned())
                .unwrap_or_else(|_| s.into_owned()),
        );
    }
    parts.join(" ")
}

/// Height to scale to, if any (never upscale; compare with the cropped height).
pub fn scale_height(job: &EncodeJob) -> Option<u32> {
    let v = job.probe.video.as_ref()?;
    let src_h = job.crop.map_or(v.height, |c| c.h);
    job.settings.downscale.height().filter(|h| *h < src_h)
}

/// Decode mode actually used: CPU-only filters (inverse telecine, `bwdif`)
/// need frames in system memory.
pub fn effective_decode(job: &EncodeJob) -> DecodeMode {
    if job.scan.needs_cpu() {
        DecodeMode::Software
    } else {
        job.decode
    }
}

fn video_filter(job: &EncodeJob) -> Option<String> {
    let v = job.probe.video.as_ref()?;
    let out_fmt = if job.settings.output_ten_bit() {
        "p010"
    } else {
        "nv12"
    };
    let src_fmt = if v.bit_depth > 8 { "p010" } else { "nv12" };
    let scale = scale_height(job);
    let mut parts: Vec<String> = Vec::new();
    match effective_decode(job) {
        DecodeMode::Software => {
            // Deinterlace / IVTC first, before cropping and scaling.
            if let Some(f) = job.scan.cpu_filter() {
                parts.push(f);
            }
            if let Some(c) = job.crop {
                parts.push(c.filter());
            }
            parts.push(format!("format={out_fmt}"));
            parts.push("hwupload".into());
            if let Some(h) = scale {
                parts.push(format!("scale_vaapi=w=-2:h={h}"));
            }
        }
        DecodeMode::Hardware => {
            let mut fmt_done = false;
            if let Some(c) = job.crop {
                if job.crop_on_gpu {
                    parts.push(c.filter());
                } else {
                    // Round trip through system memory; costs some speed.
                    parts.push(format!(
                        "hwdownload,format={src_fmt},{},format={out_fmt},hwupload",
                        c.filter()
                    ));
                    fmt_done = true;
                }
            }
            let convert = !fmt_done && src_fmt != out_fmt;
            match (scale, convert) {
                (Some(h), true) => parts.push(format!("scale_vaapi=w=-2:h={h}:format={out_fmt}")),
                (Some(h), false) => parts.push(format!("scale_vaapi=w=-2:h={h}")),
                (None, true) => parts.push(format!("scale_vaapi=format={out_fmt}")),
                (None, false) => {}
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

fn rate_control(a: &mut Args, job: &EncodeJob) {
    let s = job.settings;
    let rc = s.effective_rate_control();
    a.kv("-rc_mode", rc.ffmpeg_name());
    let quality = |a: &mut Args| match s.codec {
        VideoCodec::Av1 => a.kv("-global_quality", av1_qindex(s.qp).to_string()),
        _ if rc == RateControl::Qvbr => a.kv("-global_quality", s.qp.to_string()),
        _ => a.kv("-qp", s.qp.to_string()),
    };
    if rc == RateControl::Cqp {
        quality(a);
        return;
    }
    let from_target = s.bitrate_from_target();
    let kbps = if from_target {
        job.video_kbps.unwrap_or(s.bitrate_kbps)
    } else {
        s.bitrate_kbps
    };
    let max = match rc {
        RateControl::Cbr => kbps,
        _ if !from_target && s.maxrate_kbps > 0 => s.maxrate_kbps.max(kbps),
        _ => kbps * 3 / 2,
    };
    if rc == RateControl::Qvbr {
        quality(a);
    }
    a.kv("-b:v", format!("{kbps}k"));
    a.kv("-maxrate", format!("{max}k"));
    a.kv("-bufsize", format!("{}k", max * 2));
}

fn disposition_flags(flags: &[&str]) -> String {
    if flags.is_empty() {
        "0".into()
    } else {
        flags.join("+")
    }
}

/// Build the ffmpeg argument list (without the program name).
pub fn build(job: &EncodeJob) -> Result<Vec<OsString>, String> {
    let p = job.probe;
    let s = job.settings;
    let v = p.video.as_ref().ok_or("No video stream")?;
    if job.output == p.path.as_path() {
        return Err("The output file would overwrite the input".into());
    }
    let encode = s.codec != VideoCodec::Copy;
    let mut a = Args(Vec::new());
    a.add(&["-hide_banner", "-y"]);
    if encode {
        if effective_decode(job) == DecodeMode::Hardware {
            a.add(&["-hwaccel", "vaapi", "-hwaccel_output_format", "vaapi"]);
        }
        a.kv("-vaapi_device", job.device);
    }
    if let Some((start, len)) = job.segment {
        a.kv("-ss", format!("{start:.3}"));
        a.kv("-t", format!("{len:.3}"));
    }
    a.kv("-i", &p.path);

    if encode && let Some(vf) = video_filter(job) {
        a.kv("-vf", vf);
    }

    a.kv("-map", format!("0:{}", v.index));
    let mp4 = s.container == Container::Mp4 && !job.raw_hevc;
    let audio: Vec<_> = if job.video_only {
        Vec::new()
    } else {
        job.selection.selected_audio().collect()
    };
    let subs: Vec<_> = if job.video_only || mp4 {
        Vec::new()
    } else {
        job.selection.selected_subs().collect()
    };
    for c in &audio {
        a.kv("-map", format!("0:{}", c.index));
    }
    for c in &subs {
        a.kv("-map", format!("0:{}", c.index));
    }
    let fonts = !job.video_only && !mp4 && p.attachments > 0;
    if fonts {
        // Fonts for ASS subtitles.
        a.add(&["-map", "0:t?"]);
    }

    match s.codec.encoder() {
        None => a.add(&["-c:v", "copy"]),
        Some(enc) => {
            a.kv("-c:v", enc);
            rate_control(&mut a, job);
            match (s.codec, s.output_ten_bit()) {
                (VideoCodec::Hevc, true) => a.add(&["-profile:v", "main10"]),
                (VideoCodec::Hevc, false) => a.add(&["-profile:v", "main"]),
                (VideoCodec::Av1, _) => a.add(&["-profile:v", "main"]),
                _ => a.add(&["-profile:v", "high"]),
            }
            if let Some(level) = s.compression_level {
                a.kv("-compression_level", level.to_string());
            }
            let gop = s
                .gop
                .unwrap_or_else(|| (v.fps * job.scan.rate_factor() * 2.0).round().max(1.0) as u32);
            a.kv("-g", gop.to_string());
            if let Some(bf) = s.bframes {
                a.kv("-bf", bf.to_string());
            }
            // Keep colour signalling, except HDR tags on an 8-bit (non-HDR) output.
            if s.output_ten_bit() || !p.hdr.is_hdr() {
                if let Some(c) = &v.color_primaries {
                    a.kv("-color_primaries", c);
                }
                if let Some(c) = &v.color_transfer {
                    a.kv("-color_trc", c);
                }
                if let Some(c) = &v.color_space {
                    a.kv("-colorspace", c);
                }
                if let Some(c) = &v.color_range {
                    a.kv("-color_range", c);
                }
            }
        }
    }
    let hevc_out = s.codec == VideoCodec::Hevc || (!encode && v.codec == "hevc");
    if hevc_out && mp4 {
        // Apple players need the hvc1 tag for HEVC in MP4.
        a.add(&["-tag:v", "hvc1"]);
    }

    if job.video_only {
        a.add(&["-an", "-sn", "-dn"]);
    }

    // Audio codecs, dispositions and tags, in output order.
    for (i, c) in audio.iter().enumerate() {
        let src = p
            .audio_by_index(c.index)
            .ok_or("Selected audio track not in probe")?;
        let spec = |k: &str| format!("-{k}:a:{i}");
        match c.action {
            AudioAction::Copy => a.kv(spec("c"), "copy"),
            AudioAction::Eac3 { kbps } => {
                a.kv(spec("c"), "eac3");
                a.kv(spec("b"), format!("{kbps}k"));
                if src.channels > 6 {
                    a.kv(spec("ac"), "6");
                }
            }
            AudioAction::Aac { kbps, stereo } => {
                a.kv(spec("c"), "aac");
                a.kv(spec("b"), format!("{kbps}k"));
                if stereo && src.channels > 2 {
                    a.kv(spec("ac"), "2");
                }
            }
            AudioAction::Flac => a.kv(spec("c"), "flac"),
        }
        let mut flags = Vec::new();
        if i == 0 {
            flags.push("default");
        }
        if is_commentary(src) {
            flags.push("comment");
        }
        if is_audio_description(src) {
            flags.push("visual_impaired");
        }
        a.kv(spec("disposition"), disposition_flags(&flags));
    }
    for (i, c) in subs.iter().enumerate() {
        let src = p
            .subtitle_by_index(c.index)
            .ok_or("Selected subtitle track not in probe")?;
        // MKV can't hold MP4 text subtitles; they become SRT. Everything else is copied.
        let codec = if src.codec == "mov_text" {
            "srt"
        } else {
            "copy"
        };
        a.kv(format!("-c:s:{i}"), codec);
        let flags: &[&str] = match sub_kind(src) {
            SubKind::Forced => &["forced"],
            SubKind::Sdh => &["hearing_impaired"],
            SubKind::Full => &[],
        };
        a.kv(format!("-disposition:s:{i}"), disposition_flags(flags));
    }

    if !job.video_only {
        if s.keep_metadata {
            a.add(&["-map_metadata", "0", "-map_chapters", "0"]);
        } else {
            a.add(&["-map_metadata", "-1", "-map_chapters", "-1"]);
            // `-1` drops per-stream tags too, but the MKV muxer refuses an
            // attachment without its filename/mimetype tags, so keep those.
            if fonts {
                for i in 0..p.attachments {
                    a.kv(format!("-map_metadata:s:t:{i}"), format!("0:s:t:{i}"));
                }
            }
        }
        // Stats tags (BPS, NUMBER_OF_BYTES…) of re-encoded streams would be wrong;
        // an empty value deletes the copied tag.
        if encode {
            for k in &v.stat_tags {
                a.kv("-metadata:s:v:0", format!("{k}="));
            }
        }
        for (i, c) in audio.iter().enumerate() {
            if c.action != AudioAction::Copy
                && let Some(src) = p.audio_by_index(c.index)
            {
                for k in &src.stat_tags {
                    a.kv(format!("-metadata:s:a:{i}"), format!("{k}="));
                }
            }
        }
        let mut tags = |kind: &str, i: usize, lang: &Option<String>, title: &Option<String>| {
            if let Some(l) = lang {
                a.kv(format!("-metadata:s:{kind}:{i}"), format!("language={l}"));
            }
            if let Some(t) = title {
                a.kv(format!("-metadata:s:{kind}:{i}"), format!("title={t}"));
            }
        };
        for (i, c) in audio.iter().enumerate() {
            if let Some(src) = p.audio_by_index(c.index) {
                tags("a", i, &src.language, &src.title);
            }
        }
        for (i, c) in subs.iter().enumerate() {
            if let Some(src) = p.subtitle_by_index(c.index) {
                tags("s", i, &src.language, &src.title);
            }
        }
    }

    if mp4 {
        a.add(&["-movflags", "+faststart"]);
    }
    if job.progress {
        a.add(&["-progress", "pipe:1", "-nostats"]);
    }
    for extra in split_extra(&s.extra_args)? {
        a.0.push(extra.into());
    }
    if job.raw_hevc {
        a.add(&["-f", "hevc"]);
    }
    a.0.push(job.output.into());
    Ok(a.0)
}

/// Dolby Vision mux step: video from the RPU-injected raw HEVC (input 1),
/// everything else from the source (input 0).
pub fn build_dv_mux(job: &EncodeJob, raw_video: &Path) -> Result<Vec<OsString>, String> {
    let fps = job
        .probe
        .video
        .as_ref()
        .map(|v| v.fps_rational.clone())
        .unwrap_or_else(|| "24/1".into());
    let mut copy_settings = job.settings.clone();
    copy_settings.codec = VideoCodec::Copy;
    let inner = EncodeJob {
        settings: &copy_settings,
        crop: None,
        ..job.clone()
    };
    let mut args = build(&inner)?;
    // Insert the raw stream as input 1 after the source input and remap the video.
    let src_pos = args.iter().position(|x| x == "-i").ok_or("no input")? + 2;
    let extra: Vec<OsString> = ["-f", "hevc", "-framerate"]
        .iter()
        .map(OsString::from)
        .chain([OsString::from(&fps), "-i".into(), raw_video.into()])
        .collect();
    args.splice(src_pos..src_pos, extra);
    let v_index = job.probe.video.as_ref().map(|v| v.index).unwrap_or(0);
    if let Some(m) = args.iter().position(|x| *x == *format!("0:{v_index}")) {
        args[m] = "1:v:0".into();
    }
    Ok(args)
}

/// Size of the encoded picture after cropping and downscaling (`scale_vaapi=w=-2`).
pub fn output_dimensions(
    p: &ProbeInfo,
    s: &EncodeSettings,
    crop: Option<Crop>,
) -> Option<(u32, u32)> {
    let v = p.video.as_ref()?;
    let (w, h) = crop.map_or((v.width, v.height), |c| (c.w, c.h));
    match s.downscale.height().filter(|t| *t < h) {
        Some(t) => Some((
            ((w as f64 * t as f64 / h as f64 / 2.0).round() as u32) * 2,
            t,
        )),
        None => Some((w, h)),
    }
}

/// Does ffmpeg's stderr look like the hardware decoder couldn't handle the input?
pub fn is_hw_decode_failure(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    [
        "failed to get hw surface",
        "hwaccel initialisation returned error",
        "failed setup for format vaapi",
        "no support for codec",
        "impossible to convert between the formats",
        "failed to create decode",
        "error while decoding stream",
        "device does not support the vaapi",
    ]
    .iter()
    .any(|p| s.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe::tests::sample_4k;
    use crate::settings::{Downscale, SizeMode};
    use crate::tracks::auto_select;

    fn langs() -> Vec<String> {
        vec!["nor".into(), "eng".into()]
    }

    fn args_for(s: &EncodeSettings, tweak: impl FnOnce(&mut EncodeJob)) -> Vec<String> {
        let p = sample_4k();
        let (sel, _, _) = auto_select(&p, &langs(), s);
        let out = Path::new("/out/movie-reencoded.mkv");
        let mut job = EncodeJob::new(&p, s, &sel, out, "/dev/dri/renderD128");
        job.video_kbps = Some(4000);
        tweak(&mut job);
        build(&job)
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    fn has(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len())
            .any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn dropped_metadata_keeps_attachment_tags() {
        let mut p = sample_4k();
        p.attachments = 2;
        let s = EncodeSettings {
            keep_metadata: false,
            ..Default::default()
        };
        let (sel, _, _) = auto_select(&p, &langs(), &s);
        let out = Path::new("/out/movie-reencoded.mkv");
        let job = EncodeJob::new(&p, &s, &sel, out, "/dev/dri/renderD128");
        let a: Vec<String> = build(&job)
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert!(has(&a, &["-map", "0:t?"]));
        assert!(has(&a, &["-map_metadata", "-1"]));
        assert!(has(&a, &["-map_metadata:s:t:0", "0:s:t:0"]));
        assert!(has(&a, &["-map_metadata:s:t:1", "0:s:t:1"]));
        // Kept metadata copies them anyway; no per-stream maps needed.
        let a = args_for(&EncodeSettings::default(), |_| {});
        assert!(!a.iter().any(|x| x.starts_with("-map_metadata:s:t")));
    }

    #[test]
    fn default_shape_hevc_target_size() {
        let a = args_for(&EncodeSettings::default(), |_| {});
        assert!(has(
            &a,
            &[
                "-hwaccel",
                "vaapi",
                "-hwaccel_output_format",
                "vaapi",
                "-vaapi_device",
                "/dev/dri/renderD128"
            ]
        ));
        assert!(has(
            &a,
            &[
                "-c:v",
                "hevc_vaapi",
                "-rc_mode",
                "VBR",
                "-b:v",
                "4000k",
                "-maxrate",
                "6000k",
                "-bufsize",
                "12000k"
            ]
        ));
        assert!(has(&a, &["-profile:v", "main10"]));
        // 10-bit source to 10-bit output: no format conversion, no filter.
        assert!(!a.contains(&"-vf".to_string()));
        assert!(has(&a, &["-g", "48"]));
        assert!(has(
            &a,
            &[
                "-color_primaries",
                "bt2020",
                "-color_trc",
                "smpte2084",
                "-colorspace",
                "bt2020nc"
            ]
        ));
        assert!(has(&a, &["-progress", "pipe:1", "-nostats"]));
        assert_eq!(a.last().unwrap(), "/out/movie-reencoded.mkv");
        assert!(has(&a, &["-map_metadata", "0", "-map_chapters", "0"]));
    }

    #[test]
    fn track_maps_dispositions_and_tags() {
        let a = args_for(&EncodeSettings::default(), |_| {});
        // Video, Norwegian DTS (default), English E-AC-3 Atmos, English SRT full + forced.
        let maps: Vec<&String> = a
            .iter()
            .zip(a.iter().skip(1))
            .filter(|(k, _)| *k == "-map")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(maps, ["0:0", "0:4", "0:2", "0:6", "0:9"]);
        assert!(has(&a, &["-c:a:0", "eac3", "-b:a:0", "640k"]));
        assert!(has(&a, &["-disposition:a:0", "default"]));
        assert!(has(&a, &["-c:a:1", "copy", "-disposition:a:1", "0"]));
        assert!(has(&a, &["-c:s:0", "copy", "-disposition:s:0", "0"]));
        // Stale stats tags are cleared on the re-encoded video and converted audio only.
        assert!(has(&a, &["-metadata:s:v:0", "BPS-eng="]));
        assert!(has(&a, &["-metadata:s:v:0", "NUMBER_OF_BYTES-eng="]));
        assert!(!a.contains(&"-map_metadata:s:v:0".to_string()));
        assert!(has(&a, &["-c:s:1", "copy", "-disposition:s:1", "forced"]));
        assert!(has(&a, &["-metadata:s:a:0", "language=nob"]));
        assert!(has(&a, &["-metadata:s:a:1", "title=DD+ Atmos"]));
        assert!(has(&a, &["-metadata:s:s:1", "title=Forced"]));
        assert!(!a.contains(&"0:a?".to_string()));
    }

    #[test]
    fn mov_text_becomes_srt_in_mkv() {
        let mut p = sample_4k();
        p.subtitles[0].codec = "mov_text".into();
        let s = EncodeSettings::default();
        let (sel, _, _) = auto_select(&p, &langs(), &s);
        let out = Path::new("/o.mkv");
        let a: Vec<String> = build(&EncodeJob::new(&p, &s, &sel, out, "/d"))
            .unwrap()
            .into_iter()
            .map(|x| x.into_string().unwrap())
            .collect();
        assert!(has(&a, &["-c:s:0", "srt"]));
        assert!(has(&a, &["-c:s:1", "copy"]));
    }

    #[test]
    fn downmix_from_7_1() {
        let p = sample_4k();
        let s = EncodeSettings::default();
        let (mut sel, _, _) = auto_select(&p, &langs(), &s);
        for c in &mut sel.audio {
            c.selected = c.index == 1;
        }
        let out = Path::new("/o.mkv");
        let a: Vec<String> = build(&EncodeJob::new(&p, &s, &sel, out, "/dev/dri/renderD128"))
            .unwrap()
            .into_iter()
            .map(|x| x.into_string().unwrap())
            .collect();
        assert!(has(
            &a,
            &["-c:a:0", "eac3", "-b:a:0", "640k", "-ac:a:0", "6"]
        ));
        assert!(has(&a, &["-metadata:s:a:0", "BPS-eng="]));
        sel.audio.iter_mut().find(|c| c.index == 1).unwrap().action = AudioAction::Aac {
            kbps: 256,
            stereo: true,
        };
        let a: Vec<String> = build(&EncodeJob::new(&p, &s, &sel, out, "/d"))
            .unwrap()
            .into_iter()
            .map(|x| x.into_string().unwrap())
            .collect();
        assert!(has(
            &a,
            &["-c:a:0", "aac", "-b:a:0", "256k", "-ac:a:0", "2"]
        ));
    }

    #[test]
    fn each_codec() {
        let mut s = EncodeSettings {
            size_mode: SizeMode::ConstantQuality,
            ..Default::default()
        };
        let a = args_for(&s, |_| {});
        assert!(has(&a, &["-rc_mode", "CQP", "-qp", "25"]));
        s.codec = VideoCodec::H264;
        s.qp = 23;
        let a = args_for(&s, |_| {});
        assert!(has(
            &a,
            &[
                "-c:v",
                "h264_vaapi",
                "-rc_mode",
                "CQP",
                "-qp",
                "23",
                "-profile:v",
                "high"
            ]
        ));
        // HDR source to 8-bit: convert on the GPU, drop the HDR colour tags.
        assert!(has(&a, &["-vf", "scale_vaapi=format=nv12"]));
        assert!(!a.contains(&"-color_trc".to_string()));
        s.codec = VideoCodec::Av1;
        s.qp = 28;
        let a = args_for(&s, |_| {});
        assert!(has(
            &a,
            &[
                "-c:v",
                "av1_vaapi",
                "-rc_mode",
                "CQP",
                "-global_quality",
                "112",
                "-profile:v",
                "main"
            ]
        ));
    }

    #[test]
    fn copy_mode_is_a_remux() {
        let s = EncodeSettings {
            codec: VideoCodec::Copy,
            ..Default::default()
        };
        let a = args_for(&s, |_| {});
        assert!(!a.contains(&"-hwaccel".to_string()) && !a.contains(&"-vaapi_device".to_string()));
        assert!(has(&a, &["-c:v", "copy"]));
        assert!(!a.contains(&"-vf".to_string()) && !a.contains(&"-rc_mode".to_string()));
        assert!(!a.contains(&"-map_metadata:s:v:0".to_string()));
    }

    #[test]
    fn each_rate_control_mode() {
        let mut s = EncodeSettings {
            size_mode: SizeMode::ConstantQuality,
            bitrate_kbps: 5000,
            ..Default::default()
        };
        s.rate_control = RateControl::Cbr;
        let a = args_for(&s, |_| {});
        assert!(has(
            &a,
            &[
                "-rc_mode", "CBR", "-b:v", "5000k", "-maxrate", "5000k", "-bufsize", "10000k"
            ]
        ));
        s.rate_control = RateControl::Vbr;
        s.maxrate_kbps = 9000;
        let a = args_for(&s, |_| {});
        assert!(has(
            &a,
            &["-rc_mode", "VBR", "-b:v", "5000k", "-maxrate", "9000k"]
        ));
        s.rate_control = RateControl::Qvbr;
        let a = args_for(&s, |_| {});
        assert!(has(
            &a,
            &["-rc_mode", "QVBR", "-global_quality", "25", "-b:v", "5000k"]
        ));
        s.rate_control = RateControl::Cqp;
        let a = args_for(&s, |_| {});
        assert!(has(&a, &["-rc_mode", "CQP", "-qp", "25"]) && !a.contains(&"-b:v".to_string()));
        // Target size with an explicit VBR override still uses the computed bitrate.
        let t = EncodeSettings {
            rate_control: RateControl::Vbr,
            ..Default::default()
        };
        assert!(has(&args_for(&t, |_| {}), &["-b:v", "4000k"]));
    }

    #[test]
    fn scaling_and_bit_depth() {
        let s = EncodeSettings {
            downscale: Downscale::P1080,
            ..Default::default()
        };
        let a = args_for(&s, |_| {});
        assert!(has(&a, &["-vf", "scale_vaapi=w=-2:h=1080"]));
        let s8 = EncodeSettings {
            downscale: Downscale::P1080,
            ten_bit: false,
            ..Default::default()
        };
        assert!(has(
            &args_for(&s8, |_| {}),
            &["-vf", "scale_vaapi=w=-2:h=1080:format=nv12"]
        ));
        assert!(has(&args_for(&s8, |_| {}), &["-profile:v", "main"]));
        // Never upscale: 2160p on a 2160p source is a no-op.
        let same = EncodeSettings {
            downscale: Downscale::P2160,
            ..Default::default()
        };
        assert!(!args_for(&same, |_| {}).contains(&"-vf".to_string()));
    }

    #[test]
    fn software_decode_fallback() {
        let s = EncodeSettings {
            downscale: Downscale::P1080,
            ..Default::default()
        };
        let a = args_for(&s, |j| j.decode = DecodeMode::Software);
        assert!(!a.contains(&"-hwaccel".to_string()));
        assert!(has(&a, &["-vaapi_device", "/dev/dri/renderD128", "-i"]));
        assert!(has(
            &a,
            &["-vf", "format=p010,hwupload,scale_vaapi=w=-2:h=1080"]
        ));
        let s8 = EncodeSettings {
            ten_bit: false,
            codec: VideoCodec::H264,
            ..Default::default()
        };
        assert!(has(
            &args_for(&s8, |j| j.decode = DecodeMode::Software),
            &["-vf", "format=nv12,hwupload"]
        ));
    }

    #[test]
    fn deinterlace_on_cpu_switches_to_software_decode() {
        let s = EncodeSettings {
            deinterlace_double_rate: false,
            ..Default::default()
        };
        let a = args_for(&s, |j| {
            j.scan = Scan::Deinterlace {
                tff: false,
                double_rate: false,
            }
        });
        assert!(!a.contains(&"-hwaccel".to_string()));
        assert!(has(
            &a,
            &[
                "-vf",
                "bwdif=mode=send_frame:parity=bff:deint=all,format=p010,hwupload"
            ]
        ));
        assert!(has(&a, &["-g", "48"]));
    }

    #[test]
    fn inverse_telecine_runs_before_crop() {
        let crop = Some(Crop {
            w: 3840,
            h: 1608,
            x: 0,
            y: 276,
        });
        let a = args_for(&EncodeSettings::default(), |j| {
            j.scan = Scan::Telecine { tff: true };
            j.crop = crop;
        });
        assert!(!a.contains(&"-hwaccel".to_string()));
        assert!(has(
            &a,
            &[
                "-vf",
                "fieldmatch=order=tff:combmatch=full,yadif=deint=interlaced,decimate,crop=3840:1608:0:276,format=p010,hwupload"
            ]
        ));
        assert!(has(&a, &["-g", "38"]));
    }

    #[test]
    fn cropping_paths() {
        let crop = Some(Crop {
            w: 3840,
            h: 1608,
            x: 0,
            y: 276,
        });
        let s = EncodeSettings {
            downscale: Downscale::P1080,
            ..Default::default()
        };
        let a = args_for(&s, |j| j.crop = crop);
        assert!(has(
            &a,
            &[
                "-vf",
                "hwdownload,format=p010,crop=3840:1608:0:276,format=p010,hwupload,scale_vaapi=w=-2:h=1080"
            ]
        ));
        let a = args_for(&s, |j| {
            j.crop = crop;
            j.crop_on_gpu = true;
        });
        assert!(has(
            &a,
            &["-vf", "crop=3840:1608:0:276,scale_vaapi=w=-2:h=1080"]
        ));
        let a = args_for(&s, |j| {
            j.crop = crop;
            j.decode = DecodeMode::Software;
        });
        assert!(has(
            &a,
            &[
                "-vf",
                "crop=3840:1608:0:276,format=p010,hwupload,scale_vaapi=w=-2:h=1080"
            ]
        ));
    }

    #[test]
    fn mp4_drops_subtitles_and_adds_faststart() {
        let s = EncodeSettings {
            container: Container::Mp4,
            ..Default::default()
        };
        let a = args_for(&s, |j| j.output = Path::new("/out/x.mp4"));
        assert!(!a.contains(&"-c:s:0".to_string()));
        assert!(!a.contains(&"0:6".to_string()));
        assert!(has(&a, &["-movflags", "+faststart"]));
        assert!(has(&a, &["-tag:v", "hvc1"]));
    }

    #[test]
    fn hevc_remux_to_mp4_gets_hvc1() {
        let s = EncodeSettings {
            codec: VideoCodec::Copy,
            container: Container::Mp4,
            ..Default::default()
        };
        let a = args_for(&s, |j| j.output = Path::new("/out/x.mp4"));
        assert!(has(&a, &["-c:v", "copy"]));
        assert!(has(&a, &["-tag:v", "hvc1"]));
    }

    #[test]
    fn extra_arguments_are_split_not_shelled() {
        let s = EncodeSettings {
            extra_args: r#"-metadata title="My Film" -async_depth 4"#.into(),
            ..Default::default()
        };
        let a = args_for(&s, |_| {});
        assert!(has(
            &a,
            &[
                "-metadata",
                "title=My Film",
                "-async_depth",
                "4",
                "/out/movie-reencoded.mkv"
            ]
        ));
        assert!(split_extra("-x 'unbalanced").is_err());
        assert_eq!(split_extra("  ").unwrap(), Vec::<String>::new());
        let shown = display_command(
            "ffmpeg",
            &[OsString::from("-i"), OsString::from("my file.mkv")],
        );
        assert_eq!(shown, "ffmpeg -i 'my file.mkv'");
    }

    #[test]
    fn segments_and_video_only() {
        let s = EncodeSettings {
            size_mode: SizeMode::ConstantQuality,
            ..Default::default()
        };
        let a = args_for(&s, |j| {
            j.segment = Some((1440.0, 8.0));
            j.video_only = true;
            j.progress = false;
        });
        assert!(has(&a, &["-ss", "1440.000", "-t", "8.000", "-i"]));
        assert!(has(&a, &["-an", "-sn", "-dn"]));
        assert!(!a.contains(&"0:4".to_string()) && !a.contains(&"-progress".to_string()));
        assert!(!a.contains(&"-map_metadata".to_string()));
    }

    #[test]
    fn refuses_to_overwrite_input() {
        let p = sample_4k();
        let s = EncodeSettings::default();
        let sel = Selection::default();
        let job = EncodeJob::new(&p, &s, &sel, &p.path, "/d");
        assert!(build(&job).is_err());
    }

    #[test]
    fn dv_raw_and_mux() {
        let p = sample_4k();
        let s = EncodeSettings::default();
        let (sel, _, _) = auto_select(&p, &langs(), &s);
        let out = Path::new("/tmp/v.hevc");
        let mut job = EncodeJob::new(&p, &s, &sel, out, "/d");
        job.video_only = true;
        job.raw_hevc = true;
        let a: Vec<String> = build(&job)
            .unwrap()
            .into_iter()
            .map(|x| x.into_string().unwrap())
            .collect();
        assert!(has(&a, &["-f", "hevc", "/tmp/v.hevc"]));
        let final_out = Path::new("/out/x.mkv");
        let mux_job = EncodeJob::new(&p, &s, &sel, final_out, "/d");
        let m: Vec<String> = build_dv_mux(&mux_job, Path::new("/tmp/v_dv.hevc"))
            .unwrap()
            .into_iter()
            .map(|x| x.into_string().unwrap())
            .collect();
        assert!(has(
            &m,
            &[
                "-f",
                "hevc",
                "-framerate",
                "24000/1001",
                "-i",
                "/tmp/v_dv.hevc"
            ]
        ));
        assert!(has(&m, &["-map", "1:v:0", "-map", "0:4"]));
        assert!(has(&m, &["-c:v", "copy"]));
        assert!(!m.contains(&"-hwaccel".to_string()));
    }

    #[test]
    fn detects_hw_decode_failures() {
        assert!(is_hw_decode_failure(
            "[hevc @ 0x1] Failed setup for format vaapi: hwaccel initialisation returned error."
        ));
        assert!(!is_hw_decode_failure(
            "Conversion failed! No space left on device"
        ));
        // An encoder size limit is not a decode problem.
        assert!(!is_hw_decode_failure(
            "[hevc_vaapi @ 0x5] Hardware does not support encoding at size 320x240 (constraints: width 384-8192 height 128-4352)."
        ));
    }

    #[test]
    fn output_size_after_crop_and_scale() {
        let p = sample_4k();
        let s = EncodeSettings {
            downscale: Downscale::P1080,
            ..Default::default()
        };
        assert_eq!(output_dimensions(&p, &s, None), Some((1920, 1080)));
        let crop = Some(Crop {
            w: 3840,
            h: 1608,
            x: 0,
            y: 276,
        });
        assert_eq!(output_dimensions(&p, &s, crop), Some((2580, 1080)));
        assert_eq!(
            output_dimensions(&p, &EncodeSettings::default(), crop),
            Some((3840, 1608))
        );
    }
}
