//! Parser for `-progress pipe:1` key=value blocks.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub out_time_us: Option<i64>,
    pub speed: Option<f64>,
    pub fps: Option<f64>,
    pub total_size: Option<u64>,
    /// Video frames written so far (counted for stream copy too).
    pub frame: Option<u64>,
    pub finished: bool,
}

impl Progress {
    /// Fill in media time and speed from the frame count. ffmpeg's
    /// `out_time` is the minimum over all output streams, so a copied
    /// subtitle track holds it at `N/A` until its first cue and then at its
    /// last one; with dozens of subtitle tracks it barely moves. The video
    /// frame count doesn't have that problem. Filters here keep or lower the
    /// frame rate (`bwdif` send_frame, `decimate`), so this never runs ahead.
    pub fn with_source_fps(mut self, fps: Option<f64>) -> Self {
        let (Some(fps), Some(frame)) = (fps.filter(|f| *f > 0.0), self.frame) else {
            return self;
        };
        let us = (frame as f64 / fps * 1e6) as i64;
        if self.out_time_us.is_none_or(|t| t < us) {
            self.out_time_us = Some(us);
        }
        if self.speed.is_none_or(|s| s <= 0.0)
            && let Some(f) = self.fps.filter(|f| *f > 0.0)
        {
            self.speed = Some(f / fps);
        }
        self
    }

    /// Fraction done (0..1) against the probed duration.
    pub fn fraction(&self, duration: f64) -> f64 {
        match self.out_time_us {
            Some(us) if duration > 0.0 => (us as f64 / 1e6 / duration).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }

    /// Seconds remaining, from the remaining media time and the encode speed.
    pub fn eta(&self, duration: f64) -> Option<f64> {
        (duration > 0.0).then_some(())?;
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
            "frame" => self.current.frame = v.parse().ok(),
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
        // A stalled encode (speed 0.001x) gives absurd numbers; cap them.
        360_000.. => "> 99 h".into(),
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
    fn frames_stand_in_when_subtitles_hold_out_time() {
        // Real ffmpeg 8.1 output with a copied subtitle track that hasn't
        // reached its first cue yet.
        let mut p = ProgressParser::default();
        let mut snap = None;
        for l in "frame=1200\nfps=480.0\nout_time_us=N/A\nspeed=N/A\nprogress=continue".lines() {
            snap = p.feed(l).or(snap);
        }
        let s = snap.unwrap().with_source_fps(Some(24.0));
        assert_eq!(s.out_time_us, Some(50_000_000));
        assert_eq!(s.speed, Some(20.0));
        assert!((s.fraction(100.0) - 0.5).abs() < 1e-9);
        // A stuck out_time behind the frames is overridden; one ahead is kept.
        let stuck = Progress {
            frame: Some(240),
            out_time_us: Some(2_000_000),
            ..Default::default()
        };
        assert_eq!(
            stuck.clone().with_source_fps(Some(24.0)).out_time_us,
            Some(10_000_000)
        );
        let ahead = Progress {
            frame: Some(240),
            out_time_us: Some(12_000_000),
            speed: Some(3.0),
            ..Default::default()
        };
        let a = ahead.with_source_fps(Some(24.0));
        assert_eq!((a.out_time_us, a.speed), (Some(12_000_000), Some(3.0)));
        // No frames (audio-only job) or no fps: unchanged.
        let audio = Progress {
            out_time_us: Some(5),
            ..Default::default()
        };
        assert_eq!(audio.clone().with_source_fps(Some(24.0)), audio);
        assert_eq!(stuck.with_source_fps(None).out_time_us, Some(2_000_000));
    }

    #[test]
    fn eta_format() {
        assert_eq!(format_eta(42.4), "42 s");
        assert_eq!(format_eta(125.0), "2 min 05 s");
        assert_eq!(format_eta(3725.0), "1 h 02 min");
    }

    #[test]
    fn no_eta_without_duration_and_capped_display() {
        let p = Progress {
            out_time_us: Some(10_000_000),
            speed: Some(1.0),
            ..Default::default()
        };
        assert_eq!(p.eta(0.0), None);
        assert_eq!(p.eta(20.0), Some(10.0));
        assert_eq!(format_eta(1e7), "> 99 h");
    }
}
