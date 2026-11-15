//! Blocking job pipelines that run on worker threads: encode (with software
//! fallback and HDR verification), the Dolby Vision path, crop detection,
//! size estimation and VMAF.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::checks;
use crate::cover;
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
use crate::subs::{self, ExternalSub, JobMode, SubsTarget};
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
    /// External SRT files (the job's list; only selected ones are used).
    pub externals: Vec<ExternalSub>,
    pub mode: JobMode,
    /// alass program, when installed.
    pub alass: Option<String>,
    pub cover: crate::cover::CoverChoice,
    /// Cover art files ready to embed (filled in by the worker).
    pub covers: Vec<crate::cover::EmbedCover>,
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
        j.externals = &self.externals;
        j.covers = &self.covers;
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

/// Where an encode writes until it succeeds: `Film-reencoded.part.mkv`. A
/// crash or power loss then never leaves a truncated file under the final name.
pub fn partial_path(output: &Path) -> PathBuf {
    let stem = output.file_stem().unwrap_or_default().to_string_lossy();
    let name = match output.extension() {
        Some(ext) => format!("{stem}.part.{}", ext.to_string_lossy()),
        None => format!("{stem}.part"),
    };
    output.with_file_name(name)
}

pub fn encode(
    tools: &Tools,
    req: &Request,
    cancel: &Cancel,
    mut update: impl FnMut(Update),
) -> Outcome {
    if let JobMode::Fix(target) = req.mode {
        return fix_tracks(tools, req, target, cancel, &mut update);
    }
    // Last line of defence: the queue picks outputs that aren't the input, but
    // a failed or cancelled job deletes its output, so check the real file.
    let part = partial_path(&req.output);
    if [&req.output, &part]
        .iter()
        .any(|o| crate::queue::same_file(o, &req.probe.path))
    {
        return fail(
            "The output file would overwrite the input (same file under another path)",
            Vec::new(),
        );
    }
    let prepared = if req.externals.iter().any(|e| e.selected) {
        match prepare_externals(tools, req, cancel, &mut update) {
            Ok(p) => Some(p),
            Err(o) => return o,
        }
    } else {
        None
    };
    let cover = match prepare_cover(tools, req, req.settings.container, false, cancel) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let work = Request {
        output: part.clone(),
        externals: prepared
            .as_ref()
            .map_or_else(|| req.externals.clone(), |p| p.externals.clone()),
        covers: cover.plan.embed.clone(),
        ..req.clone()
    };
    let result = if work.dv_preserve && work.probe.hdr.dovi.is_some() {
        encode_dv(tools, &work, cancel, &mut update)
    } else {
        run_with_fallback(
            tools,
            &work,
            |d| command::build(&work.job(&work.output, d)),
            &work.output,
            cancel,
            &mut update,
        )
    };
    let decode = match result {
        Ok(d) => d,
        Err(o) => return o,
    };
    if let Err(e) = std::fs::rename(&part, &req.output) {
        let _ = std::fs::remove_file(&part);
        return fail(
            format!("Couldn't move the finished file into place: {e}"),
            Vec::new(),
        );
    }
    let encoded = req.settings.codec != VideoCodec::Copy;
    let mut out = Outcome {
        success: true,
        decode: encoded.then_some(decode),
        ..Default::default()
    };
    out.report.push(size_report(req));
    if let Some(p) = &prepared {
        out.report.extend(p.report.iter().cloned());
    }
    out.report.extend(cover.report.iter().cloned());
    out.report
        .extend(save_poster(&cover, &req.output).map(|r| r.unwrap_or_else(|e| e)));
    if req.settings.codec != VideoCodec::Copy {
        update(Update::Stage("Checking output".into()));
        out.report.extend(hdr_report(tools, req));
    }
    out
}

/// External subtitles made ready for ffmpeg: clean UTF-8 SRT files in a
/// private temp folder that is removed when this is dropped.
struct PreparedSubs {
    /// Selected externals only, `path` pointing at the prepared file.
    externals: Vec<ExternalSub>,
    report: Vec<String>,
    dir: PathBuf,
}

impl Drop for PreparedSubs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Cover art made ready for ffmpeg: extracted and converted pictures in a
/// private temp folder that is removed when this is dropped.
struct PreparedCover {
    plan: cover::Plan,
    report: Vec<String>,
    dir: Option<PathBuf>,
}

impl Drop for PreparedCover {
    fn drop(&mut self) {
        if let Some(d) = &self.dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Extract the source covers and convert new pictures, as `cover::plan`
/// says. A picture the user chose that can't be read fails the job before
/// anything is encoded; a source cover or poster that fails is reported.
fn prepare_cover(
    tools: &Tools,
    req: &Request,
    container: crate::settings::Container,
    fix: bool,
    cancel: &Cancel,
) -> Result<PreparedCover, Outcome> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = cache_dir().join(format!("cover-{}-{nanos}", std::process::id()));
    let plan = cover::plan(&req.probe, &req.cover, container, fix, &dir);
    let mut out = PreparedCover {
        dir: None,
        report: plan
            .skipped
            .iter()
            .map(|n| format!("⚠ Left out cover art {n}: MP4 only holds JPEG, PNG and BMP covers"))
            .collect(),
        plan: cover::Plan::default(),
    };
    if plan.extract.is_empty() && plan.convert.is_empty() {
        out.plan = plan;
        return Ok(out);
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| fail(format!("Couldn't create a temp folder: {e}"), Vec::new()))?;
    out.dir = Some(dir);
    out.plan = plan;
    for (i, file) in out.plan.extract.clone() {
        let args = command::build_cover_extract(&req.probe, i, &file);
        let ok = runner::run_capture(&tools.ffmpeg, &args, cancel).is_ok_and(|r| r.success);
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if !(ok && file.exists()) {
            out.plan.extract_failed(i);
            let _ = std::fs::remove_file(&file);
        }
    }
    let chosen = match &req.cover.action {
        cover::CoverAction::Replace(f) => Some(f.clone()),
        _ => None,
    };
    for (from, to) in out.plan.convert.clone() {
        let args = cover::convert_args(&from, &to, req.cover.poster_crop);
        let r = if from.exists() {
            runner::run_capture(&tools.ffmpeg, &args, cancel).ok()
        } else {
            None
        };
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if r.as_ref().is_some_and(|r| r.success) && to.exists() {
            continue;
        }
        let why = r
            .map(|r| runner::error_summary(&r.stderr))
            .unwrap_or_else(|| "the file is missing".into());
        if chosen.as_ref() == Some(&from) {
            let name = from.file_name().unwrap_or_default().to_string_lossy();
            return Err(fail(
                format!("Couldn't read the cover image {name}: {why}"),
                Vec::new(),
            ));
        }
        out.plan.poster = None;
        out.report.push(format!(
            "⚠ Couldn't make the poster from the cover art: {why}"
        ));
    }
    Ok(out)
}

/// Copy the prepared poster next to `video` (`Movie.jpg`). A different
/// poster already there is backed up first (`Movie.jpg.orig`, then
/// `Movie.jpg.orig2`, …) unless a backup already holds the same picture.
/// `Err` is a report line for a poster that couldn't be saved.
fn save_poster(cover: &PreparedCover, video: &Path) -> Option<Result<String, String>> {
    let src = cover.plan.poster.as_ref()?;
    let dest = cover::poster_path(video);
    let name = dest
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    // Ok((written, backup made))
    let r = (|| -> std::io::Result<(bool, Option<PathBuf>)> {
        let new = std::fs::read(src)?;
        let mut backup = None;
        if let Ok(old) = std::fs::read(&dest) {
            if old == new {
                return Ok((false, None));
            }
            let first = subs::backup_path(&dest);
            let mut n = 1;
            loop {
                let b = if n == 1 {
                    first.clone()
                } else {
                    let mut s = first.as_os_str().to_owned();
                    s.push(n.to_string());
                    PathBuf::from(s)
                };
                match std::fs::read(&b) {
                    Ok(kept) if kept == old => break,
                    Ok(_) => n += 1,
                    Err(_) => {
                        std::fs::write(&b, &old)?;
                        backup = Some(b);
                        break;
                    }
                }
            }
        }
        std::fs::write(&part, &new)?;
        std::fs::rename(&part, &dest)?;
        Ok((true, backup))
    })();
    Some(match r {
        Ok((_, Some(b))) => Ok(format!(
            "Saved poster {name} (the one that was there is kept as {})",
            b.file_name().unwrap_or_default().to_string_lossy()
        )),
        Ok((true, None)) => Ok(format!("Saved poster {name}")),
        Ok((false, _)) => Ok(format!("Poster {name} is already this picture")),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(format!("⚠ Couldn't save poster {name}: {e}"))
        }
    })
}

fn cancelled() -> Outcome {
    Outcome {
        cancelled: true,
        ..Default::default()
    }
}

/// Decode, sync (alass) and shift every selected external subtitle file.
/// A failed sync isn't fatal: the file is used as it is and the report says so.
fn prepare_externals(
    tools: &Tools,
    req: &Request,
    cancel: &Cancel,
    update: &mut impl FnMut(Update),
) -> Result<PreparedSubs, Outcome> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = cache_dir().join(format!("subs-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| fail(format!("Couldn't create a temp folder: {e}"), Vec::new()))?;
    let mut out = PreparedSubs {
        externals: Vec::new(),
        report: Vec::new(),
        dir: dir.clone(),
    };
    // Extracted once per job, on first use.
    let mut reference: Option<Result<(PathBuf, String), String>> = None;
    // Subtitles synced to an audio track follow that track's lip-sync offset.
    // Fix-tracks jobs shift every track with a delay, except next to the
    // video, where the audio isn't touched.
    let ref_shift = match (req.mode, subs::sync_reference(&req.probe, &req.selection)) {
        (JobMode::Fix(SubsTarget::Sidecar), _) => 0,
        (JobMode::Fix(_), Some(subs::SyncRef::Audio(i))) => subs::audio_delays(&req.selection)
            .into_iter()
            .find(|(k, _)| *k == i)
            .map_or(0, |(_, ms)| ms),
        (JobMode::Fix(_), _) => 0,
        (_, Some(subs::SyncRef::Audio(i))) => req
            .selection
            .selected_audio()
            .find(|c| c.index == i)
            .map_or(0, |c| c.offset_ms),
        _ => 0,
    };
    for (k, e) in req.externals.iter().filter(|e| e.selected).enumerate() {
        let name = e.file_name();
        let bytes = std::fs::read(&e.path)
            .map_err(|err| fail(format!("Couldn't read {name}: {err}"), Vec::new()))?;
        let (text, decode_note) = subs::decode_text_noted(&bytes);
        let mut cues =
            subs::parse_srt(&text).map_err(|err| fail(format!("{name}: {err}"), Vec::new()))?;
        let mut notes: Vec<String> = decode_note.map(String::from).into_iter().collect();
        if e.sync {
            match &req.alass {
                None => notes.push("not synced (alass isn't installed)".into()),
                Some(alass) => {
                    if reference.is_none() {
                        reference = Some(extract_reference(tools, req, &dir, cancel, update)?);
                    }
                    match reference.as_ref().expect("set above") {
                        Err(why) => notes.push(format!("not synced ({why})")),
                        Ok((rf, what)) => {
                            update(Update::Stage(format!("Syncing {name}")));
                            let input = dir.join(format!("in-{k}.srt"));
                            let synced = dir.join(format!("synced-{k}.srt"));
                            std::fs::write(&input, subs::write_srt(&cues)).map_err(|err| {
                                fail(format!("Couldn't write a temp file: {err}"), Vec::new())
                            })?;
                            let r = runner::run_capture(
                                alass,
                                &subs::alass_args(rf, &input, &synced),
                                cancel,
                            );
                            if cancel.is_cancelled() {
                                return Err(cancelled());
                            }
                            let result = match r {
                                Ok(c) if c.success => std::fs::read(&synced)
                                    .map_err(|e| e.to_string())
                                    .and_then(|b| subs::parse_srt(&subs::decode_text(&b))),
                                Ok(c) => Err(c
                                    .stderr
                                    .lines()
                                    .rev()
                                    .find(|l| !l.trim().is_empty())
                                    .unwrap_or("alass failed")
                                    .to_string()),
                                Err(e) => Err(e),
                            };
                            match result {
                                Ok(after) => {
                                    notes.push(format!(
                                        "{} against {what}",
                                        subs::describe_sync(&cues, &after)
                                    ));
                                    cues = after;
                                    if ref_shift != 0 {
                                        cues = subs::shift(&cues, ref_shift);
                                        notes.push(format!(
                                            "moved {:+.2} s with the audio",
                                            ref_shift as f64 / 1000.0
                                        ));
                                    }
                                }
                                Err(why) => {
                                    notes.push(format!("sync failed, used as it is ({why})"))
                                }
                            }
                        }
                    }
                }
            }
        }
        if e.offset_ms != 0 {
            cues = subs::shift(&cues, e.offset_ms);
            notes.push(format!(
                "moved {:+.2} s by hand",
                e.offset_ms as f64 / 1000.0
            ));
        }
        let path = dir.join(format!("{k}.srt"));
        std::fs::write(&path, subs::write_srt(&cues))
            .map_err(|err| fail(format!("Couldn't write a temp file: {err}"), Vec::new()))?;
        let mut line = format!("{} subtitles ({name})", e.language_name());
        if !notes.is_empty() {
            line.push_str(": ");
            line.push_str(&notes.join(", "));
        }
        out.report.push(line);
        out.externals.push(ExternalSub { path, ..e.clone() });
    }
    Ok(out)
}

/// The sync reference as a file, with a description ("the English audio").
/// `Err(Outcome)` only for cancel; a failed extraction is an inner `Err`.
#[allow(clippy::type_complexity)]
fn extract_reference(
    tools: &Tools,
    req: &Request,
    dir: &Path,
    cancel: &Cancel,
    update: &mut impl FnMut(Update),
) -> Result<Result<(PathBuf, String), String>, Outcome> {
    let Some(r) = subs::sync_reference(&req.probe, &req.selection) else {
        return Ok(Err("the video has no audio to sync against".into()));
    };
    let file = dir.join(match r {
        subs::SyncRef::Subtitle(_) => "reference.srt",
        subs::SyncRef::Audio(_) => "reference.wav",
    });
    update(Update::Stage(format!(
        "Reading {} for syncing",
        r.describe(&req.probe)
    )));
    let res = runner::run_ffmpeg(
        &tools.ffmpeg,
        &subs::reference_args(&req.probe.path, r, &file),
        cancel,
        |p| update(Update::Progress(p)),
    );
    if res.cancelled {
        return Err(cancelled());
    }
    if !res.success {
        return Ok(Err(format!(
            "couldn't read the sync reference: {}",
            runner::error_summary(&res.stderr_tail.join("\n"))
        )));
    }
    Ok(Ok((file, r.describe(&req.probe))))
}

/// Fix-tracks job: sync the subtitle files and save them next to the video,
/// or remux them and any audio delays into the video or a new file. Nothing
/// is re-encoded.
fn fix_tracks(
    tools: &Tools,
    req: &Request,
    target: SubsTarget,
    cancel: &Cancel,
    update: &mut impl FnMut(Update),
) -> Outcome {
    let video = &req.probe.path;
    let work = subs::FixWork {
        delays: !subs::audio_delays(&req.selection).is_empty(),
        cover: req.cover.changes_embedded(&req.probe),
        new_cover: matches!(req.cover.action, cover::CoverAction::Replace(_)),
        poster: req.cover.wants_poster(&req.probe),
    };
    // Fail on a bad job before spending time on syncing.
    if let Err(e) = subs::fix_problem(video, target, &req.externals, work) {
        return fail(e, Vec::new());
    }
    let prepared = match prepare_externals(tools, req, cancel, update) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let remux = target != SubsTarget::Sidecar && (!prepared.externals.is_empty() || work.remux());
    // Without a remux only the poster is made, and an MP4 plan extracts
    // nothing but the picture for it.
    let container = match target {
        _ if !remux => crate::settings::Container::Mp4,
        _ => subs::fix_container(video, target).unwrap_or(crate::settings::Container::Mkv),
    };
    let cover = match prepare_cover(tools, req, container, true, cancel) {
        Ok(c) => c,
        Err(o) => return o,
    };
    let done = || Outcome {
        success: true,
        ..Default::default()
    };
    let mut out = match target {
        SubsTarget::Sidecar if prepared.externals.is_empty() => done(),
        SubsTarget::Sidecar => save_sidecars(video, &prepared),
        _ if !remux => done(),
        SubsTarget::Embed => remux_fix(tools, req, &prepared, &cover.plan, video, cancel, update),
        SubsTarget::NewFile => {
            if crate::queue::same_file(&req.output, video) {
                return fail("The output file would overwrite the input", Vec::new());
            }
            remux_fix(
                tools,
                req,
                &prepared,
                &cover.plan,
                &req.output,
                cancel,
                update,
            )
        }
    };
    if out.success {
        out.report.extend(prepared.report.iter().cloned());
        out.report.extend(cover.report.iter().cloned());
        let beside = if target == SubsTarget::NewFile && remux {
            &req.output
        } else {
            video
        };
        // A job that only makes a poster has failed when there is none.
        let poster_only = !remux && prepared.externals.is_empty();
        match save_poster(&cover, beside) {
            Some(Ok(line)) => out.report.push(line),
            Some(Err(e)) if poster_only => return fail(e.trim_start_matches("⚠ "), Vec::new()),
            Some(Err(e)) => out.report.push(e),
            None if poster_only => {
                let why = cover.report.join("\n");
                return fail(
                    if why.is_empty() {
                        "Couldn't make the poster".to_string()
                    } else {
                        why.trim_start_matches("⚠ ").to_string()
                    },
                    Vec::new(),
                );
            }
            None => {}
        }
    }
    out
}

fn save_sidecars(video: &Path, prepared: &PreparedSubs) -> Outcome {
    let targets = match subs::sidecar_outputs(video, &prepared.externals) {
        Ok(t) => t,
        Err(e) => return fail(e, Vec::new()),
    };
    let part_of = |dest: &Path| {
        let mut p = dest.as_os_str().to_owned();
        p.push(".part");
        PathBuf::from(p)
    };
    let cleanup = || {
        for dest in &targets {
            let _ = std::fs::remove_file(part_of(dest));
        }
    };
    // All or nothing: back up and write every file as `.part` first, and only
    // rename once all of them are written. A failure halfway would otherwise
    // leave some files already shifted, and a retry would shift them again.
    for (e, dest) in prepared.externals.iter().zip(&targets) {
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        // Any file that gets replaced (the source synced in place, or another
        // sidecar already there) keeps its very first version as `.orig`.
        let backup = subs::backup_path(dest);
        if dest.exists()
            && !backup.exists()
            && let Err(err) = std::fs::copy(dest, &backup)
        {
            cleanup();
            return fail(format!("Couldn't back up {name}: {err}"), Vec::new());
        }
        let written = std::fs::read(&e.path)
            .map_err(|err| format!("Couldn't read a temp file: {err}"))
            .and_then(|d| {
                std::fs::write(part_of(dest), d)
                    .map_err(|err| format!("Couldn't save {name}: {err}"))
            });
        if let Err(err) = written {
            cleanup();
            return fail(err, Vec::new());
        }
    }
    let mut report = Vec::new();
    for dest in &targets {
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        if let Err(err) = std::fs::rename(part_of(dest), dest) {
            cleanup();
            return fail(format!("Couldn't save {name}: {err}"), Vec::new());
        }
        let backup = subs::backup_path(dest);
        if backup.exists() {
            report.push(format!(
                "Saved {name} (the earlier version is kept as {})",
                backup.file_name().unwrap_or_default().to_string_lossy()
            ));
        } else {
            report.push(format!("Saved {name}"));
        }
    }
    Outcome {
        success: true,
        report,
        ..Default::default()
    }
}

/// Remux into `<dest>.part.<ext>`, check the result, then move it to `dest`
/// (which is the video itself when replacing it).
fn remux_fix(
    tools: &Tools,
    req: &Request,
    prepared: &PreparedSubs,
    cover: &cover::Plan,
    dest: &Path,
    cancel: &Cancel,
    update: &mut impl FnMut(Update),
) -> Outcome {
    let video = &req.probe.path;
    let delays = &subs::audio_delays(&req.selection);
    let in_place = dest == video.as_path();
    let target = if in_place {
        SubsTarget::Embed
    } else {
        SubsTarget::NewFile
    };
    let container = match subs::fix_container(video, target) {
        Ok(c) => c,
        Err(e) => return fail(e, Vec::new()),
    };
    let part = partial_path(dest);
    let args = match command::build_fix(
        &req.probe,
        &prepared.externals,
        delays,
        cover,
        container,
        &part,
    ) {
        Ok(a) => a,
        Err(e) => return fail(e, Vec::new()),
    };
    update(Update::Stage(
        if !prepared.externals.is_empty() {
            "Adding subtitles"
        } else if !delays.is_empty() {
            "Shifting audio"
        } else {
            "Updating cover art"
        }
        .into(),
    ));
    let r = runner::run_ffmpeg(&tools.ffmpeg, &args, cancel, |p| {
        update(Update::Progress(p))
    });
    if r.cancelled {
        let _ = std::fs::remove_file(&part);
        return cancelled();
    }
    if !r.success {
        let _ = std::fs::remove_file(&part);
        return fail(
            r.error.unwrap_or_else(|| {
                format!(
                    "ffmpeg failed: {}",
                    runner::error_summary(&r.stderr_tail.join("\n"))
                )
            }),
            r.stderr_tail,
        );
    }
    update(Update::Stage("Checking the new file".into()));
    let verdict = probe::run(&tools.ffprobe, &part)
        .and_then(|out| embed_verdict(&req.probe, &out, prepared.externals.len(), delays));
    if let Err(e) = verdict {
        let _ = std::fs::remove_file(&part);
        let left = if in_place {
            "the original was left alone"
        } else {
            "nothing was written"
        };
        return fail(
            format!("The new file didn't check out, so {left}: {e}"),
            Vec::new(),
        );
    }
    if let Ok(m) = std::fs::metadata(video) {
        let _ = std::fs::set_permissions(&part, m.permissions());
    }
    if let Err(e) = std::fs::rename(&part, dest) {
        let _ = std::fs::remove_file(&part);
        let what = if in_place {
            "replace the original"
        } else {
            "save the new file"
        };
        return fail(format!("Couldn't {what}: {e}"), Vec::new());
    }
    let mut done = Vec::new();
    let n = prepared.externals.len();
    if n > 0 {
        done.push(format!(
            "added {n} subtitle track{}",
            if n == 1 { "" } else { "s" }
        ));
    }
    for (i, ms) in delays {
        if let Some(a) = req.probe.audio_by_index(*i) {
            done.push(format!(
                "shifted {} audio {:+} ms",
                crate::tracks::language_name(a.language.as_deref()),
                ms
            ));
        }
    }
    match &req.cover.action {
        cover::CoverAction::Replace(_) if req.probe.covers.is_empty() => {
            done.push("added the cover art".into())
        }
        cover::CoverAction::Replace(_) => done.push("replaced the cover art".into()),
        cover::CoverAction::Remove if !req.probe.covers.is_empty() => {
            done.push("removed the cover art".into())
        }
        _ => {}
    }
    let mut line = done.join(", ");
    if let Some(c) = line.get(..1) {
        line = c.to_uppercase() + &line[1..];
    }
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    Outcome {
        success: true,
        report: vec![format!("{line} · {name} (nothing re-encoded)")],
        ..Default::default()
    }
}

/// Does the remuxed file have everything the original had, plus `added` subtitles?
/// Audio `delays` (stream index, ms) may make it longer (positive) or, for a
/// file whose length is set by the audio, shorter (negative), but only by
/// that much and only in that direction.
fn embed_verdict(
    src: &ProbeInfo,
    out: &ProbeInfo,
    added: usize,
    delays: &[(usize, i64)],
) -> Result<(), String> {
    if src.video.is_some() && out.video.is_none() {
        return Err("the video stream is missing".into());
    }
    if out.audio.len() != src.audio.len() {
        return Err(format!(
            "{} audio tracks instead of {}",
            out.audio.len(),
            src.audio.len()
        ));
    }
    if out.subtitles.len() != src.subtitles.len() + added {
        return Err(format!(
            "{} subtitle tracks instead of {}",
            out.subtitles.len(),
            src.subtitles.len() + added
        ));
    }
    let tolerance = (src.duration * 0.005).max(1.0);
    let later = delays.iter().map(|(_, ms)| *ms).max().unwrap_or(0).max(0) as f64 / 1000.0;
    let earlier = -(delays.iter().map(|(_, ms)| *ms).min().unwrap_or(0).min(0)) as f64 / 1000.0;
    let diff = out.duration - src.duration;
    if src.duration > 0.0 && (diff > tolerance + later || -diff > tolerance + earlier) {
        return Err(format!(
            "it is {:.1} s long instead of {:.1} s",
            out.duration, src.duration
        ));
    }
    Ok(())
}

fn size_report(req: &Request) -> String {
    let actual = std::fs::metadata(&req.output).map(|m| m.len()).unwrap_or(0);
    let mut s = format!("Output {}", estimate::format_bytes(actual));
    if req.settings.bitrate_from_target() && req.settings.codec != VideoCodec::Copy {
        let target = req.settings.target_gb * estimate::GB;
        s.push_str(&format!(
            " · target {} ({:+.1}%)",
            estimate::format_bytes(target as u64),
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
    let fix = if req.settings.container != Container::Mkv {
        "Only MKV outputs can be repaired afterwards; use MKV to keep it."
    } else if Path::new("/.flatpak-info").exists() {
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
                (
                    &tools.ffmpeg,
                    &hdr::extract_video_args(
                        &req.probe.path,
                        req.probe.video.as_ref().map_or(0, |v| v.index),
                    ),
                ),
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
        // An unreadable count is a failure too: injecting unverified would put
        // per-frame metadata on the wrong frames.
        match (encoded, rpu_frames) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => {
                return Err(fail(
                    format!(
                        "Frame counts differ: encoded {a}, RPU {b}. Dolby Vision can't be injected safely."
                    ),
                    Vec::new(),
                ));
            }
            (a, b) => {
                let show = |n: Option<u64>| n.map_or("unreadable".to_string(), |n| n.to_string());
                return Err(fail(
                    format!(
                        "Couldn't verify the frame counts (encoded {}, RPU {}), so Dolby Vision can't be injected safely. Turn off Dolby Vision preservation to get HDR10.",
                        show(a),
                        show(b)
                    ),
                    info.stdout.lines().take(20).map(String::from).collect(),
                ));
            }
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
        // Verify the DV configuration made it into the container. A failed
        // check deletes the output like every other failure.
        let expect = if dovi.profile == 5 { 5 } else { 8 };
        let verdict = match probe::run(&tools.ffprobe, &req.output)
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
        };
        if verdict.is_err() {
            let _ = std::fs::remove_file(&req.output);
        }
        verdict
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
            &crop::cropdetect_args(&p.path, v.index, start, crop::SAMPLE_SECONDS),
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
        &crop::preview_args(&p.path, p.video.as_ref().map_or(0, |v| v.index), at, c, out),
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
                ref_stream: v.index,
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
/// Runs as a background helper after the job is done, never inside the encode.
pub fn vmaf_output(tools: &Tools, req: &Request, cancel: &Cancel) -> Result<VmafResult, String> {
    let v = req.probe.video.as_ref().ok_or("No video")?;
    let mut scores = Vec::new();
    for (start, secs) in estimate::sample_segments(req.probe.duration) {
        let input = vmaf::VmafInput {
            distorted: &req.output,
            distorted_seek: true,
            reference: &req.probe.path,
            ref_stream: v.index,
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
mod tests {
    use super::*;
    use crate::ffmpeg::probe::tests::sample_4k;

    #[test]
    fn embed_verdict_needs_every_stream_plus_the_new_subtitles() {
        let src = sample_4k();
        let mut out = src.clone();
        assert!(embed_verdict(&src, &out, 1, &[]).is_err());
        out.subtitles.push(Default::default());
        assert_eq!(embed_verdict(&src, &out, 1, &[]), Ok(()));
        out.audio.pop();
        assert!(
            embed_verdict(&src, &out, 1, &[])
                .unwrap_err()
                .contains("audio")
        );
        let mut short = src.clone();
        short.subtitles.push(Default::default());
        short.duration = src.duration * 0.5;
        assert!(
            embed_verdict(&src, &short, 1, &[])
                .unwrap_err()
                .contains("long")
        );
        short.duration = src.duration - 0.5;
        assert_eq!(embed_verdict(&src, &short, 1, &[]), Ok(()));
        // A positive audio delay may make the file that much longer.
        let mut long = short.clone();
        long.duration = src.duration * 1.005 + 1.0 + 20.0;
        assert!(embed_verdict(&src, &long, 1, &[]).is_err());
        assert_eq!(embed_verdict(&src, &long, 1, &[(1, 21_500)]), Ok(()));
        // ...but it doesn't excuse a file that came out short.
        let mut cut = short.clone();
        cut.duration = src.duration * 0.995 - 1.0 - 20.0;
        assert!(embed_verdict(&src, &cut, 1, &[(1, 21_500)]).is_err());
        // A negative delay may make an audio-only file that much shorter.
        assert_eq!(embed_verdict(&src, &cut, 1, &[(1, -21_500)]), Ok(()));
        assert!(embed_verdict(&src, &long, 1, &[(1, -21_500)]).is_err());
    }

    #[test]
    fn sidecars_are_all_or_nothing_and_back_up_what_they_replace() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("encoder-sidecar-test-{nanos}"));
        let tmp = dir.join("prep");
        std::fs::create_dir_all(&tmp).unwrap();
        let video = dir.join("Film.mkv");
        let ext = |file: &str, lang: &str| {
            let mut e = ExternalSub::from_path(&video, tmp.join(file), false);
            e.language = Some(lang.into());
            e
        };
        std::fs::write(tmp.join("0.srt"), "new no").unwrap();
        std::fs::write(dir.join("Film.no.srt"), "old no").unwrap();
        // The second prepared file is missing: nothing may be replaced.
        let prepared = PreparedSubs {
            externals: vec![ext("0.srt", "nor"), ext("1.srt", "eng")],
            report: Vec::new(),
            dir: tmp.clone(),
        };
        assert!(!save_sidecars(&video, &prepared).success);
        assert_eq!(
            std::fs::read_to_string(dir.join("Film.no.srt")).unwrap(),
            "old no"
        );
        assert!(!dir.join("Film.no.srt.part").exists());
        assert!(!dir.join("Film.en.srt").exists());
        drop(prepared);
        // Both present: saved, and the replaced sidecar is kept as .orig.
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("0.srt"), "new no").unwrap();
        std::fs::write(tmp.join("1.srt"), "new en").unwrap();
        let prepared = PreparedSubs {
            externals: vec![ext("0.srt", "nor"), ext("1.srt", "eng")],
            report: Vec::new(),
            dir: tmp.clone(),
        };
        assert!(save_sidecars(&video, &prepared).success);
        assert_eq!(
            std::fs::read_to_string(dir.join("Film.no.srt")).unwrap(),
            "new no"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("Film.en.srt")).unwrap(),
            "new en"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("Film.no.srt.orig")).unwrap(),
            "old no"
        );
        assert!(!dir.join("Film.en.srt.orig").exists());
        drop(prepared);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod media_tests {
    use super::*;
    use crate::ffmpeg::command::DecodeMode;
    use crate::tracks::{AudioAction, auto_select};

    fn media(name: &str) -> PathBuf {
        // Read at run time: the target folder is shared between the VM and the
        // host under different paths, so a compiled-in path can point nowhere.
        std::env::var_os("CARGO_MANIFEST_DIR")
            .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from)
            .join("dev/media")
            .join(name)
    }

    fn tools() -> Tools {
        Tools::default()
    }

    #[test]
    #[ignore]
    fn fix_tracks_keeps_cover_art_and_fonts_in_mkv() {
        let t = tools();
        let dir = std::env::temp_dir().join("encoder-test-cover");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ff = |args: &[&str]| {
            let ok = std::process::Command::new(&t.ffmpeg)
                .args(["-v", "error", "-y"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{args:?}");
        };
        let d = |n: &str| dir.join(n).to_string_lossy().into_owned();
        ff(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x64",
            "-frames:v",
            "1",
            &d("cover.jpg"),
        ]);
        std::fs::write(dir.join("font.ttf"), b"not really a font").unwrap();
        std::fs::write(
            dir.join("src.en.srt"),
            "1\n00:00:00,500 --> 00:00:01,500\nHello\n",
        )
        .unwrap();
        ff(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=2:s=64x64:r=24",
            "-f",
            "lavfi",
            "-i",
            "sine=d=2",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-attach",
            &d("font.ttf"),
            "-metadata:s:t:0",
            "mimetype=font/ttf",
            "-attach",
            &d("cover.jpg"),
            "-metadata:s:t:1",
            "mimetype=image/jpeg",
            "-metadata:s:t:1",
            "filename=cover.jpg",
            &d("src.mkv"),
        ]);
        let p = probe::run(&t.ffprobe, &dir.join("src.mkv")).unwrap();
        assert_eq!((p.attachments, p.covers.len()), (1, 1));
        let (mut sel, _, _) = auto_select(&p, &[], &EncodeSettings::default());
        for (n, delay) in [0, -100].into_iter().enumerate() {
            for c in &mut sel.audio {
                c.offset_ms = delay;
            }
            let externals = if delay == 0 {
                vec![ExternalSub::from_path(
                    &p.path,
                    dir.join("src.en.srt"),
                    false,
                )]
            } else {
                Vec::new()
            };
            let out = dir.join(format!("out{n}.mkv"));
            let req = Request {
                probe: p.clone(),
                settings: EncodeSettings::default(),
                selection: sel.clone(),
                output: out.clone(),
                device: "/dev/dri/renderD128".into(),
                crop: None,
                crop_on_gpu: false,
                scan: Scan::Progressive,
                video_kbps: None,
                dv_preserve: false,
                mkvpropedit: false,
                externals,
                mode: JobMode::Fix(SubsTarget::NewFile),
                alass: None,
                cover: Default::default(),
                covers: Vec::new(),
            };
            let o = encode(&t, &req, &Cancel::default(), |_| {});
            assert!(o.success, "{:?} {:?}", o.error, o.stderr_tail);
            let r = probe::run(&t.ffprobe, &out).unwrap();
            assert_eq!(r.covers.len(), 1, "delay {delay}: {:?}", r.streams);
            assert_eq!(r.covers[0].filename.as_deref(), Some("cover.jpg"));
            assert_eq!(r.covers[0].mimetype.as_deref(), Some("image/jpeg"));
            assert_eq!(r.attachments, 1, "font kept");
            assert_eq!(r.video.as_ref().unwrap().codec, "h264");
            // Only one real video track: the cover isn't a second one.
            let videos = r.streams.iter().filter(|(_, k)| k == "video").count();
            assert_eq!(videos, 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Replace, keep and remove cover art in fix-tracks remuxes and in
    /// stream-copy encodes (no VA-API needed), and save posters.
    #[test]
    #[ignore]
    fn cover_art_choices_and_posters() {
        use crate::cover::{CoverAction, CoverChoice};
        use crate::settings::Container;
        let t = tools();
        let dir = std::env::temp_dir().join("encoder-test-cover-choice");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ff = |args: &[&str]| {
            let ok = std::process::Command::new(&t.ffmpeg)
                .args(["-v", "error", "-y"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "{args:?}");
        };
        let d = |n: &str| dir.join(n).to_string_lossy().into_owned();
        // A wide 1920×1080 picture, and an MKV whose cover is a small JPEG.
        ff(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=1920x1080",
            "-frames:v",
            "1",
            &d("wide.png"),
        ]);
        ff(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=s=64x64",
            "-frames:v",
            "1",
            &d("old.jpg"),
        ]);
        ff(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=2:s=64x64:r=24",
            "-f",
            "lavfi",
            "-i",
            "sine=d=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-attach",
            &d("old.jpg"),
            "-metadata:s:t:0",
            "mimetype=image/jpeg",
            "-metadata:s:t:0",
            "filename=cover.jpg",
            &d("src.mkv"),
        ]);
        let p = probe::run(&t.ffprobe, &dir.join("src.mkv")).unwrap();
        assert_eq!(p.covers.len(), 1);
        let (sel, _, _) = auto_select(&p, &[], &EncodeSettings::default());
        let size = |f: &Path| ffprobe_line(f, "stream=width,height");
        let replace = CoverChoice {
            action: CoverAction::Replace(dir.join("wide.png")),
            poster_crop: true,
            save_poster: true,
        };
        let run = |mode, settings: EncodeSettings, cover: CoverChoice, out: &str| {
            let req = Request {
                probe: p.clone(),
                settings,
                selection: sel.clone(),
                output: dir.join(out),
                device: "/dev/dri/renderD128".into(),
                crop: None,
                crop_on_gpu: false,
                scan: Scan::Progressive,
                video_kbps: None,
                dv_preserve: false,
                mkvpropedit: false,
                externals: Vec::new(),
                mode,
                alass: None,
                cover,
                covers: Vec::new(),
            };
            let o = encode(&t, &req, &Cancel::default(), |_| {});
            assert!(o.success, "{out}: {:?} {:?}", o.error, o.stderr_tail);
            o
        };
        let copy = |container| EncodeSettings {
            codec: VideoCodec::Copy,
            container,
            ..Default::default()
        };

        // Fix tracks, new file: the new cover alone is the job, cropped to 2:3
        // and scaled to 1000 px high; the poster sits next to the new file.
        let o = run(
            JobMode::Fix(SubsTarget::NewFile),
            EncodeSettings::default(),
            replace.clone(),
            "fix.mkv",
        );
        assert!(
            o.report
                .iter()
                .any(|l| l.starts_with("Replaced the cover art")),
            "{:?}",
            o.report
        );
        let r = probe::run(&t.ffprobe, &dir.join("fix.mkv")).unwrap();
        assert_eq!(r.covers.len(), 1);
        assert_eq!(r.covers[0].mimetype.as_deref(), Some("image/jpeg"));
        let videos = r.streams.iter().filter(|(_, k)| k == "video").count();
        assert_eq!(videos, 1);
        assert_eq!(size(&dir.join("fix.jpg")), "667,1000");

        // Remove: no cover, no poster.
        let rm = CoverChoice {
            action: CoverAction::Remove,
            save_poster: true,
            ..Default::default()
        };
        run(
            JobMode::Fix(SubsTarget::NewFile),
            EncodeSettings::default(),
            rm.clone(),
            "rm.mkv",
        );
        assert!(
            probe::run(&t.ffprobe, &dir.join("rm.mkv"))
                .unwrap()
                .covers
                .is_empty()
        );
        assert!(!dir.join("rm.jpg").exists());

        // Sidecar with only a poster: nothing remuxed, the poster from the
        // source's cover (uncropped, 64×64) next to the video.
        let keep_poster = CoverChoice {
            save_poster: true,
            ..Default::default()
        };
        let o = run(
            JobMode::Fix(SubsTarget::Sidecar),
            EncodeSettings::default(),
            keep_poster.clone(),
            "unused.mkv",
        );
        assert!(
            o.report.iter().any(|l| l.contains("Saved poster src.jpg")),
            "{:?}",
            o.report
        );
        assert_eq!(size(&dir.join("src.jpg")), "64,64");
        // Again: the same picture, so nothing is written or backed up.
        let again = |n: &str| {
            run(
                JobMode::Fix(SubsTarget::Sidecar),
                EncodeSettings::default(),
                keep_poster.clone(),
                n,
            )
        };
        let o = again("unused.mkv");
        assert!(o.report.iter().any(|l| l.contains("already this picture")));
        assert!(!dir.join("src.jpg.orig").exists());
        // The user's own poster is backed up before it's replaced, every time
        // a different one is there, without overwriting an older backup.
        std::fs::write(dir.join("src.jpg"), b"poster A").unwrap();
        again("unused.mkv");
        assert_eq!(
            std::fs::read(dir.join("src.jpg.orig")).unwrap(),
            b"poster A"
        );
        std::fs::write(dir.join("src.jpg"), b"poster B").unwrap();
        let o = again("unused.mkv");
        assert_eq!(
            std::fs::read(dir.join("src.jpg.orig")).unwrap(),
            b"poster A"
        );
        assert_eq!(
            std::fs::read(dir.join("src.jpg.orig2")).unwrap(),
            b"poster B"
        );
        assert!(o.report.iter().any(|l| l.contains("kept as src.jpg.orig2")));
        // Poster A back again: its backup already exists, no third copy.
        std::fs::write(dir.join("src.jpg"), b"poster A").unwrap();
        again("unused.mkv");
        assert!(!dir.join("src.jpg.orig3").exists());
        assert_eq!(size(&dir.join("src.jpg")), "64,64");

        // Encode (stream copy) to MKV keeps the source's cover; it used to be lost.
        run(
            JobMode::Encode,
            copy(Container::Mkv),
            CoverChoice::default(),
            "keep.mkv",
        );
        let r = probe::run(&t.ffprobe, &dir.join("keep.mkv")).unwrap();
        assert_eq!(r.covers.len(), 1);
        assert_eq!(r.covers[0].filename.as_deref(), Some("cover.jpg"));

        // Encode to MP4 with a new cover: an attached_pic stream, not a track.
        run(JobMode::Encode, copy(Container::Mp4), replace, "new.mp4");
        let r = probe::run(&t.ffprobe, &dir.join("new.mp4")).unwrap();
        assert_eq!(r.covers.len(), 1, "{:?}", r.streams);
        assert_eq!(r.covers[0].codec, "mjpeg");
        assert_eq!(r.video.as_ref().unwrap().codec, "h264");
        assert_eq!(size(&dir.join("new.jpg")), "667,1000");

        // Remuxing that MP4 again keeps its cover as a flagged stream.
        let p4 = probe::run(&t.ffprobe, &dir.join("new.mp4")).unwrap();
        let req = Request {
            probe: p4,
            settings: EncodeSettings::default(),
            selection: sel.clone(),
            output: dir.join("again.mp4"),
            device: String::new(),
            crop: None,
            crop_on_gpu: false,
            scan: Scan::Progressive,
            video_kbps: None,
            dv_preserve: false,
            mkvpropedit: false,
            externals: Vec::new(),
            mode: JobMode::Fix(SubsTarget::NewFile),
            alass: None,
            cover: rm,
            covers: Vec::new(),
        };
        let o = encode(&t, &req, &Cancel::default(), |_| {});
        assert!(o.success, "{:?} {:?}", o.error, o.stderr_tail);
        assert!(
            probe::run(&t.ffprobe, &dir.join("again.mp4"))
                .unwrap()
                .covers
                .is_empty()
        );

        // A picture that can't be read fails the job before any work.
        std::fs::write(dir.join("bad.png"), b"not a picture").unwrap();
        let bad = CoverChoice {
            action: CoverAction::Replace(dir.join("bad.png")),
            ..Default::default()
        };
        let req = Request {
            probe: p.clone(),
            settings: copy(Container::Mkv),
            selection: sel.clone(),
            output: dir.join("bad.mkv"),
            device: String::new(),
            crop: None,
            crop_on_gpu: false,
            scan: Scan::Progressive,
            video_kbps: None,
            dv_preserve: false,
            mkvpropedit: false,
            externals: Vec::new(),
            mode: JobMode::Encode,
            alass: None,
            cover: bad,
            covers: Vec::new(),
        };
        let o = encode(&t, &req, &Cancel::default(), |_| {});
        assert!(!o.success);
        assert!(
            o.error
                .unwrap()
                .contains("Couldn't read the cover image bad.png")
        );
        assert!(!dir.join("bad.mkv").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn fix_tracks_shifts_audio_into_a_new_file_without_reencoding() {
        let t = tools();
        let p = probe::run(&t.ffprobe, &media("sample.mkv")).unwrap();
        let settings = EncodeSettings::default();
        let (mut sel, _, _) = auto_select(&p, &["eng".into()], &settings);
        // A deselected track is shifted too: fix-tracks keeps every track.
        for c in &mut sel.audio {
            c.selected = false;
            c.offset_ms = match c.index {
                1 => 300,
                4 => -100,
                _ => 0,
            };
        }
        let out = std::env::temp_dir().join("encoder-test-fix.mkv");
        let _ = std::fs::remove_file(&out);
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
            externals: Vec::new(),
            mode: JobMode::Fix(SubsTarget::NewFile),
            alass: None,
            cover: Default::default(),
            covers: Vec::new(),
        };
        let o = encode(&t, &req, &Cancel::default(), |_| {});
        assert!(o.success, "{:?} {:?}", o.error, o.stderr_tail);
        assert!(o.report[0].contains("nothing re-encoded"), "{:?}", o.report);
        let r = probe::run(&t.ffprobe, &out).unwrap();
        // Same streams, same codecs, same order: everything was copied.
        let codecs =
            |p: &ProbeInfo| -> Vec<String> { p.audio.iter().map(|a| a.codec.clone()).collect() };
        assert_eq!(codecs(&r), codecs(&p));
        assert_eq!(r.subtitles.len(), p.subtitles.len());
        assert_eq!(
            r.video.as_ref().map(|v| v.codec.clone()),
            p.video.as_ref().map(|v| v.codec.clone())
        );
        // First audio packet times, per output stream.
        let starts = std::process::Command::new(&t.ffprobe)
            .args(["-v", "error", "-select_streams", "a", "-show_entries"])
            .args(["stream=start_time", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let starts: Vec<f64> = String::from_utf8_lossy(&starts.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        let pos = p.audio.iter().position(|a| a.index == 1).unwrap();
        let neg = p.audio.iter().position(|a| a.index == 4).unwrap();
        assert!((starts[pos] - 0.3).abs() < 0.05, "{starts:?}");
        assert!((0.0..0.05).contains(&starts[neg]), "{starts:?}");
        let _ = std::fs::remove_file(&out);
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
            externals: Vec::new(),
            mode: JobMode::Encode,
            alass: None,
            cover: Default::default(),
            covers: Vec::new(),
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
            ref_stream: 0,
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
            externals: Vec::new(),
            mode: JobMode::Encode,
            alass: None,
            cover: Default::default(),
            covers: Vec::new(),
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
