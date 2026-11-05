//! Encode settings and app preferences, plus JSON persistence in
//! `$XDG_CONFIG_HOME/encoder/config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::presets::Preset;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    /// Remux only: the video stream is copied untouched.
    Copy,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 4] = [Self::Hevc, Self::Av1, Self::H264, Self::Copy];

    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC (H.265)",
            Self::Av1 => "AV1",
            Self::Copy => "Copy (remux only)",
        }
    }

    pub fn encoder(self) -> Option<&'static str> {
        match self {
            Self::H264 => Some("h264_vaapi"),
            Self::Hevc => Some("hevc_vaapi"),
            Self::Av1 => Some("av1_vaapi"),
            Self::Copy => None,
        }
    }

    /// The ffprobe codec name this encoder produces.
    pub fn probe_name(self) -> Option<&'static str> {
        match self {
            Self::H264 => Some("h264"),
            Self::Hevc => Some("hevc"),
            Self::Av1 => Some("av1"),
            Self::Copy => None,
        }
    }

    pub fn supports_10bit(self) -> bool {
        matches!(self, Self::Hevc | Self::Av1)
    }

    /// Default constant-quality value. H.264/HEVC use QP (0–51); AV1 uses a
    /// 0–63 scale like libaom/SVT-AV1 CRF, mapped to qindex by `av1_qindex`.
    pub fn default_qp(self) -> u32 {
        match self {
            Self::H264 => 23,
            Self::Hevc | Self::Copy => 25,
            Self::Av1 => 28,
        }
    }

    pub fn max_qp(self) -> u32 {
        match self {
            Self::Av1 => 63,
            _ => 51,
        }
    }

    /// Relative efficiency, higher = smaller at the same quality.
    pub fn generation(name: &str) -> u8 {
        match name {
            "mpeg2video" | "mpeg4" | "msmpeg4v3" | "vc1" | "wmv3" => 0,
            "h264" => 1,
            "vp9" | "hevc" => 2,
            "av1" => 3,
            _ => 1,
        }
    }
}

/// AV1 VA-API takes a qindex (0–255); the UI shows a 0–63 scale.
pub fn av1_qindex(q: u32) -> u32 {
    (q * 4).min(255)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizeMode {
    TargetSize,
    ConstantQuality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RateControl {
    /// VBR for target size, CQP for constant quality.
    Auto,
    Cqp,
    Vbr,
    Cbr,
    Qvbr,
}

impl RateControl {
    pub const ALL: [RateControl; 5] = [Self::Auto, Self::Cqp, Self::Vbr, Self::Cbr, Self::Qvbr];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Cqp => "CQP",
            Self::Vbr => "VBR",
            Self::Cbr => "CBR",
            Self::Qvbr => "QVBR",
        }
    }

    /// ffmpeg's `-rc_mode` value.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Auto | Self::Cqp => "CQP",
            Self::Vbr => "VBR",
            Self::Cbr => "CBR",
            Self::Qvbr => "QVBR",
        }
    }

    pub fn uses_bitrate(self) -> bool {
        matches!(self, Self::Vbr | Self::Cbr | Self::Qvbr)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Container {
    Mkv,
    Mp4,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mkv => "mkv",
            Self::Mp4 => "mp4",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Downscale {
    Keep,
    P2160,
    P1440,
    P1080,
    P720,
}

impl Downscale {
    pub const ALL: [Downscale; 5] = [
        Self::Keep,
        Self::P2160,
        Self::P1440,
        Self::P1080,
        Self::P720,
    ];

    pub fn height(self) -> Option<u32> {
        match self {
            Self::Keep => None,
            Self::P2160 => Some(2160),
            Self::P1440 => Some(1440),
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Keep => "Keep original",
            Self::P2160 => "2160p (4K)",
            Self::P1440 => "1440p",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
        }
    }

    /// Options that actually downscale a source of this height (never upscale).
    pub fn options_for(source_height: u32) -> Vec<Downscale> {
        Self::ALL
            .into_iter()
            .filter(|d| d.height().is_none_or(|h| h < source_height))
            .collect()
    }
}

/// What an Apple-incompatible multichannel track (TrueHD, DTS…) becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MultichannelFallback {
    Eac3,
    Aac,
    Flac,
}

impl MultichannelFallback {
    pub const ALL: [MultichannelFallback; 3] = [Self::Eac3, Self::Aac, Self::Flac];

    pub fn label(self) -> &'static str {
        match self {
            Self::Eac3 => "E-AC-3 5.1 · 640 kb/s",
            Self::Aac => "AAC 7.1 · all channels",
            Self::Flac => "FLAC 7.1 · lossless",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodeSettings {
    pub codec: VideoCodec,
    pub size_mode: SizeMode,
    pub target_gb: f64,
    pub qp: u32,
    pub ten_bit: bool,
    pub downscale: Downscale,
    pub container: Container,
    pub apple_tv_audio: bool,
    pub multichannel_fallback: MultichannelFallback,
    /// Fold every multichannel track to AAC 2.0.
    pub stereo_downmix: bool,
    pub rate_control: RateControl,
    /// Target bitrate for manual VBR/CBR/QVBR.
    pub bitrate_kbps: u32,
    /// 0 = automatic (1.5 × target).
    pub maxrate_kbps: u32,
    pub compression_level: Option<u32>,
    /// Keyframe interval in frames; `None` = about 2 s of frames.
    pub gop: Option<u32>,
    pub bframes: Option<u32>,
    pub keep_metadata: bool,
    pub extra_args: String,
    /// Deinterlace to the field rate (50p/60p) instead of the frame rate.
    pub deinterlace_double_rate: bool,
}

impl Default for EncodeSettings {
    /// The "Shrink for library" preset.
    fn default() -> Self {
        Self {
            codec: VideoCodec::Hevc,
            size_mode: SizeMode::TargetSize,
            target_gb: 4.0,
            qp: VideoCodec::Hevc.default_qp(),
            ten_bit: true,
            downscale: Downscale::Keep,
            container: Container::Mkv,
            apple_tv_audio: true,
            multichannel_fallback: MultichannelFallback::Eac3,
            stereo_downmix: false,
            rate_control: RateControl::Auto,
            bitrate_kbps: 8000,
            maxrate_kbps: 0,
            compression_level: None,
            gop: None,
            bframes: None,
            keep_metadata: true,
            extra_args: String::new(),
            deinterlace_double_rate: true,
        }
    }
}

impl EncodeSettings {
    /// The rate-control mode ffmpeg will actually get.
    pub fn effective_rate_control(&self) -> RateControl {
        match (self.rate_control, self.size_mode) {
            (RateControl::Auto, SizeMode::TargetSize) => RateControl::Vbr,
            (RateControl::Auto, SizeMode::ConstantQuality) => RateControl::Cqp,
            (rc, _) => rc,
        }
    }

    /// True when the video bitrate comes from the target-size maths.
    pub fn bitrate_from_target(&self) -> bool {
        self.size_mode == SizeMode::TargetSize
            && matches!(self.rate_control, RateControl::Auto | RateControl::Vbr)
    }

    pub fn output_ten_bit(&self) -> bool {
        self.ten_bit && self.codec.supports_10bit()
    }

    /// Stable key for caching sample estimates: only the settings that change the
    /// video samples. They are encoded video-only, as MKV, without extra arguments,
    /// so audio, container, metadata and extra-argument edits reuse the cache.
    /// The target size counts through the resolved bitrate (see `estimate_key`).
    pub fn video_cache_key(&self) -> String {
        let s = EncodeSettings {
            target_gb: 0.0,
            container: Container::Mkv,
            apple_tv_audio: false,
            multichannel_fallback: MultichannelFallback::Eac3,
            stereo_downmix: false,
            keep_metadata: false,
            extra_args: String::new(),
            ..self.clone()
        };
        serde_json::to_string(&s).unwrap_or_default()
    }
}

/// App-wide preferences, remembered between runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// `None` = next to the input file.
    pub output_dir: Option<PathBuf>,
    pub suffix: String,
    /// `None` = auto-detect (first AMD render node).
    pub device: Option<String>,
    /// ISO 639-2 codes in priority order; "original" = first audio track's language.
    pub preferred_languages: Vec<String>,
    pub vmaf_after_encode: bool,
    pub last_settings: EncodeSettings,
    pub last_preset: String,
    pub user_presets: Vec<Preset>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            output_dir: None,
            suffix: "-reencoded".into(),
            device: None,
            preferred_languages: vec!["nor".into(), "eng".into(), "original".into()],
            vmaf_after_encode: false,
            last_settings: EncodeSettings::default(),
            last_preset: crate::presets::DEFAULT_PRESET.into(),
            user_presets: Vec::new(),
        }
    }
}

/// `$ENCODER_CONFIG_DIR` wins (Flatpak overrides `XDG_CONFIG_HOME`, so test
/// scripts need their own variable), then `$XDG_CONFIG_HOME/encoder`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ENCODER_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return dir;
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("encoder")
}

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("encoder")
}

impl Prefs {
    pub fn load() -> Self {
        let path = config_dir().join("config.json");
        // Settings saved before the rename to encodeR.
        let old = config_dir().with_file_name("reencoder").join("config.json");
        std::fs::read_to_string(&path)
            .or_else(|_| std::fs::read_to_string(old))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let dir = config_dir();
        let Ok(json) = serde_json::to_string_pretty(self) else {
            return;
        };
        let tmp = dir.join("config.json.tmp");
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("config.json"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_rate_control_follows_size_mode() {
        let mut s = EncodeSettings::default();
        assert_eq!(s.effective_rate_control(), RateControl::Vbr);
        s.size_mode = SizeMode::ConstantQuality;
        assert_eq!(s.effective_rate_control(), RateControl::Cqp);
        s.rate_control = RateControl::Qvbr;
        assert_eq!(s.effective_rate_control(), RateControl::Qvbr);
    }

    #[test]
    fn downscale_never_offers_upscale() {
        let opts = Downscale::options_for(1080);
        assert_eq!(opts, vec![Downscale::Keep, Downscale::P720]);
        assert_eq!(Downscale::options_for(2160).len(), 4);
    }

    #[test]
    fn prefs_roundtrip_with_missing_fields() {
        let p: Prefs = serde_json::from_str(r#"{"suffix":"-x"}"#).unwrap();
        assert_eq!(p.suffix, "-x");
        assert_eq!(p.last_settings.codec, VideoCodec::Hevc);
    }

    #[test]
    fn partial_user_presets_parse() {
        // The shape scripts/hw-verify.sh writes.
        let p: Prefs = serde_json::from_str(
            r#"{ "user_presets": [ { "name": "T", "settings": { "codec": "H264", "size_mode": "ConstantQuality", "qp": 23, "ten_bit": false, "downscale": "P1080" } } ] }"#,
        )
        .unwrap();
        let s = &p.user_presets[0].settings;
        assert_eq!(
            (s.codec, s.downscale, s.qp),
            (VideoCodec::H264, Downscale::P1080, 23)
        );
        assert!(s.apple_tv_audio);
    }

    #[test]
    fn av1_qindex_is_clamped() {
        assert_eq!(av1_qindex(28), 112);
        assert_eq!(av1_qindex(63), 252);
        assert_eq!(av1_qindex(100), 255);
    }
}
