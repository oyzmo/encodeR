//! Track listing, language names, ranking and default selection. Pure functions.

use serde::{Deserialize, Serialize};

use crate::ffmpeg::probe::{
    AudioStream, ProbeInfo, SubtitleStream, audio_bitrate_guess, subtitle_bitrate_guess,
};
use crate::settings::{EncodeSettings, MultichannelFallback};

// ---- Languages ----

/// (canonical ISO 639-2/T code, aliases, English name).
const LANGUAGES: &[(&str, &[&str], &str)] = &[
    ("nor", &["no"], "Norwegian"),
    ("nob", &["nb"], "Norwegian Bokmål"),
    ("nno", &["nn"], "Norwegian Nynorsk"),
    ("eng", &["en"], "English"),
    ("jpn", &["ja"], "Japanese"),
    ("swe", &["sv"], "Swedish"),
    ("dan", &["da"], "Danish"),
    ("fin", &["fi"], "Finnish"),
    ("isl", &["is", "ice"], "Icelandic"),
    ("deu", &["de", "ger"], "German"),
    ("fra", &["fr", "fre"], "French"),
    ("spa", &["es"], "Spanish"),
    ("ita", &["it"], "Italian"),
    ("por", &["pt"], "Portuguese"),
    ("nld", &["nl", "dut"], "Dutch"),
    ("pol", &["pl"], "Polish"),
    ("ces", &["cs", "cze"], "Czech"),
    ("slk", &["sk", "slo"], "Slovak"),
    ("hun", &["hu"], "Hungarian"),
    ("ron", &["ro", "rum"], "Romanian"),
    ("ell", &["el", "gre"], "Greek"),
    ("tur", &["tr"], "Turkish"),
    ("rus", &["ru"], "Russian"),
    ("ukr", &["uk"], "Ukrainian"),
    ("zho", &["zh", "chi"], "Chinese"),
    ("cmn", &[], "Mandarin"),
    ("yue", &[], "Cantonese"),
    ("kor", &["ko"], "Korean"),
    ("tha", &["th"], "Thai"),
    ("vie", &["vi"], "Vietnamese"),
    ("ind", &["id"], "Indonesian"),
    ("msa", &["ms", "may"], "Malay"),
    ("hin", &["hi"], "Hindi"),
    ("tam", &["ta"], "Tamil"),
    ("tel", &["te"], "Telugu"),
    ("ara", &["ar"], "Arabic"),
    ("heb", &["he"], "Hebrew"),
    ("fas", &["fa", "per"], "Persian"),
    ("hrv", &["hr"], "Croatian"),
    ("srp", &["sr"], "Serbian"),
    ("slv", &["sl"], "Slovenian"),
    ("bul", &["bg"], "Bulgarian"),
    ("est", &["et"], "Estonian"),
    ("lav", &["lv"], "Latvian"),
    ("lit", &["lt"], "Lithuanian"),
    ("cat", &["ca"], "Catalan"),
    ("eus", &["eu", "baq"], "Basque"),
    ("glg", &["gl"], "Galician"),
    ("fil", &["tl", "tgl"], "Filipino"),
    ("sme", &["se"], "Northern Sami"),
    ("zxx", &[], "No linguistic content"),
];

pub const ORIGINAL: &str = "original";

fn entry(code: &str) -> Option<&'static (&'static str, &'static [&'static str], &'static str)> {
    let c = code.trim().to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|(canon, aliases, _)| *canon == c || aliases.contains(&c.as_str()))
}

/// Canonical code for a tag (`ger` → `deu`, `nb` → `nob`); unknown codes pass through.
pub fn canonical(code: &str) -> String {
    entry(code).map_or_else(|| code.trim().to_ascii_lowercase(), |e| e.0.to_string())
}

/// Grouping key: all Norwegian variants form one group.
pub fn family(code: &str) -> String {
    match canonical(code).as_str() {
        "nob" | "nno" => "nor".into(),
        "cmn" => "zho".into(),
        c => c.to_string(),
    }
}

pub fn language_name(code: Option<&str>) -> String {
    match code {
        None | Some("und") | Some("") => "Unknown".into(),
        Some(c) => entry(c).map_or_else(|| c.to_string(), |e| e.2.to_string()),
    }
}

pub fn family_name(fam: &str) -> String {
    if fam.is_empty() {
        "Unknown".into()
    } else {
        language_name(Some(fam))
    }
}

/// Parse "Norwegian, eng, jpn, original" into codes. Returns the unknown words too.
pub fn parse_language_list(text: &str) -> (Vec<String>, Vec<String>) {
    let mut codes = Vec::new();
    let mut unknown = Vec::new();
    for word in text
        .split([',', ';'])
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        let lower = word.to_lowercase();
        let code = if lower == ORIGINAL || lower == "original language" {
            Some(ORIGINAL.to_string())
        } else if let Some(e) = entry(&lower) {
            Some(e.0.to_string())
        } else {
            LANGUAGES
                .iter()
                .find(|(_, _, name)| name.to_lowercase() == lower)
                .map(|e| e.0.to_string())
        };
        match code {
            Some(c) if !codes.contains(&c) => codes.push(c),
            Some(_) => {}
            None => unknown.push(word.to_string()),
        }
    }
    (codes, unknown)
}

pub fn format_language_list(codes: &[String]) -> String {
    codes
        .iter()
        .map(|c| {
            if c == ORIGINAL {
                "Original".to_string()
            } else {
                language_name(Some(c))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ---- Codec facts ----

pub fn is_atmos(a: &AudioStream) -> bool {
    a.profile.contains("Atmos")
        || a.title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains("atmos"))
}

pub fn codec_name(codec: &str, profile: &str) -> String {
    match codec {
        "truehd" | "mlp" => "TrueHD".into(),
        "eac3" => "E-AC-3".into(),
        "ac3" => "AC-3".into(),
        "dts" if profile.contains("MA") => "DTS-HD MA".into(),
        "dts" if profile.contains("HRA") || profile.contains("HD") => "DTS-HD".into(),
        "dts" => "DTS".into(),
        "aac" => "AAC".into(),
        "flac" => "FLAC".into(),
        "alac" => "ALAC".into(),
        "mp3" => "MP3".into(),
        "opus" => "Opus".into(),
        "vorbis" => "Vorbis".into(),
        c if c.starts_with("pcm_") => "PCM".into(),
        "subrip" | "srt" => "SRT".into(),
        "ass" | "ssa" => "ASS".into(),
        "webvtt" => "WebVTT".into(),
        "mov_text" => "MP4 text".into(),
        "hdmv_pgs_subtitle" => "PGS".into(),
        "dvd_subtitle" => "VobSub".into(),
        "dvb_subtitle" => "DVB".into(),
        c => c.to_uppercase(),
    }
}

pub fn channel_label(ch: u32) -> String {
    match ch {
        1 => "Mono".into(),
        2 => "2.0".into(),
        3 => "2.1".into(),
        6 => "5.1".into(),
        7 => "6.1".into(),
        8 => "7.1".into(),
        n => format!("{n} ch"),
    }
}

/// "TrueHD 7.1 Atmos".
pub fn audio_label(a: &AudioStream) -> String {
    let mut s = format!(
        "{} {}",
        codec_name(&a.codec, &a.profile),
        channel_label(a.channels)
    );
    if is_atmos(a) {
        s.push_str(" Atmos");
    }
    s
}

/// Audio Apple TV 4K plays natively.
pub fn apple_compatible(codec: &str) -> bool {
    matches!(codec, "aac" | "ac3" | "eac3" | "flac" | "alac" | "mp3") || codec.starts_with("pcm_")
}

pub fn is_text_subtitle(codec: &str) -> bool {
    matches!(
        codec,
        "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text"
    )
}

fn codec_tier(a: &AudioStream) -> u8 {
    match a.codec.as_str() {
        "truehd" | "mlp" | "flac" | "alac" => 4,
        "dts" if a.profile.contains("MA") => 4,
        c if c.starts_with("pcm_") => 4,
        "dts" | "eac3" => 3,
        "ac3" => 2,
        "aac" | "mp3" | "opus" | "vorbis" => 1,
        _ => 0,
    }
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn is_commentary(a: &AudioStream) -> bool {
    if a.disposition.comment {
        return true;
    }
    let title = a.title.as_deref().unwrap_or("").to_lowercase();
    // "Director's Cut" names the main track of that version, not a commentary.
    ["commentary", "kommentar"]
        .iter()
        .any(|k| title.contains(k))
        || (title.contains("director") && !title.contains("cut"))
}

pub fn is_audio_description(a: &AudioStream) -> bool {
    if a.disposition.visual_impaired || a.disposition.descriptions {
        return true;
    }
    let title = a.title.as_deref().unwrap_or("");
    let lower = title.to_lowercase();
    [
        "audio description",
        "descriptive",
        "synstolk",
        "described video",
    ]
    .iter()
    .any(|k| lower.contains(k))
        || words(title).iter().any(|w| w == "ad")
}

// ---- Ranking ----

#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    pub index: usize,
    /// Plain-language reasons, for the "why" tooltip.
    pub reasons: Vec<String>,
}

fn audio_key(
    a: &AudioStream,
    order: usize,
    apple: bool,
    duration: f64,
) -> (
    bool,
    bool,
    bool,
    u32,
    bool,
    u8,
    u64,
    std::cmp::Reverse<usize>,
) {
    let atmos = is_atmos(a);
    let compatible = apple_compatible(&a.codec);
    // Atmos carries a full bed, so it counts as 7.1 whatever the base layout says.
    let channels = if atmos { a.channels.max(8) } else { a.channels };
    (
        !(is_commentary(a) || is_audio_description(a)),
        // Copying E-AC-3 Atmos is the only way to keep Atmos on Apple TV.
        apple && atmos && compatible,
        a.disposition.default,
        channels,
        apple && compatible,
        codec_tier(a),
        audio_bitrate_guess(a, duration),
        std::cmp::Reverse(order),
    )
}

fn audio_reasons(a: &AudioStream, apple: bool) -> Vec<String> {
    let mut r = Vec::new();
    if apple && is_atmos(a) && apple_compatible(&a.codec) {
        r.push("keeps Atmos on Apple TV".into());
    }
    if a.disposition.default {
        r.push("default".into());
    }
    r.push(channel_label(a.channels));
    if is_commentary(a) {
        r.push("commentary".into());
    } else if is_audio_description(a) {
        r.push("audio description".into());
    } else {
        r.push("not commentary".into());
    }
    if apple && apple_compatible(&a.codec) {
        r.push("plays on Apple TV".into());
    }
    r
}

/// Rank audio tracks (best first). `streams` are in input order.
pub fn rank_audio(streams: &[&AudioStream], apple: bool, duration: f64) -> Vec<Ranked> {
    let mut v: Vec<(usize, &AudioStream)> = streams.iter().copied().enumerate().collect();
    v.sort_by(|(i, a), (j, b)| {
        audio_key(b, *j, apple, duration).cmp(&audio_key(a, *i, apple, duration))
    });
    v.into_iter()
        .map(|(_, a)| Ranked {
            index: a.index,
            reasons: audio_reasons(a, apple),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubKind {
    Forced,
    Sdh,
    Full,
}

pub fn sub_kind(s: &SubtitleStream) -> SubKind {
    let title = s.title.as_deref().unwrap_or("");
    let w = words(title);
    if s.disposition.forced || w.iter().any(|w| w == "forced") {
        SubKind::Forced
    } else if s.disposition.hearing_impaired
        || w.iter().any(|w| w == "sdh" || w == "cc" || w == "hearing")
    {
        SubKind::Sdh
    } else {
        SubKind::Full
    }
}

pub fn sub_kind_label(k: SubKind) -> &'static str {
    match k {
        SubKind::Full => "full",
        SubKind::Sdh => "SDH",
        SubKind::Forced => "forced",
    }
}

pub fn rank_subtitles(streams: &[&SubtitleStream]) -> Vec<Ranked> {
    let key = |s: &SubtitleStream, i: usize| {
        (
            sub_kind(s),
            is_text_subtitle(&s.codec),
            s.disposition.default,
            std::cmp::Reverse(i),
        )
    };
    let mut v: Vec<(usize, &SubtitleStream)> = streams.iter().copied().enumerate().collect();
    v.sort_by_key(|(i, s)| std::cmp::Reverse(key(s, *i)));
    v.into_iter()
        .map(|(_, s)| {
            let mut reasons = vec![sub_kind_label(sub_kind(s)).to_string()];
            reasons.push(
                if is_text_subtitle(&s.codec) {
                    "text (SRT/ASS)"
                } else {
                    "image (PGS)"
                }
                .into(),
            );
            Ranked {
                index: s.index,
                reasons,
            }
        })
        .collect()
}

// ---- Audio actions ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioAction {
    Copy,
    Eac3 {
        kbps: u32,
    },
    /// `stereo` downmixes to at most two channels.
    Aac {
        kbps: u32,
        stereo: bool,
    },
    Flac,
}

impl AudioAction {
    /// Choices offered in the per-track dropdown for a source with `channels`.
    pub fn choices(channels: u32) -> Vec<AudioAction> {
        let mut v = vec![
            Self::Copy,
            Self::Eac3 { kbps: 640 },
            Self::Eac3 { kbps: 448 },
        ];
        v.push(Self::Aac {
            kbps: 256,
            stereo: true,
        });
        v.push(Self::Aac {
            kbps: 192,
            stereo: true,
        });
        if channels > 2 {
            v.push(Self::Aac {
                kbps: 64 * channels.min(8),
                stereo: false,
            });
        }
        v.push(Self::Flac);
        v
    }

    pub fn output_channels(self, src: u32) -> u32 {
        match self {
            Self::Copy | Self::Flac => src,
            Self::Eac3 { .. } => src.min(6),
            Self::Aac { stereo: true, .. } => src.min(2),
            Self::Aac { stereo: false, .. } => src.min(8),
        }
    }

    pub fn label(self, src_channels: u32) -> String {
        let ch = channel_label(self.output_channels(src_channels));
        match self {
            Self::Copy => "Copy".into(),
            Self::Eac3 { kbps } => format!("E-AC-3 {ch} · {kbps} kb/s"),
            Self::Aac { kbps, .. } => format!("AAC {ch} · {kbps} kb/s"),
            Self::Flac => format!("FLAC {ch} · lossless"),
        }
    }

    pub fn target_codec(self) -> Option<&'static str> {
        match self {
            Self::Copy => None,
            Self::Eac3 { .. } => Some("eac3"),
            Self::Aac { .. } => Some("aac"),
            Self::Flac => Some("flac"),
        }
    }
}

/// Default action for a track given the "Apple TV compatible audio only" setting.
pub fn default_action(a: &AudioStream, s: &EncodeSettings) -> AudioAction {
    if s.stereo_downmix && a.channels > 2 {
        return AudioAction::Aac {
            kbps: 224,
            stereo: true,
        };
    }
    if !s.apple_tv_audio || apple_compatible(&a.codec) {
        return AudioAction::Copy;
    }
    if a.channels <= 2 {
        return AudioAction::Aac {
            kbps: if a.channels == 1 { 128 } else { 224 },
            stereo: true,
        };
    }
    match s.multichannel_fallback {
        MultichannelFallback::Eac3 => AudioAction::Eac3 { kbps: 640 },
        MultichannelFallback::Aac => AudioAction::Aac {
            kbps: 64 * a.channels.min(8),
            stereo: false,
        },
        MultichannelFallback::Flac => AudioAction::Flac,
    }
}

/// "TrueHD 7.1 Atmos → E-AC-3 5.1 (Atmos and 2 channels lost; Apple TV can't play TrueHD)".
pub fn conversion_note(a: &AudioStream, action: AudioAction) -> Option<String> {
    let codec = action.target_codec()?;
    let out_ch = action.output_channels(a.channels);
    let mut lost = Vec::new();
    if is_atmos(a) {
        lost.push("Atmos".to_string());
    }
    if out_ch < a.channels {
        let n = a.channels - out_ch;
        lost.push(format!("{n} channel{}", if n == 1 { "" } else { "s" }));
    }
    let mut why = Vec::new();
    if !lost.is_empty() {
        why.push(format!("{} lost", lost.join(" and ")));
    }
    if !apple_compatible(&a.codec) {
        why.push(format!(
            "Apple TV can't play {}",
            codec_name(&a.codec, &a.profile)
        ));
    }
    let mut note = format!(
        "{} → {} {}",
        audio_label(a),
        codec_name(codec, ""),
        channel_label(out_ch)
    );
    if !why.is_empty() {
        note.push_str(&format!(" ({})", why.join("; ")));
    }
    Some(note)
}

/// Bytes this track will take in the output.
pub fn output_audio_bytes(a: &AudioStream, action: AudioAction, duration: f64) -> u64 {
    let bits = match action {
        AudioAction::Copy => {
            return a.size_bytes.unwrap_or_else(|| {
                (audio_bitrate_guess(a, duration) as f64 * duration / 8.0) as u64
            });
        }
        AudioAction::Eac3 { kbps } | AudioAction::Aac { kbps, .. } => {
            kbps as f64 * 1000.0 * duration
        }
        // FLAC from a lossy source is ~ the PCM size halved; from lossless ~ the source size.
        AudioAction::Flac => 300_000.0 * action.output_channels(a.channels) as f64 * duration,
    };
    (bits / 8.0) as u64
}

pub fn subtitle_bytes(s: &SubtitleStream, duration: f64) -> u64 {
    s.size_bytes
        .unwrap_or_else(|| (subtitle_bitrate_guess(s, duration) as f64 * duration / 8.0) as u64)
}

// ---- Selection ----

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioChoice {
    pub index: usize,
    pub selected: bool,
    pub action: AudioAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubChoice {
    pub index: usize,
    pub selected: bool,
}

/// A language group as shown in the track picker.
#[derive(Clone, Debug, PartialEq)]
pub struct LangGroup {
    pub family: String,
    pub name: String,
    pub audio: Vec<Ranked>,
    pub subs: Vec<Ranked>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PgsQuestion {
    pub family: String,
    pub name: String,
    /// Image subtitle tracks that would be selected on "Keep".
    pub indices: Vec<usize>,
}

/// Track choices for a job, in output order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    pub audio: Vec<AudioChoice>,
    pub subs: Vec<SubChoice>,
}

impl Selection {
    pub fn selected_audio(&self) -> impl Iterator<Item = &AudioChoice> {
        self.audio.iter().filter(|c| c.selected)
    }

    pub fn selected_subs(&self) -> impl Iterator<Item = &SubChoice> {
        self.subs.iter().filter(|c| c.selected)
    }

    pub fn audio_bytes(&self, p: &ProbeInfo) -> u64 {
        self.selected_audio()
            .filter_map(|c| {
                p.audio_by_index(c.index)
                    .map(|a| output_audio_bytes(a, c.action, p.duration))
            })
            .sum()
    }

    pub fn subtitle_bytes(&self, p: &ProbeInfo) -> u64 {
        self.selected_subs()
            .filter_map(|c| {
                p.subtitle_by_index(c.index)
                    .map(|s| subtitle_bytes(s, p.duration))
            })
            .sum()
    }

    pub fn set_pgs(&mut self, q: &PgsQuestion, keep: bool) {
        for c in self
            .subs
            .iter_mut()
            .filter(|c| q.indices.contains(&c.index))
        {
            c.selected = keep;
        }
    }

    /// Re-derive the default action of every audio track (after the Apple TV
    /// setting or fallback changes).
    pub fn reset_actions(&mut self, p: &ProbeInfo, s: &EncodeSettings) {
        for c in &mut self.audio {
            if let Some(a) = p.audio_by_index(c.index) {
                c.action = default_action(a, s);
            }
        }
    }
}

/// Resolve "original" and Norwegian variants into ordered, unique families.
pub fn resolve_preferences(p: &ProbeInfo, prefs: &[String]) -> Vec<String> {
    let original = p
        .audio
        .first()
        .and_then(|a| a.language.as_deref())
        .map(family);
    let mut out: Vec<String> = Vec::new();
    for code in prefs {
        let fam = if code == ORIGINAL {
            original.clone()
        } else {
            Some(family(code))
        };
        if let Some(f) = fam
            && !out.contains(&f)
        {
            out.push(f);
        }
    }
    out
}

pub fn language_groups(p: &ProbeInfo, prefs: &[String], apple: bool) -> Vec<LangGroup> {
    let fams = resolve_preferences(p, prefs);
    let fam_of = |l: &Option<String>| l.as_deref().map(family).unwrap_or_default();
    let mut order: Vec<String> = Vec::new();
    for f in p
        .audio
        .iter()
        .map(|a| fam_of(&a.language))
        .chain(p.subtitles.iter().map(|s| fam_of(&s.language)))
    {
        if !order.contains(&f) {
            order.push(f);
        }
    }
    order.sort_by_key(|f| fams.iter().position(|x| x == f).unwrap_or(usize::MAX));
    order
        .into_iter()
        .map(|f| {
            let audio: Vec<&AudioStream> = p
                .audio
                .iter()
                .filter(|a| fam_of(&a.language) == f)
                .collect();
            let subs: Vec<&SubtitleStream> = p
                .subtitles
                .iter()
                .filter(|s| fam_of(&s.language) == f)
                .collect();
            LangGroup {
                name: family_name(&f),
                audio: rank_audio(&audio, apple, p.duration),
                subs: rank_subtitles(&subs),
                family: f,
            }
        })
        .collect()
}

/// Default selection: top audio per preferred language, full + forced subtitles,
/// SRT over PGS. Languages with only image subtitles come back as questions.
pub fn auto_select(
    p: &ProbeInfo,
    prefs: &[String],
    s: &EncodeSettings,
) -> (Selection, Vec<LangGroup>, Vec<PgsQuestion>) {
    let fams = resolve_preferences(p, prefs);
    let groups = language_groups(p, prefs, s.apple_tv_audio);
    let mut sel = Selection::default();
    let mut questions = Vec::new();
    for g in &groups {
        let preferred = fams.contains(&g.family);
        for (rank, r) in g.audio.iter().enumerate() {
            let a = p.audio_by_index(r.index).expect("ranked from probe");
            sel.audio.push(AudioChoice {
                index: r.index,
                selected: preferred && rank == 0,
                action: default_action(a, s),
            });
        }
        let mut chosen: Vec<usize> = Vec::new();
        let mut image_only: Vec<usize> = Vec::new();
        if preferred {
            let subs: Vec<&SubtitleStream> = g
                .subs
                .iter()
                .filter_map(|r| p.subtitle_by_index(r.index))
                .collect();
            // Full subtitles, falling back to SDH when there are no plain ones.
            let full_kind = if subs.iter().any(|s| sub_kind(s) == SubKind::Full) {
                SubKind::Full
            } else {
                SubKind::Sdh
            };
            for kind in [full_kind, SubKind::Forced] {
                let of_kind: Vec<&&SubtitleStream> =
                    subs.iter().filter(|s| sub_kind(s) == kind).collect();
                // Ranked order already puts text before image within a kind.
                if let Some(best) = of_kind.first() {
                    if is_text_subtitle(&best.codec) {
                        chosen.push(best.index);
                    } else {
                        image_only.push(best.index);
                    }
                }
            }
        }
        if !image_only.is_empty() {
            questions.push(PgsQuestion {
                family: g.family.clone(),
                name: g.name.clone(),
                indices: image_only,
            });
        }
        for r in &g.subs {
            sel.subs.push(SubChoice {
                index: r.index,
                selected: chosen.contains(&r.index),
            });
        }
    }
    // Nothing in a preferred language: keep the best track of the first group.
    if !sel.audio.iter().any(|c| c.selected)
        && let Some(first) = sel.audio.first_mut()
    {
        first.selected = true;
    }
    (sel, groups, questions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe::{Disposition, tests::sample_4k};

    fn a(index: usize, codec: &str, ch: u32, lang: &str) -> AudioStream {
        AudioStream {
            index,
            codec: codec.into(),
            channels: ch,
            language: Some(lang.into()),
            bit_rate: Some(448_000),
            ..Default::default()
        }
    }

    fn prefs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn language_names_and_families() {
        assert_eq!(language_name(Some("nob")), "Norwegian Bokmål");
        assert_eq!(language_name(Some("ger")), "German");
        assert_eq!(language_name(Some("und")), "Unknown");
        assert_eq!(language_name(None), "Unknown");
        assert_eq!(language_name(Some("xyz")), "xyz");
        assert_eq!(family("nb"), "nor");
        assert_eq!(family("fre"), "fra");
        let (codes, unknown) =
            parse_language_list("Norwegian, English, jpn, original, klingon, eng");
        assert_eq!(codes, prefs(&["nor", "eng", "jpn", "original"]));
        assert_eq!(unknown, vec!["klingon".to_string()]);
    }

    #[test]
    fn commentary_and_description_detection() {
        let mut t = a(1, "ac3", 2, "eng");
        t.title = Some("Commentary by the director".into());
        assert!(is_commentary(&t));
        t.title = Some("Kommentarspor".into());
        assert!(is_commentary(&t));
        t.title = Some("Director's Cut · DTS-HD MA 7.1".into());
        assert!(!is_commentary(&t));
        t.title = Some("Director's commentary".into());
        assert!(is_commentary(&t));
        t.title = Some("English AD".into());
        assert!(!is_commentary(&t) && is_audio_description(&t));
        t.title = Some("Made in Heaven".into());
        assert!(!is_audio_description(&t));
        t.title = None;
        t.disposition = Disposition {
            comment: true,
            ..Default::default()
        };
        assert!(is_commentary(&t));
    }

    #[test]
    fn ranking_order_of_criteria() {
        let mut comm = a(1, "truehd", 8, "eng");
        comm.title = Some("Director's commentary".into());
        let mut def = a(2, "ac3", 6, "eng");
        def.disposition.default = true;
        let more_ch = a(3, "dts", 8, "eng");
        let r = rank_audio(&[&comm, &def, &more_ch], false, 100.0);
        assert_eq!(r.iter().map(|r| r.index).collect::<Vec<_>>(), vec![2, 3, 1]);
        assert!(r[0].reasons.contains(&"default".to_string()));

        // Same channels: codec quality, then bitrate, then stream order.
        let thd = a(4, "truehd", 6, "eng");
        let ac3 = a(5, "ac3", 6, "eng");
        let mut ac3_hi = a(6, "ac3", 6, "eng");
        ac3_hi.bit_rate = Some(640_000);
        let ac3_dup = a(7, "ac3", 6, "eng");
        let r = rank_audio(&[&ac3, &thd, &ac3_dup, &ac3_hi], false, 100.0);
        assert_eq!(
            r.iter().map(|r| r.index).collect::<Vec<_>>(),
            vec![4, 6, 5, 7]
        );
    }

    #[test]
    fn apple_mode_prefers_playable_equal_track() {
        let thd = a(1, "truehd", 6, "eng");
        let eac3 = a(2, "eac3", 6, "eng");
        // Without Apple mode the lossless track wins on codec quality.
        assert_eq!(rank_audio(&[&thd, &eac3], false, 100.0)[0].index, 1);
        assert_eq!(rank_audio(&[&thd, &eac3], true, 100.0)[0].index, 2);
        // E-AC-3 Atmos beats a default TrueHD Atmos 7.1 in Apple mode.
        let p = sample_4k();
        let eng: Vec<&AudioStream> = p
            .audio
            .iter()
            .filter(|a| a.language.as_deref() == Some("eng"))
            .collect();
        assert_eq!(rank_audio(&eng, true, p.duration)[0].index, 2);
        assert_eq!(rank_audio(&eng, false, p.duration)[0].index, 1);
    }

    #[test]
    fn subtitle_ranking() {
        let p = sample_4k();
        let eng: Vec<&SubtitleStream> = p
            .subtitles
            .iter()
            .filter(|s| s.language.as_deref() == Some("eng"))
            .collect();
        let r: Vec<usize> = rank_subtitles(&eng).iter().map(|r| r.index).collect();
        // full SRT, full PGS, SDH, forced
        assert_eq!(r, vec![6, 7, 10, 9]);
    }

    #[test]
    fn auto_selection_on_sample() {
        let p = sample_4k();
        let s = EncodeSettings::default();
        let (sel, groups, questions) = auto_select(&p, &prefs(&["nor", "eng", "original"]), &s);
        // Groups in preference order, unpreferred last.
        assert_eq!(
            groups.iter().map(|g| g.family.as_str()).collect::<Vec<_>>(),
            vec!["nor", "eng", "jpn"]
        );
        let picked: Vec<usize> = sel.selected_audio().map(|c| c.index).collect();
        // Norwegian DTS first (becomes default), then English E-AC-3 Atmos.
        assert_eq!(picked, vec![4, 2]);
        // DTS is converted for Apple TV, E-AC-3 copied.
        assert_eq!(sel.audio[0].action, AudioAction::Eac3 { kbps: 640 });
        assert_eq!(
            sel.audio.iter().find(|c| c.index == 2).unwrap().action,
            AudioAction::Copy
        );
        // English full SRT + forced SRT; full PGS deselected.
        let subs: Vec<usize> = sel.selected_subs().map(|c| c.index).collect();
        assert_eq!(subs, vec![6, 9]);
        // Norwegian only has PGS: ask.
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].family, "nor");
        assert_eq!(questions[0].indices, vec![8]);
        let mut sel2 = sel.clone();
        sel2.set_pgs(&questions[0], true);
        assert!(sel2.subs.iter().find(|c| c.index == 8).unwrap().selected);
    }

    #[test]
    fn original_language_and_fallback_selection() {
        let mut p = sample_4k();
        p.audio.retain(|a| a.language.as_deref() == Some("jpn"));
        let (sel, _, _) = auto_select(&p, &prefs(&["original"]), &EncodeSettings::default());
        assert_eq!(
            sel.selected_audio().map(|c| c.index).collect::<Vec<_>>(),
            vec![5]
        );
        // No preferred language present: the first group's best track is kept.
        let (sel, _, _) = auto_select(&p, &prefs(&["fra"]), &EncodeSettings::default());
        assert_eq!(sel.selected_audio().count(), 1);
    }

    #[test]
    fn sdh_used_when_no_full_subtitles() {
        let mut p = sample_4k();
        p.subtitles.retain(|s| s.index == 10 || s.index == 9);
        let (sel, _, q) = auto_select(&p, &prefs(&["eng"]), &EncodeSettings::default());
        assert_eq!(
            sel.selected_subs().map(|c| c.index).collect::<Vec<_>>(),
            vec![10, 9]
        );
        assert!(q.is_empty());
    }

    #[test]
    fn default_actions_and_notes() {
        let s = EncodeSettings::default();
        let p = sample_4k();
        let thd = &p.audio[0];
        assert_eq!(default_action(thd, &s), AudioAction::Eac3 { kbps: 640 });
        assert_eq!(
            conversion_note(thd, AudioAction::Eac3 { kbps: 640 }).unwrap(),
            "TrueHD 7.1 Atmos → E-AC-3 5.1 (Atmos and 2 channels lost; Apple TV can't play TrueHD)"
        );
        let mut st = a(9, "dts", 2, "eng");
        st.profile = "DTS".into();
        assert_eq!(
            default_action(&st, &s),
            AudioAction::Aac {
                kbps: 224,
                stereo: true
            }
        );
        assert_eq!(
            conversion_note(&st, default_action(&st, &s)).unwrap(),
            "DTS 2.0 → AAC 2.0 (Apple TV can't play DTS)"
        );
        let mut aac = s.clone();
        aac.multichannel_fallback = MultichannelFallback::Aac;
        assert_eq!(
            default_action(thd, &aac),
            AudioAction::Aac {
                kbps: 512,
                stereo: false
            }
        );
        let mut off = s.clone();
        off.apple_tv_audio = false;
        assert_eq!(default_action(thd, &off), AudioAction::Copy);
        assert_eq!(default_action(&p.audio[1], &s), AudioAction::Copy);
        assert!(conversion_note(&p.audio[1], AudioAction::Copy).is_none());

        let mut down = off.clone();
        down.stereo_downmix = true;
        let stereo = AudioAction::Aac {
            kbps: 224,
            stereo: true,
        };
        assert_eq!(default_action(thd, &down), stereo);
        assert_eq!(default_action(&p.audio[1], &down), stereo);
        assert_eq!(default_action(&st, &down), AudioAction::Copy);
        assert_eq!(
            conversion_note(thd, stereo).unwrap(),
            "TrueHD 7.1 Atmos → AAC 2.0 (Atmos and 6 channels lost; Apple TV can't play TrueHD)"
        );
    }

    #[test]
    fn audio_sizes() {
        let p = sample_4k();
        assert_eq!(
            output_audio_bytes(&p.audio[0], AudioAction::Copy, p.duration),
            4_050_000_000
        );
        assert_eq!(
            output_audio_bytes(&p.audio[0], AudioAction::Eac3 { kbps: 640 }, p.duration),
            576_000_000
        );
    }
}
