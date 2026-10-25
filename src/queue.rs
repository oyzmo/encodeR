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
    /// From idet analysis; `None` until analysed or when inconclusive.
    pub scan_detected: Option<ScanType>,
    pub scan_override: ScanOverride,
    pub estimate: EstimateState,
    pub vmaf: VmafState,
    pub progress: Option<Progress>,
    pub stage: Option<String>,
    pub decode_used: Option<DecodeMode>,
    pub error: Option<String>,
    pub stderr_tail: Vec<String>,
    /// Lines shown after a finished job (size vs target, HDR check, VMAF…).
    pub report: Vec<String>,
    pub output: Option<PathBuf>,
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
            scan_detected: None,
            scan_override: ScanOverride::Auto,
            estimate: EstimateState::None,
            vmaf: VmafState::None,
            progress: None,
            stage: None,
            decode_used: None,
            error: None,
            stderr_tail: Vec::new(),
            report: Vec::new(),
            output: None,
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

    /// Why Dolby Vision can't be kept with the current settings, if it can't.
    pub fn dv_blocker(&self, dovi_tool: bool) -> Option<&'static str> {
        let p = self.probe.as_ref()?;
        let v = p.video.as_ref()?;
        crate::hdr::dv_blocker(
            p.hdr.dovi,
            self.settings.codec == crate::settings::VideoCodec::Hevc,
            self.settings
                .downscale
                .height()
                .is_some_and(|h| h < v.height),
            dovi_tool,
        )
    }

    /// The crop that will actually be applied.
    pub fn active_crop(&self) -> Option<Crop> {
        match (&self.crop, self.crop_enabled && !self.dv_preserve) {
            (CropState::Found(c), true) => Some(*c),
            _ => None,
        }
    }
}

/// Output path: `<dir>/<stem><suffix>.<ext>`, never the input itself.
pub fn output_path(input: &Path, prefs: &Prefs, container: Container) -> PathBuf {
    let dir = prefs
        .output_dir
        .clone()
        .or_else(|| input.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    let mut out = dir.join(format!("{stem}{}.{}", prefs.suffix, container.extension()));
    if out == input {
        out = dir.join(format!("{stem}-reencoded.{}", container.extension()));
    }
    out
}

/// The next job to encode: the first pending one, in queue order.
pub fn next_pending(jobs: &[Job]) -> Option<u64> {
    // Wait for open subtitle questions and the interlace/crop analysis.
    jobs.iter()
        .find(|j| {
            j.state == JobState::Pending
                && j.pgs_pending.is_empty()
                && j.crop != CropState::Detecting
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
