//! Parser for `-progress pipe:1` key=value blocks.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub out_time_us: Option<i64>,
    pub speed: Option<f64>,
    pub fps: Option<f64>,
    pub total_size: Option<u64>,
    pub finished: bool,
}

impl Progress {
    /// Fraction done (0..1) against the probed duration.
    pub fn fraction(&self, duration: f64) -> f64 {
        match self.out_time_us {
            Some(us) if duration > 0.0 => (us as f64 / 1e6 / duration).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    /// Seconds remaining, from the remaining media time and the encode speed.
    pub fn eta(&self, duration: f64) -> Option<f64> {
        let speed = self.speed.filter(|s| *s > 0.0)?;
        let done = self.out_time_us? as f64 / 1e6;
        Some(((duration - done).max(0.0)) / speed)
    }
}

/// Accumulates lines; returns a snapshot at the end of each block (`progress=…`).
#[derive(Default)]
pub struct ProgressParser {
    current: Progress,
}

impl ProgressParser {
    pub fn feed(&mut self, line: &str) -> Option<Progress> {
        let (k, v) = line.trim().split_once('=')?;
        let v = v.trim();
        match k {
            // `out_time_ms` is also in microseconds (a long-standing ffmpeg quirk).
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = v.parse::<i64>()
                    && us >= 0
                {
                    self.current.out_time_us = Some(us);
                }
            }
            "speed" => self.current.speed = v.trim_end_matches('x').trim().parse().ok(),
            "fps" => self.current.fps = v.parse().ok(),
            "total_size" => self.current.total_size = v.parse().ok(),
            "progress" => {
                self.current.finished = v == "end";
                return Some(self.current.clone());
            }
            _ => {}
        }
        None
    }
}

pub fn format_eta(secs: f64) -> String {
    let s = secs.round() as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {:02} s", s / 60, s % 60),
        _ => format!("{} h {:02} min", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blocks() {
        let mut p = ProgressParser::default();
        let block = "frame=240\nfps=96.5\nbitrate=4000.0kbits/s\ntotal_size=5000000\nout_time_us=10000000\n\
                     out_time_ms=10000000\nout_time=00:00:10.000000\ndup_frames=0\nspeed=4.02x\nprogress=continue\n";
        let mut snap = None;
        for l in block.lines() {
            if let Some(s) = p.feed(l) {
                snap = Some(s);
            }
        }
        let s = snap.unwrap();
        assert_eq!(s.out_time_us, Some(10_000_000));
        assert_eq!(s.speed, Some(4.02));
        assert_eq!(s.fps, Some(96.5));
        assert_eq!(s.total_size, Some(5_000_000));
        assert!(!s.finished);
        assert!((s.fraction(100.0) - 0.1).abs() < 1e-9);
        assert!((s.eta(100.0).unwrap() - 90.0 / 4.02).abs() < 1e-9);
    }

    #[test]
    fn handles_na_and_end() {
        let mut p = ProgressParser::default();
        assert!(p.feed("out_time_us=N/A").is_none());
        assert!(p.feed("speed=N/A").is_none());
        let s = p.feed("progress=end").unwrap();
        assert!(s.finished && s.out_time_us.is_none() && s.speed.is_none());
        assert_eq!(s.eta(10.0), None);
        assert_eq!(s.fraction(10.0), 0.0);
        assert!(p.feed("garbage").is_none());
    }

    #[test]
    fn negative_time_ignored_and_fraction_clamped() {
        let mut p = ProgressParser::default();
        p.feed("out_time_us=-5000");
        p.feed("out_time_us=200000000");
        let s = p.feed("progress=continue").unwrap();
        assert_eq!(s.fraction(100.0), 1.0);
    }

    #[test]
    fn eta_format() {
        assert_eq!(format_eta(42.4), "42 s");
        assert_eq!(format_eta(125.0), "2 min 05 s");
        assert_eq!(format_eta(3725.0), "1 h 02 min");
    }
}
