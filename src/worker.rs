//! Blocking job pipelines that run on worker threads: encode (with software
//! fallback and HDR verification), the Dolby Vision path, crop detection,
//! size estimation and VMAF.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::checks;
use crate::crop::{self, Crop};
use crate::ffmpeg::command::{self, DecodeMode, EncodeJob};
use crate::ffmpeg::estimate::{self, SizeRange};
use crate::ffmpeg::probe::{self, ProbeInfo};
use crate::ffmpeg::progress::Progress;
use crate::ffmpeg::runner::{self, Cancel};
use crate::ffmpeg::vmaf::{self, VmafResult};
use crate::hdr;
use crate::scan::{self, Scan, ScanType};
use crate::settings::{Container, EncodeSettings, VideoCodec, cache_dir};
use crate::tracks::Selection;

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: String,
    pub ffprobe: String,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
        }
    }
}

/// Owned copy of everything an encode needs (sent to the worker thread).
#[derive(Clone, Debug)]
pub struct Request {
    pub probe: ProbeInfo,
    pub settings: EncodeSettings,
    pub selection: Selection,
    pub output: PathBuf,
    pub device: String,
    pub crop: Option<Crop>,
    pub crop_on_gpu: bool,
    pub scan: Scan,
    pub video_kbps: Option<u32>,
    pub dv_preserve: bool,
    pub mkvpropedit: bool,
    pub vmaf_after: bool,
}

impl Request {
    pub fn job<'a>(&'a self, output: &'a Path, decode: DecodeMode) -> EncodeJob<'a> {
        let mut j = EncodeJob::new(
            &self.probe,
            &self.settings,
            &self.selection,
            output,
            &self.device,
        );
        j.decode = decode;
        j.crop = self.crop;
        j.crop_on_gpu = self.crop_on_gpu;
        j.scan = self.scan;
        j.video_kbps = self.video_kbps;
        j
    }

    pub fn first_decode(&self) -> DecodeMode {
        let v = self.probe.video.as_ref();
        if self.scan.needs_cpu() {
            DecodeMode::Software
        } else if v.is_some_and(|v| checks::hw_decodable(&v.codec, &v.pix_fmt, v.bit_depth)) {
            DecodeMode::Hardware
        } else {
            DecodeMode::Software
        }
    }
}

pub enum Update {
    Progress(Progress),
    Stage(String),
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub success: bool,
    pub cancelled: bool,
    pub error: Option<String>,
    pub stderr_tail: Vec<String>,
    pub decode: Option<DecodeMode>,
    pub report: Vec<String>,
}

fn fail(msg: impl Into<String>, tail: Vec<String>) -> Outcome {
    Outcome {
        error: Some(msg.into()),
        stderr_tail: tail,
        ..Default::default()
    }
}

/// Run one ffmpeg encode, retrying once in software-decode mode when the
/// hardware decoder rejects the input.
fn run_with_fallback(
    tools: &Tools,
    req: &Request,
    build: impl Fn(DecodeMode) -> Result<Vec<OsString>, String>,
    output: &Path,
    cancel: &Cancel,
    update: &mut impl FnMut(Update),
) -> Result<DecodeMode, Outcome> {
    let mut decode = req.first_decode();
    loop {
        let args = build(decode).map_err(|e| fail(e, Vec::new()))?;
        if req.settings.codec != VideoCodec::Copy {
            update(Update::Stage(format!("Encoding · {}", decode.label())));
        }
        let r = runner::run_ffmpeg(&tools.ffmpeg, &args, cancel, |p| {
            update(Update::Progress(p))
        });
        if r.cancelled {
            let _ = std::fs::remove_file(output);
            return Err(Outcome {
                cancelled: true,
                ..Default::default()
            });
        }
        if let Some(e) = r.error {
            return Err(fail(e, r.stderr_tail));
        }
        if r.success {
            return Ok(decode);
        }
        let _ = std::fs::remove_file(output);
        if decode == DecodeMode::Hardware
            && req.settings.codec != VideoCodec::Copy
            && command::is_hw_decode_failure(&r.stderr_tail.join("\n"))
        {
            decode = DecodeMode::Software;
            continue;
        }
        let tail: Vec<String> = r
            .stderr_tail
            .iter()
            .rev()
            .take(runner::STDERR_LINES)
            .rev()
            .cloned()
            .collect();
        return Err(fail(
            format!(
                "ffmpeg failed: {}",
                runner::error_summary(&r.stderr_tail.join("\n"))
            ),
            tail,
        ));
    }
}

pub fn encode(
    tools: &Tools,
    req: &Request,
    cancel: &Cancel,
    mut update: impl FnMut(Update),
) -> Outcome {
    let result = if req.dv_preserve && req.probe.hdr.dovi.is_some() {
        encode_dv(tools, req, cancel, &mut update)
    } else {
        run_with_fallback(
            tools,
            req,
            |d| command::build(&req.job(&req.output, d)),
            &req.output,
            cancel,
            &mut update,
        )
    };
    let decode = match result {
        Ok(d) => d,
        Err(o) => return o,
    };
    let encoded = req.settings.codec != VideoCodec::Copy;
    let mut out = Outcome {
        success: true,
        decode: encoded.then_some(decode),
        ..Default::default()
    };
    out.report.push(size_report(req));
    if req.settings.codec != VideoCodec::Copy {
        update(Update::Stage("Checking output".into()));
        out.report.extend(hdr_report(tools, req));
        if req.vmaf_after {
            update(Update::Stage("Scoring quality (VMAF)".into()));
            match vmaf_output(tools, req, cancel) {
                Ok(r) => out.report.push(vmaf::describe(r, req.probe.hdr.is_hdr())),
                Err(e) if cancel.is_cancelled() => out.report.push(format!("VMAF skipped: {e}")),
                Err(e) => out.report.push(format!("VMAF failed: {e}")),
            }
        }
    }
    out
}

fn size_report(req: &Request) -> String {
    let actual = std::fs::metadata(&req.output).map(|m| m.len()).unwrap_or(0);
    let mut s = format!("Output {}", estimate::format_bytes(actual));
    if req.settings.bitrate_from_target() && req.settings.codec != VideoCodec::Copy {
        let target = req.settings.target_gb * estimate::GB;
        s.push_str(&format!(
            " · target {:.1} GB ({:+.1}%)",
            req.settings.target_gb,
            (actual as f64 / target - 1.0) * 100.0
        ));
    }
    if req.probe.size_bytes > 0 {
        s.push_str(&format!(
            " · {:.0}% of the original",
            actual as f64 / req.probe.size_bytes as f64 * 100.0
        ));
    }
    s
}

/// Verify that HDR10 static metadata survived; repair the MKV header with
/// mkvpropedit when the encoder dropped it.
fn hdr_report(tools: &Tools, req: &Request) -> Vec<String> {
    let src = &req.probe.hdr;
    if !src.pq || !req.settings.output_ten_bit() {
        return Vec::new();
    }
    let Ok(out) = probe::run(&tools.ffprobe, &req.output) else {
        return vec!["Couldn't read the output to check HDR metadata".into()];
    };
    let Some(c) = hdr::compare(src, &out.hdr) else {
        return Vec::new();
    };
    if !c.transfer_kept {
        return vec!["⚠ Output is not tagged as PQ/HDR10".into()];
    }
    if c.mastering_kept && c.cll_kept {
        return vec![
            "HDR10 metadata carried over (PQ, BT.2020, mastering display, light levels)".into(),
        ];
    }
    if req.settings.container == Container::Mkv && req.mkvpropedit {
        let r = runner::run_capture(
            "mkvpropedit",
            &hdr::mkvpropedit_args(&req.output, src),
            &Cancel::default(),
        );
        if r.is_ok_and(|r| r.success) {
            return vec!["HDR10 mastering metadata was missing; written into the MKV header with mkvpropedit".into()];
        }
    }
    let fix = if Path::new("/.flatpak-info").exists() {
        "The Flatpak can't repair this; the native install can, with mkvtoolnix installed."
    } else {
        "Install mkvtoolnix to repair MKV files automatically."
    };
    vec![format!(
        "⚠ HDR10 mastering display / light level metadata did not carry over (PQ and BT.2020 tags are present). {fix}"
    )]
}

fn encode_dv(
    tools: &Tools,
    req: &Request,
    cancel: &Cancel,
    update: &mut impl FnMut(Update),
) -> Result<DecodeMode, Outcome> {
    let dovi = req
        .probe
        .hdr
        .dovi
        .ok_or_else(|| fail("No Dolby Vision metadata", Vec::new()))?;
    let dir = cache_dir().join(format!(
        "dv-{}-{}",
        std::process::id(),
        req.probe.duration as u64
    ));
    std::fs::create_dir_all(&dir).map_err(|e| fail(e.to_string(), Vec::new()))?;
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&dir);
    };
    let rpu = dir.join("rpu.bin");
    let video = dir.join("video.hevc");
    let injected = dir.join("video_dv.hevc");
    let step =
        |r: Result<runner::Captured, String>, what: &str| -> Result<runner::Captured, Outcome> {
            match r {
                Ok(c) if c.success => Ok(c),
                Ok(c) if c.cancelled => Err(Outcome {
                    cancelled: true,
                    ..Default::default()
                }),
                Ok(c) => Err(fail(
                    format!("{what} failed"),
                    c.stderr
                        .lines()
                        .rev()
                        .take(20)
                        .map(String::from)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect(),
                )),
                Err(e) => Err(fail(e, Vec::new())),
            }
        };
    let result = (|| {
        update(Update::Stage("Dolby Vision · extracting RPU".into()));
        step(
            runner::run_pipe(
                (&tools.ffmpeg, &hdr::extract_video_args(&req.probe.path)),
                ("dovi_tool", &hdr::extract_rpu_args(dovi.profile, &rpu)),
                cancel,
            ),
            "RPU extraction",
        )?;
        let decode = run_with_fallback(
            tools,
            req,
            |d| {
                let mut j = req.job(&video, d);
                j.crop = None;
                j.video_only = true;
                j.raw_hevc = true;
                command::build(&j)
            },
            &video,
            cancel,
            update,
        )?;
        update(Update::Stage("Dolby Vision · checking frame counts".into()));
        let info = step(
            runner::run_capture("dovi_tool", &hdr::rpu_info_args(&rpu), cancel),
            "dovi_tool info",
        )?;
        let count_args: Vec<OsString> = [
            "-v",
            "error",
            "-count_packets",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
        ]
        .iter()
        .map(OsString::from)
        .chain([video.clone().into_os_string()])
        .collect();
        let count = step(
            runner::run_capture(&tools.ffprobe, &count_args, cancel),
            "Frame count",
        )?;
        let encoded: Option<u64> = count.stdout.trim().parse().ok();
        let rpu_frames = hdr::parse_rpu_frames(&info.stdout);
        if let (Some(a), Some(b)) = (encoded, rpu_frames)
            && a != b
        {
            return Err(fail(
                format!(
                    "Frame counts differ: encoded {a}, RPU {b}. Dolby Vision can't be injected safely."
                ),
                Vec::new(),
            ));
        }
        update(Update::Stage("Dolby Vision · injecting RPU".into()));
        step(
            runner::run_capture(
                "dovi_tool",
                &hdr::inject_rpu_args(&video, &rpu, &injected),
                cancel,
            ),
            "RPU injection",
        )?;
        let _ = std::fs::remove_file(&video);
        update(Update::Stage("Dolby Vision · muxing".into()));
        let mux = command::build_dv_mux(&req.job(&req.output, decode), &injected)
            .map_err(|e| fail(e, Vec::new()))?;
        let r = runner::run_ffmpeg(&tools.ffmpeg, &mux, cancel, |p| update(Update::Progress(p)));
        if r.cancelled {
            let _ = std::fs::remove_file(&req.output);
            return Err(Outcome {
                cancelled: true,
                ..Default::default()
            });
        }
        if !r.success {
            let _ = std::fs::remove_file(&req.output);
            return Err(fail("Muxing failed", r.stderr_tail));
        }
        // Verify the DV configuration made it into the container.
        let expect = if dovi.profile == 5 { 5 } else { 8 };
        match probe::run(&tools.ffprobe, &req.output)
            .ok()
            .and_then(|p| p.hdr.dovi)
        {
            Some(d) if d.profile == expect => Ok(decode),
            Some(d) => Err(fail(
                format!(
                    "Output has Dolby Vision profile {}, expected {expect}",
                    d.profile
                ),
                Vec::new(),
            )),
            None => Err(fail(
                "Dolby Vision metadata is missing from the output",
                Vec::new(),
            )),
        }
    })();
    cleanup();
    result
}

/// Result of the background analysis after probing.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub crop: Result<Option<Crop>, String>,
    /// `None` when idet saw too little to decide.
    pub scan: Option<ScanType>,
}

/// Black bars and interlacing, from one pass over several segments (CPU decode).
pub fn analyze(tools: &Tools, p: &ProbeInfo, cancel: &Cancel) -> Result<Analysis, String> {
    let v = p.video.as_ref().ok_or("No video")?;
    let mut crops = Vec::new();
    let mut idet = scan::IdetCounts::default();
    let (mut ok_runs, mut failure) = (0, None);
    for start in crop::sample_starts(p.duration, crop::SAMPLES, crop::SAMPLE_SECONDS) {
        let r = runner::run_capture(
            &tools.ffmpeg,
            &crop::cropdetect_args(&p.path, start, crop::SAMPLE_SECONDS),
            cancel,
        )?;
        if r.cancelled {
            return Err("Cancelled".into());
        }
        if !r.success {
            failure = Some(runner::error_summary(&r.stderr));
            continue;
        }
        ok_runs += 1;
        crops.extend(crop::parse_cropdetect(&r.stderr));
        if let Some(c) = scan::parse_idet(&r.stderr) {
            idet.add(&c);
        }
    }
    // A failing ffmpeg (e.g. a build without a filter) is an error, not "inconclusive".
    if ok_runs == 0 {
        return Err(format!(
            "Analysis failed: {}",
            failure.unwrap_or_else(|| "no segments".into())
        ));
    }
    let crop = if crops.is_empty() {
        Err("cropdetect found nothing".into())
    } else {
        Ok(crop::aggregate(&crops, v.width, v.height))
    };
    let flagged_progressive = v.field_order.as_deref() == Some("progressive");
    Ok(Analysis {
        crop,
        scan: scan::classify(&idet, v.fps, flagged_progressive),
    })
}

pub fn preview_frame(
    tools: &Tools,
    p: &ProbeInfo,
    at: f64,
    c: Option<&Crop>,
    out: &Path,
) -> Result<(), String> {
    let r = runner::run_capture(
        &tools.ffmpeg,
        &crop::preview_args(&p.path, at, c, out),
        &Cancel::default(),
    )?;
    if r.success {
        Ok(())
    } else {
        Err(r
            .stderr
            .lines()
            .last()
            .unwrap_or("preview failed")
            .to_string())
    }
}

/// Sample-encode and extrapolate the video size (CQP / QVBR).
pub fn estimate_video(tools: &Tools, req: &Request, cancel: &Cancel) -> Result<SizeRange, String> {
    let job = req.job(Path::new("/dev/null"), req.first_decode());
    let samples = estimate::encode_samples(&tools.ffmpeg, &job, cancel, false)?;
    let pts: Vec<(u64, f64)> = samples.iter().map(|s| (s.bytes, s.secs)).collect();
    estimate::extrapolate(&pts, req.probe.duration).ok_or_else(|| "No sample data".into())
}

fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Before encoding: encode the sample segments and score them against the source.
pub fn vmaf_samples(tools: &Tools, req: &Request, cancel: &Cancel) -> Result<VmafResult, String> {
    let v = req.probe.video.as_ref().ok_or("No video")?;
    let job = req.job(Path::new("/dev/null"), req.first_decode());
    let samples = estimate::encode_samples(&tools.ffmpeg, &job, cancel, true)?;
    let mut scores = Vec::new();
    let mut err = None;
    for s in &samples {
        if err.is_none() {
            let input = vmaf::VmafInput {
                distorted: &s.path,
                distorted_seek: false,
                reference: &req.probe.path,
                start: s.start,
                secs: s.secs,
                width: v.width,
                height: v.height,
                ten_bit: v.bit_depth > 8,
                crop: req.crop,
                ref_filter: req.scan.cpu_filter(),
                threads: threads(),
            };
            match runner::run_capture(&tools.ffmpeg, &vmaf::args(&input), cancel) {
                Ok(r) if r.cancelled => err = Some("Cancelled".to_string()),
                Ok(r) => match vmaf::parse_score(&r.stderr) {
                    Some(sc) => scores.push(sc),
                    None => {
                        err = Some(
                            r.stderr
                                .lines()
                                .last()
                                .unwrap_or("no VMAF score")
                                .to_string(),
                        )
                    }
                },
                Err(e) => err = Some(e),
            }
        }
        let _ = std::fs::remove_file(&s.path);
    }
    if let Some(e) = err {
        return Err(e);
    }
    vmaf::summarize(&scores).ok_or_else(|| "No VMAF scores".into())
}

/// After encoding: score segments of the finished file at the sample timestamps.
fn vmaf_output(tools: &Tools, req: &Request, cancel: &Cancel) -> Result<VmafResult, String> {
    let v = req.probe.video.as_ref().ok_or("No video")?;
    let mut scores = Vec::new();
    for (start, secs) in estimate::sample_segments(req.probe.duration) {
        let input = vmaf::VmafInput {
            distorted: &req.output,
            distorted_seek: true,
            reference: &req.probe.path,
            start,
            secs,
            width: v.width,
            height: v.height,
            ten_bit: v.bit_depth > 8,
            crop: req.crop,
            ref_filter: req.scan.cpu_filter(),
            threads: threads(),
        };
        let r = runner::run_capture(&tools.ffmpeg, &vmaf::args(&input), cancel)?;
        if r.cancelled {
            return Err("Cancelled".into());
        }
        scores.push(vmaf::parse_score(&r.stderr).ok_or("no VMAF score")?);
    }
    vmaf::summarize(&scores).ok_or_else(|| "No VMAF scores".into())
}

/// Real-ffmpeg tests against the clips in `dev/media` (made by `dev/make-media.sh`).
/// Run with `cargo test -- --ignored`.
#[cfg(test)]
mod media_tests {
    use super::*;
    use crate::ffmpeg::command::DecodeMode;
    use crate::tracks::{AudioAction, auto_select};

    fn media(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("dev/media")
            .join(name)
    }

    fn tools() -> Tools {
        Tools::default()
    }

    #[test]
    #[ignore]
    fn remux_with_audio_conversion_and_dispositions() {
        let t = tools();
        let p = probe::run(&t.ffprobe, &media("sample.mkv")).unwrap();
        let settings = EncodeSettings {
            codec: VideoCodec::Copy,
            ..Default::default()
        };
        let (sel, _, q) = auto_select(&p, &["eng".into(), "nor".into()], &settings);
        assert!(q.is_empty());
        // TrueHD (default, eng) → E-AC-3; Norwegian AAC copied; commentary deselected.
        let picked: Vec<(usize, AudioAction)> =
            sel.selected_audio().map(|c| (c.index, c.action)).collect();
        assert_eq!(
            picked,
            vec![(1, AudioAction::Eac3 { kbps: 640 }), (4, AudioAction::Copy)]
        );
        let out = std::env::temp_dir().join("encoder-test-remux.mkv");
        let req = Request {
            probe: p.clone(),
            settings,
            selection: sel,
            output: out.clone(),
            device: "/dev/dri/renderD128".into(),
            crop: None,
            crop_on_gpu: false,
            scan: Scan::Progressive,
            video_kbps: None,
            dv_preserve: false,
            mkvpropedit: false,
            vmaf_after: false,
        };
        let mut progress = 0;
        let o = encode(&t, &req, &Cancel::default(), |u| {
            if let Update::Progress(_) = u {
                progress += 1;
            }
        });
        assert!(o.success, "{:?} {:?}", o.error, o.stderr_tail);
        assert!(progress > 0);
        assert!(o.report[0].starts_with("Output "));
        let r = probe::run(&t.ffprobe, &out).unwrap();
        let codecs: Vec<&str> = r.audio.iter().map(|a| a.codec.as_str()).collect();
        assert_eq!(codecs, ["eac3", "aac"]);
        assert!(r.audio[0].disposition.default && !r.audio[1].disposition.default);
        assert_eq!(r.audio[0].language.as_deref(), Some("eng"));
        assert_eq!(r.audio[0].title.as_deref(), Some("TrueHD 7.1"));
        assert_eq!(r.audio[1].language.as_deref(), Some("nor"));
        // English SRT (full) and Norwegian forced SRT, kept as text, forced flag kept.
        let subs: Vec<(&str, Option<&str>, bool)> = r
            .subtitles
            .iter()
            .map(|s| {
                (
                    s.codec.as_str(),
                    s.language.as_deref(),
                    s.disposition.forced,
                )
            })
            .collect();
        assert_eq!(
            subs,
            [
                ("subrip", Some("eng"), false),
                ("subrip", Some("nob"), true)
            ]
        );
        assert!((r.duration - p.duration).abs() < 0.5);
        // Chapters are kept.
        let ch = runner::run_capture(
            &t.ffprobe,
            &["-v", "error", "-show_chapters", "-of", "csv=p=0"]
                .map(OsString::from)
                .iter()
                .cloned()
                .chain([out.clone().into_os_string()])
                .collect::<Vec<_>>(),
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(ch.stdout.lines().count(), 3, "{}", ch.stdout);
        assert!(ch.stdout.contains("Middle"));
        let _ = std::fs::remove_file(out);
    }

    #[test]
    #[ignore]
    fn cancel_stops_ffmpeg_quickly() {
        let args: Vec<OsString> = [
            "-hide_banner",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25",
            "-f",
            "null",
            "-progress",
            "pipe:1",
            "-nostats",
            "-",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let cancel = Cancel::default();
        let c2 = cancel.clone();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1200));
            c2.cancel();
        });
        let mut got = false;
        let r = runner::run_ffmpeg("ffmpeg", &args, &cancel, |_| got = true);
        assert!(r.cancelled && !r.success);
        assert!(got, "progress parsed before cancel");
        // `q` is honoured well before the 4 s kill deadline.
        assert!(
            started.elapsed().as_secs_f64() < 4.0,
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    #[ignore]
    fn detects_letterbox() {
        let t = tools();
        let p = probe::run(&t.ffprobe, &media("sample.mkv")).unwrap();
        let a = analyze(&t, &p, &Cancel::default()).unwrap();
        assert_eq!(a.scan, Some(ScanType::Progressive));
        let c = a.crop.unwrap().unwrap();
        assert_eq!((c.w, c.h, c.x, c.y), (1920, 800, 0, 140));
        let png = std::env::temp_dir().join("encoder-preview.png");
        preview_frame(&t, &p, 10.0, Some(&c), &png).unwrap();
        assert!(std::fs::metadata(&png).unwrap().len() > 1000);
    }

    #[test]
    #[ignore]
    fn hdr10_metadata_from_frames() {
        let t = tools();
        let p = probe::run(&t.ffprobe, &media("hdr10.mkv")).unwrap();
        assert!(p.hdr.pq && p.hdr.bt2020);
        assert_eq!(p.hdr.cll, Some((1000, 400)));
        let m = p.hdr.mastering.clone().unwrap();
        assert!((m.max_luminance - 1000.0).abs() < 1e-6);
        assert_eq!(p.hdr.badge().as_deref(), Some("HDR10"));
    }

    #[test]
    #[ignore]
    fn vmaf_graph_runs() {
        // A software encode stands in for the VA-API sample.
        let t = tools();
        let src = media("sample.mkv");
        let dist = std::env::temp_dir().join("encoder-vmaf-dist.mkv");
        let enc: Vec<OsString> = [
            "-hide_banner",
            "-v",
            "error",
            "-y",
            "-ss",
            "5",
            "-t",
            "3",
            "-i",
        ]
        .iter()
        .map(OsString::from)
        .chain([src.clone().into_os_string()])
        .chain(
            [
                "-map",
                "0:v:0",
                "-vf",
                "scale=-2:720",
                "-c:v",
                "libx264",
                "-crf",
                "30",
                "-preset",
                "ultrafast",
            ]
            .iter()
            .map(OsString::from),
        )
        .chain([dist.clone().into_os_string()])
        .collect();
        assert!(
            runner::run_capture(&t.ffmpeg, &enc, &Cancel::default())
                .unwrap()
                .success
        );
        let input = vmaf::VmafInput {
            distorted: &dist,
            distorted_seek: false,
            reference: &src,
            start: 5.0,
            secs: 3.0,
            width: 1920,
            height: 1080,
            ten_bit: false,
            crop: None,
            ref_filter: None,
            threads: 4,
        };
        let r = runner::run_capture(&t.ffmpeg, &vmaf::args(&input), &Cancel::default()).unwrap();
        let score = vmaf::parse_score(&r.stderr).unwrap_or_else(|| panic!("{}", r.stderr));
        assert!((40.0..100.0).contains(&score), "{score}");
        let _ = std::fs::remove_file(dist);
    }

    #[test]
    #[ignore]
    fn sample_encode_reports_missing_vaapi_clearly() {
        // In a VM without VA-API the estimate must fail with a message, not hang.
        let t = tools();
        let p = probe::run(&t.ffprobe, &media("sample.mkv")).unwrap();
        let s = EncodeSettings {
            size_mode: crate::settings::SizeMode::ConstantQuality,
            ..Default::default()
        };
        let (sel, _, _) = auto_select(&p, &["eng".into()], &s);
        let req = Request {
            probe: p,
            settings: s,
            selection: sel,
            output: "/dev/null".into(),
            device: "/dev/dri/renderD128".into(),
            crop: None,
            crop_on_gpu: false,
            scan: Scan::Progressive,
            video_kbps: None,
            dv_preserve: false,
            mkvpropedit: false,
            vmaf_after: false,
        };
        assert_eq!(req.first_decode(), DecodeMode::Hardware);
        match estimate_video(&t, &req, &Cancel::default()) {
            Ok(r) => assert!(r.hi > 0), // a real GPU
            Err(e) => assert!(e.starts_with("Sample encode failed"), "{e}"),
        }
    }

    fn ffprobe_line(file: &Path, entries: &str) -> String {
        let args: Vec<OsString> = [
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            entries,
            "-of",
            "csv=p=0",
        ]
        .iter()
        .map(OsString::from)
        .chain([file.as_os_str().to_owned()])
        .collect();
        runner::run_capture("ffprobe", &args, &Cancel::default())
            .unwrap()
            .stdout
            .trim()
            .to_string()
    }

    #[test]
    #[ignore]
    fn detects_interlacing_from_the_picture() {
        let t = tools();
        let cases = [
            ("interlaced-576i.mkv", ScanType::Interlaced { tff: true }),
            ("telecine-480i.mkv", ScanType::Telecine { tff: true }),
            // Flagged interlaced, actually progressive.
            ("fakeint-1080.mkv", ScanType::Progressive),
            ("sample.mkv", ScanType::Progressive),
        ];
        for (file, want) in cases {
            let p = probe::run(&t.ffprobe, &media(file)).unwrap();
            let a = analyze(&t, &p, &Cancel::default()).unwrap();
            assert_eq!(a.scan, Some(want), "{file}");
        }
    }

    /// The CPU filters the encode would use give clean progressive output with
    /// the right frame rate and aspect ratio (software x264 stands in for VA-API).
    #[test]
    #[ignore]
    fn deinterlace_and_ivtc_filters_are_correct() {
        let t = tools();
        let run = |file: &str, plan: Scan| -> PathBuf {
            let out = std::env::temp_dir().join(format!("encoder-scan-{file}"));
            let args: Vec<OsString> = ["-hide_banner", "-v", "error", "-y", "-i"]
                .iter()
                .map(OsString::from)
                .chain([media(file).into_os_string()])
                .chain(["-map", "0:v:0", "-vf"].iter().map(OsString::from))
                .chain([OsString::from(plan.cpu_filter().unwrap())])
                .chain(
                    ["-c:v", "libx264", "-preset", "ultrafast", "-crf", "16"]
                        .iter()
                        .map(OsString::from),
                )
                .chain([out.clone().into_os_string()])
                .collect();
            let r = runner::run_capture(&t.ffmpeg, &args, &Cancel::default()).unwrap();
            assert!(r.success, "{}", r.stderr);
            out
        };
        let still_interlaced = |f: &Path| {
            let p = probe::run(&t.ffprobe, f).unwrap();
            analyze(&t, &p, &Cancel::default()).unwrap().scan
        };
        // 25i → 50p, anamorphic aspect kept.
        let int = run(
            "interlaced-576i.mkv",
            Scan::Deinterlace {
                tff: true,
                double_rate: true,
            },
        );
        assert_eq!(
            ffprobe_line(
                &int,
                "stream=r_frame_rate,sample_aspect_ratio,nb_read_frames"
            ),
            "64:45,50/1,600"
        );
        assert_eq!(still_interlaced(&int), Some(ScanType::Progressive));
        // 25i → 25p.
        let half = run(
            "interlaced-576i.mkv",
            Scan::Deinterlace {
                tff: true,
                double_rate: false,
            },
        );
        assert_eq!(
            ffprobe_line(&half, "stream=r_frame_rate,nb_read_frames"),
            "25/1,300"
        );
        // 29.97 telecine → 23.976 film frames (4 of every 5), no repeated fields left.
        let ivtc = run("telecine-480i.mkv", Scan::Telecine { tff: true });
        assert_eq!(
            ffprobe_line(&ivtc, "stream=r_frame_rate,nb_read_frames"),
            "24000/1001,288"
        );
        assert_eq!(still_interlaced(&ivtc), Some(ScanType::Progressive));
        for f in [int, half, ivtc] {
            let _ = std::fs::remove_file(f);
        }
    }

    #[test]
    #[ignore]
    fn analysis_failure_is_an_error() {
        let t = tools();
        let p = probe::run(&t.ffprobe, &media("sample.mkv")).unwrap();
        // An ffmpeg that always fails, like a build missing a filter.
        let broken = Tools {
            ffmpeg: "false".into(),
            ..t
        };
        let e = analyze(&broken, &p, &Cancel::default()).unwrap_err();
        assert!(e.starts_with("Analysis failed"), "{e}");
    }
}
