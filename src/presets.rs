//! Built-in and user presets.

use serde::{Deserialize, Serialize};

use crate::settings::{Downscale, EncodeSettings, SizeMode, VideoCodec};

pub const DEFAULT_PRESET: &str = "Shrink for library";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub settings: EncodeSettings,
}

pub fn builtins() -> Vec<Preset> {
    let base = EncodeSettings::default();
    vec![
        Preset {
            name: DEFAULT_PRESET.into(),
            settings: base.clone(),
        },
        Preset {
            name: "Archive HEVC 10-bit".into(),
            settings: EncodeSettings {
                size_mode: SizeMode::ConstantQuality,
                qp: 20,
                ..base.clone()
            },
        },
        Preset {
            name: "Small H.264 1080p".into(),
            settings: EncodeSettings {
                codec: VideoCodec::H264,
                ten_bit: false,
                downscale: Downscale::P1080,
                target_gb: 2.0,
                qp: VideoCodec::H264.default_qp(),
                ..base.clone()
            },
        },
        Preset {
            name: "AV1 efficient".into(),
            settings: EncodeSettings {
                codec: VideoCodec::Av1,
                size_mode: SizeMode::ConstantQuality,
                qp: VideoCodec::Av1.default_qp(),
                ..base
            },
        },
    ]
}

pub fn is_builtin(name: &str) -> bool {
    builtins().iter().any(|p| p.name == name)
}

/// Built-ins first, then the user's.
pub fn all(user: &[Preset]) -> Vec<Preset> {
    let mut v = builtins();
    v.extend(user.iter().cloned());
    v
}

pub fn find(user: &[Preset], name: &str) -> Option<Preset> {
    all(user).into_iter().find(|p| p.name == name)
}

fn check_name(user: &[Preset], name: &str, except: Option<&str>) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("The preset needs a name".into());
    }
    if is_builtin(name) {
        return Err(format!("“{name}” is a built-in preset"));
    }
    if Some(name) != except && user.iter().any(|p| p.name == name) {
        return Err(format!("A preset called “{name}” already exists"));
    }
    Ok(name.to_string())
}

/// Save (or overwrite) a user preset.
pub fn save(user: &mut Vec<Preset>, name: &str, settings: &EncodeSettings) -> Result<(), String> {
    let name = check_name(user, name, Some(name.trim()))?;
    match user.iter_mut().find(|p| p.name == name) {
        Some(p) => p.settings = settings.clone(),
        None => user.push(Preset {
            name,
            settings: settings.clone(),
        }),
    }
    Ok(())
}

pub fn rename(user: &mut [Preset], old: &str, new: &str) -> Result<(), String> {
    let new = check_name(user, new, Some(old))?;
    let p = user
        .iter_mut()
        .find(|p| p.name == old)
        .ok_or("No such preset")?;
    p.name = new;
    Ok(())
}

pub fn delete(user: &mut Vec<Preset>, name: &str) {
    user.retain(|p| p.name != name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preset_matches_spec() {
        let p = find(&[], DEFAULT_PRESET).unwrap();
        assert_eq!(p.settings, EncodeSettings::default());
        assert_eq!(p.settings.size_mode, SizeMode::TargetSize);
        assert!(p.settings.ten_bit && p.settings.apple_tv_audio);
        assert_eq!(builtins().len(), 4);
    }

    #[test]
    fn save_rename_delete() {
        let mut user = Vec::new();
        let s = EncodeSettings {
            target_gb: 3.0,
            ..Default::default()
        };
        save(&mut user, " Mine ", &s).unwrap();
        assert_eq!(user[0].name, "Mine");
        save(&mut user, "Mine", &EncodeSettings::default()).unwrap();
        assert_eq!(user.len(), 1);
        assert!(save(&mut user, DEFAULT_PRESET, &s).is_err());
        assert!(save(&mut user, "  ", &s).is_err());
        save(&mut user, "Other", &s).unwrap();
        assert!(rename(&mut user, "Other", "Mine").is_err());
        rename(&mut user, "Other", "Renamed").unwrap();
        assert!(find(&user, "Renamed").is_some());
        delete(&mut user, "Renamed");
        assert!(find(&user, "Renamed").is_none());
        assert_eq!(all(&user).len(), 5);
    }
}
