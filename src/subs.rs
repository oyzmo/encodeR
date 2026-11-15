//! External subtitle files: text decoding, SRT parsing and writing, offsets,
//! sidecar file names, the sync reference and the sync report. Pure
//! functions; the file and process work lives in `worker`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ffmpeg::probe::ProbeInfo;
use crate::settings::Container;
use crate::tracks::{self, Selection, SubKind};

// ---- Job data ----

/// Kind of an external subtitle file, set from its name and editable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExtKind {
    #[default]
    Full,
    Sdh,
    Forced,
}

impl ExtKind {
    pub const ALL: [ExtKind; 3] = [Self::Full, Self::Sdh, Self::Forced];

    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::Sdh => "SDH (hearing impaired)",
            Self::Forced => "Forced (foreign parts only)",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Sdh => "SDH",
            Self::Forced => "forced",
        }
    }

    /// `-disposition` value in the output.
    pub fn disposition(self) -> &'static str {
        match self {
            Self::Full => "0",
            Self::Sdh => "hearing_impaired",
            Self::Forced => "forced",
        }
    }

    fn sub_kind(self) -> SubKind {
        match self {
            Self::Full => SubKind::Full,
            Self::Sdh => SubKind::Sdh,
            Self::Forced => SubKind::Forced,
        }
    }
}

/// An SRT file added to a job.
#[derive(Clone, Debug, PartialEq)]
pub struct ExternalSub {
    pub path: PathBuf,
    /// ISO 639-2 code, `None` = unknown.
    pub language: Option<String>,
    pub kind: ExtKind,
    pub selected: bool,
    /// Sync to the video with alass before use.
    pub sync: bool,
    /// Manual shift in milliseconds, applied after syncing.
    pub offset_ms: i64,
    /// Found next to the video rather than added by the user: only these are
    /// thinned out to the preferred languages after the probe.
    pub auto: bool,
    /// File size, read once when the file is added.
    pub bytes: u64,
}

impl ExternalSub {
    /// From a file name: language and kind guessed from `Movie.no.forced.srt`.
    /// Reads the file size (the one bit of I/O here), so later size maths
    /// never touches the disk.
    pub fn from_path(video: &Path, path: PathBuf, sync: bool) -> Self {
        let g = guess_from_name(video, &path);
        let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
        Self {
            path,
            language: g.language,
            kind: g.kind,
            selected: true,
            sync,
            offset_ms: 0,
            auto: false,
            bytes,
        }
    }

    /// Is this file one of the video's own sidecars (`Movie.no.srt` next to
    /// `Movie.mkv`)? Decided from the names alone.
    pub fn is_sidecar_of(&self, video: &Path) -> bool {
        self.path.parent() == video.parent() && belongs_to(video, &self.path)
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn language_name(&self) -> String {
        tracks::language_name(self.language.as_deref())
    }
}

/// Where a "fix tracks only" job puts its result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SubsTarget {
    /// `Movie.no.srt` next to the video; the video isn't touched.
    #[default]
    Sidecar,
    /// Remux into the video file, replacing it once the copy checks out.
    Embed,
    /// Remux into a new file in the output folder; the original stays.
    NewFile,
}

impl SubsTarget {
    pub const ALL: [SubsTarget; 3] = [Self::NewFile, Self::Embed, Self::Sidecar];

    pub fn label(self) -> &'static str {
        match self {
            Self::Sidecar => "Subtitles next to the video",
            Self::Embed => "Replace the original file",
            Self::NewFile => "Write a new file",
        }
    }

    /// Does this target remux the video (so audio timing can change)?
    pub fn remuxes(self) -> bool {
        self != Self::Sidecar
    }
}

/// What a job does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobMode {
    /// Re-encode or remux into a new file (external subtitles are muxed in).
    #[default]
    Encode,
    /// Fix tracks only: add or sync subtitles and shift audio timing. Every
    /// stream is copied; the video is never re-encoded.
    Fix(SubsTarget),
}

impl JobMode {
    pub fn fix_only(self) -> bool {
        matches!(self, Self::Fix(_))
    }

    /// Writes next to or into its own input rather than to the output folder.
    pub fn in_place(self) -> bool {
        matches!(self, Self::Fix(SubsTarget::Sidecar | SubsTarget::Embed))
    }
}

/// Audio delays a fix-tracks job applies: every track with an offset, selected
/// or not, since that mode keeps every track.
pub fn audio_delays(sel: &Selection) -> Vec<(usize, i64)> {
    sel.audio
        .iter()
        .filter(|c| c.offset_ms != 0)
        .map(|c| (c.index, c.offset_ms))
        .collect()
}

/// Why a fix-tracks job can't run, if it can't.
/// What a fix-tracks job does besides subtitle files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FixWork {
    /// An audio track has a delay.
    pub delays: bool,
    /// The embedded cover art changes.
    pub cover: bool,
    /// A new picture goes in as the cover.
    pub new_cover: bool,
    /// A poster is saved next to the video.
    pub poster: bool,
}

impl From<bool> for FixWork {
    /// Audio delays only.
    fn from(delays: bool) -> Self {
        Self {
            delays,
            ..Self::default()
        }
    }
}

impl FixWork {
    /// Something to remux besides subtitles.
    pub fn remux(self) -> bool {
        self.delays || self.cover
    }
}

pub fn fix_problem(
    video: &Path,
    target: SubsTarget,
    externals: &[ExternalSub],
    work: FixWork,
) -> Result<(), String> {
    let subs = externals.iter().any(|e| e.selected);
    match target {
        SubsTarget::Sidecar if work.delays => Err(
            "Audio delays need the file to be remuxed: pick “Replace the original file” or “Write a new file”.".into(),
        ),
        SubsTarget::Sidecar if work.cover => Err(
            "Changing the embedded cover art needs the file to be remuxed: pick “Replace the original file” or “Write a new file”.".into(),
        ),
        SubsTarget::Sidecar if !subs && !work.poster => Err(NOTHING_TO_FIX.into()),
        SubsTarget::Sidecar if !subs => Ok(()),
        SubsTarget::Sidecar => sidecar_outputs(video, externals).map(|_| ()),
        SubsTarget::Embed
            if work.new_cover
                && video
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("mov")) =>
        {
            Err(crate::ffmpeg::command::MOV_COVER.into())
        }
        _ if !subs && !work.remux() && !work.poster => Err(NOTHING_TO_FIX.into()),
        // A poster alone is saved without remuxing.
        _ if !subs && !work.remux() => Ok(()),
        SubsTarget::Embed if !subs && fix_container(video, target).is_err() => Err(format!(
            "A .{} file can't be remuxed in place here. Pick “Write a new file” (it becomes an MKV).",
            video.extension().unwrap_or_default().to_string_lossy()
        )),
        _ => fix_container(video, target).map(|_| ()),
    }
}

pub const NOTHING_TO_FIX: &str = "Nothing to fix: add subtitle files (or put files like Movie.no.srt next to the video), set an audio delay on a track, or change the cover art.";

/// Container for a fix-tracks remux: the input's own, or MKV for a new file
/// from a container that can't hold text subtitles.
pub fn fix_container(video: &Path, target: SubsTarget) -> Result<Container, String> {
    match (embed_container(video), target) {
        (Err(_), SubsTarget::NewFile) => Ok(Container::Mkv),
        (r, _) => r,
    }
}

// ---- Text decoding ----

/// Windows-1252 code points for bytes 0x80–0x9F (the rest equal Latin-1).
const CP1252: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

/// Decode a subtitle file: UTF-8 (with or without BOM), UTF-16 (with a BOM,
/// or recognised by its NUL bytes), otherwise Windows-1252, which is what most
/// older Nordic SRTs are.
pub fn decode_text(bytes: &[u8]) -> String {
    decode_text_noted(bytes).0
}

/// [`decode_text`], plus a report note when the text had to be guessed as
/// Windows-1252: Polish or Russian files in other code pages come out garbled,
/// and the note says why.
pub fn decode_text_noted(bytes: &[u8]) -> (String, Option<&'static str>) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return (String::from_utf8_lossy(rest).into_owned(), None);
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| {
                if le {
                    u16::from_le_bytes(c)
                } else {
                    u16::from_be_bytes(c)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return (utf16(rest, true), None);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return (utf16(rest, false), None);
    }
    // UTF-16 without a BOM: ASCII-heavy text has a NUL in every other byte.
    let head = &bytes[..bytes.len().min(2048)];
    let nuls = |odd: usize| {
        head.iter()
            .skip(odd)
            .step_by(2)
            .filter(|&&b| b == 0)
            .count()
    };
    let half = head.len() / 2;
    if half >= 8 {
        let (even, odd) = (nuls(0), nuls(1));
        if odd * 10 > half * 7 && even * 10 < half {
            return (utf16(bytes, true), None);
        }
        if even * 10 > half * 7 && odd * 10 < half {
            return (utf16(bytes, false), None);
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return (s.to_string(), None);
    }
    let text = bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => char::from_u32(CP1252[(b - 0x80) as usize] as u32).unwrap_or('?'),
            _ => b as char,
        })
        .collect();
    (
        text,
        Some("not UTF-8, read as Windows-1252 (check the letters if it isn't a Western language)"),
    )
}

// ---- SRT ----

/// One subtitle, times in milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cue {
    pub start: i64,
    pub end: i64,
    pub text: String,
}

/// `01:02:03,456`, `01:02:03.456`, `02:03,456` → milliseconds.
fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (hms, frac) = s.split_once([',', '.']).unwrap_or((s, "0"));
    let parts: Vec<&str> = hms.split(':').collect();
    let nums: Option<Vec<i64>> = parts.iter().map(|p| p.trim().parse().ok()).collect();
    let nums = nums?;
    let secs = match nums.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        _ => return None,
    };
    let frac = frac.trim();
    if frac.is_empty() || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // "5" = 500 ms, "05" = 50 ms, "0500" = 500 ms.
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    Some(secs * 1000 + ms)
}

fn format_time(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// Parse SRT text. Tolerates missing or wrong cue numbers, CRLF, `.` as the
/// millisecond separator and position coordinates after the times. Cues come
/// back sorted by start time (ffmpeg wants them in order).
pub fn parse_srt(text: &str) -> Result<Vec<Cue>, String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text.lines().collect();
    let timing = |l: &str| l.contains("-->");
    let number = |l: &str| l.trim().parse::<u64>().is_ok();
    // Does line `i` start the next cue (a timing line, or a number before one)?
    let next_cue = |i: usize| {
        lines.get(i).is_some_and(|l| timing(l))
            || (lines.get(i).is_some_and(|l| number(l))
                && lines.get(i + 1).is_some_and(|l| timing(l)))
    };
    let mut cues = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let Some((a, b)) = line.split_once("-->") else {
            continue;
        };
        let end_field = b.split_whitespace().next().unwrap_or("");
        let (Some(start), Some(end)) = (parse_time(a), parse_time(end_field)) else {
            continue;
        };
        let mut body: Vec<&str> = Vec::new();
        loop {
            while i < lines.len() && !lines[i].trim().is_empty() && !next_cue(i) {
                body.push(lines[i]);
                i += 1;
            }
            // A blank line inside a cue: the text after it still belongs to
            // this cue if another cue follows it (stray text at the very end
            // of a file is credits or junk, and is dropped).
            let mut j = i;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j == i || j >= lines.len() || next_cue(j) || body.is_empty() {
                break;
            }
            let mut k = j;
            while k < lines.len() && !lines[k].trim().is_empty() && !next_cue(k) {
                k += 1;
            }
            let followed = (k..lines.len())
                .find(|&n| !lines[n].trim().is_empty())
                .is_some_and(next_cue);
            if !followed {
                break;
            }
            body.push("");
            i = j;
        }
        let text = body.join("\n").trim_end().to_string();
        if !text.is_empty() {
            cues.push(Cue { start, end, text });
        }
    }
    if cues.is_empty() {
        return Err("No subtitles found (is it an SRT file?)".into());
    }
    cues.sort_by_key(|c| c.start);
    Ok(cues)
}

/// Clean SRT: renumbered, `\n` line endings, UTF-8.
pub fn write_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            format_time(c.start),
            format_time(c.end),
            c.text
        ));
    }
    out
}

/// Shift every cue by `ms`. Cues pushed entirely before 0 are dropped; a cue
/// that straddles 0 starts at 0.
pub fn shift(cues: &[Cue], ms: i64) -> Vec<Cue> {
    cues.iter()
        .filter(|c| c.end + ms > 0)
        .map(|c| Cue {
            start: (c.start + ms).max(0),
            end: c.end + ms,
            text: c.text.clone(),
        })
        .collect()
}

// ---- Sync report ----

/// Framerate pairs that subtitle files are commonly off by.
const FPS_PAIRS: [(f64, &str); 3] = [
    (25.0 / 23.976, "23.976 ↔ 25 fps"),
    (25.0 / 24.0, "24 ↔ 25 fps"),
    (24.0 / 23.976, "23.976 ↔ 24 fps"),
];

fn secs(ms: i64) -> String {
    format!("{:+.2} s", ms as f64 / 1000.0)
}

/// Plain-language summary of what syncing changed.
pub fn describe_sync(before: &[Cue], after: &[Cue]) -> String {
    let (Some(b0), Some(b1), Some(a0), Some(a1)) =
        (before.first(), before.last(), after.first(), after.last())
    else {
        return "no subtitles left after syncing".into();
    };
    let deltas: Vec<i64> = if before.len() == after.len() {
        before
            .iter()
            .zip(after)
            .map(|(b, a)| a.start - b.start)
            .collect()
    } else {
        vec![a0.start - b0.start, a1.start - b1.start]
    };
    let min = *deltas.iter().min().unwrap_or(&0);
    let max = *deltas.iter().max().unwrap_or(&0);
    if min.abs().max(max.abs()) < 40 {
        return "already in sync".into();
    }
    let span_b = (b1.start - b0.start) as f64;
    let span_a = (a1.start - a0.start) as f64;
    if span_b > 60_000.0 && span_a > 0.0 {
        let r = span_a / span_b;
        if let Some((_, label)) = FPS_PAIRS
            .iter()
            .find(|(f, _)| (r - f).abs() < 0.0002 || (1.0 / r - f).abs() < 0.0002)
        {
            return format!(
                "framerate corrected ({label}), {} at the start",
                secs(a0.start - b0.start)
            );
        }
    }
    if max - min < 80 {
        return format!("shifted {}", secs((min + max) / 2));
    }
    format!("adjusted in parts, {} to {}", secs(min), secs(max))
}

// ---- File names ----

/// Language and kind read from a subtitle file name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NameGuess {
    pub language: Option<String>,
    pub kind: ExtKind,
}

/// Native and English language names seen in subtitle file names.
const NAME_WORDS: &[(&str, &str)] = &[
    ("norsk", "nor"),
    ("norwegian", "nor"),
    ("bokmal", "nob"),
    ("bokmål", "nob"),
    ("nynorsk", "nno"),
    ("english", "eng"),
    ("svenska", "swe"),
    ("swedish", "swe"),
    ("dansk", "dan"),
    ("danish", "dan"),
    ("suomi", "fin"),
    ("finnish", "fin"),
    ("deutsch", "deu"),
    ("german", "deu"),
    ("francais", "fra"),
    ("français", "fra"),
    ("french", "fra"),
    ("espanol", "spa"),
    ("español", "spa"),
    ("spanish", "spa"),
    ("italiano", "ita"),
    ("italian", "ita"),
    ("nederlands", "nld"),
    ("dutch", "nld"),
    ("japanese", "jpn"),
    ("polski", "pol"),
    ("polish", "pol"),
    ("portuguese", "por"),
    ("russian", "rus"),
];

fn kind_word(w: &str) -> Option<ExtKind> {
    match w {
        "forced" | "foreign" => Some(ExtKind::Forced),
        "sdh" | "hi" | "cc" | "hoh" => Some(ExtKind::Sdh),
        _ => None,
    }
}

fn language_word(w: &str) -> Option<String> {
    if let Some((_, code)) = NAME_WORDS.iter().find(|(n, _)| *n == w) {
        return Some(code.to_string());
    }
    (w.len() == 2 || w.len() == 3)
        .then(|| tracks::known_language(w))
        .flatten()
}

fn tokens(s: &str) -> Vec<String> {
    s.split(['.', '_', '-', ' ', '[', ']', '(', ')'])
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Extensions of the video files the app takes.
pub const VIDEO_EXTS: [&str; 8] = ["mkv", "mp4", "m4v", "mov", "ts", "m2ts", "avi", "webm"];

pub fn is_video(p: &Path) -> bool {
    p.extension()
        .is_some_and(|e| VIDEO_EXTS.iter().any(|v| e.eq_ignore_ascii_case(v)))
}

/// Is this a subtitle file we take (`.srt`), or an archive that may hold some (`.zip`)?
pub fn is_srt(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("srt"))
}

pub fn is_zip(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Limits for SRT files taken from an archive: a real one is well under
/// 1 MB, and a subtitle pack holds a few dozen at most.
pub struct ZipLimits {
    pub file: u64,
    pub total: u64,
    pub count: usize,
}

pub const ZIP_LIMITS: ZipLimits = ZipLimits {
    file: 16 * 1024 * 1024,
    total: 64 * 1024 * 1024,
    count: 200,
};

/// File name for a zip entry: its base name, with the folder it sits in
/// folded in when that folder names a language or kind the file name lacks
/// (`Norwegian/Movie.srt` → `Movie.Norwegian.srt`). None for entries that
/// aren't SRT files, hidden files and macOS `__MACOSX` junk. Folder parts are
/// otherwise dropped, so nothing can land outside the unpack folder.
pub fn zip_entry_name(full: &str) -> Option<String> {
    let full = full.replace('\\', "/");
    let mut parts: Vec<&str> = full.split('/').filter(|p| !p.is_empty()).collect();
    let name = parts.pop()?;
    if parts.first() == Some(&"__MACOSX") || name.starts_with('.') || !is_srt(Path::new(name)) {
        return None;
    }
    let folder = parts.last().map(|f| tokens(f)).unwrap_or_default();
    let describes = !folder.is_empty()
        && folder
            .iter()
            .all(|w| language_word(w).is_some() || kind_word(w).is_some());
    let has_language = guess_from_name(Path::new(""), Path::new(name))
        .language
        .is_some();
    if describes && !has_language {
        let stem = &name[..name.len() - 4];
        return Some(format!("{stem}.{}.srt", parts.last()?.replace(' ', ".")));
    }
    Some(name.to_string())
}

/// Unpack the SRT files of a zip archive into `dir` and return their paths,
/// writing each file as it's read. Two files with the same name go into
/// numbered subfolders (`dir/2/Movie.srt`), so the name, which carries the
/// language, stays as it is.
pub fn unpack_zip_from<R: std::io::Read + std::io::Seek>(
    reader: R,
    dir: &Path,
    limits: &ZipLimits,
) -> Result<Vec<PathBuf>, String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| e.to_string())?;
    let mut out: Vec<PathBuf> = Vec::new();
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        if !f.is_file() {
            continue;
        }
        let Some(name) = zip_entry_name(&f.name().map_err(|e| e.to_string())?) else {
            continue;
        };
        if out.len() >= limits.count {
            return Err(format!("more than {} subtitle files in it", limits.count));
        }
        let too_big = || format!("{name} is too big to be a subtitle file");
        if f.size() > limits.file {
            return Err(too_big());
        }
        let mut data = Vec::new();
        (&mut f)
            .take(limits.file + 1)
            .read_to_end(&mut data)
            .map_err(|e| format!("{name}: {e}"))?;
        if data.len() as u64 > limits.file {
            return Err(too_big());
        }
        total += data.len() as u64;
        if total > limits.total {
            return Err("its subtitles are too big in total".into());
        }
        let taken = |p: &PathBuf| {
            out.iter().any(|o| {
                o.parent() == p.parent()
                    && o.file_name()
                        .is_some_and(|n| n.eq_ignore_ascii_case(p.file_name().unwrap_or_default()))
            })
        };
        let mut path = dir.join(&name);
        let mut slot = 2;
        while taken(&path) {
            path = dir.join(slot.to_string()).join(&name);
            slot += 1;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, data).map_err(|e| e.to_string())?;
        out.push(path);
    }
    Ok(out)
}

/// Unpack the SRT files of `zip` into `dir` (created if needed). An archive
/// without any is an error, so the user hears of it. A finished unpack is
/// recorded in `dir`, and a second call returns the same files without
/// unpacking again, so adding the same zip twice gives the same paths.
pub fn unpack_zip(zip: &Path, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let name = zip.file_name().unwrap_or_default().to_string_lossy();
    let done = dir.join(".unpacked");
    if let Ok(list) = std::fs::read_to_string(&done) {
        let paths: Vec<PathBuf> = list.lines().map(|l| dir.join(l)).collect();
        if !paths.is_empty() && paths.iter().all(|p| p.is_file()) {
            return Ok(paths);
        }
    }
    let file = std::fs::File::open(zip).map_err(|e| format!("Couldn't open {name}: {e}"))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't unpack {name}: {e}"))?;
    let unpacked = unpack_zip_from(std::io::BufReader::new(file), dir, &ZIP_LIMITS);
    let paths = match unpacked {
        Ok(p) if p.is_empty() => Err(format!("{name} has no SRT subtitles in it")),
        Ok(p) => Ok(p),
        Err(e) => Err(format!("Couldn't unpack {name}: {e}")),
    };
    match &paths {
        Ok(p) => {
            let list: Vec<String> = p
                .iter()
                .filter_map(|p| p.strip_prefix(dir).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let _ = std::fs::write(&done, list.join("\n"));
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
    paths
}

/// Of `videos`, the one this subtitle file belongs to: the longest name wins,
/// so `Blade.Runner.2049.en.srt` goes to `Blade.Runner.2049.mkv`, not to
/// `Blade.Runner.mkv`.
pub fn best_owner<'a>(videos: impl IntoIterator<Item = &'a Path>, sub: &Path) -> Option<&'a Path> {
    videos
        .into_iter()
        .filter(|v| belongs_to(v, sub))
        .max_by_key(|v| v.file_stem().map_or(0, |s| s.len()))
}

/// Does this subtitle file belong to the video (`Movie.mkv` ↔ `Movie.no.srt`)?
pub fn belongs_to(video: &Path, sub: &Path) -> bool {
    let (Some(v), Some(s)) = (video.file_stem(), sub.file_stem()) else {
        return false;
    };
    let (v, s) = (v.to_string_lossy(), s.to_string_lossy());
    s == v
        || s.strip_prefix(v.as_ref())
            .is_some_and(|r| r.starts_with('.'))
}

/// Release-style word (`1080p`, `x264`, `2019`): the title is over.
fn release_word(w: &str) -> bool {
    w.chars().any(|c| c.is_ascii_digit())
}

/// Guess language and kind from `Movie.no.forced.srt`, `Movie.English.SDH.srt`,
/// `2_Norwegian.srt`. Only the part after the video's name is read when the
/// file belongs to the video. Otherwise only the last two words count, and a
/// two-letter code only after a release word (`…1080p.BluRay.en.srt`), so
/// titles like "Dr.No" or "The.Day.It" aren't taken for languages.
pub fn guess_from_name(video: &Path, sub: &Path) -> NameGuess {
    let stem = sub
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let v_stem = video
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let own = belongs_to(video, sub);
    let mut words = if own {
        tokens(&stem[v_stem.len()..])
    } else {
        tokens(&stem)
    };
    // Unrelated name: may a two-letter code at position `i` be a language?
    let mut short_ok = vec![true; words.len()];
    if !own {
        for (i, ok) in short_ok.iter_mut().enumerate() {
            let alone = words.len() == 1;
            let after_release = words[..i].iter().any(|w| release_word(w))
                && words[i + 1..].iter().all(|w| kind_word(w).is_some());
            *ok = alone || after_release;
        }
        if words.len() > 2 {
            let cut = words.len() - 2;
            words.drain(..cut);
            short_ok.drain(..cut);
        }
    }
    let mut g = NameGuess::default();
    for (w, ok) in words.iter().zip(&short_ok).rev() {
        if let Some(k) = kind_word(w) {
            if g.kind == ExtKind::Full {
                g.kind = k;
            }
        } else if g.language.is_none()
            && (*ok || w.len() != 2)
            && let Some(l) = language_word(w)
        {
            g.language = Some(l);
        }
    }
    g
}

/// Plex sidecar name: `Movie.no.srt`, `Movie.en.forced.srt`, `Movie.en.sdh.srt`.
/// Two-letter codes where they exist; an unknown language gets no code.
pub fn sidecar_path(video: &Path, language: Option<&str>, kind: ExtKind) -> PathBuf {
    let stem = video
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut name = stem;
    if let Some(l) = language.filter(|l| !l.is_empty() && *l != "und") {
        name.push('.');
        name.push_str(&tracks::short_code(l));
    }
    match kind {
        ExtKind::Full => {}
        ExtKind::Sdh => name.push_str(".sdh"),
        ExtKind::Forced => name.push_str(".forced"),
    }
    name.push_str(".srt");
    video.with_file_name(name)
}

/// Where the original goes when a sidecar is synced in place.
pub fn backup_path(srt: &Path) -> PathBuf {
    let mut s = srt.as_os_str().to_owned();
    s.push(".orig");
    PathBuf::from(s)
}

/// Sidecar outputs of a job, with an error if two would get the same name.
pub fn sidecar_outputs(video: &Path, ext: &[ExternalSub]) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    for e in ext.iter().filter(|e| e.selected) {
        let p = sidecar_path(video, e.language.as_deref(), e.kind);
        if out.contains(&p) {
            return Err(format!(
                "Two subtitle files would both be saved as “{}”. Change the language or kind of one.",
                p.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        out.push(p);
    }
    Ok(out)
}

/// Subtitle files next to the video that belong to it, and not to another
/// video in the folder with a longer name.
pub fn find_sidecars(video: &Path, dir_entries: &[PathBuf]) -> Vec<PathBuf> {
    let videos: Vec<&Path> = dir_entries
        .iter()
        .map(PathBuf::as_path)
        .filter(|p| is_video(p) && *p != video)
        .chain([video])
        .collect();
    let mut v: Vec<PathBuf> = dir_entries
        .iter()
        .filter(|p| {
            p.extension().is_some_and(|e| e.eq_ignore_ascii_case("srt"))
                && best_owner(videos.iter().copied(), p) == Some(video)
        })
        .cloned()
        .collect();
    v.sort();
    v
}

/// Read the video's folder and return its sidecar subtitle files (I/O).
pub fn discover(video: &Path) -> Vec<PathBuf> {
    let Some(dir) = video.parent() else {
        return Vec::new();
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .map(|p| {
                    // Keep the video's own spelling of the folder.
                    video.with_file_name(p.file_name().unwrap_or_default())
                })
                .collect()
        })
        .unwrap_or_default();
    find_sidecars(video, &entries)
}

/// Container an in-place embed writes, from the video's extension.
pub fn embed_container(video: &Path) -> Result<Container, String> {
    let ext = video
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mkv" | "mk3d" => Ok(Container::Mkv),
        "mp4" | "m4v" | "mov" => Ok(Container::Mp4),
        _ => Err(format!(
            "A .{ext} file can't hold text subtitles. Save them next to the video instead, or re-encode to MKV."
        )),
    }
}

// ---- Selection ----

/// An external SRT replaces an embedded image (PGS) subtitle of the same
/// language and kind: Plex burns PGS in, so the SRT is the one to keep.
pub fn prefer_externals(sel: &mut Selection, p: &ProbeInfo, ext: &[ExternalSub]) {
    let covered: Vec<(String, SubKind)> = ext
        .iter()
        .filter(|e| e.selected)
        .filter_map(|e| Some((tracks::family(e.language.as_deref()?), e.kind.sub_kind())))
        .collect();
    for c in sel.subs.iter_mut().filter(|c| c.selected) {
        let Some(s) = p.subtitle_by_index(c.index) else {
            continue;
        };
        let fam = s
            .language
            .as_deref()
            .map(tracks::family)
            .unwrap_or_default();
        if !tracks::is_text_subtitle(&s.codec) && covered.contains(&(fam, tracks::sub_kind(s))) {
            c.selected = false;
        }
    }
}

/// Languages whose "only PGS" question an external full or SDH SRT answers.
pub fn covers_language(ext: &[ExternalSub], family: &str) -> bool {
    ext.iter().any(|e| {
        e.selected
            && e.kind != ExtKind::Forced
            && e.language.as_deref().map(tracks::family).as_deref() == Some(family)
    })
}

// ---- Syncing ----

/// What alass aligns the subtitles against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncRef {
    /// An embedded text subtitle (stream index): most accurate.
    Subtitle(usize),
    /// An audio track (stream index), extracted to a small WAV.
    Audio(usize),
}

/// Prefer an embedded full text subtitle; otherwise the main selected audio
/// track, then the first audio track.
pub fn sync_reference(p: &ProbeInfo, sel: &Selection) -> Option<SyncRef> {
    let text_subs: Vec<_> = p
        .subtitles
        .iter()
        .filter(|s| tracks::is_text_subtitle(&s.codec))
        .collect();
    let best = text_subs
        .iter()
        .find(|s| tracks::sub_kind(s) == SubKind::Full)
        .or_else(|| {
            text_subs
                .iter()
                .find(|s| tracks::sub_kind(s) == SubKind::Sdh)
        });
    if let Some(s) = best {
        return Some(SyncRef::Subtitle(s.index));
    }
    sel.selected_audio()
        .next()
        .map(|c| c.index)
        .or_else(|| p.audio.first().map(|a| a.index))
        .map(SyncRef::Audio)
}

impl SyncRef {
    pub fn describe(self, p: &ProbeInfo) -> String {
        match self {
            Self::Subtitle(i) => {
                let lang = p
                    .subtitle_by_index(i)
                    .map(|s| tracks::language_name(s.language.as_deref()))
                    .unwrap_or_default();
                format!("the embedded {lang} subtitles")
            }
            Self::Audio(i) => {
                let lang = p
                    .audio_by_index(i)
                    .map(|a| tracks::language_name(a.language.as_deref()))
                    .unwrap_or_default();
                format!("the {lang} audio")
            }
        }
    }
}

/// ffmpeg arguments that extract the sync reference to `out` (`.srt` or `.wav`).
pub fn reference_args(video: &Path, r: SyncRef, out: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["-hide_banner", "-y", "-nostdin", "-i"]
        .iter()
        .map(OsString::from)
        .collect();
    a.push(video.into());
    let rest: Vec<String> = match r {
        SyncRef::Subtitle(i) => vec!["-map".into(), format!("0:{i}"), "-c:s".into(), "srt".into()],
        // 8 kHz mono is what alass's voice detection works on anyway.
        SyncRef::Audio(i) => vec![
            "-map".into(),
            format!("0:{i}"),
            "-vn".into(),
            "-ac".into(),
            "1".into(),
            "-ar".into(),
            "8000".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
        ],
    };
    a.extend(rest.into_iter().map(OsString::from));
    a.extend(["-progress", "pipe:1", "-nostats"].map(OsString::from));
    a.push(out.into());
    a
}

/// alass arguments; inputs are always clean UTF-8 SRT written by us.
pub fn alass_args(reference: &Path, input: &Path, output: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    if reference
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("srt"))
    {
        a.extend(["--encoding-ref", "utf-8"].map(OsString::from));
    }
    a.extend(["--encoding-inc", "utf-8"].map(OsString::from));
    a.push(reference.into());
    a.push(input.into());
    a.push(output.into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe::{AudioStream, Disposition, SubtitleStream};
    use crate::tracks::SubChoice;

    fn cue(start: i64, end: i64, text: &str) -> Cue {
        Cue {
            start,
            end,
            text: text.into(),
        }
    }

    #[test]
    fn decodes_utf8_boms_and_cp1252() {
        assert_eq!(decode_text("blåbær".as_bytes()), "blåbær");
        assert_eq!(decode_text(b"\xEF\xBB\xBFhei"), "hei");
        // Windows-1252: å = E5, ø = F8, curly quote = 93/94, € = 80.
        assert_eq!(
            decode_text(b"bl\xE5b\xE6r \x93s\xF8t\x94 \x80"),
            "blåbær “søt” €"
        );
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("hå".encode_utf16().flat_map(|u| u.to_le_bytes()))
            .collect();
        assert_eq!(decode_text(&le), "hå");
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("hå".encode_utf16().flat_map(|u| u.to_be_bytes()))
            .collect();
        assert_eq!(decode_text(&be), "hå");
        let bomless: Vec<u8> = "1\n00:00:01,000 --> 00:00:02,000\nHei på deg\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(
            decode_text_noted(&bomless),
            (
                "1\n00:00:01,000 --> 00:00:02,000\nHei på deg\n".into(),
                None
            )
        );
        assert!(decode_text_noted(b"bl\xE5").1.is_some());
        assert!(decode_text_noted("blå".as_bytes()).1.is_none());
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_time("01:02:03,456"), Some(3_723_456));
        assert_eq!(parse_time("01:02:03.456"), Some(3_723_456));
        assert_eq!(parse_time("02:03,4"), Some(123_400));
        assert_eq!(parse_time("00:00:01,0500"), Some(1_050));
        assert_eq!(parse_time("x"), None);
        assert_eq!(parse_time("00:00:01,"), None);
    }

    #[test]
    fn parses_messy_srt() {
        let text = "\u{feff}1\r\n00:00:05,000 --> 00:00:07,500 X1:10 X2:20\r\nSecond\r\nline two\r\n\r\n\
                    7\r\n00:00:01.000 --> 00:00:02.000\r\nFirst\r\n\r\n\
                    3\r\n00:00:09,000 --> 00:00:10,000\r\n\r\n\
                    00:00:11,000 --> 00:00:12,000\nNo number\n42\n00:00:13,000 --> 00:00:14,000\nAfter a number\n";
        let cues = parse_srt(text).unwrap();
        assert_eq!(
            cues,
            vec![
                cue(1000, 2000, "First"),
                cue(5000, 7500, "Second\nline two"),
                cue(11_000, 12_000, "No number"),
                cue(13_000, 14_000, "After a number"),
            ]
        );
        assert!(parse_srt("just text\nno cues").is_err());
        // A blank line inside a cue keeps the text after it; stray text at
        // the end of the file is dropped.
        let gap = "1\n00:00:01,000 --> 00:00:02,000\nLine one\n\nLine two\n\n\
                   2\n00:00:03,000 --> 00:00:04,000\nNext\n\nSubtitles by someone\n";
        assert_eq!(
            parse_srt(gap).unwrap(),
            vec![
                cue(1000, 2000, "Line one\n\nLine two"),
                cue(3000, 4000, "Next")
            ]
        );
    }

    #[test]
    fn writes_clean_srt_and_round_trips() {
        let cues = vec![
            cue(1000, 2500, "Hei"),
            cue(3_723_456, 3_724_000, "To\nlinjer"),
        ];
        let out = write_srt(&cues);
        assert_eq!(
            out,
            "1\n00:00:01,000 --> 00:00:02,500\nHei\n\n2\n01:02:03,456 --> 01:02:04,000\nTo\nlinjer\n\n"
        );
        assert_eq!(parse_srt(&out).unwrap(), cues);
    }

    #[test]
    fn shifting_drops_and_clamps_at_zero() {
        let cues = vec![
            cue(500, 1500, "a"),
            cue(1000, 3000, "b"),
            cue(5000, 6000, "c"),
        ];
        assert_eq!(
            shift(&cues, -2000),
            vec![cue(0, 1000, "b"), cue(3000, 4000, "c")]
        );
        assert_eq!(shift(&cues, 250)[0], cue(750, 1750, "a"));
    }

    #[test]
    fn sync_report() {
        let before: Vec<Cue> = (0..50)
            .map(|i| cue(i * 60_000, i * 60_000 + 2000, "x"))
            .collect();
        assert_eq!(describe_sync(&before, &before), "already in sync");
        let shifted = shift(&before, 0)
            .into_iter()
            .map(|c| cue(c.start + 1240, c.end + 1240, "x"))
            .collect::<Vec<_>>();
        assert_eq!(describe_sync(&before, &shifted), "shifted +1.24 s");
        // 25 fps subtitles on a 23.976 fps video: times stretch by 25/23.976.
        let stretched: Vec<Cue> = before
            .iter()
            .map(|c| {
                let s = (c.start as f64 * 25.0 / 23.976) as i64 + 500;
                cue(s, s + 2000, "x")
            })
            .collect();
        assert_eq!(
            describe_sync(&before, &stretched),
            "framerate corrected (23.976 ↔ 25 fps), +0.50 s at the start"
        );
        let parts: Vec<Cue> = before
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let d = if i < 25 { 1000 } else { 3000 };
                cue(c.start + d, c.end + d, "x")
            })
            .collect();
        assert_eq!(
            describe_sync(&before, &parts),
            "adjusted in parts, +1.00 s to +3.00 s"
        );
    }

    #[test]
    fn guesses_language_and_kind_from_names() {
        let v = Path::new("/m/Movie (2020).mkv");
        let g = |n: &str| guess_from_name(v, Path::new(n));
        assert_eq!(
            g("/m/Movie (2020).no.srt"),
            NameGuess {
                language: Some("nor".into()),
                kind: ExtKind::Full
            }
        );
        assert_eq!(
            g("/m/Movie (2020).en.forced.srt"),
            NameGuess {
                language: Some("eng".into()),
                kind: ExtKind::Forced
            }
        );
        assert_eq!(
            g("/m/Movie (2020).English.SDH.srt").language.as_deref(),
            Some("eng")
        );
        assert_eq!(g("/m/Movie (2020).nb.hi.srt").kind, ExtKind::Sdh);
        assert_eq!(
            g("/m/Movie (2020).nb.hi.srt").language.as_deref(),
            Some("nob")
        );
        assert_eq!(g("/m/Movie (2020).srt"), NameGuess::default());
        assert_eq!(g("/x/2_Norwegian.srt").language.as_deref(), Some("nor"));
        assert_eq!(g("/x/eng.srt").language.as_deref(), Some("eng"));
        // Unrelated name: only the last two words count, and a two-letter
        // code only after a release word.
        assert_eq!(g("/x/The.Day.It.Rained.1080p.srt").language, None);
        assert_eq!(g("/x/The.Day.It.srt").language, None);
        assert_eq!(g("/x/Dr.No.srt").language, None);
        assert_eq!(g("/x/Leave.No.Trace.srt").language, None);
        assert_eq!(
            g("/x/Other.2019.1080p.BluRay.x264-GRP.en.srt")
                .language
                .as_deref(),
            Some("eng")
        );
        let forced = g("/x/Other.2019.1080p.no.forced.srt");
        assert_eq!(forced.language.as_deref(), Some("nor"));
        assert_eq!(forced.kind, ExtKind::Forced);
        assert_eq!(g("/x/no.srt").language.as_deref(), Some("nor"));
        assert_eq!(
            g("/x/Some.Film.Norwegian.srt").language.as_deref(),
            Some("nor")
        );
    }

    #[test]
    fn sidecar_names_follow_plex() {
        let v = Path::new("/lib/Film.mkv");
        assert_eq!(
            sidecar_path(v, Some("nor"), ExtKind::Full),
            Path::new("/lib/Film.no.srt")
        );
        assert_eq!(
            sidecar_path(v, Some("eng"), ExtKind::Forced),
            Path::new("/lib/Film.en.forced.srt")
        );
        assert_eq!(
            sidecar_path(v, Some("nob"), ExtKind::Sdh),
            Path::new("/lib/Film.nb.sdh.srt")
        );
        assert_eq!(
            sidecar_path(v, Some("cmn"), ExtKind::Full),
            Path::new("/lib/Film.cmn.srt")
        );
        assert_eq!(
            sidecar_path(v, None, ExtKind::Full),
            Path::new("/lib/Film.srt")
        );
        assert_eq!(
            backup_path(Path::new("/lib/Film.no.srt")),
            Path::new("/lib/Film.no.srt.orig")
        );
    }

    #[test]
    fn sidecar_clash_is_an_error() {
        let v = Path::new("/lib/Film.mkv");
        let mut a = ExternalSub::from_path(v, "/x/a.srt".into(), false);
        a.language = Some("eng".into());
        let mut b = a.clone();
        b.path = "/x/b.srt".into();
        assert!(sidecar_outputs(v, &[a.clone(), b.clone()]).is_err());
        b.kind = ExtKind::Forced;
        assert_eq!(
            sidecar_outputs(v, &[a.clone(), b.clone()]).unwrap().len(),
            2
        );
        b.kind = ExtKind::Full;
        b.selected = false;
        assert_eq!(sidecar_outputs(v, &[a, b]).unwrap().len(), 1);
    }

    #[test]
    fn finds_sidecars_and_embed_containers() {
        let v = Path::new("/lib/Film.mkv");
        let entries: Vec<PathBuf> = [
            "/lib/Film.no.srt",
            "/lib/Film.SRT",
            "/lib/Film 2.no.srt",
            "/lib/Filmen.srt",
            "/lib/Film.nfo",
            "/lib/Film.en.forced.srt",
            "/lib/Film.2049.mkv",
            "/lib/Film.2049.en.srt",
            "/lib/Film.no.nfo",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(
            find_sidecars(v, &entries),
            vec![
                PathBuf::from("/lib/Film.SRT"),
                PathBuf::from("/lib/Film.en.forced.srt"),
                PathBuf::from("/lib/Film.no.srt"),
            ]
        );
        // The longer-named video owns its own subtitles.
        assert_eq!(
            find_sidecars(Path::new("/lib/Film.2049.mkv"), &entries),
            vec![PathBuf::from("/lib/Film.2049.en.srt")]
        );
        let videos = [Path::new("/a/Film.mkv"), Path::new("/b/Film.2049.mkv")];
        assert_eq!(
            best_owner(videos, Path::new("/c/Film.2049.no.srt")),
            Some(Path::new("/b/Film.2049.mkv"))
        );
        assert_eq!(best_owner(videos, Path::new("/c/Other.srt")), None);
        assert_eq!(embed_container(v), Ok(Container::Mkv));
        assert_eq!(embed_container(Path::new("a.M4V")), Ok(Container::Mp4));
        assert!(embed_container(Path::new("a.avi")).is_err());
        assert!(embed_container(Path::new("a.webm")).is_err());
    }

    fn sub(index: usize, codec: &str, lang: &str, forced: bool) -> SubtitleStream {
        SubtitleStream {
            index,
            codec: codec.into(),
            language: Some(lang.into()),
            disposition: Disposition {
                forced,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn external_srt_replaces_pgs_of_same_language_and_kind() {
        let p = ProbeInfo {
            subtitles: vec![
                sub(2, "hdmv_pgs_subtitle", "nor", false),
                sub(3, "hdmv_pgs_subtitle", "nor", true),
                sub(4, "hdmv_pgs_subtitle", "eng", false),
                sub(5, "subrip", "nob", false),
            ],
            ..Default::default()
        };
        let mut sel = Selection {
            audio: vec![],
            subs: [2, 3, 4, 5]
                .iter()
                .map(|&index| SubChoice {
                    index,
                    selected: true,
                })
                .collect(),
        };
        let v = Path::new("/l/F.mkv");
        let ext = vec![ExternalSub::from_path(v, "/l/F.no.srt".into(), true)];
        prefer_externals(&mut sel, &p, &ext);
        let on: Vec<usize> = sel.selected_subs().map(|c| c.index).collect();
        // Norwegian full PGS goes; forced PGS, English PGS and the embedded SRT stay.
        assert_eq!(on, vec![3, 4, 5]);
        assert!(covers_language(&ext, "nor"));
        assert!(!covers_language(&ext, "eng"));
    }

    #[test]
    fn sync_reference_prefers_embedded_text_subtitles() {
        let audio = |index| AudioStream {
            index,
            codec: "aac".into(),
            ..Default::default()
        };
        let mut p = ProbeInfo {
            audio: vec![audio(1), audio(2)],
            subtitles: vec![
                sub(3, "hdmv_pgs_subtitle", "eng", false),
                sub(4, "subrip", "eng", true),
            ],
            ..Default::default()
        };
        let mut sel = Selection::default();
        // Only a forced text track (too few lines to sync against): use audio.
        assert_eq!(sync_reference(&p, &sel), Some(SyncRef::Audio(1)));
        sel.audio.push(crate::tracks::AudioChoice {
            index: 2,
            selected: true,
            action: crate::tracks::AudioAction::Copy,
            offset_ms: 0,
        });
        assert_eq!(sync_reference(&p, &sel), Some(SyncRef::Audio(2)));
        p.subtitles.push(sub(5, "ass", "jpn", false));
        assert_eq!(sync_reference(&p, &sel), Some(SyncRef::Subtitle(5)));
        assert_eq!(
            sync_reference(&ProbeInfo::default(), &Selection::default()),
            None
        );
    }

    #[test]
    fn reference_and_alass_arguments() {
        let a = reference_args(
            Path::new("/v.mkv"),
            SyncRef::Audio(3),
            Path::new("/t/r.wav"),
        );
        let s: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(s.windows(2).any(|w| w == ["-map", "0:3"]));
        assert!(s.windows(2).any(|w| w == ["-ar", "8000"]));
        assert_eq!(s.last().unwrap(), "/t/r.wav");
        let a = reference_args(
            Path::new("/v.mkv"),
            SyncRef::Subtitle(5),
            Path::new("/t/r.srt"),
        );
        let s: Vec<String> = a.iter().map(|x| x.to_string_lossy().into_owned()).collect();
        assert!(s.windows(2).any(|w| w == ["-c:s", "srt"]));
        let a = alass_args(
            Path::new("r.wav"),
            Path::new("in.srt"),
            Path::new("out.srt"),
        );
        assert_eq!(
            a,
            ["--encoding-inc", "utf-8", "r.wav", "in.srt", "out.srt"]
                .map(OsString::from)
                .to_vec()
        );
        let a = alass_args(
            Path::new("r.srt"),
            Path::new("in.srt"),
            Path::new("out.srt"),
        );
        assert_eq!(a.len(), 7);
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn zip_entry_names() {
        assert_eq!(
            zip_entry_name("film.nor.srt").as_deref(),
            Some("film.nor.srt")
        );
        assert_eq!(
            zip_entry_name("../../etc/evil.srt").as_deref(),
            Some("evil.srt")
        );
        assert_eq!(zip_entry_name("a\\b\\c.SRT").as_deref(), Some("c.SRT"));
        assert_eq!(zip_entry_name("film.nfo"), None);
        assert_eq!(zip_entry_name("__MACOSX/._film.srt"), None);
        assert_eq!(zip_entry_name(".hidden.srt"), None);
        assert_eq!(zip_entry_name("dir/"), None);
        // A language folder names the file; a file with its own language keeps it.
        assert_eq!(
            zip_entry_name("Subs/Norwegian/Movie.srt").as_deref(),
            Some("Movie.Norwegian.srt")
        );
        assert_eq!(
            zip_entry_name("English SDH/Movie.srt").as_deref(),
            Some("Movie.English.SDH.srt")
        );
        assert_eq!(
            zip_entry_name("English/Movie.2019.1080p.no.srt").as_deref(),
            Some("Movie.2019.1080p.no.srt")
        );
        assert_eq!(
            zip_entry_name("Subs/Movie.srt").as_deref(),
            Some("Movie.srt")
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("encoder-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn zips_unpack_with_names_intact_and_limits() {
        let z = zip_of(&[
            ("English/Movie.srt", b"1"),
            ("Norwegian/Movie.srt", b"2"),
            ("a/Film.2019.no.srt", b"3"),
            ("b/film.2019.NO.srt", b"4"),
            ("film.nfo", b"x"),
        ]);
        let dir = scratch("unzip");
        let got = unpack_zip_from(std::io::Cursor::new(z.clone()), &dir, &ZIP_LIMITS).unwrap();
        let rel: Vec<String> = got
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rel,
            [
                "Movie.English.srt",
                "Movie.Norwegian.srt",
                "Film.2019.no.srt",
                "2/film.2019.NO.srt"
            ]
        );
        assert_eq!(std::fs::read(&got[3]).unwrap(), b"4");
        let tight = |file, total, count| ZipLimits { file, total, count };
        let d2 = scratch("unzip-limits");
        assert!(unpack_zip_from(std::io::Cursor::new(z.clone()), &d2, &tight(0, 99, 99)).is_err());
        assert!(unpack_zip_from(std::io::Cursor::new(z.clone()), &d2, &tight(9, 2, 99)).is_err());
        assert!(unpack_zip_from(std::io::Cursor::new(z), &d2, &tight(9, 99, 3)).is_err());
        assert!(
            unpack_zip_from(
                std::io::Cursor::new(b"not a zip".to_vec()),
                &d2,
                &ZIP_LIMITS
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn unpacking_the_same_zip_twice_gives_the_same_files() {
        let dir = scratch("unzip-twice");
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("subs.zip");
        std::fs::write(&zip, zip_of(&[("Movie.no.srt", b"1")])).unwrap();
        let out = dir.join("out");
        let first = unpack_zip(&zip, &out).unwrap();
        std::fs::write(&first[0], b"changed").unwrap();
        assert_eq!(unpack_zip(&zip, &out).unwrap(), first);
        assert_eq!(std::fs::read(&first[0]).unwrap(), b"changed");
        let empty = dir.join("empty.zip");
        std::fs::write(&empty, zip_of(&[("readme.nfo", b"x")])).unwrap();
        let e = unpack_zip(&empty, &dir.join("out2")).unwrap_err();
        assert!(e.contains("no SRT"), "{e}");
        assert!(!dir.join("out2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
