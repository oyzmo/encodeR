//! HDR10 / HLG / HDR10+ / Dolby Vision detection, and the experimental
//! metadata preservation pipelines: Dolby Vision (`dovi_tool`) and HDR10+
//! (`hdr10plus_tool`). Both extract per-frame metadata from the source,
//! inject it into the raw re-encoded HEVC stream, and mux afterwards.

use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mastering {
    /// Chromaticities as (x, y) in 0..1: red, green, blue, white point.
    pub red: (f64, f64),
    pub green: (f64, f64),
    pub blue: (f64, f64),
    pub white: (f64, f64),
    /// cd/m².
    pub min_luminance: f64,
    pub max_luminance: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dovi {
    pub profile: u32,
    pub level: u32,
    pub compat_id: u32,
    pub el_present: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HdrInfo {
    pub pq: bool,
    pub hlg: bool,
    pub bt2020: bool,
    pub mastering: Option<Mastering>,
    /// (MaxCLL, MaxFALL).
    pub cll: Option<(u32, u32)>,
    pub hdr10plus: bool,
    pub dovi: Option<Dovi>,
}

impl HdrInfo {
    pub fn is_hdr(&self) -> bool {
        self.pq || self.hlg || self.dovi.is_some()
    }

    /// Short badge for the job row, e.g. "DV 8.1 · HDR10".
    pub fn badge(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(d) = self.dovi {
            parts.push(match (d.profile, d.compat_id) {
                (8, c) if c > 0 => format!("DV 8.{c}"),
                (p, _) => format!("DV {p}"),
            });
        }
        if self.hdr10plus {
            parts.push("HDR10+".into());
        } else if self.pq {
            parts.push("HDR10".into());
        }
        if self.hlg {
            parts.push("HLG".into());
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

fn ratio(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::String(s) => match s.split_once('/') {
            Some((n, d)) => {
                let d: f64 = d.parse().ok()?;
                (d != 0.0).then(|| n.parse::<f64>().ok().map(|n| n / d))?
            }
            None => s.parse().ok(),
        },
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn int(v: &Value, key: &str) -> Option<u32> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64().map(|n| n as u32),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn apply_side_data(info: &mut HdrInfo, list: &[Value]) {
    for sd in list {
        match sd
            .get("side_data_type")
            .and_then(Value::as_str)
            .unwrap_or("")
        {
            "DOVI configuration record" => {
                info.dovi = Some(Dovi {
                    profile: int(sd, "dv_profile").unwrap_or(0),
                    level: int(sd, "dv_level").unwrap_or(0),
                    compat_id: int(sd, "dv_bl_signal_compatibility_id").unwrap_or(0),
                    el_present: int(sd, "el_present_flag").unwrap_or(0) != 0,
                })
            }
            "Mastering display metadata" => {
                let xy =
                    |a: &str, b: &str| (ratio(sd, a).unwrap_or(0.0), ratio(sd, b).unwrap_or(0.0));
                info.mastering = Some(Mastering {
                    red: xy("red_x", "red_y"),
                    green: xy("green_x", "green_y"),
                    blue: xy("blue_x", "blue_y"),
                    white: xy("white_point_x", "white_point_y"),
                    min_luminance: ratio(sd, "min_luminance").unwrap_or(0.0),
                    max_luminance: ratio(sd, "max_luminance").unwrap_or(0.0),
                })
            }
            "Content light level metadata" => {
                info.cll = Some((
                    int(sd, "max_content").unwrap_or(0),
                    int(sd, "max_average").unwrap_or(0),
                ))
            }
            t if t.contains("2094-40") || t.contains("HDR10+") => info.hdr10plus = true,
            _ => {}
        }
    }
}

/// HDR info from an ffprobe `-show_streams` video stream object.
pub fn detect_stream(stream: &Value) -> HdrInfo {
    let mut info = HdrInfo::default();
    if let Some(list) = stream.get("side_data_list").and_then(Value::as_array) {
        apply_side_data(&mut info, list);
    }
    info
}

/// Merge the side data of the first frame (`-show_frames -read_intervals %+#1`).
pub fn merge_frame_side_data(info: &mut HdrInfo, frames_json: &str) {
    let Ok(root) = serde_json::from_str::<Value>(frames_json) else {
        return;
    };
    let frames = root.get("frames").and_then(Value::as_array);
    for f in frames.into_iter().flatten() {
        if let Some(list) = f.get("side_data_list").and_then(Value::as_array) {
            apply_side_data(info, list);
        }
    }
}

/// What survived an encode, comparing source and output HDR info.
#[derive(Clone, Debug, PartialEq)]
pub struct HdrCarryOver {
    pub mastering_kept: bool,
    pub cll_kept: bool,
    pub transfer_kept: bool,
}

pub fn compare(source: &HdrInfo, output: &HdrInfo) -> Option<HdrCarryOver> {
    if !source.pq {
        return None;
    }
    Some(HdrCarryOver {
        mastering_kept: source.mastering.is_none() || output.mastering.is_some(),
        cll_kept: source.cll.is_none() || output.cll.is_some(),
        transfer_kept: output.pq,
    })
}

/// `mkvpropedit` arguments that write HDR10 static metadata into the MKV
/// `Colour` element, used when the encoder didn't carry it (e.g. `av1_vaapi`).
pub fn mkvpropedit_args(file: &Path, info: &HdrInfo) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![file.into(), "--edit".into(), "track:v1".into()];
    let mut set = |k: &str, v: String| {
        a.push("--set".into());
        a.push(format!("{k}={v}").into());
    };
    set("colour-transfer-characteristics", "16".into());
    set("colour-primaries", "9".into());
    set("colour-matrix-coefficients", "9".into());
    if let Some(m) = &info.mastering {
        for (name, (x, y)) in [("red", m.red), ("green", m.green), ("blue", m.blue)] {
            set(
                &format!("chromaticity-coordinates-{name}-x"),
                format!("{x:.5}"),
            );
            set(
                &format!("chromaticity-coordinates-{name}-y"),
                format!("{y:.5}"),
            );
        }
        set("white-coordinates-x", format!("{:.5}", m.white.0));
        set("white-coordinates-y", format!("{:.5}", m.white.1));
        set("max-luminance", format!("{:.4}", m.max_luminance));
        set("min-luminance", format!("{:.4}", m.min_luminance));
    }
    if let Some((cll, fall)) = info.cll {
        set("max-content-light", cll.to_string());
        set("max-frame-light", fall.to_string());
    }
    a
}

// ---- Dolby Vision preservation (experimental) ----

/// What the metadata-injection pipelines need to know about a job: the raw
/// HEVC stream must line up frame for frame with the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InjectJob {
    pub hevc: bool,
    pub ten_bit: bool,
    pub scaling: bool,
    /// Double-rate deinterlacing or inverse telecine changes the frame count.
    pub frames_change: bool,
}

impl InjectJob {
    pub fn new(
        settings: &crate::settings::EncodeSettings,
        probe: &crate::ffmpeg::probe::ProbeInfo,
        scan: crate::scan::Scan,
    ) -> Self {
        Self {
            hevc: settings.codec == crate::settings::VideoCodec::Hevc,
            ten_bit: settings.output_ten_bit(),
            scaling: probe
                .video
                .as_ref()
                .is_some_and(|v| settings.downscale.height().is_some_and(|h| h < v.height)),
            frames_change: !scan.keeps_frame_count(),
        }
    }

    /// Ready for injection.
    #[cfg(test)]
    pub const OK: Self = Self {
        hevc: true,
        ten_bit: true,
        scaling: false,
        frames_change: false,
    };
}

#[derive(Clone, Copy)]
enum InjectBlock {
    Codec,
    Depth,
    Scaling,
    Frames,
}

fn inject_blocker(j: InjectJob) -> Option<InjectBlock> {
    if !j.hevc {
        Some(InjectBlock::Codec)
    } else if !j.ten_bit {
        Some(InjectBlock::Depth)
    } else if j.scaling {
        Some(InjectBlock::Scaling)
    } else if j.frames_change {
        Some(InjectBlock::Frames)
    } else {
        None
    }
}

/// Why DV can't be preserved for this job, if it can't.
pub fn dv_blocker(dovi: Option<Dovi>, job: InjectJob, dovi_tool: bool) -> Option<&'static str> {
    let d = dovi?;
    if !dovi_tool {
        return Some("dovi_tool is not installed");
    }
    match inject_blocker(job) {
        Some(InjectBlock::Codec) => Some("Dolby Vision preservation needs HEVC output"),
        Some(InjectBlock::Depth) => Some("Dolby Vision preservation needs 10-bit output"),
        Some(InjectBlock::Scaling) => {
            Some("Dolby Vision preservation can't be combined with downscaling")
        }
        Some(InjectBlock::Frames) => Some(
            "Dolby Vision preservation can't be combined with deinterlacing to double rate or inverse telecine (they change the frame count); set Scan to Progressive if the source isn't really interlaced",
        ),
        None if !matches!(d.profile, 5 | 7 | 8) => Some("Unsupported Dolby Vision profile"),
        None => None,
    }
}

/// Why HDR10+ can't be preserved for this job, if it can't (`None` also when
/// the source has no HDR10+).
pub fn hdr10plus_blocker(hdr10plus: bool, job: InjectJob, tool: bool) -> Option<&'static str> {
    if !hdr10plus {
        return None;
    }
    if !tool {
        return Some("hdr10plus_tool is not installed");
    }
    inject_blocker(job).map(|b| match b {
        InjectBlock::Codec => "HDR10+ preservation needs HEVC output",
        InjectBlock::Depth => "HDR10+ preservation needs 10-bit output",
        InjectBlock::Scaling => "HDR10+ preservation can't be combined with downscaling",
        InjectBlock::Frames => {
            "HDR10+ preservation can't be combined with deinterlacing to double rate or inverse telecine (they change the frame count); set Scan to Progressive if the source isn't really interlaced"
        }
    })
}

/// `dovi_tool` conversion mode: profile 7 → 8.1 (`-m 2`); 5 and 8 stay as they are.
pub fn dovi_mode(profile: u32) -> Option<&'static str> {
    (profile == 7).then_some("2")
}

/// ffmpeg half of the RPU extraction pipe: the source video as Annex B on stdout.
/// `stream` is the probed main video's absolute index (cover art can precede it).
pub fn extract_video_args(input: &Path, stream: usize) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["-hide_banner", "-v", "error", "-i"]
        .map(OsString::from)
        .to_vec();
    a.push(input.into());
    a.push("-map".into());
    a.push(format!("0:{stream}").into());
    for s in [
        "-c:v",
        "copy",
        "-bsf:v",
        "hevc_mp4toannexb",
        "-f",
        "hevc",
        "-",
    ] {
        a.push(s.into());
    }
    a
}

/// dovi_tool half of the pipe, reading the stream from stdin.
pub fn extract_rpu_args(profile: u32, rpu_out: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    if let Some(m) = dovi_mode(profile) {
        a.push("-m".into());
        a.push(m.into());
    }
    a.extend([
        "extract-rpu".into(),
        "-".into(),
        "-o".into(),
        rpu_out.into(),
    ]);
    a
}

pub fn inject_rpu_args(video: &Path, rpu: &Path, out: &Path) -> Vec<OsString> {
    vec![
        "inject-rpu".into(),
        "-i".into(),
        video.into(),
        "--rpu-in".into(),
        rpu.into(),
        "-o".into(),
        out.into(),
    ]
}

pub fn rpu_info_args(rpu: &Path) -> Vec<OsString> {
    vec!["info".into(), "-i".into(), rpu.into(), "--summary".into()]
}

/// Frame count from `dovi_tool info --summary` ("Frames: 172800").
pub fn parse_rpu_frames(summary: &str) -> Option<u64> {
    summary.lines().find_map(|l| {
        let l = l.trim();
        let rest = l
            .strip_prefix("Frames:")
            .or_else(|| l.strip_prefix("Frame count:"))?;
        rest.trim().parse().ok()
    })
}

// ---- HDR10+ preservation (experimental) ----

/// hdr10plus_tool half of the extraction pipe (stdin from [`extract_video_args`]).
pub fn extract_hdr10plus_args(json_out: &Path) -> Vec<OsString> {
    vec!["extract".into(), "-".into(), "-o".into(), json_out.into()]
}

pub fn inject_hdr10plus_args(video: &Path, json: &Path, out: &Path) -> Vec<OsString> {
    vec![
        "inject".into(),
        "-i".into(),
        video.into(),
        "-j".into(),
        json.into(),
        "-o".into(),
        out.into(),
    ]
}

/// Frames described by an hdr10plus_tool JSON file: one `SceneInfo` entry
/// per frame. Reads from a file without building the whole document (a
/// feature film's metadata runs to ~100 MB).
pub fn hdr10plus_json_frames(r: impl std::io::Read) -> Option<u64> {
    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(rename = "SceneInfo")]
        scenes: Vec<serde::de::IgnoredAny>,
    }
    let d: Doc = serde_json::from_reader(std::io::BufReader::new(r)).ok()?;
    Some(d.scenes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe::tests::sample_4k;

    #[test]
    fn detects_hdr10_and_dv_from_probe() {
        let p = sample_4k();
        assert!(p.hdr.pq && p.hdr.bt2020 && !p.hdr.hlg);
        let d = p.hdr.dovi.unwrap();
        assert_eq!(
            (d.profile, d.level, d.compat_id, d.el_present),
            (8, 6, 1, false)
        );
        assert_eq!(p.hdr.badge().as_deref(), Some("DV 8.1 · HDR10"));
    }

    #[test]
    fn frame_side_data_gives_mastering_cll_and_hdr10plus() {
        let mut info = HdrInfo {
            pq: true,
            ..Default::default()
        };
        merge_frame_side_data(
            &mut info,
            r#"{"frames":[{"side_data_list":[
              {"side_data_type":"Mastering display metadata","red_x":"34000/50000","red_y":"16000/50000",
               "green_x":"13250/50000","green_y":"34500/50000","blue_x":"7500/50000","blue_y":"3000/50000",
               "white_point_x":"15635/50000","white_point_y":"16450/50000","min_luminance":"1/10000",
               "max_luminance":"10000000/10000"},
              {"side_data_type":"Content light level metadata","max_content":1000,"max_average":400},
              {"side_data_type":"HDR Dynamic Metadata SMPTE2094-40 (HDR10+)"}]}]}"#,
        );
        let m = info.mastering.as_ref().unwrap();
        assert!((m.red.0 - 0.68).abs() < 1e-9);
        assert!((m.max_luminance - 1000.0).abs() < 1e-9);
        assert!((m.min_luminance - 0.0001).abs() < 1e-12);
        assert_eq!(info.cll, Some((1000, 400)));
        assert!(info.hdr10plus);
        assert_eq!(info.badge().as_deref(), Some("HDR10+"));
    }

    #[test]
    fn profile_5_and_7_badges() {
        let mut i = HdrInfo {
            dovi: Some(Dovi {
                profile: 5,
                level: 6,
                compat_id: 0,
                el_present: false,
            }),
            ..Default::default()
        };
        assert_eq!(i.badge().as_deref(), Some("DV 5"));
        i.dovi = Some(Dovi {
            profile: 7,
            level: 6,
            compat_id: 6,
            el_present: true,
        });
        i.pq = true;
        assert_eq!(i.badge().as_deref(), Some("DV 7 · HDR10"));
    }

    #[test]
    fn hlg_and_sdr() {
        let p = crate::ffmpeg::probe::parse(r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc",
            "width":1920,"height":1080,"color_transfer":"arib-std-b67","color_primaries":"bt2020"}],"format":{}}"#)
        .unwrap();
        assert!(p.hdr.hlg && p.hdr.is_hdr());
        assert_eq!(p.hdr.badge().as_deref(), Some("HLG"));
        assert!(HdrInfo::default().badge().is_none());
    }

    #[test]
    fn dv_eligibility_and_modes() {
        let d = Some(Dovi {
            profile: 7,
            level: 6,
            compat_id: 6,
            el_present: true,
        });
        let ok = InjectJob::OK;
        assert_eq!(dv_blocker(None, ok, true), None);
        assert!(dv_blocker(d, ok, false).is_some());
        assert!(dv_blocker(d, InjectJob { hevc: false, ..ok }, true).is_some());
        assert!(
            dv_blocker(
                d,
                InjectJob {
                    scaling: true,
                    ..ok
                },
                true
            )
            .is_some()
        );
        // 8-bit HEVC output can't carry the RPU's 10-bit base layer.
        assert!(
            dv_blocker(
                d,
                InjectJob {
                    ten_bit: false,
                    ..ok
                },
                true
            )
            .is_some()
        );
        assert!(
            dv_blocker(
                d,
                InjectJob {
                    frames_change: true,
                    ..ok
                },
                true
            )
            .is_some()
        );
        assert_eq!(dv_blocker(d, ok, true), None);
        assert_eq!(dovi_mode(7), Some("2"));
        assert_eq!(dovi_mode(8), None);
        let a = extract_rpu_args(7, Path::new("/t/rpu.bin"));
        assert_eq!(
            a,
            ["-m", "2", "extract-rpu", "-", "-o", "/t/rpu.bin"].map(OsString::from)
        );
    }

    #[test]
    fn parses_rpu_summary() {
        let s = "Parsing RPU file...\nSummary:\n  Frames: 172800\n  Profile: 8\n";
        assert_eq!(parse_rpu_frames(s), Some(172_800));
        assert_eq!(parse_rpu_frames("nothing"), None);
    }

    #[test]
    fn compare_reports_lost_mastering() {
        let src = HdrInfo {
            pq: true,
            mastering: Some(Mastering::default()),
            cll: Some((1, 1)),
            ..Default::default()
        };
        let out = HdrInfo {
            pq: true,
            ..Default::default()
        };
        let c = compare(&src, &out).unwrap();
        assert!(!c.mastering_kept && !c.cll_kept && c.transfer_kept);
        assert!(compare(&HdrInfo::default(), &out).is_none());
    }

    #[test]
    fn mkvpropedit_writes_colour_element() {
        let info = HdrInfo {
            pq: true,
            cll: Some((1000, 400)),
            mastering: Some(Mastering {
                max_luminance: 1000.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let a: Vec<String> = mkvpropedit_args(Path::new("o.mkv"), &info)
            .iter()
            .map(|s| s.to_string_lossy().into())
            .collect();
        assert_eq!(&a[..3], ["o.mkv", "--edit", "track:v1"]);
        assert!(a.contains(&"max-content-light=1000".to_string()));
        assert!(a.contains(&"max-luminance=1000.0000".to_string()));
    }

    #[test]
    fn hdr10plus_blocker_rules() {
        let ok = InjectJob::OK;
        let no = |f: fn(&mut InjectJob)| {
            let mut j = ok;
            f(&mut j);
            hdr10plus_blocker(true, j, true)
        };
        assert_eq!(
            hdr10plus_blocker(false, InjectJob { hevc: false, ..ok }, false),
            None
        );
        assert_eq!(
            hdr10plus_blocker(true, ok, false),
            Some("hdr10plus_tool is not installed")
        );
        assert_eq!(
            no(|j| j.hevc = false),
            Some("HDR10+ preservation needs HEVC output")
        );
        assert_eq!(
            no(|j| j.ten_bit = false),
            Some("HDR10+ preservation needs 10-bit output")
        );
        assert_eq!(
            no(|j| j.scaling = true),
            Some("HDR10+ preservation can't be combined with downscaling")
        );
        assert!(
            no(|j| j.frames_change = true)
                .unwrap()
                .contains("frame count")
        );
        assert_eq!(hdr10plus_blocker(true, ok, true), None);
    }

    #[test]
    fn inject_job_from_settings() {
        use crate::scan::Scan;
        let p = sample_4k();
        let s = crate::settings::EncodeSettings::default();
        assert_eq!(InjectJob::new(&s, &p, Scan::Progressive), InjectJob::OK);
        let single = Scan::Deinterlace {
            tff: true,
            double_rate: false,
        };
        assert!(!InjectJob::new(&s, &p, single).frames_change);
        let double = Scan::Deinterlace {
            tff: true,
            double_rate: true,
        };
        assert!(InjectJob::new(&s, &p, double).frames_change);
        assert!(InjectJob::new(&s, &p, Scan::Telecine { tff: true }).frames_change);
        let small = crate::settings::EncodeSettings {
            downscale: crate::settings::Downscale::P1080,
            ..Default::default()
        };
        assert!(InjectJob::new(&small, &p, Scan::Progressive).scaling);
    }

    #[test]
    fn hdr10plus_tool_arguments() {
        let s = |v: Vec<OsString>| -> Vec<String> {
            v.iter().map(|s| s.to_string_lossy().into()).collect()
        };
        assert_eq!(
            s(extract_hdr10plus_args(Path::new("m.json"))),
            ["extract", "-", "-o", "m.json"]
        );
        assert_eq!(
            s(inject_hdr10plus_args(
                Path::new("v.hevc"),
                Path::new("m.json"),
                Path::new("o.hevc")
            )),
            ["inject", "-i", "v.hevc", "-j", "m.json", "-o", "o.hevc"]
        );
    }

    #[test]
    fn hdr10plus_json_counts_frames() {
        let json = r#"{"JSONInfo":{"HDR10plusProfile":"B","Version":"1.0"},
            "SceneInfo":[{"SceneFrameIndex":0,"LuminanceParameters":{"MaxScl":[1,2,3]}},
                         {"SceneFrameIndex":1},{"SceneFrameIndex":2}],
            "SceneInfoSummary":{"SceneFirstFrameIndex":[0],"SceneFrameNumbers":[3]}}"#;
        assert_eq!(hdr10plus_json_frames(json.as_bytes()), Some(3));
        assert_eq!(hdr10plus_json_frames(&b"{}"[..]), None);
        assert_eq!(hdr10plus_json_frames(&b"not json"[..]), None);
    }
}
