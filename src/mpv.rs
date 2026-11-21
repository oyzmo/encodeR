//! Live lip-sync tuning in mpv. The app opens the source in mpv on the track
//! being tuned and watches mpv's `audio-delay` over its JSON IPC socket, so
//! every Ctrl + / Ctrl − in mpv lands in the track's offset as it happens.
//! mpv's sign matches `AudioChoice::offset_ms` (positive = sound later).

use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Ctrl + / Ctrl − step in mpv, the same as the app's spin button arrows.
pub const STEP_MS: i64 = 50;

/// mpv's arguments: the source opened `percent` into the file (past most
/// intros) on audio track `aid` (1-based among audio tracks), with the
/// current delay already applied.
pub fn args(input: &Path, aid: usize, delay_ms: i64, socket: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        format!("--aid={aid}"),
        format!("--audio-delay={}", delay_ms as f64 / 1000.0),
        "--start=10%".into(),
        "--force-window=yes".into(),
        "--keep-open=yes".into(),
        "--title=Lip sync — Ctrl + / Ctrl − to shift the sound, close when it matches".into(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut ipc = OsString::from("--input-ipc-server=");
    ipc.push(socket);
    a.push(ipc);
    // A file name starting with "-" mustn't read as an option.
    a.push("--".into());
    a.push(input.into());
    a
}

/// Commands sent once connected: watch the delay, and rebind mpv's delay keys
/// to [`STEP_MS`] for this window only (the user's input.conf is untouched).
pub fn setup_commands() -> Vec<String> {
    let step = STEP_MS as f64 / 1000.0;
    let mut cmds = vec![json!({"command": ["observe_property", 1, "audio-delay"]}).to_string()];
    for (key, sign) in [
        ("ctrl++", 1.0),
        ("ctrl+=", 1.0),
        ("ctrl+KP_ADD", 1.0),
        ("ctrl+-", -1.0),
        ("ctrl+KP_SUBTRACT", -1.0),
    ] {
        let cmd = format!("add audio-delay {}", sign * step);
        cmds.push(json!({"command": ["keybind", key, cmd]}).to_string());
    }
    cmds
}

pub fn set_delay_command(ms: i64) -> String {
    json!({"command": ["set_property", "audio-delay", ms as f64 / 1000.0]}).to_string()
}

/// The delay in ms from an mpv `audio-delay` change event, else `None`
/// (command replies and other events).
pub fn parse_delay(line: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["event"] != "property-change" || v["name"] != "audio-delay" {
        return None;
    }
    Some((v["data"].as_f64()? * 1000.0).round() as i64)
}

pub enum Event {
    Delay(i64),
    /// mpv exited or never opened its socket.
    Closed,
}

/// A running mpv window. Dropping it closes mpv.
pub struct Session {
    child: Child,
    stream: Arc<Mutex<Option<UnixStream>>>,
    socket: PathBuf,
}

impl Session {
    /// Starts mpv; its events arrive on `tx` from a reader thread.
    pub fn start(
        input: &Path,
        aid: usize,
        delay_ms: i64,
        tx: async_channel::Sender<Event>,
    ) -> std::io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let socket = dir.join(format!("encoder-mpv-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let child = Command::new("mpv")
            .args(args(input, aid, delay_ms, &socket))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let stream = Arc::new(Mutex::new(None));
        let shared = stream.clone();
        let path = socket.clone();
        std::thread::spawn(move || {
            // mpv creates the socket shortly after starting.
            let deadline = Instant::now() + Duration::from_secs(10);
            let conn = loop {
                match UnixStream::connect(&path) {
                    Ok(s) => break Some(s),
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100))
                    }
                    Err(_) => break None,
                }
            };
            let Some(mut conn) = conn else {
                let _ = tx.send_blocking(Event::Closed);
                return;
            };
            for c in setup_commands() {
                let _ = writeln!(conn, "{c}");
            }
            if let Ok(w) = conn.try_clone() {
                *shared.lock().unwrap() = Some(w);
            }
            for line in BufReader::new(conn).lines() {
                let Ok(line) = line else { break };
                if let Some(ms) = parse_delay(&line)
                    && tx.send_blocking(Event::Delay(ms)).is_err()
                {
                    return;
                }
            }
            let _ = tx.send_blocking(Event::Closed);
        });
        Ok(Self {
            child,
            stream,
            socket,
        })
    }

    /// Moves mpv's delay to `ms` (the app's own spin button was used).
    pub fn set_delay(&self, ms: i64) {
        if let Some(s) = self.stream.lock().unwrap().as_mut() {
            let _ = writeln!(s, "{}", set_delay_command(ms));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_open_the_track_with_its_delay() {
        let a = args(
            Path::new("-film.mkv"),
            2,
            -150,
            Path::new("/run/user/1000/s.sock"),
        );
        let a: Vec<String> = a.iter().map(|s| s.to_string_lossy().into()).collect();
        assert!(a.contains(&"--aid=2".into()));
        assert!(a.contains(&"--audio-delay=-0.15".into()));
        assert!(a.contains(&"--input-ipc-server=/run/user/1000/s.sock".into()));
        assert_eq!(a[a.len() - 2..], ["--".to_string(), "-film.mkv".into()]);
    }

    #[test]
    fn parses_only_delay_changes() {
        let ev = r#"{"event":"property-change","id":1,"name":"audio-delay","data":0.15000000001}"#;
        assert_eq!(parse_delay(ev), Some(150));
        let neg = r#"{"event":"property-change","id":1,"name":"audio-delay","data":-0.05}"#;
        assert_eq!(parse_delay(neg), Some(-50));
        assert_eq!(parse_delay(r#"{"request_id":0,"error":"success"}"#), None);
        let other = r#"{"event":"property-change","id":2,"name":"volume","data":80}"#;
        assert_eq!(parse_delay(other), None);
        assert_eq!(parse_delay("not json"), None);
    }

    #[test]
    fn keys_step_like_the_spin_button() {
        let c = setup_commands();
        assert!(c[0].contains("observe_property"));
        assert!(
            c.iter()
                .any(|c| c.contains("ctrl++") && c.contains("add audio-delay 0.05"))
        );
        assert!(
            c.iter()
                .any(|c| c.contains("ctrl+-") && c.contains("add audio-delay -0.05"))
        );
        let set: Value = serde_json::from_str(&set_delay_command(-250)).unwrap();
        assert_eq!(set["command"][2], -0.25);
    }
}
