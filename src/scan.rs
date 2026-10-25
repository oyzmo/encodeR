//! Interlace / telecine detection (ffmpeg `idet`) and the matching filters.
//! Detection looks at the picture, not the flags: streams are often flagged
//! interlaced while carrying progressive film, and the other way round.

use serde::{Deserialize, Serialize};

/// What the picture really is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanType {
    Progressive,
    Interlaced {
        tff: bool,
    },
    /// Film with 3:2 pulldown baked in (29.97 fps from 23.976).
    Telecine {
        tff: bool,
    },
}

impl ScanType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Progressive => "Progressive",
            Self::Interlaced { tff: true } => "Interlaced (top field first)",
            Self::Interlaced { tff: false } => "Interlaced (bottom field first)",
            Self::Telecine { .. } => "Telecined film (3:2 pulldown)",
        }
    }
}

/// Per-job choice; `Auto` follows detection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScanOverride {
    #[default]
    Auto,
    Progressive,
    Deinterlace,
    InverseTelecine,
}

impl ScanOverride {
    pub const ALL: [ScanOverride; 4] = [
        Self::Auto,
        Self::Progressive,
        Self::Deinterlace,
        Self::InverseTelecine,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Progressive => "Leave as is",
            Self::Deinterlace => "Deinterlace",
            Self::InverseTelecine => "Inverse telecine",
        }
    }
}

/// What the encode does with the picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scan {
    #[default]
    Progressive,
    Deinterlace {
        tff: bool,
        double_rate: bool,
    },
    Telecine {
        tff: bool,
    },
}

impl Scan {
    /// Output frame rate as a multiple of the input rate.
    pub fn rate_factor(self) -> f64 {
        match self {
            Self::Progressive
            | Self::Deinterlace {
                double_rate: false, ..
            } => 1.0,
            Self::Deinterlace {
                double_rate: true, ..
            } => 2.0,
            Self::Telecine { .. } => 0.8,
        }
    }

    /// CPU filter chain (system-memory frames).
    pub fn cpu_filter(self) -> Option<String> {
        let parity = |tff: bool| if tff { "tff" } else { "bff" };
        match self {
            Self::Progressive => None,
            Self::Deinterlace { tff, double_rate } => Some(format!(
                "bwdif=mode={}:parity={}:deint=all",
                if double_rate {
                    "send_field"
                } else {
                    "send_frame"
                },
                parity(tff)
            )),
            // Match fields back into film frames, clean up leftover combing, drop the duplicates.
            Self::Telecine { tff } => Some(format!(
                "fieldmatch=order={}:combmatch=full,yadif=deint=interlaced,decimate",
                parity(tff)
            )),
        }
    }

    /// Deinterlacing and IVTC run on the CPU. `deinterlace_vaapi` on radeonsi
    /// (Mesa 26.2) left most frames combed in hardware tests, while `bwdif` was clean.
    pub fn needs_cpu(self) -> bool {
        self != Self::Progressive
    }

    pub fn describe(self, fps: f64) -> String {
        let out = fps * self.rate_factor();
        let rate = if (out - out.round()).abs() < 0.01 {
            format!("{out:.0}")
        } else {
            format!("{out:.3}")
        };
        match self {
            Self::Progressive => "No deinterlacing".into(),
            Self::Deinterlace { .. } => format!("Deinterlacing to {rate}p"),
            Self::Telecine { .. } => format!("Restoring the original film frames ({rate} fps)"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdetCounts {
    pub tff: u64,
    pub bff: u64,
    pub progressive: u64,
    pub rep_neither: u64,
    pub rep_top: u64,
    pub rep_bottom: u64,
}

impl IdetCounts {
    pub fn add(&mut self, o: &IdetCounts) {
        self.tff += o.tff;
        self.bff += o.bff;
        self.progressive += o.progressive;
        self.rep_neither += o.rep_neither;
        self.rep_top += o.rep_top;
        self.rep_bottom += o.rep_bottom;
    }
}

fn field(line: &str, name: &str) -> Option<u64> {
    let rest = line.split(name).nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

/// Final idet summary from ffmpeg's stderr (idet also prints an all-zero
/// summary when the graph is set up; the last one wins).
pub fn parse_idet(stderr: &str) -> Option<IdetCounts> {
    let multi = stderr
        .lines()
        .rev()
        .find(|l| l.contains("Multi frame detection:"))?;
    let rep = stderr
        .lines()
        .rev()
        .find(|l| l.contains("Repeated Fields:"))?;
    Some(IdetCounts {
        tff: field(multi, "TFF:")?,
        bff: field(multi, "BFF:")?,
        progressive: field(multi, "Progressive:")?,
        rep_neither: field(rep, "Neither:")?,
        rep_top: field(rep, "Top:")?,
        rep_bottom: field(rep, "Bottom:")?,
    })
}

/// Classify from idet counts. `fps` is the stream rate; `flagged_progressive`
/// raises the bar for calling a progressive-flagged stream interlaced.
pub fn classify(c: &IdetCounts, fps: f64, flagged_progressive: bool) -> Option<ScanType> {
    let determined = c.tff + c.bff + c.progressive;
    let rep_total = c.rep_neither + c.rep_top + c.rep_bottom;
    if determined < 30 && rep_total < 30 {
        return None;
    }
    let tff = c.tff >= c.bff;
    let ntsc = (29.9..=30.1).contains(&fps);
    // 3:2 pulldown repeats a field in 2 of every 5 frames (40 %).
    if ntsc && rep_total > 0 && (c.rep_top + c.rep_bottom) as f64 / rep_total as f64 >= 0.15 {
        return Some(ScanType::Telecine { tff });
    }
    let ratio = if determined == 0 {
        0.0
    } else {
        (c.tff + c.bff) as f64 / determined as f64
    };
    let threshold = if flagged_progressive { 0.5 } else { 0.3 };
    Some(if ratio >= threshold {
        ScanType::Interlaced { tff }
    } else {
        ScanType::Progressive
    })
}

/// Fallback when detection isn't available: ffprobe's `field_order`.
pub fn from_field_order(field_order: Option<&str>) -> ScanType {
    match field_order {
        Some("tt" | "tb") => ScanType::Interlaced { tff: true },
        Some("bb" | "bt") => ScanType::Interlaced { tff: false },
        _ => ScanType::Progressive,
    }
}

/// The plan for an encode.
pub fn plan(
    detected: Option<ScanType>,
    field_order: Option<&str>,
    choice: ScanOverride,
    double_rate: bool,
) -> Scan {
    let seen = detected.unwrap_or_else(|| from_field_order(field_order));
    let tff = match seen {
        ScanType::Interlaced { tff } | ScanType::Telecine { tff } => tff,
        ScanType::Progressive => {
            from_field_order(field_order) != ScanType::Interlaced { tff: false }
        }
    };
    let kind = match choice {
        ScanOverride::Auto => seen,
        ScanOverride::Progressive => ScanType::Progressive,
        ScanOverride::Deinterlace => ScanType::Interlaced { tff },
        ScanOverride::InverseTelecine => ScanType::Telecine { tff },
    };
    match kind {
        ScanType::Progressive => Scan::Progressive,
        ScanType::Interlaced { tff } => Scan::Deinterlace { tff, double_rate },
        ScanType::Telecine { tff } => Scan::Telecine { tff },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real idet output from the clips made by dev/make-media.sh.
    const INTERLACED: &str = "[Parsed_idet_0 @ 0x1] Repeated Fields: Neither:     0 Top:     0 Bottom:     0\n\
        [Parsed_idet_0 @ 0x1] Single frame detection: TFF:     0 BFF:     0 Progressive:     0 Undetermined:     0\n\
        [Parsed_idet_0 @ 0x1] Multi frame detection: TFF:     0 BFF:     0 Progressive:     0 Undetermined:     0\n\
        [Parsed_idet_0 @ 0x1] Repeated Fields: Neither:   300 Top:     0 Bottom:     0\n\
        [Parsed_idet_0 @ 0x1] Single frame detection: TFF:   300 BFF:     0 Progressive:     0 Undetermined:     0\n\
        [Parsed_idet_0 @ 0x1] Multi frame detection: TFF:   300 BFF:     0 Progressive:     0 Undetermined:     0";
    const TELECINE: &str = "Repeated Fields: Neither:   216 Top:    72 Bottom:    72\n\
        Multi frame detection: TFF:   356 BFF:     0 Progressive:     4 Undetermined:     0";
    const PROGRESSIVE: &str = "Repeated Fields: Neither:   300 Top:     0 Bottom:     0\n\
        Multi frame detection: TFF:     0 BFF:     0 Progressive:   300 Undetermined:     0";

    #[test]
    fn parses_last_idet_summary() {
        let c = parse_idet(INTERLACED).unwrap();
        assert_eq!(
            (c.tff, c.bff, c.progressive, c.rep_neither),
            (300, 0, 0, 300)
        );
        assert!(parse_idet("nothing").is_none());
    }

    #[test]
    fn classifies_real_clips() {
        let i = parse_idet(INTERLACED).unwrap();
        assert_eq!(
            classify(&i, 25.0, false),
            Some(ScanType::Interlaced { tff: true })
        );
        let t = parse_idet(TELECINE).unwrap();
        assert_eq!(
            classify(&t, 29.97, false),
            Some(ScanType::Telecine { tff: true })
        );
        // Repeated fields at a PAL rate are not 3:2 pulldown.
        assert_eq!(
            classify(&t, 25.0, false),
            Some(ScanType::Interlaced { tff: true })
        );
        let p = parse_idet(PROGRESSIVE).unwrap();
        // Flagged interlaced, but the picture is progressive: leave it alone.
        assert_eq!(classify(&p, 25.0, false), Some(ScanType::Progressive));
    }

    #[test]
    fn progressive_flag_raises_the_bar() {
        let c = IdetCounts {
            tff: 40,
            bff: 0,
            progressive: 60,
            rep_neither: 100,
            ..Default::default()
        };
        assert_eq!(
            classify(&c, 25.0, false),
            Some(ScanType::Interlaced { tff: true })
        );
        assert_eq!(classify(&c, 25.0, true), Some(ScanType::Progressive));
        let bff = IdetCounts {
            tff: 5,
            bff: 90,
            progressive: 5,
            rep_neither: 100,
            ..Default::default()
        };
        assert_eq!(
            classify(&bff, 29.97, false),
            Some(ScanType::Interlaced { tff: false })
        );
        assert_eq!(
            classify(
                &IdetCounts {
                    tff: 3,
                    ..Default::default()
                },
                25.0,
                false
            ),
            None
        );
    }

    #[test]
    fn plans_follow_detection_then_overrides() {
        let int = Some(ScanType::Interlaced { tff: false });
        assert_eq!(
            plan(int, None, ScanOverride::Auto, true),
            Scan::Deinterlace {
                tff: false,
                double_rate: true
            }
        );
        assert_eq!(
            plan(int, None, ScanOverride::Progressive, true),
            Scan::Progressive
        );
        assert_eq!(
            plan(int, None, ScanOverride::InverseTelecine, true),
            Scan::Telecine { tff: false }
        );
        // Not analysed yet: fall back to the flags.
        assert_eq!(
            plan(None, Some("tt"), ScanOverride::Auto, false),
            Scan::Deinterlace {
                tff: true,
                double_rate: false
            }
        );
        assert_eq!(
            plan(None, Some("progressive"), ScanOverride::Auto, true),
            Scan::Progressive
        );
        // Detection wins over wrong flags.
        assert_eq!(
            plan(
                Some(ScanType::Progressive),
                Some("tt"),
                ScanOverride::Auto,
                true
            ),
            Scan::Progressive
        );
        assert_eq!(
            plan(
                Some(ScanType::Progressive),
                Some("bb"),
                ScanOverride::Deinterlace,
                true
            ),
            Scan::Deinterlace {
                tff: false,
                double_rate: true
            }
        );
    }

    #[test]
    fn filters_and_rates() {
        let d = Scan::Deinterlace {
            tff: true,
            double_rate: true,
        };
        assert_eq!(
            d.cpu_filter().unwrap(),
            "bwdif=mode=send_field:parity=tff:deint=all"
        );
        assert_eq!(d.rate_factor(), 2.0);
        assert!(d.needs_cpu() && !Scan::Progressive.needs_cpu());
        let t = Scan::Telecine { tff: false };
        assert_eq!(
            t.cpu_filter().unwrap(),
            "fieldmatch=order=bff:combmatch=full,yadif=deint=interlaced,decimate"
        );
        assert!(t.needs_cpu());
        assert_eq!(
            t.describe(29.97),
            "Restoring the original film frames (23.976 fps)"
        );
        assert_eq!(d.describe(25.0), "Deinterlacing to 50p");
        assert!(Scan::Progressive.cpu_filter().is_none());
    }
}
