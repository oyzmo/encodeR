//! "Media info": the probed file as labelled sections, for display and copying.

use crate::ffmpeg::estimate::format_bytes;
use crate::ffmpeg::probe::{Disposition, ProbeInfo};
use crate::tracks;
use crate::ui::row::duration_text;

pub struct Section {
    pub title: String,
    pub fields: Vec<(String, String)>,
}

impl Section {
    fn new(title: impl Into<String>) -> Self {
        Section {
            title: title.into(),
            fields: Vec::new(),
        }
    }

    fn add(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.trim().is_empty() {
            self.fields.push((key.into(), value));
        }
    }
}

fn bitrate(b: Option<u64>) -> String {
    match b {
        Some(b) if b >= 1_000_000 => format!("{:.1} Mb/s", b as f64 / 1e6),
        Some(b) if b > 0 => format!("{} kb/s", b / 1000),
        _ => String::new(),
    }
}

fn flags(d: &Disposition) -> String {
    let mut f = Vec::new();
    for (on, name) in [
        (d.default, "default"),
        (d.forced, "forced"),
        (d.hearing_impaired, "SDH"),
        (d.comment, "commentary"),
        (d.visual_impaired || d.descriptions, "audio description"),
        (d.original, "original"),
        (d.dub, "dub"),
    ] {
        if on {
            f.push(name);
        }
    }
    f.join(", ")
}

fn language(code: Option<&str>) -> String {
    let name = tracks::language_name(code);
    match code {
        Some(c) if !c.is_empty() && c != "und" => format!("{name} ({c})"),
        _ => name,
    }
}

fn aspect(w: u32, h: u32) -> String {
    if h == 0 {
        return String::new();
    }
    format!("{:.2}:1", w as f64 / h as f64)
}

/// Sections: General, Video, one per audio and subtitle track, Attachments.
pub fn sections(p: &ProbeInfo) -> Vec<Section> {
    let mut out = Vec::new();

    let mut g = Section::new("General");
    g.add("File", p.path.display().to_string());
    g.add("Container", p.format_name.clone());
    g.add("Duration", duration_text(p.duration));
    g.add("Size", format_bytes(p.size_bytes));
    g.add(
        "Overall bitrate",
        bitrate(p.bit_rate.or_else(|| {
            (p.duration > 0.0).then(|| (p.size_bytes as f64 * 8.0 / p.duration) as u64)
        })),
    );
    g.add(
        "Streams",
        format!(
            "{} video, {} audio, {} subtitle",
            usize::from(p.video.is_some()),
            p.audio.len(),
            p.subtitles.len()
        ),
    );
    out.push(g);

    if let Some(v) = &p.video {
        let mut s = Section::new(format!("Video · stream {}", v.index));
        s.add(
            "Codec",
            format!("{} {}", v.codec.to_uppercase(), v.profile).trim(),
        );
        s.add(
            "Resolution",
            format!("{}×{} ({})", v.width, v.height, aspect(v.width, v.height)),
        );
        let fps = format!("{:.3} fps", v.fps).replace(".000", "");
        s.add(
            "Frame rate",
            if v.fps_rational.is_empty() || v.fps_rational.ends_with("/1") {
                fps
            } else {
                format!("{fps} ({})", v.fps_rational)
            },
        );
        s.add("Bit depth", format!("{}-bit ({})", v.bit_depth, v.pix_fmt));
        s.add("Bitrate", bitrate(p.video_bit_rate()));
        if let Some(b) = v.size_bytes {
            s.add("Stream size", format_bytes(b));
        }
        if let Some(n) = v.frame_count {
            s.add("Frames", n.to_string());
        }
        if let Some(f) = &v.field_order {
            s.add(
                "Scan",
                if f == "progressive" {
                    "progressive".into()
                } else {
                    format!("interlaced ({f})")
                },
            );
        }
        let colour: Vec<&str> = [
            &v.color_primaries,
            &v.color_transfer,
            &v.color_space,
            &v.color_range,
        ]
        .into_iter()
        .filter_map(|c| c.as_deref())
        .collect();
        s.add("Colour", colour.join(" · "));
        let h = &p.hdr;
        s.add("HDR", h.badge().unwrap_or_else(|| "SDR".into()));
        if let Some(m) = &h.mastering {
            s.add(
                "Mastering display",
                format!("{}–{} cd/m²", m.min_luminance, m.max_luminance),
            );
        }
        if let Some((cll, fall)) = h.cll {
            s.add("MaxCLL / MaxFALL", format!("{cll} / {fall} cd/m²"));
        }
        if let Some(d) = h.dovi {
            s.add(
                "Dolby Vision",
                format!(
                    "profile {}, level {}, compatibility {}{}",
                    d.profile,
                    d.level,
                    d.compat_id,
                    if d.el_present {
                        ", enhancement layer"
                    } else {
                        ""
                    }
                ),
            );
        }
        out.push(s);
    }

    for (n, a) in p.audio.iter().enumerate() {
        let mut s = Section::new(format!("Audio {} · stream {}", n + 1, a.index));
        s.add("Language", language(a.language.as_deref()));
        s.add("Title", a.title.clone().unwrap_or_default());
        s.add("Format", tracks::audio_label(a));
        s.add(
            "Codec",
            format!("{} {}", a.codec, a.profile).trim().to_string(),
        );
        s.add(
            "Channels",
            format!("{} ({})", a.channels, a.layout).replace(" ()", ""),
        );
        s.add("Bitrate", bitrate(a.bit_rate));
        if let Some(b) = a.size_bytes {
            s.add("Stream size", format_bytes(b));
        }
        s.add("Flags", flags(&a.disposition));
        s.add(
            "Apple TV",
            if tracks::apple_compatible(&a.codec) {
                "plays natively"
            } else {
                "can't play natively"
            },
        );
        out.push(s);
    }

    for (n, t) in p.subtitles.iter().enumerate() {
        let mut s = Section::new(format!("Subtitle {} · stream {}", n + 1, t.index));
        s.add("Language", language(t.language.as_deref()));
        s.add("Title", t.title.clone().unwrap_or_default());
        s.add(
            "Format",
            format!(
                "{} ({})",
                tracks::codec_name(&t.codec, ""),
                if tracks::is_text_subtitle(&t.codec) {
                    "text"
                } else {
                    "image"
                }
            ),
        );
        if let Some(b) = t.size_bytes {
            s.add("Stream size", format_bytes(b));
        }
        s.add("Flags", flags(&t.disposition));
        out.push(s);
    }

    if p.attachments > 0 || !p.covers.is_empty() {
        let mut s = Section::new("Attachments");
        if !p.covers.is_empty() {
            s.add(
                "Cover art",
                p.covers
                    .iter()
                    .map(|c| c.file_name())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        if p.attachments > 0 {
            s.add("Attachments", p.attachments.to_string());
        }
        out.push(s);
    }
    out
}

/// Plain text for the clipboard.
pub fn to_text(sections: &[Section]) -> String {
    let width = sections
        .iter()
        .flat_map(|s| s.fields.iter().map(|(k, _)| k.chars().count()))
        .max()
        .unwrap_or(0);
    sections
        .iter()
        .map(|s| {
            let mut t = format!("{}\n", s.title);
            for (k, v) in &s.fields {
                t.push_str(&format!("  {k:width$}  {v}\n"));
            }
            t
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe::tests::sample_4k;

    fn field<'a>(s: &'a [Section], title: &str, key: &str) -> Option<&'a str> {
        s.iter()
            .find(|x| x.title.starts_with(title))?
            .fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn sections_cover_every_stream() {
        let p = sample_4k();
        let s = sections(&p);
        assert_eq!(s[0].title, "General");
        assert_eq!(
            field(&s, "General", "Size"),
            Some(format_bytes(60_000_000_000)).as_deref()
        );
        assert_eq!(
            field(&s, "General", "Streams"),
            Some("1 video, 5 audio, 5 subtitle")
        );
        assert!(
            field(&s, "Video", "Resolution")
                .unwrap()
                .starts_with("3840×2160")
        );
        assert_eq!(field(&s, "Video", "Bitrate"), Some("58.0 Mb/s"));
        assert_eq!(
            s.iter().filter(|x| x.title.starts_with("Audio ")).count(),
            5
        );
        assert_eq!(
            s.iter()
                .filter(|x| x.title.starts_with("Subtitle "))
                .count(),
            5
        );
        assert!(
            field(&s, "Audio 3", "Flags")
                .unwrap()
                .contains("commentary")
        );
        assert!(field(&s, "Audio 4", "Language").unwrap().contains("(nob)"));
        assert_eq!(field(&s, "Subtitle 5", "Flags"), Some("SDH"));
    }

    #[test]
    fn empty_values_are_left_out() {
        let s = sections(&sample_4k());
        assert!(
            s.iter()
                .all(|x| x.fields.iter().all(|(_, v)| !v.trim().is_empty()))
        );
    }

    #[test]
    fn text_has_aligned_fields() {
        let t = to_text(&sections(&sample_4k()));
        assert!(t.starts_with("General\n  "));
        assert!(t.contains("\nVideo · stream 0\n"));
    }
}
