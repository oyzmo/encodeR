//! VMAF quality check: libvmaf command lines, score parsing and plain-language bands.

use std::ffi::OsString;
use std::path::Path;

use crate::crop::Crop;

pub struct VmafInput<'a> {
    pub distorted: &'a Path,
    /// Seek into the distorted file too (post-encode check of the full output).
    pub distorted_seek: bool,
    pub reference: &'a Path,
    pub start: f64,
    pub secs: f64,
    pub width: u32,
    pub height: u32,
    pub ten_bit: bool,
    pub crop: Option<Crop>,
    /// The encode's deinterlace / IVTC filter, applied to the reference too so frames line up.
    pub ref_filter: Option<String>,
    pub threads: usize,
}

pub fn model_for(height: u32) -> &'static str {
    if height >= 2000 {
        "vmaf_4k_v0.6.1"
    } else {
        "vmaf_v0.6.1"
    }
}

pub fn args(i: &VmafInput) -> Vec<OsString> {
    let (w, h) = i.crop.map_or((i.width, i.height), |c| (c.w, c.h));
    let fmt = if i.ten_bit { "yuv420p10le" } else { "yuv420p" };
    let mut pre = i
        .ref_filter
        .as_ref()
        .map(|f| format!("{f},"))
        .unwrap_or_default();
    if let Some(c) = i.crop {
        pre.push_str(&format!("{},", c.filter()));
    }
    // The encode is scaled back to the source resolution for the comparison.
    let graph = format!(
        "[0:v]scale={w}:{h}:flags=bicubic,format={fmt},setpts=PTS-STARTPTS[dist];\
         [1:v]{pre}format={fmt},setpts=PTS-STARTPTS[ref];\
         [dist][ref]libvmaf=model=version={}:n_threads={}",
        model_for(i.height),
        i.threads.max(1)
    );
    let seek = |v: &mut Vec<OsString>| {
        v.push("-ss".into());
        v.push(format!("{:.3}", i.start).into());
        v.push("-t".into());
        v.push(format!("{:.3}", i.secs).into());
    };
    let mut a: Vec<OsString> = ["-hide_banner", "-nostdin"].map(OsString::from).to_vec();
    if i.distorted_seek {
        seek(&mut a);
    }
    a.push("-i".into());
    a.push(i.distorted.into());
    seek(&mut a);
    a.push("-i".into());
    a.push(i.reference.into());
    for s in ["-lavfi", &graph, "-f", "null", "-"] {
        a.push(s.into());
    }
    a
}

/// "[Parsed_libvmaf_4 @ 0x…] VMAF score: 93.452100".
pub fn parse_score(stderr: &str) -> Option<f64> {
    stderr.lines().rev().find_map(|l| {
        l.split_once("VMAF score:")
            .and_then(|(_, s)| s.trim().parse().ok())
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VmafResult {
    pub mean: f64,
    pub worst: f64,
}

pub fn summarize(scores: &[f64]) -> Option<VmafResult> {
    if scores.is_empty() {
        return None;
    }
    Some(VmafResult {
        mean: scores.iter().sum::<f64>() / scores.len() as f64,
        worst: scores.iter().copied().fold(f64::MAX, f64::min),
    })
}

pub fn band(score: f64) -> &'static str {
    match score {
        s if s >= 95.0 => "visually transparent",
        s if s >= 90.0 => "very good",
        s if s >= 80.0 => "good, visible loss on close inspection",
        _ => "poor",
    }
}

/// "VMAF 93.4 (very good) · worst segment 88.1".
pub fn describe(r: VmafResult, hdr: bool) -> String {
    let mut s = format!(
        "VMAF {:.1} ({}) · worst segment {:.1}",
        r.mean,
        band(r.mean),
        r.worst
    );
    if hdr {
        s.push_str(" · approximate (VMAF is not calibrated for HDR)");
    }
    s
}

/// Below this, target-size mode suggests a bigger target or downscaling.
pub const SUGGEST_BELOW: f64 = 90.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_graph_with_scale_back_and_model() {
        let i = VmafInput {
            distorted: Path::new("/c/s.mkv"),
            distorted_seek: false,
            reference: Path::new("/m/in.mkv"),
            start: 100.0,
            secs: 8.0,
            width: 3840,
            height: 2160,
            ten_bit: true,
            ref_filter: Some("bwdif=mode=send_field:parity=tff:deint=all".into()),
            crop: Some(Crop {
                w: 3840,
                h: 1608,
                x: 0,
                y: 276,
            }),
            threads: 4,
        };
        let a: Vec<String> = args(&i)
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        let g = &a[a.iter().position(|x| x == "-lavfi").unwrap() + 1];
        assert!(g.contains("[0:v]scale=3840:1608:flags=bicubic,format=yuv420p10le"));
        assert!(g.contains("[1:v]bwdif=mode=send_field:parity=tff:deint=all,crop=3840:1608:0:276,format=yuv420p10le"));
        assert!(g.contains("version=vmaf_4k_v0.6.1"));
        // Reference is seeked; the sample (already cut) isn't.
        assert_eq!(&a[2..4], ["-i", "/c/s.mkv"]);
        assert_eq!(&a[4..8], ["-ss", "100.000", "-t", "8.000"]);
        assert_eq!(model_for(1080), "vmaf_v0.6.1");
    }

    #[test]
    fn parses_and_summarizes() {
        let log = "frame=  200\n[Parsed_libvmaf_2 @ 0x5588] VMAF score: 93.452100\n";
        assert_eq!(parse_score(log), Some(93.4521));
        assert_eq!(parse_score("nothing"), None);
        let r = summarize(&[96.0, 91.0, 86.0]).unwrap();
        assert!((r.mean - 91.0).abs() < 1e-9);
        assert_eq!(r.worst, 86.0);
        assert_eq!(band(96.0), "visually transparent");
        assert_eq!(band(85.0), "good, visible loss on close inspection");
        assert_eq!(band(70.0), "poor");
        assert!(describe(r, true).contains("approximate"));
        assert_eq!(
            describe(r, false),
            "VMAF 91.0 (very good) · worst segment 86.0"
        );
    }
}
