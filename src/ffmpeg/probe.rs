//! ffprobe: run it and turn its JSON into `ProbeInfo`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::hdr::{self, HdrInfo};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disposition {
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
    pub comment: bool,
    pub visual_impaired: bool,
    pub descriptions: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoStream {
    pub index: usize,
    pub codec: String,
    pub profile: String,
    pub width: u32,
    pub height: u32,
    pub pix_fmt: String,
    pub bit_depth: u32,
    pub fps: f64,
    /// Frame rate as ffprobe gave it, e.g. "24000/1001".
    pub fps_rational: String,
    pub bit_rate: Option<u64>,
    pub size_bytes: Option<u64>,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub color_space: Option<String>,
    pub color_range: Option<String>,
    pub frame_count: Option<u64>,
    /// ffprobe `field_order` (`progressive`, `tt`, `bb`, `tb`, `bt`); flags only, not proof.
    pub field_order: Option<String>,
    /// Per-stream statistics tags (BPS, NUMBER_OF_BYTES…) that go stale on re-encode.
    pub stat_tags: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioStream {
    /// Absolute stream index in the input (`0:N`).
    pub index: usize,
    pub codec: String,
    pub profile: String,
    pub channels: u32,
    pub layout: String,
    pub bit_rate: Option<u64>,
    pub size_bytes: Option<u64>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub disposition: Disposition,
    pub stat_tags: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubtitleStream {
    pub index: usize,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub disposition: Disposition,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeInfo {
    pub path: PathBuf,
    pub duration: f64,
    pub size_bytes: u64,
    pub format_name: String,
    pub bit_rate: Option<u64>,
    pub video: Option<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    pub attachments: usize,
    pub hdr: HdrInfo,
}

impl ProbeInfo {
    pub fn audio_by_index(&self, index: usize) -> Option<&AudioStream> {
        self.audio.iter().find(|a| a.index == index)
    }

    pub fn subtitle_by_index(&self, index: usize) -> Option<&SubtitleStream> {
        self.subtitles.iter().find(|s| s.index == index)
    }

    /// Video bitrate: from the stream, or the container total minus audio/subs.
    pub fn video_bit_rate(&self) -> Option<u64> {
        let v = self.video.as_ref()?;
        if let Some(b) = v.bit_rate {
            return Some(b);
        }
        if let Some(bytes) = v.size_bytes
            && self.duration > 0.0
        {
            return Some((bytes as f64 * 8.0 / self.duration) as u64);
        }
        let total = self.bit_rate.or_else(|| {
            (self.duration > 0.0).then(|| (self.size_bytes as f64 * 8.0 / self.duration) as u64)
        })?;
        let audio: u64 = self
            .audio
            .iter()
            .map(|a| audio_bitrate_guess(a, self.duration))
            .sum();
        let subs: u64 = self
            .subtitles
            .iter()
            .map(|s| subtitle_bitrate_guess(s, self.duration))
            .sum();
        Some(total.saturating_sub(audio + subs))
    }
}

/// Best-known bitrate (bit/s) of a subtitle track, guessing from the codec if unknown.
pub fn subtitle_bitrate_guess(s: &SubtitleStream, duration: f64) -> u64 {
    if let Some(bytes) = s.size_bytes
        && duration > 0.0
    {
        return (bytes as f64 * 8.0 / duration) as u64;
    }
    if crate::tracks::is_text_subtitle(&s.codec) {
        100
    } else {
        30_000
    }
}

/// Best-known bitrate (bit/s) of an audio track, guessing from the codec if unknown.
pub fn audio_bitrate_guess(a: &AudioStream, duration: f64) -> u64 {
    if let Some(b) = a.bit_rate {
        return b;
    }
    if let Some(bytes) = a.size_bytes
        && duration > 0.0
    {
        return (bytes as f64 * 8.0 / duration) as u64;
    }
    let ch = a.channels.max(1) as u64;
    match a.codec.as_str() {
        "truehd" | "mlp" => 500_000 * ch,
        "dts" if a.profile.contains("MA") => 400_000 * ch,
        "flac" | "alac" => 350_000 * ch,
        c if c.starts_with("pcm_") => 48_000 * 24 * ch,
        "dts" => 1_509_000,
        "eac3" | "ac3" => 640_000,
        _ => 96_000 * ch,
    }
}

pub fn run(ffprobe: &str, path: &Path) -> Result<ProbeInfo, String> {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("Could not run ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let mut info = parse(&String::from_utf8_lossy(&out.stdout))?;
    info.path = path.to_path_buf();
    if let Some(v) = &info.video {
        // HDR10 mastering data and HDR10+ live in frame side data.
        let frame = Command::new(ffprobe)
            .args(["-v", "error", "-print_format", "json", "-select_streams"])
            // A bare number is a valid stream specifier: the absolute index.
            .arg(v.index.to_string())
            .args([
                "-show_frames",
                "-read_intervals",
                "%+#1",
                "-show_entries",
                "frame=side_data_list",
            ])
            .arg(path)
            .output();
        if let Ok(f) = frame
            && f.status.success()
        {
            hdr::merge_frame_side_data(&mut info.hdr, &String::from_utf8_lossy(&f.stdout));
        }
    }
    Ok(info)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn num<T: std::str::FromStr>(v: &Value, key: &str) -> Option<T> {
    match v.get(key)? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// Tag lookup, case-insensitive, also matching mkvmerge's `NAME-eng` variants.
/// Keys of mkvmerge/ffmpeg statistics tags on a stream (exact spelling, e.g. `BPS-eng`).
fn stat_tags(v: &Value) -> Vec<String> {
    const STATS: [&str; 8] = [
        "BPS",
        "NUMBER_OF_BYTES",
        "NUMBER_OF_FRAMES",
        "DURATION",
        "ENCODER",
        "_STATISTICS_TAGS",
        "_STATISTICS_WRITING_APP",
        "_STATISTICS_WRITING_DATE_UTC",
    ];
    v.get("tags")
        .and_then(Value::as_object)
        .map(|t| {
            t.keys()
                .filter(|k| {
                    let base = k.split('-').next().unwrap_or(k);
                    STATS.iter().any(|s| s.eq_ignore_ascii_case(base))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn tag(v: &Value, name: &str) -> Option<String> {
    let tags = v.get("tags")?.as_object()?;
    tags.iter()
        .find(|(k, _)| {
            k.eq_ignore_ascii_case(name)
                || k.len() > name.len()
                    && k[..name.len()].eq_ignore_ascii_case(name)
                    && k.as_bytes()[name.len()] == b'-'
        })
        .and_then(|(_, v)| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
}

fn disposition(v: &Value) -> Disposition {
    let d = |k: &str| {
        v.get("disposition")
            .and_then(|d| d.get(k))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            != 0
    };
    Disposition {
        default: d("default"),
        forced: d("forced"),
        hearing_impaired: d("hearing_impaired") || d("captions"),
        comment: d("comment"),
        visual_impaired: d("visual_impaired"),
        descriptions: d("descriptions"),
    }
}

fn rate(v: &str) -> Option<f64> {
    let (n, d) = v.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

fn parse_duration_tag(s: &str) -> Option<f64> {
    let mut parts = s.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

fn bit_depth(pix_fmt: &str, raw: Option<u32>) -> u32 {
    if let Some(b) = raw.filter(|b| *b > 0) {
        return b;
    }
    if pix_fmt.contains("12") {
        12
    } else if pix_fmt.contains("10") || pix_fmt == "p010le" {
        10
    } else {
        8
    }
}

/// mkvmerge statistics tags (BPS, NUMBER_OF_BYTES, DURATION…) are copied
/// unchanged by tools that cut or remux, so they can describe some other,
/// longer file. A stream's tags are trusted only if they fit this file: its
/// DURATION tag is no longer than the file (a cut is always shorter; subtitles
/// may legitimately end early), and its bytes or BPS × duration fit the size.
fn stats_tags_fit(st: &Value, duration: f64, file_size: u64) -> bool {
    let size_limit = file_size as f64 * 1.02;
    if duration > 0.0
        && let Some(d) = tag(st, "DURATION").and_then(|d| parse_duration_tag(&d))
        && d > duration * 1.01 + 1.0
    {
        return false;
    }
    if file_size > 0
        && let Some(b) = tag(st, "NUMBER_OF_BYTES").and_then(|b| b.parse::<f64>().ok())
        && b > size_limit
    {
        return false;
    }
    if file_size > 0
        && duration > 0.0
        && let Some(bps) = tag(st, "BPS").and_then(|b| b.parse::<f64>().ok())
        && bps * duration / 8.0 > size_limit
    {
        return false;
    }
    true
}

pub fn parse(json: &str) -> Result<ProbeInfo, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| format!("Bad ffprobe output: {e}"))?;
    let format = root.get("format").cloned().unwrap_or(Value::Null);
    let mut info = ProbeInfo {
        duration: num(&format, "duration").unwrap_or(0.0),
        size_bytes: num(&format, "size").unwrap_or(0),
        format_name: s(&format, "format_name").unwrap_or_default(),
        bit_rate: num(&format, "bit_rate"),
        ..Default::default()
    };
    let streams = root
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Duration from the container or the video stream itself, never from tags,
    // so it can be used to check the tags.
    let real_duration = if info.duration > 0.0 {
        info.duration
    } else {
        streams
            .iter()
            .find(|st| st.get("codec_type").and_then(Value::as_str) == Some("video"))
            .and_then(|st| num(st, "duration"))
            .unwrap_or(0.0)
    };
    for st in &streams {
        let index: usize = num(st, "index").unwrap_or(0);
        let codec = s(st, "codec_name").unwrap_or_else(|| "unknown".into());
        let stats_ok = stats_tags_fit(st, real_duration, info.size_bytes);
        let size_bytes = tag(st, "NUMBER_OF_BYTES")
            .filter(|_| stats_ok)
            .and_then(|b| b.parse().ok());
        let bit_rate = num(st, "bit_rate").or_else(|| {
            tag(st, "BPS")
                .filter(|_| stats_ok)
                .and_then(|b| b.parse().ok())
        });
        match st.get("codec_type").and_then(Value::as_str) {
            Some("video") => {
                let attached = st
                    .get("disposition")
                    .and_then(|d| d.get("attached_pic"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    != 0;
                if attached || info.video.is_some() {
                    continue;
                }
                if info.duration <= 0.0 {
                    info.duration = num(st, "duration")
                        .or_else(|| {
                            tag(st, "DURATION")
                                .filter(|_| stats_ok)
                                .and_then(|d| parse_duration_tag(&d))
                        })
                        .unwrap_or(0.0);
                }
                let pix_fmt = s(st, "pix_fmt").unwrap_or_default();
                let fps_rational = ["avg_frame_rate", "r_frame_rate"]
                    .iter()
                    .filter_map(|k| s(st, k))
                    .find(|r| rate(r).is_some())
                    .unwrap_or_else(|| "24/1".into());
                let fps = rate(&fps_rational).unwrap_or(24.0);
                info.video = Some(VideoStream {
                    index,
                    profile: s(st, "profile").unwrap_or_default(),
                    width: num(st, "width").unwrap_or(0),
                    height: num(st, "height").unwrap_or(0),
                    bit_depth: bit_depth(&pix_fmt, num(st, "bits_per_raw_sample")),
                    pix_fmt,
                    fps,
                    fps_rational,
                    bit_rate,
                    size_bytes,
                    color_primaries: s(st, "color_primaries").filter(|c| c != "unknown"),
                    color_transfer: s(st, "color_transfer").filter(|c| c != "unknown"),
                    color_space: s(st, "color_space").filter(|c| c != "unknown"),
                    color_range: s(st, "color_range").filter(|c| c != "unknown"),
                    stat_tags: stat_tags(st),
                    field_order: s(st, "field_order").filter(|f| f != "unknown"),
                    frame_count: num(st, "nb_frames").or_else(|| {
                        tag(st, "NUMBER_OF_FRAMES")
                            .filter(|_| stats_ok)
                            .and_then(|n| n.parse().ok())
                    }),
                    codec,
                });
                info.hdr = hdr::detect_stream(st);
            }
            Some("audio") => info.audio.push(AudioStream {
                index,
                profile: s(st, "profile").unwrap_or_default(),
                channels: num(st, "channels").unwrap_or(2),
                layout: s(st, "channel_layout").unwrap_or_default(),
                bit_rate,
                size_bytes,
                language: tag(st, "language").filter(|l| l != "und"),
                title: tag(st, "title"),
                disposition: disposition(st),
                stat_tags: stat_tags(st),
                codec,
            }),
            Some("subtitle") => info.subtitles.push(SubtitleStream {
                index,
                language: tag(st, "language").filter(|l| l != "und"),
                title: tag(st, "title"),
                disposition: disposition(st),
                size_bytes,
                codec,
            }),
            Some("attachment") => info.attachments += 1,
            _ => {}
        }
    }
    if info.video.is_none() {
        return Err("No video stream found".into());
    }
    if let Some(v) = &info.video {
        let transfer = v.color_transfer.as_deref().unwrap_or("");
        info.hdr.pq = transfer == "smpte2084";
        info.hdr.hlg = transfer == "arib-std-b67";
        info.hdr.bt2020 = v.color_primaries.as_deref() == Some("bt2020");
    }
    Ok(info)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A representative 4K remux: HEVC Main10 HDR10 + DV 8.1, five audio tracks, four subs.
    pub const SAMPLE_4K: &str = r#"{
      "streams": [
        {"index":0,"codec_name":"hevc","profile":"Main 10","codec_type":"video","width":3840,"height":2160,
         "pix_fmt":"yuv420p10le","color_range":"tv","color_space":"bt2020nc","color_transfer":"smpte2084",
         "color_primaries":"bt2020","avg_frame_rate":"24000/1001","r_frame_rate":"24000/1001",
         "side_data_list":[{"side_data_type":"DOVI configuration record","dv_version_major":1,"dv_version_minor":0,
           "dv_profile":8,"dv_level":6,"rpu_present_flag":1,"el_present_flag":0,"bl_present_flag":1,
           "dv_bl_signal_compatibility_id":1}],
         "disposition":{"default":1,"forced":0},
         "tags":{"BPS-eng":"58000000","NUMBER_OF_BYTES-eng":"52200000000","NUMBER_OF_FRAMES-eng":"172800","DURATION-eng":"02:00:00.000000000"}},
        {"index":1,"codec_name":"truehd","profile":"Dolby TrueHD + Dolby Atmos","codec_type":"audio","channels":8,
         "channel_layout":"7.1","disposition":{"default":1,"comment":0},
         "tags":{"language":"eng","title":"TrueHD Atmos 7.1","BPS-eng":"4500000","NUMBER_OF_BYTES-eng":"4050000000"}},
        {"index":2,"codec_name":"eac3","profile":"Dolby Digital Plus + Dolby Atmos","codec_type":"audio","channels":6,
         "channel_layout":"5.1(side)","bit_rate":"768000","disposition":{"default":0},
         "tags":{"language":"eng","title":"DD+ Atmos"}},
        {"index":3,"codec_name":"ac3","codec_type":"audio","channels":2,"channel_layout":"stereo","bit_rate":"224000",
         "disposition":{"default":0,"comment":1},"tags":{"language":"eng","title":"Commentary with director"}},
        {"index":4,"codec_name":"dts","profile":"DTS","codec_type":"audio","channels":6,"channel_layout":"5.1(side)",
         "bit_rate":"1509000","disposition":{"default":0},"tags":{"language":"nob"}},
        {"index":5,"codec_name":"ac3","codec_type":"audio","channels":6,"channel_layout":"5.1(side)",
         "bit_rate":"448000","disposition":{"default":0},"tags":{"language":"jpn"}},
        {"index":6,"codec_name":"subrip","codec_type":"subtitle","disposition":{"default":0,"forced":0},
         "tags":{"language":"eng","NUMBER_OF_BYTES-eng":"80000"}},
        {"index":7,"codec_name":"hdmv_pgs_subtitle","codec_type":"subtitle","disposition":{"default":0,"forced":0},
         "tags":{"language":"eng","NUMBER_OF_BYTES-eng":"30000000"}},
        {"index":8,"codec_name":"hdmv_pgs_subtitle","codec_type":"subtitle","disposition":{"default":0,"forced":0},
         "tags":{"language":"nor","NUMBER_OF_BYTES-eng":"25000000"}},
        {"index":9,"codec_name":"subrip","codec_type":"subtitle","disposition":{"default":0,"forced":1},
         "tags":{"language":"eng","title":"Forced"}},
        {"index":10,"codec_name":"subrip","codec_type":"subtitle","disposition":{"default":0,"hearing_impaired":1},
         "tags":{"language":"eng","title":"SDH"}}
      ],
      "format":{"format_name":"matroska,webm","duration":"7200.000000","size":"60000000000","bit_rate":"66666666"}
    }"#;

    pub fn sample_4k() -> ProbeInfo {
        parse(SAMPLE_4K).unwrap()
    }

    #[test]
    fn parses_streams_and_tags() {
        let p = sample_4k();
        assert_eq!(p.duration, 7200.0);
        let v = p.video.as_ref().unwrap();
        assert_eq!((v.width, v.height, v.bit_depth), (3840, 2160, 10));
        assert!((v.fps - 23.976).abs() < 0.01);
        assert_eq!(v.bit_rate, Some(58_000_000));
        assert_eq!(v.frame_count, Some(172_800));
        assert_eq!(p.audio.len(), 5);
        assert_eq!(p.subtitles.len(), 5);
        assert_eq!(p.audio[0].size_bytes, Some(4_050_000_000));
        assert_eq!(p.audio[0].bit_rate, Some(4_500_000));
        assert!(p.audio[2].disposition.comment);
        assert_eq!(p.audio[3].language.as_deref(), Some("nob"));
        assert!(p.subtitles[3].disposition.forced);
        assert!(p.subtitles[4].disposition.hearing_impaired);
    }

    #[test]
    fn stale_stats_tags_from_a_cut_are_ignored() {
        // A 90 s clip cut from a film keeps the film's NUMBER_OF_BYTES/BPS.
        let p = parse(
            r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,
            "tags":{"BPS":"10077489","NUMBER_OF_BYTES":"10986674277"}}],
            "format":{"duration":"92.757","size":"129182697","bit_rate":"11141601"}}"#,
        )
        .unwrap();
        let v = p.video.as_ref().unwrap();
        assert_eq!((v.size_bytes, v.bit_rate), (None, None));
        // Falls back to the container bitrate.
        assert_eq!(p.video_bit_rate(), Some(11_141_601));
        // Tags that fit the file are used.
        assert_eq!(sample_4k().video.unwrap().size_bytes, Some(52_200_000_000));
    }

    #[test]
    fn stale_stats_tags_are_checked_per_stream() {
        // Opening logos trimmed from a 2 h film: the video's tags still say
        // 2 h, which fits the size but not the duration. The audio was
        // re-tagged and is kept; the subtitle legitimately ends early.
        let p = parse(
            r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,
            "tags":{"BPS":"8000000","NUMBER_OF_BYTES":"7200000000","NUMBER_OF_FRAMES":"172800","DURATION":"02:00:00.000"}},
            {"index":1,"codec_type":"audio","codec_name":"ac3","channels":6,
            "tags":{"BPS":"640000","NUMBER_OF_BYTES":"568800000","DURATION":"01:58:30.000"}},
            {"index":2,"codec_type":"subtitle","codec_name":"subrip",
            "tags":{"NUMBER_OF_BYTES":"90000","DURATION":"01:50:00.000"}}],
            "format":{"duration":"7110","size":"7300000000","bit_rate":"8213783"}}"#,
        )
        .unwrap();
        let v = p.video.as_ref().unwrap();
        assert_eq!(
            (v.size_bytes, v.bit_rate, v.frame_count),
            (None, None, None)
        );
        assert_eq!(p.audio[0].bit_rate, Some(640_000));
        assert_eq!(p.subtitles[0].size_bytes, Some(90_000));
        // BPS-only stream whose rate can't fit the file.
        let p = parse(
            r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,
            "tags":{"BPS":"10077489"}}],"format":{"duration":"92.757","size":"12918269"}}"#,
        )
        .unwrap();
        assert_eq!(p.video.unwrap().bit_rate, None);
    }

    #[test]
    fn stale_duration_tag_is_not_used_as_the_duration() {
        // No container or stream duration: the film's DURATION tag on a small
        // cut doesn't fit the size, so the duration stays unknown.
        let p = parse(
            r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,
            "tags":{"NUMBER_OF_BYTES":"10986674277","DURATION":"02:00:00.000"}},
            {"index":1,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle","tags":{"NUMBER_OF_BYTES":"80000"}}],
            "format":{"size":"129182697","bit_rate":"11141601"}}"#,
        )
        .unwrap();
        assert_eq!(p.duration, 0.0);
        // Used to overflow dividing the subtitle bytes by a zero duration.
        assert_eq!(p.video_bit_rate(), Some(11_141_601 - 30_000));
    }

    #[test]
    fn und_language_is_none_and_no_video_is_error() {
        let p = parse(r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":10,"height":10},
            {"index":1,"codec_type":"audio","codec_name":"aac","tags":{"language":"und"}}],"format":{}}"#)
        .unwrap();
        assert_eq!(p.audio[0].language, None);
        assert!(parse(r#"{"streams":[],"format":{}}"#).is_err());
    }

    #[test]
    fn video_bitrate_falls_back_to_container_minus_audio() {
        let p = parse(r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080},
            {"index":1,"codec_type":"audio","codec_name":"aac","channels":2,"bit_rate":"128000"}],
            "format":{"duration":"100","size":"125000000","bit_rate":"10000000"}}"#)
        .unwrap();
        assert_eq!(p.video_bit_rate(), Some(9_872_000));
    }
}
