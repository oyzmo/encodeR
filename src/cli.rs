//! Headless modes for verifying the pipeline on a machine (no window needed):
//! `--print-command` shows what would run, `--encode` runs it and reports.

use std::path::{Path, PathBuf};

use crate::ffmpeg::capabilities::SizeLimits;
use crate::ffmpeg::estimate::{self, SizeRange};
use crate::ffmpeg::runner::Cancel;
use crate::ffmpeg::{capabilities, probe};
use crate::scan::{self, ScanOverride, ScanType};
use crate::settings::{EncodeSettings, Prefs, RateControl, VideoCodec};
use crate::subs::{self, ExternalSub, JobMode, SubsTarget};
use crate::worker::{self, Request, Tools, Update};
use crate::{checks, match_source, presets, queue, tracks};

pub struct Options {
    pub file: PathBuf,
    pub preset: Option<String>,
    pub out_dir: Option<PathBuf>,
    /// External SRT files to mux in (`--subs`).
    pub subs: Vec<PathBuf>,
    /// `--keep-dv` / `--keep-hdr10plus`: the experimental metadata pipelines,
    /// off by default like the app's switches.
    pub keep_dv: bool,
    pub keep_hdr10plus: bool,
}

struct Prepared {
    req: Request,
    tools: Tools,
    limits: Option<SizeLimits>,
    detected: Option<ScanType>,
    /// Why HDR10+ can't be kept, for the checks.
    hdr10plus_blocked: Option<&'static str>,
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
    let mut p = probe::run(&tools.ffprobe, &o.file)?;
    if let Some(v) = &p.video {
        let d = crate::damage::check(
            &tools.ffprobe,
            &p.path,
            v.index,
            p.duration,
            &Cancel::default(),
        )?;
        eprintln!(
            "damage  {}",
            d.as_ref()
                .map_or_else(|| "none found".into(), |d| d.describe())
        );
        p.set_damage(d);
    }
    let (mut sel, _, questions) = tracks::auto_select(&p, &prefs.preferred_languages, &settings);
    let externals: Vec<ExternalSub> = o
        .subs
        .iter()
        .map(|s| ExternalSub::from_path(&o.file, s.clone(), prefs.sync_subs))
        .collect();
    subs::prefer_externals(&mut sel, &p, &externals);
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
    let inject = crate::hdr::InjectJob::new(&settings, &p, scan_plan);
    let dv_blocked = crate::hdr::dv_blocker(p.hdr.dovi, inject, caps.dovi_tool);
    let hdr10plus_blocked =
        crate::hdr::hdr10plus_blocker(p.hdr.hdr10plus, inject, caps.hdr10plus_tool);
    let refuse = |flag: &str, what: &str, has: bool, why: Option<&str>| {
        if !has {
            Err(format!("{flag}: the source has no {what}"))
        } else {
            why.map_or(Ok(()), |w| Err(format!("{flag}: {w}")))
        }
    };
    if o.keep_dv {
        refuse(
            "--keep-dv",
            "Dolby Vision",
            p.hdr.dovi.is_some(),
            dv_blocked,
        )?;
    }
    if o.keep_hdr10plus {
        refuse(
            "--keep-hdr10plus",
            "HDR10+",
            p.hdr.hdr10plus,
            hdr10plus_blocked,
        )?;
    }
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
        dv_preserve: o.keep_dv,
        hdr10plus_preserve: o.keep_hdr10plus,
        mkvpropedit: caps.mkvpropedit,
        externals,
        mode: JobMode::Encode,
        alass: caps.alass.clone(),
        cover: Default::default(),
        covers: Vec::new(),
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
        hdr10plus_blocked,
    })
}

fn command_line(req: &Request, tools: &Tools) -> String {
    req.describe_commands(&tools.ffmpeg)
}

fn print_checks(
    req: &Request,
    limits: Option<SizeLimits>,
    detected: Option<ScanType>,
    est: Option<SizeRange>,
    hdr10plus_blocked: Option<&'static str>,
) {
    for i in checks::run(&checks::CheckInput {
        probe: &req.probe,
        settings: &req.settings,
        selection: &req.selection,
        video_kbps: req.video_kbps,
        estimate: est,
        vmaf: None,
        dv_preserve: req.dv_preserve,
        hdr10plus_preserve: req.hdr10plus_preserve,
        hdr10plus_blocked,
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
    println!("{}", command_line(&p.req, &p.tools));
    print_checks(&p.req, p.limits, p.detected, None, p.hdr10plus_blocked);
    Ok(())
}

/// `--match`: the "match source quality" estimate, then the VMAF search.
pub fn match_quality(o: &Options) -> Result<(), String> {
    let Prepared { req, tools, .. } = prepare(o)?;
    let p = &req.probe;
    let v = p.video.as_ref().ok_or("No video")?;
    let src = p.video_bit_rate().ok_or("Unknown source video bitrate")? as f64 / 1000.0;
    let codec = req.settings.codec;
    let factor = match_source::factor(&v.codec, codec)
        .ok_or_else(|| format!("No efficiency figure for {} → {}", v.codec, codec.label()))?;
    let kbps = match_source::estimate_kbps(p, &req.settings).ok_or("No estimate")?;
    let (audio, subs) = (
        req.selection.audio_bytes(p),
        req.selection.subtitle_bytes(p),
    );
    let gb = |k| match_source::target_gb(k, p.duration, audio, subs);
    println!(
        "source  {} {:.0} kb/s video · {}",
        v.codec,
        src,
        estimate::format_bytes(p.size_bytes)
    );
    println!(
        "estimate  {} needs {factor:.2}× → {kbps} kb/s video → {:.1} GB",
        codec.label(),
        gb(kbps)
    );
    let found = worker::match_quality(&tools, &req, kbps, &Cancel::default(), |line| {
        eprintln!("· {line}")
    })?;
    match found.kbps {
        Some(k) => println!(
            "vmaf  {k} kb/s → {:.1} GB (mean {:.2}, worst {:.2}); measured factor {:.2}×",
            gb(k),
            found.score.mean,
            found.score.worst,
            k as f64 / src
        ),
        None => println!(
            "vmaf  not transparent even at {} kb/s ({:.1} GB): mean {:.2}, worst {:.2}",
            found.tried_max,
            gb(found.tried_max),
            found.score.mean,
            found.score.worst
        ),
    }
    Ok(())
}

pub fn encode(o: &Options) -> Result<(), String> {
    let Prepared {
        req,
        tools,
        limits,
        detected,
        hdr10plus_blocked,
    } = prepare(o)?;
    println!("input   {}", req.probe.path.display());
    println!("command {}", command_line(&req, &tools));
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
    // Like the app: size-dependent checks (output not smaller, same codec)
    // need the estimate.
    print_checks(&req, limits, detected, est, hdr10plus_blocked);
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
    let mut subs = Vec::new();
    let (mut keep_dv, mut keep_hdr10plus) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--preset" => preset = Some(it.next().ok_or("--preset needs a name")?.clone()),
            "--subs" => subs.push(PathBuf::from(it.next().ok_or("--subs needs a file")?)),
            "--out" => out_dir = Some(PathBuf::from(it.next().ok_or("--out needs a folder")?)),
            "--keep-dv" => keep_dv = true,
            "--keep-hdr10plus" => keep_hdr10plus = true,
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
        subs,
        keep_dv,
        keep_hdr10plus,
    })
}

/// A folder deleted when it goes out of scope.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `--add-subs VIDEO [SRT…] [--embed] [--no-sync]`: the subtitles-only job.
/// SRT arguments may be zips of SRT files. Without any, the video's sidecars
/// are used.
pub fn add_subs(args: &[String]) -> Result<(), String> {
    let mut video: Option<PathBuf> = None;
    let mut files = Vec::new();
    let mut target = SubsTarget::Sidecar;
    let mut sync = true;
    for a in args {
        match a.as_str() {
            "--embed" => target = SubsTarget::Embed,
            "--no-sync" => sync = false,
            f if f.starts_with("--") => return Err(format!("Unexpected argument {f}")),
            f if video.is_none() => video = Some(PathBuf::from(f)),
            f => files.push(PathBuf::from(f)),
        }
    }
    let video = video.ok_or("Missing VIDEO")?;
    // Zips of SRT files are unpacked to a temp folder for this run, removed
    // however the run ends. The time in the name keeps two runs apart in a
    // Flatpak, where every sandbox has the same pid.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let unzip = TempDir(
        crate::settings::cache_dir().join(format!("unzip-{}-cli-{nanos}", std::process::id())),
    );
    let unzip_dir = &unzip.0;
    let mut unpacked = Vec::new();
    for (k, f) in files.into_iter().enumerate() {
        if subs::is_zip(&f) {
            unpacked.extend(subs::unpack_zip(&f, &unzip_dir.join(k.to_string()))?);
        } else {
            unpacked.push(f);
        }
    }
    let mut files = unpacked;
    if files.is_empty() {
        files = subs::discover(&video);
    }
    if files.is_empty() {
        return Err("No subtitle files given or found next to the video".into());
    }
    let prefs = Prefs::load();
    let tools = Tools::default();
    let p = probe::run(&tools.ffprobe, &video)?;
    let settings = prefs.last_settings.clone();
    let (sel, _, _) = tracks::auto_select(&p, &prefs.preferred_languages, &settings);
    let externals: Vec<ExternalSub> = files
        .into_iter()
        .map(|f| ExternalSub::from_path(&video, f, sync))
        .collect();
    let alass = crate::alass::find();
    for e in &externals {
        println!(
            "subs    {} · {} · {}",
            e.file_name(),
            e.language_name(),
            e.kind.short()
        );
    }
    let req = Request {
        output: video.clone(),
        device: String::new(),
        crop: None,
        crop_on_gpu: false,
        scan: crate::scan::Scan::Progressive,
        video_kbps: None,
        dv_preserve: false,
        hdr10plus_preserve: false,
        mkvpropedit: false,
        externals,
        mode: JobMode::Fix(target),
        alass,
        cover: Default::default(),
        covers: Vec::new(),
        settings,
        selection: sel,
        probe: p,
    };
    let out = worker::encode(&tools, &req, &Cancel::default(), |u| {
        if let Update::Stage(s) = u {
            eprintln!("· {s}");
        }
    });
    if !out.success {
        for l in &out.stderr_tail {
            eprintln!("  | {l}");
        }
        return Err(out.error.unwrap_or_else(|| "cancelled".into()));
    }
    for line in &out.report {
        println!("result  {line}");
    }
    Ok(())
}
