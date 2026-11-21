//! Damaged and incomplete files (an unfinished download, a bad copy): one
//! ffprobe pass over every video packet, without decoding, finds where the
//! data really ends and where the demuxer had to skip over garbage.
//!
//! Missing torrent pieces read back as zeros, which breaks the container
//! structure, so the demuxer reports them ("0x00 at pos … invalid") and the
//! timestamps jump. Damage inside a frame that leaves the container intact
//! only shows up when decoding, and isn't looked for here.

use std::ffi::OsString;
use std::path::Path;

use crate::ffmpeg::runner::{self, Cancel};

/// What the check found. Only kept when something is wrong (`any()`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Damage {
    /// Length the container header claims.
    pub header_duration: f64,
    /// Where the video data actually ends (from the first packet).
    pub data_end: f64,
    /// Damaged spots before the end (seconds from the start), merged.
    pub holes: Vec<f64>,
    /// Read errors ffprobe reported.
    pub errors: usize,
    pub first_error: Option<String>,
}

/// Holes closer together than this count as one.
const MERGE_SECS: f64 = 5.0;
/// A jump in timestamps this long marks skipped data (when there were errors).
const GAP_SECS: f64 = 1.0;

impl Damage {
    /// The end is missing: the data stops well before the header's length.
    pub fn truncated(&self) -> bool {
        self.missing_secs() > (self.header_duration * 0.002).max(2.0)
    }

    pub fn missing_secs(&self) -> f64 {
        (self.header_duration - self.data_end).max(0.0)
    }

    /// Damage before the end (a located hole, or errors that aren't just the
    /// cut-off end).
    pub fn mid_damage(&self) -> bool {
        !self.holes.is_empty() || (self.errors > 0 && !self.truncated())
    }

    pub fn any(&self) -> bool {
        self.truncated() || self.mid_damage()
    }

    /// Short label for the job row badge.
    pub fn badge(&self) -> &'static str {
        if self.mid_damage() {
            "Damaged"
        } else {
            "Incomplete"
        }
    }

    /// "13:40, 22:10 and 2 more", or `None` when nothing could be located.
    pub fn where_text(&self) -> Option<String> {
        let shown: Vec<String> = self.holes.iter().take(3).map(|t| clock(*t)).collect();
        let rest = self.holes.len().saturating_sub(shown.len());
        match (shown.len(), rest) {
            (0, _) => None,
            (1, _) => Some(shown[0].clone()),
            (_, 0) => {
                let (last, first) = shown.split_last().expect("two or more");
                Some(format!("{} and {last}", first.join(", ")))
            }
            _ => Some(format!("{} and {rest} more", shown.join(", "))),
        }
    }

    /// One sentence on what's wrong, without advice.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.mid_damage() {
            parts.push(match self.where_text() {
                Some(w) => format!("The file is damaged around {w}"),
                None => format!(
                    "The file is damaged ({} read error{})",
                    self.errors,
                    if self.errors == 1 { "" } else { "s" }
                ),
            });
        }
        if self.truncated() {
            let s = format!(
                "the data ends at {} of {}, so the last {} is missing",
                clock(self.data_end),
                clock(self.header_duration),
                clock(self.missing_secs())
            );
            parts.push(if parts.is_empty() {
                format!("The file is incomplete: {s}")
            } else {
                s
            });
        }
        format!("{}.", parts.join(", and "))
    }
}

/// `h:mm:ss` or `m:ss`.
pub fn clock(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// ffprobe listing every packet of the video stream: pts, dts, duration, byte position.
pub fn check_args(path: &Path, video_index: usize) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["-v", "error", "-select_streams"]
        .iter()
        .map(OsString::from)
        .collect();
    a.push(video_index.to_string().into());
    a.extend(
        [
            "-show_entries",
            "packet=pts_time,dts_time,duration_time,pos",
            "-of",
            "csv=p=0",
        ]
        .iter()
        .map(OsString::from),
    );
    a.push(path.as_os_str().to_owned());
    a
}

struct Packet {
    time: f64,
    end: f64,
    pos: Option<u64>,
}

fn field(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.trim().parse().ok())
}

fn packets(csv: &str) -> Vec<Packet> {
    csv.lines()
        .filter_map(|line| {
            let mut f = line.split(',');
            let (pts, dts, dur, pos) = (f.next(), f.next(), f.next(), f.next());
            let time = field(pts).or(field(dts))?;
            Some(Packet {
                time,
                end: time + field(dur).unwrap_or(0.0),
                pos: pos.and_then(|p| p.trim().parse().ok()),
            })
        })
        .collect()
}

/// Byte offset in an error line: "at pos 10495187" or "offset 0xbb4abc".
fn error_pos(line: &str) -> Option<u64> {
    if let Some(i) = line.find("at pos ") {
        let n: String = line[i + 7..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        return n.parse().ok();
    }
    let i = line.find("offset 0x")?;
    let n: String = line[i + 9..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    u64::from_str_radix(&n, 16).ok()
}

/// Turn the packet listing and ffprobe's errors into a verdict. `None` when
/// the file looks whole.
pub fn analyse(csv: &str, stderr: &str, header_duration: f64) -> Option<Damage> {
    let pk = packets(csv);
    let start = pk.iter().map(|p| p.time).fold(f64::INFINITY, f64::min);
    if pk.is_empty() || !start.is_finite() {
        return None;
    }
    let errors: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let data_end = pk.iter().map(|p| p.end).fold(0.0, f64::max) - start;
    let mut holes = Vec::new();
    if !errors.is_empty() {
        // Timestamps that jump forward: the demuxer skipped data there.
        // B-frames reorder pts a little, so compare with the highest so far.
        let mut high = f64::NEG_INFINITY;
        for p in &pk {
            if high.is_finite() && p.time - high > GAP_SECS {
                holes.push(high - start);
            }
            high = high.max(p.time);
        }
        // Errors with a byte position: the last packet stored before it.
        let located: Vec<(u64, f64)> = pk
            .iter()
            .filter_map(|p| p.pos.map(|b| (b, p.time)))
            .collect();
        for pos in errors.iter().filter_map(|l| error_pos(l)) {
            let i = located.partition_point(|(b, _)| *b <= pos);
            if i > 0 {
                holes.push(located[i - 1].1 - start);
            }
        }
    }
    // Damage at the very end is the missing tail, not a hole.
    holes.retain(|t| *t < data_end - 2.0);
    holes.sort_by(f64::total_cmp);
    let mut merged: Vec<f64> = Vec::new();
    for t in holes {
        if merged.last().is_none_or(|m| t - m > MERGE_SECS) {
            merged.push(t);
        }
    }
    let d = Damage {
        header_duration,
        data_end,
        holes: merged,
        errors: errors.len(),
        first_error: errors.first().map(|l| runner::error_summary(l)),
    };
    d.any().then_some(d)
}

/// Read the whole file (container only, no decoding; seconds for a local
/// file, longer over a network).
pub fn check(
    ffprobe: &str,
    path: &Path,
    video_index: usize,
    header_duration: f64,
    cancel: &Cancel,
) -> Result<Option<Damage>, String> {
    let r = runner::run_capture(ffprobe, &check_args(path, video_index), cancel)?;
    if r.cancelled {
        return Err("Cancelled".into());
    }
    // ffprobe exits 0 on a cut-off file; a failure here is something else.
    if !r.success && r.stdout.trim().is_empty() {
        return Err(runner::error_summary(&r.stderr));
    }
    Ok(analyse(&r.stdout, &r.stderr, header_duration))
}

/// A friendlier message for files ffprobe can't open at all because they're
/// incomplete.
pub fn explain_probe_error(stderr: &str) -> Option<&'static str> {
    if stderr.contains("moov atom not found") {
        return Some(
            "This MP4 is incomplete: its index (the moov atom) is written at the end of the file, and the end is missing, so nothing can read it. Finish the download if you can. A repair tool such as untrunc can rebuild the index using a complete file from the same source.",
        );
    }
    if stderr.contains("EBML header parsing failed") {
        return Some(
            "This MKV is damaged at the start (its header is unreadable), so nothing can read it. Finish or redo the download.",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 24 fps packets from `from` to `to` seconds, ~28 KB each.
    fn run_of(from: f64, to: f64, pos0: u64) -> String {
        let mut s = String::new();
        let mut t = from;
        let mut pos = pos0;
        while t < to - 1e-9 {
            s.push_str(&format!("{t:.3},{:.3},0.042,{pos}\n", t - 0.042));
            t += 1.0 / 24.0;
            pos += 28_000;
        }
        s
    }

    #[test]
    fn whole_file_is_clean() {
        assert_eq!(analyse(&run_of(0.0, 30.0, 0), "", 30.02), None);
    }

    #[test]
    fn cut_off_end_is_incomplete() {
        let csv = run_of(0.0, 20.9, 0);
        let err = "[matroska,webm @ 0x55] File ended prematurely\n";
        let d = analyse(&csv, err, 30.021).unwrap();
        assert!(d.truncated() && !d.mid_damage());
        assert!((d.data_end - 20.93).abs() < 0.05, "{}", d.data_end);
        assert_eq!(d.badge(), "Incomplete");
        assert_eq!(
            d.describe(),
            "The file is incomplete: the data ends at 0:21 of 0:30, so the last 0:09 is missing."
        );
        // Cut off without a single error (a clean cluster boundary) still counts.
        assert!(analyse(&csv, "", 30.021).unwrap().truncated());
    }

    #[test]
    fn holes_are_located_from_gaps_and_byte_positions() {
        // Zeros at ~13.7 s (skips to 15.3 s) and at ~22.2 s (skips to 25.75 s).
        let csv = run_of(0.0, 13.7, 0)
            + &run_of(15.333, 22.2, 10_000_000)
            + &run_of(25.75, 30.0, 17_500_000);
        let err = "[matroska,webm @ 0x1] 0x00 at pos 10495187 (0xa024d3) invalid as first byte of an EBML number\n\
                   [matroska,webm @ 0x1] 0x00 at pos 17043867 (0x104119b) invalid as first byte of an EBML number\n";
        let d = analyse(&csv, err, 30.021).unwrap();
        assert!(!d.truncated());
        assert_eq!(d.holes.len(), 2, "{:?}", d.holes);
        assert!((d.holes[0] - 13.67).abs() < 0.1 && (d.holes[1] - 22.17).abs() < 0.1);
        assert_eq!(d.where_text().unwrap(), "0:14 and 0:22");
        assert_eq!(d.badge(), "Damaged");
    }

    #[test]
    fn gaps_without_errors_are_not_damage() {
        // A timestamp jump on its own (e.g. variable frame rate) isn't proof.
        let csv = run_of(0.0, 10.0, 0) + &run_of(15.0, 30.0, 300_000);
        assert_eq!(analyse(&csv, "", 30.0), None);
    }

    #[test]
    fn errors_in_the_missing_tail_are_not_holes() {
        // Zeros over the last seconds: the error sits after the last packet.
        let csv = run_of(0.0, 23.3, 0);
        let err = "[matroska,webm @ 0x1] 0x00 at pos 99999999 (0x5f5e0ff) invalid as first byte of an EBML number\n";
        let d = analyse(&csv, err, 30.0).unwrap();
        assert!(d.truncated() && d.holes.is_empty() && !d.mid_damage());
    }

    #[test]
    fn nearby_holes_merge_and_long_lists_are_shortened() {
        let mut csv = String::new();
        let mut errs = String::new();
        let mut pos = 0;
        for k in 0..6 {
            let t0 = k as f64 * 100.0;
            csv += &run_of(t0, t0 + 50.0, pos);
            pos += 50 * 24 * 28_000 + 1_000_000;
            // Two errors 3 s apart in each run: one hole.
            errs += &format!("x at pos {}\n", pos - 1_000_000 - 4 * 24 * 28_000);
            errs += &format!("x at pos {}\n", pos - 1_000_000 - 24 * 28_000);
        }
        let d = analyse(&csv, &errs, 550.0).unwrap();
        // Gaps (50 → 100 s) and the errors near 46/49 s fall in one 5 s window.
        assert_eq!(d.holes.len(), 6, "{:?}", d.holes);
        assert_eq!(d.where_text().unwrap(), "0:46, 2:26, 4:06 and 3 more");
    }

    #[test]
    fn unlocated_errors_still_count_as_damage() {
        let d = analyse(
            &run_of(0.0, 30.0, 0),
            "[h264 @ 0x1] Invalid NAL unit size\n",
            30.0,
        )
        .unwrap();
        assert!(d.mid_damage() && d.where_text().is_none());
        assert_eq!(d.describe(), "The file is damaged (1 read error).");
    }

    #[test]
    fn start_offset_is_ignored() {
        // MPEG-TS often starts at 1.4 s or later.
        let csv = run_of(600.0, 630.0, 0);
        assert_eq!(analyse(&csv, "", 30.0), None);
        let d = analyse(&run_of(600.0, 620.0, 0), "", 30.0).unwrap();
        assert!((d.data_end - 20.0).abs() < 0.05);
    }

    #[test]
    fn error_positions_parse() {
        assert_eq!(
            error_pos("0x00 at pos 10495187 (0xa024d3) invalid"),
            Some(10495187)
        );
        assert_eq!(
            error_pos("stream 0, offset 0xbb4abc: partial file"),
            Some(0xbb4abc)
        );
        assert_eq!(error_pos("File ended prematurely"), None);
    }

    #[test]
    fn probe_errors_get_explained() {
        assert!(explain_probe_error("[mov] moov atom not found\nx: Invalid data").is_some());
        assert!(explain_probe_error("No such file or directory").is_none());
    }

    #[test]
    fn clock_format() {
        assert_eq!(clock(59.6), "1:00");
        assert_eq!(clock(4330.0), "1:12:10");
    }
}
