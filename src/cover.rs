//! Cover art: keep, replace or remove the embedded picture, and optionally
//! save it next to the video as a poster (`Movie.jpg`, which Plex uses).
//! Pure planning here; the worker runs the extract/convert steps.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ffmpeg::probe::ProbeInfo;
use crate::settings::Container;

/// Longest side of a converted cover or poster, in pixels. Never upscaled.
pub const MAX_SIDE: u32 = 1000;

/// File types the cover picker offers (any single-frame image ffmpeg reads).
pub const IMAGE_SUFFIXES: [&str; 9] = [
    "jpg", "jpeg", "png", "webp", "avif", "bmp", "tif", "tiff", "gif",
];

/// Picture codecs the MP4 muxer takes as cover art (`covr`).
const MP4_COVER_CODECS: [&str; 3] = ["mjpeg", "png", "bmp"];

#[derive(Clone, Debug, Default, PartialEq)]
pub enum CoverAction {
    /// Keep the source's cover art (if any) as it is.
    #[default]
    Keep,
    /// Use this image instead, converted to a JPEG of at most [`MAX_SIDE`].
    Replace(PathBuf),
    /// No cover art in the output.
    Remove,
}

/// Per-job cover art choice.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CoverChoice {
    pub action: CoverAction,
    /// Centre-crop converted images to the 2:3 poster shape.
    pub poster_crop: bool,
    /// Also write the picture as `<video name>.jpg` next to the result.
    pub save_poster: bool,
}

impl CoverChoice {
    /// Does the embedded cover art of the result differ from the source's?
    pub fn changes_embedded(&self, p: &ProbeInfo) -> bool {
        match self.action {
            CoverAction::Keep => false,
            CoverAction::Replace(_) => true,
            CoverAction::Remove => !p.covers.is_empty(),
        }
    }

    /// Is there a picture to save as a poster?
    pub fn has_picture(&self, p: &ProbeInfo) -> bool {
        match self.action {
            CoverAction::Keep => !p.covers.is_empty(),
            CoverAction::Replace(_) => true,
            CoverAction::Remove => false,
        }
    }

    pub fn wants_poster(&self, p: &ProbeInfo) -> bool {
        self.save_poster && self.has_picture(p)
    }

    /// One line for the editor row.
    pub fn summary(&self, p: &ProbeInfo) -> String {
        match &self.action {
            CoverAction::Keep if p.covers.is_empty() => "None".into(),
            CoverAction::Keep => format!("From the source ({})", p.covers[0].file_name()),
            CoverAction::Replace(f) => {
                let name = f.file_name().unwrap_or_default().to_string_lossy();
                if self.poster_crop {
                    format!("{name}, cropped to 2:3")
                } else {
                    name.into_owned()
                }
            }
            CoverAction::Remove => "Removed from the output".into(),
        }
    }
}

/// A picture file to embed in the output.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbedCover {
    pub file: PathBuf,
    pub mime: String,
    /// Attachment file name in MKV.
    pub name: String,
    /// The source cover stream it was extracted from.
    pub source: Option<usize>,
}

/// What the worker has to do for a job's cover art.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// Source cover streams to copy out unchanged: (stream index, file).
    pub extract: Vec<(usize, PathBuf)>,
    /// Images to convert to a normalised JPEG: (from, to). `from` may be a
    /// file from `extract`.
    pub convert: Vec<(PathBuf, PathBuf)>,
    /// Pictures that go into the output.
    pub embed: Vec<EmbedCover>,
    /// Source cover streams not to copy (fix-tracks remuxes copy everything else).
    pub drop: Vec<usize>,
    /// Finished JPEG to save next to the result.
    pub poster: Option<PathBuf>,
    /// Source covers left out because the output can't hold them (names).
    pub skipped: Vec<String>,
}

/// Plan a job's cover art. `fix` = a fix-tracks remux, which copies every
/// source stream; an encode maps only the main video. Files go in `dir`.
pub fn plan(p: &ProbeInfo, c: &CoverChoice, container: Container, fix: bool, dir: &Path) -> Plan {
    let mut out = Plan::default();
    let all: Vec<usize> = p.covers.iter().map(|c| c.index).collect();
    let extracted =
        |n: usize, cv: &crate::ffmpeg::probe::Cover| dir.join(format!("{n}-{}", cv.file_name()));
    match &c.action {
        CoverAction::Keep => {
            // An MP4 remux copies a cover as a flagged stream already. MKV
            // needs it back as an attachment (the muxer would turn a copied
            // cover into a second video track), and an encode doesn't map it.
            if !(fix && container == Container::Mp4) {
                for (n, cv) in p.covers.iter().enumerate() {
                    // MP4 only holds JPEG, PNG and BMP covers; an MKV's GIF or
                    // TIFF attachment would stop the muxer.
                    if container == Container::Mp4 && !MP4_COVER_CODECS.contains(&cv.codec.as_str())
                    {
                        out.skipped.push(cv.file_name());
                        continue;
                    }
                    let file = extracted(n, cv);
                    out.extract.push((cv.index, file.clone()));
                    out.embed.push(EmbedCover {
                        file,
                        mime: cv.mime(),
                        name: cv.file_name(),
                        source: Some(cv.index),
                    });
                }
                if fix {
                    out.drop = all;
                }
            }
            if c.save_poster
                && let Some(first) = p.covers.first()
            {
                let file = extracted(0, first);
                if !out.extract.iter().any(|(i, _)| *i == first.index) {
                    out.extract.push((first.index, file.clone()));
                }
                let poster = dir.join("poster.jpg");
                out.convert.push((file, poster.clone()));
                out.poster = Some(poster);
            }
        }
        CoverAction::Replace(src) => {
            let file = dir.join("cover.jpg");
            out.convert.push((src.clone(), file.clone()));
            out.embed.push(EmbedCover {
                file: file.clone(),
                mime: "image/jpeg".into(),
                name: "cover.jpg".into(),
                source: None,
            });
            out.drop = all;
            if c.save_poster {
                out.poster = Some(file);
            }
        }
        CoverAction::Remove => out.drop = all,
    }
    out
}

impl Plan {
    /// Source cover `index` couldn't be extracted: an MKV remux copies it as
    /// a stream instead, an encode goes without it.
    pub fn extract_failed(&mut self, index: usize) {
        self.embed.retain(|e| e.source != Some(index));
        self.drop.retain(|i| *i != index);
        self.extract.retain(|(i, _)| *i != index);
    }
}

/// The video filter that normalises a cover: optional 2:3 centre crop, then
/// scaled down to fit [`MAX_SIDE`], 4:2:0 for players that dislike 4:4:4 JPEG.
pub fn convert_filter(crop: bool) -> String {
    let m = MAX_SIDE;
    let scale = format!(
        "scale='min({m},iw)':'min({m},ih)':force_original_aspect_ratio=decrease,format=yuvj420p"
    );
    if crop {
        format!("crop='min(iw,ih*2/3)':'min(ih,iw*3/2)',{scale}")
    } else {
        scale
    }
}

/// Convert image `from` to a normalised JPEG `to`.
pub fn convert_args(from: &Path, to: &Path, crop: bool) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["-hide_banner", "-y", "-v", "error", "-i"]
        .iter()
        .map(OsString::from)
        .collect();
    a.push(from.into());
    for s in ["-frames:v", "1", "-vf"] {
        a.push(s.into());
    }
    a.push(convert_filter(crop).into());
    for s in ["-q:v", "2", "-f", "image2", "-update", "1"] {
        a.push(s.into());
    }
    a.push(to.into());
    a
}

/// Where the poster for `video` goes: `Movie.mkv` → `Movie.jpg`.
pub fn poster_path(video: &Path) -> PathBuf {
    video.with_extension("jpg")
}

/// Is `path` an image the cover picker accepts?
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_SUFFIXES.iter().any(|s| e.eq_ignore_ascii_case(s)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::probe::Cover;

    fn probe(covers: usize) -> ProbeInfo {
        let mut p = ProbeInfo::default();
        for n in 0..covers {
            p.covers.push(Cover {
                index: 5 + n,
                codec: "mjpeg".into(),
                filename: Some(format!("c{n}.jpg")),
                mimetype: None,
            });
        }
        p
    }

    fn choice(action: CoverAction, poster: bool) -> CoverChoice {
        CoverChoice {
            action,
            poster_crop: false,
            save_poster: poster,
        }
    }

    const D: &str = "/t";

    #[test]
    fn keep_in_an_encode_extracts_and_embeds_every_cover() {
        let p = probe(2);
        let pl = plan(
            &p,
            &choice(CoverAction::Keep, false),
            Container::Mkv,
            false,
            Path::new(D),
        );
        assert_eq!(pl.extract.len(), 2);
        assert_eq!(pl.embed[1].file, PathBuf::from("/t/1-c1.jpg"));
        assert_eq!(pl.embed[1].source, Some(6));
        assert!(pl.drop.is_empty() && pl.convert.is_empty() && pl.poster.is_none());
    }

    #[test]
    fn an_mp4_encode_leaves_out_covers_it_cant_hold() {
        let mut p = probe(2);
        p.covers[1].codec = "gif".into();
        let pl = plan(
            &p,
            &choice(CoverAction::Keep, false),
            Container::Mp4,
            false,
            Path::new(D),
        );
        assert_eq!(pl.embed.len(), 1);
        assert_eq!(pl.embed[0].source, Some(5));
        assert_eq!(pl.skipped, vec!["c1.jpg".to_string()]);
        // MKV takes any picture as an attachment.
        let mkv = plan(
            &p,
            &choice(CoverAction::Keep, false),
            Container::Mkv,
            false,
            Path::new(D),
        );
        assert!(mkv.embed.len() == 2 && mkv.skipped.is_empty());
    }

    #[test]
    fn keep_in_a_remux_drops_only_for_mkv() {
        let p = probe(1);
        let mkv = plan(
            &p,
            &choice(CoverAction::Keep, false),
            Container::Mkv,
            true,
            Path::new(D),
        );
        assert_eq!((mkv.drop.clone(), mkv.embed.len()), (vec![5], 1));
        let mp4 = plan(
            &p,
            &choice(CoverAction::Keep, false),
            Container::Mp4,
            true,
            Path::new(D),
        );
        assert_eq!(mp4, Plan::default());
        // A poster from an MP4 remux still needs the cover copied out.
        let mp4p = plan(
            &p,
            &choice(CoverAction::Keep, true),
            Container::Mp4,
            true,
            Path::new(D),
        );
        assert_eq!(mp4p.extract, vec![(5, PathBuf::from("/t/0-c0.jpg"))]);
        assert!(mp4p.embed.is_empty() && mp4p.drop.is_empty());
        assert_eq!(
            mp4p.convert,
            vec![(PathBuf::from("/t/0-c0.jpg"), PathBuf::from("/t/poster.jpg"))]
        );
        assert_eq!(mp4p.poster, Some(PathBuf::from("/t/poster.jpg")));
        // MKV with a poster: extracted once, used for both.
        let mkvp = plan(
            &p,
            &choice(CoverAction::Keep, true),
            Container::Mkv,
            true,
            Path::new(D),
        );
        assert_eq!(mkvp.extract.len(), 1);
    }

    #[test]
    fn replace_converts_once_and_drops_the_old_covers() {
        let p = probe(2);
        let c = choice(CoverAction::Replace("/pics/art.png".into()), true);
        let pl = plan(&p, &c, Container::Mkv, true, Path::new(D));
        assert!(pl.extract.is_empty());
        assert_eq!(
            pl.convert,
            vec![(
                PathBuf::from("/pics/art.png"),
                PathBuf::from("/t/cover.jpg")
            )]
        );
        assert_eq!(pl.embed.len(), 1);
        assert_eq!(
            (pl.embed[0].mime.as_str(), pl.embed[0].name.as_str()),
            ("image/jpeg", "cover.jpg")
        );
        assert_eq!(pl.drop, vec![5, 6]);
        assert_eq!(pl.poster, Some(PathBuf::from("/t/cover.jpg")));
    }

    #[test]
    fn remove_drops_everything_and_has_no_poster() {
        let p = probe(1);
        let c = choice(CoverAction::Remove, true);
        let pl = plan(&p, &c, Container::Mp4, true, Path::new(D));
        assert_eq!(pl.drop, vec![5]);
        assert!(pl.embed.is_empty() && pl.poster.is_none());
        assert!(!c.wants_poster(&p) && c.changes_embedded(&p));
        // Removing nothing changes nothing.
        assert!(!c.changes_embedded(&probe(0)));
    }

    #[test]
    fn failed_extract_falls_back_to_copying_the_stream() {
        let p = probe(2);
        let mut pl = plan(
            &p,
            &choice(CoverAction::Keep, false),
            Container::Mkv,
            true,
            Path::new(D),
        );
        pl.extract_failed(5);
        assert_eq!(pl.drop, vec![6]);
        assert_eq!(pl.embed.len(), 1);
        assert_eq!(pl.embed[0].source, Some(6));
    }

    #[test]
    fn poster_and_picture_rules() {
        let none = probe(0);
        let one = probe(1);
        let keep = choice(CoverAction::Keep, true);
        assert!(!keep.wants_poster(&none) && keep.wants_poster(&one));
        assert!(!keep.changes_embedded(&one));
        let rep = choice(CoverAction::Replace("/a.jpg".into()), false);
        assert!(rep.has_picture(&none) && !rep.wants_poster(&none));
        assert_eq!(
            poster_path(Path::new("/m/Film-reencoded.mkv")),
            PathBuf::from("/m/Film-reencoded.jpg")
        );
        assert!(is_image(Path::new("/a/Poster.JPG")) && !is_image(Path::new("/a/film.mkv")));
        assert!(is_image(Path::new("/a/scan.tiff")) && is_image(Path::new("/a/a.gif")));
    }

    #[test]
    fn convert_filter_crops_to_2_3_and_never_upscales() {
        assert!(!convert_filter(false).contains("crop"));
        let f = convert_filter(true);
        assert!(f.starts_with("crop='min(iw,ih*2/3)':'min(ih,iw*3/2)',scale='min(1000,iw)'"));
        assert!(f.contains("force_original_aspect_ratio=decrease"));
        let a: Vec<String> = convert_args(Path::new("/a.png"), Path::new("/b.jpg"), true)
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(a.last().unwrap(), "/b.jpg");
        assert!(a.windows(2).any(|w| w == ["-frames:v", "1"]));
    }
}
