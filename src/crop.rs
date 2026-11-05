//! Black-bar detection: cropdetect over several segments, aggregated to the
//! least aggressive crop so changing aspect ratios (IMAX scenes) are never cut.

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Crop {
    pub w: u32,
    pub h: u32,
    pub x: u32,
    pub y: u32,
}

impl Crop {
    pub fn filter(&self) -> String {
        format!("crop={}:{}:{}:{}", self.w, self.h, self.x, self.y)
    }
}

/// Crops saving less than this share of the picture are ignored.
pub const MIN_SAVING: f64 = 0.02;
pub const SAMPLES: usize = 6;
pub const SAMPLE_SECONDS: f64 = 4.0;

/// Parse every `crop=w:h:x:y` from cropdetect's stderr log.
pub fn parse_cropdetect(log: &str) -> Vec<Crop> {
    log.lines()
        .filter_map(|l| {
            let c = l.rsplit_once("crop=")?.1.trim();
            let mut it = c.split(':').map(|n| n.trim().parse::<i64>());
            let (w, h, x, y) = (
                it.next()?.ok()?,
                it.next()?.ok()?,
                it.next()?.ok()?,
                it.next()?.ok()?,
            );
            // cropdetect prints negative sizes for an all-black frame.
            (w > 0 && h > 0 && x >= 0 && y >= 0).then_some(Crop {
                w: w as u32,
                h: h as u32,
                x: x as u32,
                y: y as u32,
            })
        })
        .collect()
}

/// Union of all detected rectangles (the largest picture area), rounded to
/// even sizes and clamped to the frame. `None` if it saves under 2 %.
pub fn aggregate(crops: &[Crop], width: u32, height: u32) -> Option<Crop> {
    let first = crops.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x + first.w, first.y + first.h);
    for c in &crops[1..] {
        x0 = x0.min(c.x);
        y0 = y0.min(c.y);
        x1 = x1.max(c.x + c.w);
        y1 = y1.max(c.y + c.h);
    }
    x1 = x1.min(width);
    y1 = y1.min(height);
    // Round the offset down and the size up to even numbers (never cuts picture).
    let x = x0 & !1;
    let y = y0 & !1;
    let w = ((x1 - x + 1) & !1).min(width - x);
    let h = ((y1 - y + 1) & !1).min(height - y);
    let crop = Crop { w, h, x, y };
    (saving(&crop, width, height) >= MIN_SAVING).then_some(crop)
}

pub fn saving(c: &Crop, width: u32, height: u32) -> f64 {
    let full = width as f64 * height as f64;
    if full == 0.0 {
        0.0
    } else {
        1.0 - (c.w as f64 * c.h as f64) / full
    }
}

/// "3840×2160 → 3840×1608, saves 26% of pixels".
pub fn describe(c: &Crop, width: u32, height: u32) -> String {
    format!(
        "{width}×{height} → {}×{}, saves {:.0}% of pixels",
        c.w,
        c.h,
        saving(c, width, height) * 100.0
    )
}

/// Evenly spread sample start times, skipping the very start and end (credits).
pub fn sample_starts(duration: f64, n: usize, len: f64) -> Vec<f64> {
    if duration <= len * 2.0 {
        return vec![0.0];
    }
    (1..=n)
        .map(|i| {
            (duration * i as f64 / (n as f64 + 1.0))
                .min(duration - len)
                .max(0.0)
        })
        .collect()
}

/// One analysis segment (software decode): `idet` on every frame for interlace
/// detection, then `cropdetect` at 4 fps.
/// `stream` is the probed main video's absolute index (cover art can precede it).
pub fn cropdetect_args(input: &Path, stream: usize, start: f64, len: f64) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    for s in [
        "-hide_banner",
        "-nostdin",
        "-v",
        "info",
        "-ss",
        &format!("{start:.3}"),
        "-t",
        &format!("{len:.3}"),
        "-i",
    ] {
        a.push(s.into());
    }
    a.push(input.into());
    a.push("-map".into());
    a.push(format!("0:{stream}").into());
    for s in [
        "-an",
        "-sn",
        "-dn",
        "-vf",
        // A limit below 1 scales with bit depth; an absolute 24 would count
        // 10-bit black (64) as picture and find no bars in HDR sources.
        "idet,fps=4,cropdetect=limit=0.094:round=2:reset=0",
        "-f",
        "null",
        "-",
    ] {
        a.push(s.into());
    }
    a
}

/// One preview frame as PNG with the crop area outlined.
pub fn preview_args(
    input: &Path,
    stream: usize,
    at: f64,
    crop: Option<&Crop>,
    out: &Path,
) -> Vec<OsString> {
    let mut vf = String::new();
    if let Some(c) = crop {
        vf.push_str(&format!(
            "drawbox=x={}:y={}:w={}:h={}:color=0x7FA07A@0.9:t=8,",
            c.x, c.y, c.w, c.h
        ));
    }
    // PNG previews of 10-bit/HDR sources stay viewable at this size.
    vf.push_str("scale=w=960:h=-2,format=rgb24");
    let mut a: Vec<OsString> = Vec::new();
    for s in [
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-ss",
        &format!("{at:.3}"),
        "-i",
    ] {
        a.push(s.into());
    }
    a.push(input.into());
    let map = format!("0:{stream}");
    for s in ["-map", &map, "-frames:v", "1", "-vf", &vf] {
        a.push(s.into());
    }
    a.push(out.into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cropdetect_limit_scales_with_bit_depth() {
        // An absolute limit (24) is below 10-bit black (64): no bars would be found.
        let a = cropdetect_args(Path::new("/in.mkv"), 0, 0.0, 4.0);
        let vf = a
            .iter()
            .find_map(|x| x.to_str().filter(|s| s.contains("cropdetect")));
        assert!(vf.unwrap().contains("cropdetect=limit=0.094:"));
    }

    #[test]
    fn maps_the_main_video_not_cover_art() {
        // Stream 0 is cover art here; the probe found the film at index 2.
        let has_map = |a: &[OsString]| a.windows(2).any(|w| w[0] == "-map" && w[1] == "0:2");
        assert!(has_map(&cropdetect_args(Path::new("/in.mkv"), 2, 0.0, 4.0)));
        assert!(has_map(&preview_args(
            Path::new("/in.mkv"),
            2,
            1.0,
            None,
            Path::new("/p.png")
        )));
        assert!(has_map(&crate::hdr::extract_video_args(
            Path::new("/in.mkv"),
            2
        )));
    }

    #[test]
    fn parses_cropdetect_lines() {
        let log = "[Parsed_cropdetect_1 @ 0x55] x1:0 x2:1919 y1:140 y2:939 w:1920 h:800 x:0 y:140 pts:12 t:0.5 limit:0.09 crop=1920:800:0:140\n\
                   frame= 10 fps=0.0\n\
                   [Parsed_cropdetect_1 @ 0x55] x1:1919 x2:0 y1:1079 y2:0 w:-1904 h:-1072 x:1912 y:1076 crop=-1904:-1072:1912:1076";
        assert_eq!(
            parse_cropdetect(log),
            vec![Crop {
                w: 1920,
                h: 800,
                x: 0,
                y: 140
            }]
        );
    }

    #[test]
    fn least_aggressive_crop_wins() {
        // A 2.39:1 scene and a taller IMAX scene: keep the IMAX area.
        let crops = [
            Crop {
                w: 3840,
                h: 1608,
                x: 0,
                y: 276,
            },
            Crop {
                w: 3840,
                h: 2160,
                x: 0,
                y: 0,
            },
        ];
        assert_eq!(aggregate(&crops, 3840, 2160), None);
        let crops = [
            Crop {
                w: 3840,
                h: 1608,
                x: 0,
                y: 276,
            },
            Crop {
                w: 3840,
                h: 1800,
                x: 0,
                y: 180,
            },
        ];
        assert_eq!(
            aggregate(&crops, 3840, 2160),
            Some(Crop {
                w: 3840,
                h: 1800,
                x: 0,
                y: 180
            })
        );
    }

    #[test]
    fn rounds_to_even_and_never_cuts() {
        let crops = [Crop {
            w: 1917,
            h: 799,
            x: 1,
            y: 141,
        }];
        let c = aggregate(&crops, 1920, 1080).unwrap();
        assert_eq!((c.x, c.y), (0, 140));
        assert!(c.w.is_multiple_of(2) && c.h.is_multiple_of(2));
        assert!(c.x + c.w >= 1918 && c.y + c.h >= 940);
        assert!(c.x + c.w <= 1920 && c.y + c.h <= 1080);
    }

    #[test]
    fn ignores_tiny_crops() {
        // 1080 → 1064 saves 1.5 %.
        assert_eq!(
            aggregate(
                &[Crop {
                    w: 1920,
                    h: 1064,
                    x: 0,
                    y: 8
                }],
                1920,
                1080
            ),
            None
        );
        // 1080 → 1040 saves 3.7 %.
        assert!(
            aggregate(
                &[Crop {
                    w: 1920,
                    h: 1040,
                    x: 0,
                    y: 20
                }],
                1920,
                1080
            )
            .is_some()
        );
        assert_eq!(aggregate(&[], 1920, 1080), None);
    }

    #[test]
    fn describes_crop() {
        let c = Crop {
            w: 3840,
            h: 1608,
            x: 0,
            y: 276,
        };
        assert_eq!(
            describe(&c, 3840, 2160),
            "3840×2160 → 3840×1608, saves 26% of pixels"
        );
    }

    #[test]
    fn sample_positions() {
        assert_eq!(sample_starts(5.0, 6, 4.0), vec![0.0]);
        let s = sample_starts(700.0, 6, 4.0);
        assert_eq!(s.len(), 6);
        assert!((s[0] - 100.0).abs() < 1e-9 && (s[5] - 600.0).abs() < 1e-9);
    }
}
