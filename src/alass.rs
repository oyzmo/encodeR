//! alass, the subtitle sync tool: finding it, and the one-click install of
//! the official static Linux build into encodeR's own data folder (no root,
//! nothing added to PATH). The Flatpak bundles the same binary.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::ffmpeg::runner;

const URL: &str = "https://github.com/kaegi/alass/releases/download/v2.0.0/alass-linux64";
/// sha256 of `alass-linux64` v2.0.0; a download that doesn't match is thrown away.
const SHA256: &str = "7bd0b9ae7e035d3ba940eacffb21243614df36231d47f21f0b4ce42001ab7fcd";
/// Name prefix of a download in progress: `alass.part-<pid>`.
const PART: &str = "alass.part-";

/// Where the one-click install puts it: `~/.local/share/encoder/bin/alass`.
pub fn installed_path() -> PathBuf {
    crate::settings::data_dir().join("bin").join("alass")
}

/// The alass program to run: on PATH (`alass`, or `alass-cli` from `cargo
/// install`), else the one installed by the app, else `~/.local/bin` or
/// `~/.cargo/bin`, which an app started from GNOME often doesn't have on PATH.
/// Only files that may be run count, so a download without `chmod +x` still
/// gets the "isn't installed" check and its help.
pub fn find() -> Option<String> {
    if let Some(p) = ["alass", "alass-cli"]
        .into_iter()
        .find(|p| runner::which(p))
    {
        return Some(p.into());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    first_executable(fallbacks(&installed_path(), home.as_deref()))
}

/// The places looked at after PATH, in order.
fn fallbacks(installed: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![installed.to_path_buf()];
    if let Some(h) = home {
        v.push(h.join(".local/bin/alass"));
        v.push(h.join(".cargo/bin/alass-cli"));
        v.push(h.join(".cargo/bin/alass"));
    }
    v
}

fn first_executable(candidates: Vec<PathBuf>) -> Option<String> {
    candidates
        .into_iter()
        .find(|p| runner::is_executable(p))
        .map(|p| p.to_string_lossy().into_owned())
}

/// Can the app install it? Only the x86-64 build exists, and inside a
/// Flatpak it's bundled (and the sandbox has no network). Worked out once.
pub fn installable() -> bool {
    static INSTALLABLE: OnceLock<bool> = OnceLock::new();
    *INSTALLABLE.get_or_init(|| {
        cfg!(all(target_os = "linux", target_arch = "x86_64"))
            && !Path::new("/.flatpak-info").exists()
            && runner::which("curl")
            && runner::which("sha256sum")
    })
}

/// Is `sha256sum` output (read from stdin: `<hash>  -`) the pinned hash?
fn hash_matches(sha256sum_output: &str) -> bool {
    sha256sum_output
        .split_whitespace()
        .next()
        .is_some_and(|h| h.eq_ignore_ascii_case(SHA256))
}

/// Downloads left by an earlier run that quit halfway (its process is gone).
fn stale_parts(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let pid: u32 = name.strip_prefix(PART)?.parse().ok()?;
            let gone = pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists();
            gone.then(|| e.path())
        })
        .collect()
}

/// Download, verify and install alass; returns the path to run. Blocking:
/// call from a worker thread.
pub fn install() -> Result<String, String> {
    let dest = installed_path();
    let dir = dest.parent().unwrap_or(&dest).to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    for p in stale_parts(&dir) {
        let _ = std::fs::remove_file(p);
    }
    let part = dir.join(format!("{PART}{}", std::process::id()));
    let result = (|| {
        // -q: ignore ~/.curlrc. HTTPS only, redirects included.
        let out = std::process::Command::new("curl")
            .args([
                "-q",
                "-fsSL",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--max-time",
                "300",
                "-o",
            ])
            .arg(&part)
            .arg(URL)
            .output()
            .map_err(|e| format!("Couldn't run curl: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "Download failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // The file goes in on stdin, so odd characters in its path can't
        // change the output format.
        let input = std::fs::File::open(&part).map_err(|e| e.to_string())?;
        let sum = std::process::Command::new("sha256sum")
            .stdin(input)
            .output()
            .map_err(|e| format!("Couldn't run sha256sum: {e}"))?;
        if !hash_matches(&String::from_utf8_lossy(&sum.stdout)) {
            return Err(
                "The download doesn't match the expected checksum, so it wasn't installed".into(),
            );
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
        Ok(dest.to_string_lossy().into_owned())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_is_compared_exactly() {
        assert!(hash_matches(&format!("{SHA256}  -\n")));
        assert!(hash_matches(&SHA256.to_uppercase()));
        assert!(!hash_matches(&format!("0{}  -", &SHA256[1..])));
        assert!(!hash_matches(""));
        assert!(installed_path().ends_with("encoder/bin/alass"));
    }

    #[test]
    fn found_in_order_and_only_when_executable() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("encoder-test-alass-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let installed = d.join("data/bin/alass");
        let list = fallbacks(&installed, Some(&d));
        assert_eq!(list[0], installed);
        assert_eq!(list[2], d.join(".cargo/bin/alass-cli"));
        assert_eq!(first_executable(list.clone()), None);
        let make = |p: &Path, mode| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        make(&list[2], 0o755);
        make(&installed, 0o644); // downloaded, but not made executable
        assert_eq!(
            first_executable(list.clone()),
            Some(list[2].to_string_lossy().into())
        );
        make(&installed, 0o755);
        assert_eq!(
            first_executable(list),
            Some(installed.to_string_lossy().into())
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stale_downloads_are_found() {
        let d = std::env::temp_dir().join(format!("encoder-test-part-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let own = d.join(format!("{PART}{}", std::process::id()));
        let dead = d.join(format!("{PART}{}", u32::MAX));
        for p in [&own, &dead, &d.join("alass")] {
            std::fs::write(p, b"x").unwrap();
        }
        assert_eq!(stale_parts(&d), vec![dead]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
