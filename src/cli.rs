//! Headless modes for verifying the pipeline on a machine (no window needed):
//! `--print-command` shows what would run, `--encode` runs it and reports.

use std::path::{Path, PathBuf};

use crate::ffmpeg::capabilities::SizeLimits;
use crate::ffmpeg::estimate::{self, SizeRange};
use crate::ffmpeg::runner::Cancel;
use crate::ffmpeg::{capabilities, command, probe};
use crate::scan::{self, ScanOverride, ScanType};
use crate::settings::{EncodeSettings, Prefs, RateControl, VideoCodec};
use crate::worker::{self, Request, Tools, Update};
use crate::{checks, presets, queue, tracks};

pub struct Options {
    pub file: PathBuf,
    pub preset: Option<String>,
    pub out_dir: Option<PathBuf>,
}

struct Prepared {
    req: Request,
    tools: Tools,
    limits: Option<SizeLimits>,
    detected: Option<ScanType>,
}

fn prepare(o: &Options) -> Result<Prepared, String> {
    let mut prefs = Prefs::load();
    if o.out_dir.is_some() {
        prefs.output_dir = o.out_dir.clone();
    }
    let settings: EncodeSettings = match &o.preset {
        Some(name) => {
            presets::find(&prefs.user_presets, name)
                .ok_or(format!("No preset called “{name}”"))?
                .settings
        }
        None => prefs.last_settings.clone(),
    };
    let tools = Tools::default();
    let p = probe::run(&tools.ffprobe, &o.file)?;
    let (sel, _, questions) = tracks::auto_select(&p, &prefs.preferred_languages, &settings);
    for q in &questions {
        eprintln!(
            "note: only PGS subtitles for {}; the app would ask (left out here)",
            q.name
        );
    }
    let caps = capabilities::detect(&tools.ffmpeg, &tools.ffprobe, prefs.device.as_deref());
    let cc = caps.codec(settings.codec);
    if !cc.available {
        return Err(format!(
            "{} unavailable: {}",
            settings.codec.label(),
            cc.reason.unwrap_or_default()
        ));
    }
    let kbps = (settings.bitrate_from_target() && settings.codec != VideoCodec::Copy)
        .then(|| {
            estimate::target_video_kbps(
                settings.target_gb,
                p.duration,
                sel.audio_bytes(&p),
                sel.subtitle_bytes(&p),
            )
        })
        .flatten();
    // Same automatic interlace decision as the app (crop stays off, as in the app's default).
    let detected = worker::analyze(&tools, &p, &Cancel::default())?.scan;
    let field_order = p.video.as_ref().and_then(|v| v.field_order.clone());
    let scan_plan = scan::plan(
        detected,
        field_order.as_deref(),
        ScanOverride::Auto,
        settings.deinterlace_double_rate,
    );
    let fps = p.video.as_ref().map_or(0.0, |v| v.fps);
    eprintln!(
        "scan    flags {} · detected {} · {}",
        field_order.as_deref().unwrap_or("none"),
        detected.map_or("inconclusive", |d| d.label()),
        scan_plan.describe(fps)
    );
    let req = Request {
        scan: scan_plan,
        output: queue::output_path(&p.path, &prefs, settings.container),
        device: caps
            .device
            .clone()
            .unwrap_or_else(|| "/dev/dri/renderD128".into()),
        crop: None,
        crop_on_gpu: caps.crop_on_gpu,
        video_kbps: kbps,
        dv_preserve: p.hdr.dovi.is_some()
            && crate::hdr::dv_blocker(
                p.hdr.dovi,
                settings.codec == VideoCodec::Hevc,
                p.video
                    .as_ref()
                    .is_some_and(|v| settings.downscale.height().is_some_and(|h| h < v.height)),
                caps.dovi_tool,
            )
            .is_none(),
        mkvpropedit: caps.mkvpropedit,
        vmaf_after: false,
        settings,
        selection: sel,
        probe: p,
    };
    eprintln!(
        "device {} · crop on GPU: {} · 10-bit: {} · rate control: {:?}",
        req.device, caps.crop_on_gpu, cc.ten_bit, cc.rate_controls
    );
    if !caps.missing_filters.is_empty() {
        eprintln!(
            "warning: ffmpeg lacks filters: {}",
            caps.missing_filters.join(", ")
        );
    }
    let limits = cc.limits;
    Ok(Prepared {
        req,
        tools,
        limits,
        detected,
    })
}

fn command_line(req: &Request, tools: &Tools) -> Result<String, String> {
    let j = req.job(&req.output, req.first_decode());
    Ok(command::display_command(
        &tools.ffmpeg,
        &command::build(&j)?,
    ))
}

fn print_checks(req: &Request, limits: Option<SizeLimits>, detected: Option<ScanType>) {
    for i in checks::run(&checks::CheckInput {
        probe: &req.probe,
        settings: &req.settings,
        selection: &req.selection,
        video_kbps: req.video_kbps,
        estimate: None,
        vmaf: None,
        dv_preserve: req.dv_preserve,
        crop: false,
        active_crop: None,
        encoder_limits: limits,
        scan_detected: detected,
        scan: req.scan,
    }) {
        eprintln!("check {:?}: {}", i.severity, i.message);
    }
}

pub fn print_command(o: &Options) -> Result<(), String> {
    let p = prepare(o)?;
    println!("{}", command_line(&p.req, &p.tools)?);
    print_checks(&p.req, p.limits, p.detected);
    Ok(())
}

pub fn encode(o: &Options) -> Result<(), String> {
    let Prepared {
        req,
        tools,
        limits,
        detected,
    } = prepare(o)?;
    println!("input   {}", req.probe.path.display());
    println!("command {}", command_line(&req, &tools)?);
    print_checks(&req, limits, detected);
    let audio = req.selection.audio_bytes(&req.probe);
    let subs = req.selection.subtitle_bytes(&req.probe);
    let rc = req.settings.effective_rate_control();
    let est: Option<SizeRange> = if req.settings.codec == VideoCodec::Copy {
        let s = estimate::copy_size(&req.probe, &req.selection);
        Some(SizeRange { lo: s, hi: s })
    } else if rc.uses_bitrate() && rc != RateControl::Qvbr {
        let k = req.video_kbps.unwrap_or(req.settings.bitrate_kbps);
        Some(estimate::bitrate_range(
            k,
            req.probe.duration,
            audio,
            subs,
            false,
        ))
    } else {
        let t = std::time::Instant::now();
        let r = worker::estimate_video(&tools, &req, &Cancel::default())?;
        println!("sampled estimate in {:.1} s", t.elapsed().as_secs_f64());
        Some(estimate::with_streams(r, audio, subs))
    };
    if let Some(e) = est {
        println!("estimate {}", estimate::describe(e, req.probe.size_bytes));
    }
    let started = std::time::Instant::now();
    let mut last = -1i64;
    let out = worker::encode(&tools, &req, &Cancel::default(), |u| match u {
        Update::Stage(s) => eprintln!("· {s}"),
        Update::Progress(p) => {
            let pct = (p.fraction(req.probe.duration) * 100.0) as i64;
            if pct / 10 != last / 10 {
                eprintln!("  {pct}% {:.2}×", p.speed.unwrap_or(0.0));
                last = pct;
            }
        }
    });
    if !out.success {
        for l in &out.stderr_tail {
            eprintln!("  | {l}");
        }
        return Err(out.error.unwrap_or_else(|| "cancelled".into()));
    }
    println!("decode  {}", out.decode.map_or("-", |d| d.label()));
    println!("time    {:.1} s", started.elapsed().as_secs_f64());
    for line in &out.report {
        println!("result  {line}");
    }
    if let (Some(e), Ok(m)) = (est, std::fs::metadata(&req.output)) {
        let actual = m.len() as f64;
        let mid = (e.lo + e.hi) as f64 / 2.0;
        println!(
            "estimate vs actual: {:+.1}% (estimate midpoint {}, actual {})",
            (mid / actual - 1.0) * 100.0,
            estimate::format_bytes(mid as u64),
            estimate::format_bytes(actual as u64)
        );
    }
    println!("output  {}", req.output.display());
    Ok(())
}

pub fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut file = None;
    let mut preset = None;
    let mut out_dir = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--preset" => preset = Some(it.next().ok_or("--preset needs a name")?.clone()),
            "--out" => out_dir = Some(PathBuf::from(it.next().ok_or("--out needs a folder")?)),
            f if !f.starts_with("--") && file.is_none() => file = Some(PathBuf::from(f)),
            other => return Err(format!("Unexpected argument {other}")),
        }
    }
    let file = file.ok_or("Missing FILE")?;
    if !Path::new(&file).is_file() {
        return Err(format!("{} is not a file", file.display()));
    }
    Ok(Options {
        file,
        preset,
        out_dir,
    })
}
