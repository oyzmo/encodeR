//! Startup capability check: which encoders, profiles and rate-control modes
//! the VA-API driver really accepts, plus optional tools.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::thread;

use super::runner::{self, Cancel};
use crate::settings::{RateControl, VideoCodec};

#[derive(Clone, Debug, Default)]
pub struct CodecCaps {
    pub available: bool,
    /// Why it isn't, for the row subtitle.
    pub reason: Option<String>,
    pub ten_bit: bool,
    pub rate_controls: Vec<RateControl>,
    /// Frame size range the hardware encoder accepts, from the driver's error message.
    pub limits: Option<SizeLimits>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeLimits {
    pub min_w: u32,
    pub max_w: u32,
    pub min_h: u32,
    pub max_h: u32,
}

impl SizeLimits {
    pub fn fits(&self, w: u32, h: u32) -> bool {
        (self.min_w..=self.max_w).contains(&w) && (self.min_h..=self.max_h).contains(&h)
    }
}

/// "… (constraints: width 384-8192 height 128-4352)".
pub fn parse_limits(stderr: &str) -> Option<SizeLimits> {
    let rest = stderr.split("constraints:").nth(1)?;
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>());
    let mut next = || nums.next()?.ok();
    Some(SizeLimits {
        min_w: next()?,
        max_w: next()?,
        min_h: next()?,
        max_h: next()?,
    })
}

#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    pub ffmpeg: bool,
    pub ffprobe: bool,
    pub ffmpeg_version: String,
    pub devices: Vec<RenderDevice>,
    pub device: Option<String>,
    pub codecs: HashMap<VideoCodec, CodecCaps>,
    pub crop_on_gpu: bool,
    pub libvmaf: bool,
    /// Filters the app needs that this ffmpeg lacks.
    pub missing_filters: Vec<String>,
    pub dovi_tool: bool,
    pub mkvpropedit: bool,
}

impl Capabilities {
    pub fn codec(&self, c: VideoCodec) -> CodecCaps {
        if c == VideoCodec::Copy {
            return CodecCaps {
                available: self.ffmpeg,
                ten_bit: true,
                ..Default::default()
            };
        }
        self.codecs.get(&c).cloned().unwrap_or_default()
    }

    pub fn any_hw_encoder(&self) -> bool {
        self.codecs.values().any(|c| c.available)
    }

    pub fn rate_control_ok(&self, c: VideoCodec, rc: RateControl) -> bool {
        rc == RateControl::Auto
            || c == VideoCodec::Copy
            || self.codec(c).rate_controls.contains(&rc)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderDevice {
    pub path: String,
    pub vendor: String,
    pub driver: String,
}

impl RenderDevice {
    pub fn is_amd(&self) -> bool {
        self.vendor == "0x1002"
    }

    pub fn label(&self) -> String {
        let name = match self.vendor.as_str() {
            "0x1002" => "AMD",
            "0x8086" => "Intel",
            "0x10de" => "NVIDIA",
            "0x1af4" => "Virtio",
            _ => "GPU",
        };
        format!("{name} ({})", self.path)
    }
}

/// Filters the app's pipelines use (libvmaf is optional and checked separately).
pub const REQUIRED_FILTERS: [&str; 10] = [
    "scale_vaapi",
    "hwupload",
    "idet",
    "cropdetect",
    "bwdif",
    "fieldmatch",
    "yadif",
    "decimate",
    "crop",
    "drawbox",
];

pub fn render_devices() -> Vec<RenderDevice> {
    let mut v: Vec<RenderDevice> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.starts_with("renderD").then(|| {
                let sys = PathBuf::from("/sys/class/drm").join(&name).join("device");
                let read = |f: &str| {
                    std::fs::read_to_string(sys.join(f))
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                };
                let driver = std::fs::read_link(sys.join("driver"))
                    .ok()
                    .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                RenderDevice {
                    path: format!("/dev/dri/{name}"),
                    vendor: read("vendor"),
                    driver,
                }
            })
        })
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

/// The configured device, or the first AMD render node, or the first one.
pub fn pick_device(devices: &[RenderDevice], configured: Option<&str>) -> Option<String> {
    if let Some(c) = configured.filter(|c| devices.iter().any(|d| d.path == *c)) {
        return Some(c.to_string());
    }
    devices
        .iter()
        .find(|d| d.is_amd())
        .or(devices.first())
        .map(|d| d.path.clone())
}

fn test_args(
    device: &str,
    encoder: &str,
    fmt: &str,
    extra: &[&str],
    vf_extra: &str,
) -> Vec<OsString> {
    let vf = format!("format={fmt},hwupload{vf_extra}");
    let mut a: Vec<OsString> = Vec::new();
    for s in [
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-vaapi_device",
        device,
        "-f",
        "lavfi",
        "-i",
        // RDNA4 rejects frames narrower than 384 (HEVC) / 320 (AV1); stay well above.
        "testsrc2=size=640x360:rate=25",
        "-frames:v",
        "2",
        "-vf",
        &vf,
        "-c:v",
        encoder,
    ] {
        a.push(s.into());
    }
    a.extend(extra.iter().map(OsString::from));
    for s in ["-f", "null", "-"] {
        a.push(s.into());
    }
    a
}

fn quality_args(codec: VideoCodec) -> [&'static str; 2] {
    match codec {
        VideoCodec::Av1 => ["-global_quality", "120"],
        _ => ["-qp", "25"],
    }
}

fn probe_codec(ffmpeg: &str, device: &str, codec: VideoCodec, listed: bool) -> CodecCaps {
    let Some(enc) = codec.encoder() else {
        return CodecCaps::default();
    };
    if !listed {
        return CodecCaps {
            reason: Some(format!(
                "{enc} is missing from ffmpeg -encoders (install the RPM Fusion ffmpeg)"
            )),
            ..Default::default()
        };
    }
    let cancel = Cancel::default();
    let run = |fmt: &str, extra: &[&str]| {
        runner::run_capture(ffmpeg, &test_args(device, enc, fmt, extra, ""), &cancel)
    };
    let q = quality_args(codec);
    match run("nv12", &["-rc_mode", "CQP", q[0], q[1]]) {
        Ok(r) if r.success => {}
        Ok(r) => {
            return CodecCaps {
                reason: Some(format!(
                    "Test encode failed: {}",
                    runner::error_summary(&r.stderr)
                )),
                ..Default::default()
            };
        }
        Err(e) => {
            return CodecCaps {
                reason: Some(e),
                ..Default::default()
            };
        }
    }
    let ten_bit = codec.supports_10bit() && {
        let profile = if codec == VideoCodec::Hevc {
            "main10"
        } else {
            "main"
        };
        run("p010", &["-profile:v", profile]).is_ok_and(|r| r.success)
    };
    let mut rate_controls = vec![RateControl::Cqp];
    for rc in [RateControl::Vbr, RateControl::Cbr, RateControl::Qvbr] {
        let mut extra = vec![
            "-rc_mode",
            rc.ffmpeg_name(),
            "-b:v",
            "1000k",
            "-maxrate",
            "1500k",
        ];
        if rc == RateControl::Qvbr {
            extra.extend(["-global_quality", "25"]);
        }
        if run("nv12", &extra).is_ok_and(|r| r.success) {
            rate_controls.push(rc);
        }
    }
    // A deliberately tiny frame makes the driver print its size constraints.
    let tiny = test_args(
        device,
        enc,
        "nv12",
        &["-rc_mode", "CQP", q[0], q[1]],
        ",scale_vaapi=w=64:h=64",
    );
    let limits = runner::run_capture(ffmpeg, &tiny, &cancel)
        .ok()
        .and_then(|r| parse_limits(&r.stderr));
    CodecCaps {
        available: true,
        limits,
        reason: None,
        ten_bit,
        rate_controls,
    }
}

/// Runs the whole check (≈ 1–3 s with a GPU). Call from a worker thread.
pub fn detect(ffmpeg: &str, ffprobe: &str, configured_device: Option<&str>) -> Capabilities {
    let cancel = Cancel::default();
    let mut caps = Capabilities {
        ffprobe: runner::which(ffprobe),
        dovi_tool: runner::which("dovi_tool"),
        mkvpropedit: runner::which("mkvpropedit"),
        devices: render_devices(),
        ..Default::default()
    };
    caps.device = pick_device(&caps.devices, configured_device);
    let encoders = match runner::run_capture(
        ffmpeg,
        &["-hide_banner".into(), "-encoders".into()],
        &cancel,
    ) {
        Ok(r) if r.success => r.stdout,
        _ => return caps,
    };
    caps.ffmpeg = true;
    if let Ok(r) = runner::run_capture(ffmpeg, &["-hide_banner".into(), "-version".into()], &cancel)
    {
        caps.ffmpeg_version = r.stdout.lines().next().unwrap_or_default().to_string();
    }
    if let Ok(r) = runner::run_capture(ffmpeg, &["-hide_banner".into(), "-filters".into()], &cancel)
    {
        let names: Vec<&str> = r
            .stdout
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .collect();
        caps.libvmaf = names.contains(&"libvmaf");
        caps.missing_filters = REQUIRED_FILTERS
            .iter()
            .filter(|f| !names.contains(f))
            .map(|f| f.to_string())
            .collect();
    }
    let listed = |enc: &str| {
        encoders
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(enc))
    };
    let Some(device) = caps.device.clone() else {
        for c in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            caps.codecs.insert(
                c,
                CodecCaps {
                    reason: Some("No /dev/dri/renderD* device found".into()),
                    ..Default::default()
                },
            );
        }
        return caps;
    };
    let handles: Vec<_> = [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1]
        .into_iter()
        .map(|c| {
            let (ffmpeg, device) = (ffmpeg.to_string(), device.clone());
            let is_listed = listed(c.encoder().unwrap_or_default());
            thread::spawn(move || (c, probe_codec(&ffmpeg, &device, c, is_listed)))
        })
        .collect();
    for h in handles {
        if let Ok((c, cc)) = h.join() {
            caps.codecs.insert(c, cc);
        }
    }
    // Can `crop` run on VA-API surfaces, keeping the whole path on the GPU?
    if let Some(c) = [VideoCodec::Hevc, VideoCodec::H264, VideoCodec::Av1]
        .into_iter()
        .find(|c| caps.codec(*c).available)
    {
        let enc = c.encoder().unwrap_or_default();
        let q = quality_args(c);
        let args = test_args(
            &device,
            enc,
            "nv12",
            &["-rc_mode", "CQP", q[0], q[1]],
            ",crop=512:272:0:44",
        );
        caps.crop_on_gpu = runner::run_capture(ffmpeg, &args, &cancel).is_ok_and(|r| r.success);
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(path: &str, vendor: &str) -> RenderDevice {
        RenderDevice {
            path: path.into(),
            vendor: vendor.into(),
            driver: String::new(),
        }
    }

    #[test]
    fn parses_size_limits() {
        let l = parse_limits("[hevc_vaapi @ 0x1] Hardware does not support encoding at size 64x64 (constraints: width 384-8192 height 128-4352).").unwrap();
        assert_eq!(
            l,
            SizeLimits {
                min_w: 384,
                max_w: 8192,
                min_h: 128,
                max_h: 4352
            }
        );
        assert!(l.fits(1920, 1080) && !l.fits(320, 240));
        assert_eq!(parse_limits("no constraints here"), None);
    }

    #[test]
    fn prefers_configured_then_amd() {
        let d = vec![
            dev("/dev/dri/renderD128", "0x8086"),
            dev("/dev/dri/renderD129", "0x1002"),
        ];
        assert_eq!(
            pick_device(&d, None).as_deref(),
            Some("/dev/dri/renderD129")
        );
        assert_eq!(
            pick_device(&d, Some("/dev/dri/renderD128")).as_deref(),
            Some("/dev/dri/renderD128")
        );
        assert_eq!(
            pick_device(&d, Some("/dev/dri/renderD200")).as_deref(),
            Some("/dev/dri/renderD129")
        );
        assert_eq!(pick_device(&[], None), None);
        assert_eq!(d[1].label(), "AMD (/dev/dri/renderD129)");
    }
}
