//! Job model and state machine (Probing → Pending → Running → Done / Failed / Cancelled).

use std::path::{Path, PathBuf};

use crate::crop::Crop;
use crate::ffmpeg::command::DecodeMode;
use crate::ffmpeg::estimate::SizeRange;
use crate::ffmpeg::probe::ProbeInfo;
use crate::ffmpeg::progress::Progress;
use crate::ffmpeg::vmaf::VmafResult;
use crate::scan::{self, Scan, ScanOverride, ScanType};
use crate::settings::{Container, EncodeSettings, Prefs};
use crate::subs::{ExternalSub, JobMode};
use crate::tracks::{LangGroup, PgsQuestion, Selection};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Probing,
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    ProbeOk,
    ProbeFailed,
    Start,
    Succeeded,
    Failed,
    Cancel,
    Retry,
}

impl JobState {
    /// The next state, or `None` if the event doesn't apply.
    pub fn next(self, e: Event) -> Option<JobState> {
        use Event as E;
        use JobState as S;
        match (self, e) {
            (S::Probing, E::ProbeOk) => Some(S::Pending),
            (S::Probing, E::ProbeFailed) => Some(S::Failed),
            (S::Pending, E::Start) => Some(S::Running),
            (S::Running, E::Succeeded) => Some(S::Done),
            (S::Running, E::Failed) => Some(S::Failed),
            (S::Running | S::Pending, E::Cancel) => Some(S::Cancelled),
            (S::Failed | S::Cancelled | S::Done, E::Retry) => Some(S::Pending),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Probing => "Reading file…",
            Self::Pending => "Waiting",
            Self::Running => "Encoding",
            Self::Done => "Done",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
        }
    }

    pub fn editable(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Failed | Self::Cancelled | Self::Done
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CropState {
    Unknown,
    Detecting,
    None,
    Found(Crop),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum EstimateState {
    None,
    Running,
    Ready(SizeRange),
    Failed(String),
}

/// "Match source quality" VMAF search.
#[derive(Clone, Debug, PartialEq)]
pub enum MatchState {
    None,
    /// What it's trying right now.
    Running(String),
    /// With the match key it was measured for (see `State::match_key`).
    Ready(String, crate::match_source::Found),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum VmafState {
    None,
    Running,
    Ready(VmafResult),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: u64,
    pub batch: u64,
    pub input: PathBuf,
    pub state: JobState,
    pub probe: Option<ProbeInfo>,
    pub settings: EncodeSettings,
    pub preset: String,
    pub selection: Selection,
    pub groups: Vec<LangGroup>,
    pub pgs_pending: Vec<PgsQuestion>,
    pub crop: CropState,
    pub crop_enabled: bool,
    pub dv_preserve: bool,
    pub hdr10plus_preserve: bool,
    /// From idet analysis; `None` until analysed or when inconclusive.
    pub scan_detected: Option<ScanType>,
    pub scan_override: ScanOverride,
    /// The damage check is reading the file (the verdict lands in `probe.damage`).
    pub damage_checking: bool,
    pub estimate: EstimateState,
    pub vmaf: VmafState,
    pub match_check: MatchState,
    pub progress: Option<Progress>,
    pub stage: Option<String>,
    pub decode_used: Option<DecodeMode>,
    pub error: Option<String>,
    pub stderr_tail: Vec<String>,
    /// Lines shown after a finished job (size vs target, HDR check, VMAF…).
    pub report: Vec<String>,
    pub output: Option<PathBuf>,
    /// Existing output the user agreed to replace (asked when Start is pressed).
    pub replace_ok: Option<PathBuf>,
    pub mode: JobMode,
    /// SRT files added to the job, muxed in or saved next to the video.
    pub externals: Vec<ExternalSub>,
    /// Keep, replace or remove the cover art; save a poster.
    pub cover: crate::cover::CoverChoice,
    /// Files a subtitles-only job may replace, as the user approved them.
    /// Anything the job would replace that isn't listed is asked about again.
    pub subs_ok: Vec<PathBuf>,
    /// A file the user picked with Save As, instead of `<folder>/<name><suffix>`.
    pub output_override: Option<PathBuf>,
}

impl Job {
    pub fn new(
        id: u64,
        batch: u64,
        input: PathBuf,
        settings: EncodeSettings,
        preset: String,
    ) -> Self {
        Self {
            id,
            batch,
            input,
            state: JobState::Probing,
            probe: None,
            settings,
            preset,
            selection: Selection::default(),
            groups: Vec::new(),
            pgs_pending: Vec::new(),
            crop: CropState::Unknown,
            crop_enabled: false,
            dv_preserve: false,
            hdr10plus_preserve: false,
            scan_detected: None,
            scan_override: ScanOverride::Auto,
            damage_checking: false,
            estimate: EstimateState::None,
            vmaf: VmafState::None,
            match_check: MatchState::None,
            progress: None,
            stage: None,
            decode_used: None,
            error: None,
            stderr_tail: Vec::new(),
            report: Vec::new(),
            output: None,
            replace_ok: None,
            mode: JobMode::Encode,
            externals: Vec::new(),
            cover: Default::default(),
            subs_ok: Vec::new(),
            output_override: None,
        }
    }

    pub fn apply(&mut self, e: Event) -> bool {
        match self.state.next(e) {
            Some(s) => {
                self.state = s;
                true
            }
            None => false,
        }
    }

    /// What a fix-tracks run of this job does besides subtitle files.
    pub fn fix_work(&self) -> crate::subs::FixWork {
        let p = self.probe.as_ref();
        crate::subs::FixWork {
            delays: !crate::subs::audio_delays(&self.selection).is_empty(),
            cover: p.is_some_and(|p| self.cover.changes_embedded(p)),
            new_cover: matches!(self.cover.action, crate::cover::CoverAction::Replace(_)),
            poster: p.is_some_and(|p| self.cover.wants_poster(p)),
        }
    }

    /// Does this job write a video file? A fix-tracks job that only saves
    /// sidecars or a poster leaves the video alone.
    pub fn remuxes(&self) -> bool {
        use crate::subs::{JobMode, SubsTarget};
        match self.mode {
            JobMode::Encode => true,
            JobMode::Fix(SubsTarget::Sidecar) => false,
            JobMode::Fix(_) => self.externals.iter().any(|e| e.selected) || self.fix_work().remux(),
        }
    }

    /// Does this job write a new file in the output folder?
    pub fn writes_output(&self) -> bool {
        !self.mode.in_place() && self.remuxes()
    }

    pub fn file_name(&self) -> String {
        self.input
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Deinterlace / inverse telecine plan for the encode.
    pub fn scan_plan(&self) -> Scan {
        let field_order = self
            .probe
            .as_ref()
            .and_then(|p| p.video.as_ref())
            .and_then(|v| v.field_order.as_deref());
        scan::plan(
            self.scan_detected,
            field_order,
            self.scan_override,
            self.settings.deinterlace_double_rate,
        )
    }

    /// What the metadata-injection pipelines need to know about this job.
    fn inject_job(&self) -> Option<crate::hdr::InjectJob> {
        let p = self.probe.as_ref()?;
        Some(crate::hdr::InjectJob::new(
            &self.settings,
            p,
            self.scan_plan(),
        ))
    }

    /// Why Dolby Vision can't be kept with the current settings, if it can't.
    pub fn dv_blocker(&self, dovi_tool: bool) -> Option<&'static str> {
        crate::hdr::dv_blocker(self.probe.as_ref()?.hdr.dovi, self.inject_job()?, dovi_tool)
    }

    /// Why HDR10+ can't be kept with the current settings, if it can't.
    pub fn hdr10plus_blocker(&self, tool: bool) -> Option<&'static str> {
        crate::hdr::hdr10plus_blocker(self.probe.as_ref()?.hdr.hdr10plus, self.inject_job()?, tool)
    }

    /// Turns off Dolby Vision / HDR10+ preservation that the current settings
    /// rule out (AV1, downscaling, a scan plan that changes the frame count).
    /// A flag left on would send the job down a pipeline that can only fail,
    /// and would keep crop off.
    pub fn drop_blocked_metadata(&mut self, dovi_tool: bool, hdr10plus_tool: bool) {
        if self.dv_preserve && self.dv_blocker(dovi_tool).is_some() {
            self.dv_preserve = false;
        }
        if self.hdr10plus_preserve && self.hdr10plus_blocker(hdr10plus_tool).is_some() {
            self.hdr10plus_preserve = false;
        }
    }

    /// Per-frame metadata will be injected, so the frame must stay uncropped.
    pub fn injects_metadata(&self) -> bool {
        self.dv_preserve || self.hdr10plus_preserve
    }

    /// After external subtitles change: an SRT replaces the embedded PGS of
    /// its language and kind, and answers that language's PGS question.
    pub fn externals_changed(&mut self) {
        if let Some(p) = &self.probe {
            crate::subs::prefer_externals(&mut self.selection, p, &self.externals);
        }
        let ext = self.externals.clone();
        self.pgs_pending
            .retain(|q| !crate::subs::covers_language(&ext, &q.family));
    }

    /// Bytes of the selected external subtitle files.
    pub fn external_bytes(&self) -> u64 {
        self.externals
            .iter()
            .filter(|e| e.selected)
            .map(|e| e.bytes)
            .sum()
    }

    /// Container of the file this job writes to the output folder.
    pub fn output_container(&self) -> crate::settings::Container {
        match self.mode {
            JobMode::Fix(t) => crate::subs::fix_container(&self.input, t)
                .unwrap_or(crate::settings::Container::Mkv),
            JobMode::Encode => self.settings.container,
        }
    }

    /// Existing files an in-place fix-tracks job would replace right now: the video
    /// for an embed, or sidecars already there that aren't their own source.
    pub fn subs_replacements(&self) -> Vec<PathBuf> {
        use crate::subs::{JobMode, SubsTarget};
        match self.mode {
            JobMode::Encode | JobMode::Fix(SubsTarget::NewFile) => Vec::new(),
            JobMode::Fix(SubsTarget::Embed) if self.remuxes() => vec![self.input.clone()],
            JobMode::Fix(SubsTarget::Embed) => Vec::new(),
            JobMode::Fix(SubsTarget::Sidecar) => {
                let Ok(outs) = crate::subs::sidecar_outputs(&self.input, &self.externals) else {
                    return Vec::new();
                };
                outs.into_iter()
                    .zip(self.externals.iter().filter(|e| e.selected))
                    .filter(|(o, e)| o.exists() && !same_file(o, &e.path))
                    .map(|(o, _)| o)
                    .collect()
            }
        }
    }

    /// Of [`Self::subs_replacements`], the files the user hasn't approved.
    pub fn unapproved_replacements(&self) -> Vec<PathBuf> {
        self.subs_replacements()
            .into_iter()
            .filter(|p| !self.subs_ok.contains(p))
            .collect()
    }

    /// The crop that will actually be applied.
    pub fn active_crop(&self) -> Option<Crop> {
        match (&self.crop, self.crop_enabled && !self.injects_metadata()) {
            (CropState::Found(c), true) => Some(*c),
            _ => None,
        }
    }
}

/// Output path: `<dir>/<stem><suffix>.<ext>`, never the input itself.
pub fn output_path(input: &Path, prefs: &Prefs, container: Container) -> PathBuf {
    output_path_avoiding(input, prefs, container, &[])
}

/// Like `output_path`, but also never one of `taken` (other jobs' inputs and
/// outputs). Clashes fall back to `-reencoded`, then `-reencoded-2`, `-3`…
pub fn output_path_avoiding(
    input: &Path,
    prefs: &Prefs,
    container: Container,
    taken: &[PathBuf],
) -> PathBuf {
    let dir = prefs
        .output_dir
        .clone()
        .or_else(|| input.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let ext = container.extension();
    let clash = |p: &Path| same_file(p, input) || taken.iter().any(|t| same_file(p, t));
    let mut out = dir.join(format!("{stem}{}.{ext}", sanitize_suffix(&prefs.suffix)));
    if clash(&out) {
        out = dir.join(format!("{stem}-reencoded.{ext}"));
    }
    let mut n = 2;
    while clash(&out) {
        out = dir.join(format!("{stem}-reencoded-{n}.{ext}"));
        n += 1;
    }
    out
}

/// A path the user picked: its extension follows the container, and it is
/// never the input or one of `taken`. Clashes become `name-2.ext`, `-3`…
pub fn override_path(
    chosen: &Path,
    input: &Path,
    container: Container,
    taken: &[PathBuf],
) -> PathBuf {
    let ext = container.extension();
    let dir = chosen.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = chosen
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let clash = |p: &Path| same_file(p, input) || taken.iter().any(|t| same_file(p, t));
    let mut out = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while clash(&out) {
        out = dir.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    out
}

/// A file-name suffix can't leave the output folder or name a subfolder.
pub fn sanitize_suffix(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '/' | '\0')).collect()
}

/// Whether two paths name the same file, whatever the spelling: the same inode
/// when both exist (symlinks, case-insensitive shares, relative paths),
/// otherwise the same canonical path (a missing file resolves via its folder).
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if a == b {
        return true;
    }
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        (Err(_), Err(_)) => resolve(a).is_some_and(|x| resolve(b) == Some(x)),
        // One exists and the other doesn't: different files.
        _ => false,
    }
}

fn resolve(p: &Path) -> Option<PathBuf> {
    let parent = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Some(std::fs::canonicalize(parent).ok()?.join(p.file_name()?))
}

/// The next job to encode: the first pending one, in queue order.
pub fn next_pending(jobs: &[Job]) -> Option<u64> {
    // Wait for open subtitle questions, the interlace/crop analysis and the
    // damage check (a damaged file is decoded differently).
    jobs.iter()
        .find(|j| {
            j.state == JobState::Pending
                && j.pgs_pending.is_empty()
                && j.crop != CropState::Detecting
                && !j.damage_checking
        })
        .map(|j| j.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine() {
        use Event::*;
        use JobState::*;
        assert_eq!(Probing.next(ProbeOk), Some(Pending));
        assert_eq!(Probing.next(ProbeFailed), Some(JobState::Failed));
        assert_eq!(Pending.next(Start), Some(Running));
        assert_eq!(Running.next(Succeeded), Some(Done));
        assert_eq!(Running.next(Event::Failed), Some(JobState::Failed));
        assert_eq!(Running.next(Cancel), Some(Cancelled));
        assert_eq!(Cancelled.next(Retry), Some(Pending));
        assert_eq!(Done.next(Start), None);
        assert_eq!(Probing.next(Start), None);
        assert_eq!(Running.next(Retry), None);
    }

    #[test]
    fn output_paths() {
        let p = Prefs::default();
        assert_eq!(
            output_path(Path::new("/m/Film.mkv"), &p, Container::Mkv),
            PathBuf::from("/m/Film-reencoded.mkv")
        );
        let mut p2 = Prefs {
            suffix: String::new(),
            ..Default::default()
        };
        // Same name as the input: never overwrite it.
        assert_eq!(
            output_path(Path::new("/m/Film.mkv"), &p2, Container::Mkv),
            PathBuf::from("/m/Film-reencoded.mkv")
        );
        assert_eq!(
            output_path(Path::new("/m/Film.mkv"), &p2, Container::Mp4),
            PathBuf::from("/m/Film.mp4")
        );
        p2.output_dir = Some("/out".into());
        assert_eq!(
            output_path(Path::new("/m/Film.mkv"), &p2, Container::Mkv),
            PathBuf::from("/out/Film.mkv")
        );
    }

    #[test]
    fn output_never_reaches_the_input_by_another_path() {
        let root = std::env::temp_dir().join(format!("encoder-outpath-{}", std::process::id()));
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let input = real.join("Film.mkv");
        std::fs::write(&input, b"x").unwrap();
        let p = Prefs {
            suffix: String::new(),
            output_dir: Some(link.clone()),
            ..Default::default()
        };
        // link/Film.mkv is the input itself.
        assert!(same_file(&link.join("Film.mkv"), &input));
        let out = output_path(&input, &p, Container::Mkv);
        assert_eq!(out, link.join("Film-reencoded.mkv"));
        // Another queued job's input or output is avoided too.
        let other = real.join("Film-reencoded.mkv");
        let out = output_path_avoiding(&input, &p, Container::Mkv, std::slice::from_ref(&other));
        assert_eq!(out, link.join("Film-reencoded-2.mkv"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn picked_output_follows_container_and_avoids_clashes() {
        let input = Path::new("/m/Film.mkv");
        assert_eq!(
            override_path(Path::new("/o/Mine.mkv"), input, Container::Mp4, &[]),
            PathBuf::from("/o/Mine.mp4")
        );
        assert_eq!(
            override_path(Path::new("/o/Mine"), input, Container::Mkv, &[]),
            PathBuf::from("/o/Mine.mkv")
        );
        // Picking the input itself, or another job's file, gets a number.
        assert_eq!(
            override_path(input, input, Container::Mkv, &[]),
            PathBuf::from("/m/Film-2.mkv")
        );
        let taken = [PathBuf::from("/o/Mine.mkv"), PathBuf::from("/o/Mine-2.mkv")];
        assert_eq!(
            override_path(Path::new("/o/Mine.mkv"), input, Container::Mkv, &taken),
            PathBuf::from("/o/Mine-3.mkv")
        );
    }

    #[test]
    fn suffix_cannot_leave_the_folder() {
        let p = Prefs {
            suffix: "/../../x".into(),
            ..Default::default()
        };
        assert_eq!(
            output_path(Path::new("/m/Film.mkv"), &p, Container::Mkv),
            PathBuf::from("/m/Film....x.mkv")
        );
    }

    #[test]
    fn picks_first_pending_without_open_questions() {
        let s = EncodeSettings::default();
        let mut a = Job::new(1, 1, "/a.mkv".into(), s.clone(), String::new());
        let mut b = Job::new(2, 1, "/b.mkv".into(), s.clone(), String::new());
        let mut c = Job::new(3, 1, "/c.mkv".into(), s, String::new());
        a.state = JobState::Done;
        b.state = JobState::Pending;
        b.pgs_pending.push(PgsQuestion {
            family: "nor".into(),
            name: "Norwegian".into(),
            indices: vec![3],
        });
        c.state = JobState::Pending;
        assert_eq!(next_pending(&[a, b, c]), Some(3));
    }
}
