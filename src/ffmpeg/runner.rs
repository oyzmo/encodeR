//! Child-process helpers: explicit argv, never a shell. Everything here blocks
//! and is meant to run on a worker thread.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::progress::{Progress, ProgressParser};

/// Shared cancel flag.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Counts running background helpers (estimates, VMAF, analysis) so quitting
/// can wait for them to kill their ffmpeg instead of leaving it orphaned.
#[derive(Clone, Default)]
pub struct Helpers(Arc<AtomicUsize>);

/// Held by a helper thread for as long as it may have a child process running.
pub struct HelperGuard(Arc<AtomicUsize>);

impl Helpers {
    /// Take on the main thread before spawning, so a quit can't miss the helper.
    pub fn enter(&self) -> HelperGuard {
        self.0.fetch_add(1, Ordering::SeqCst);
        HelperGuard(self.0.clone())
    }

    /// Wait until every helper has finished, or `timeout` has passed.
    pub fn wait_idle(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.0.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for HelperGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Caps how many background helpers run at once, so dropping 30 files doesn't
/// start 30 ffprobes, 30 analyses and 30 sample encodes side by side.
pub struct Limiter {
    used: Mutex<usize>,
    freed: std::sync::Condvar,
    max: usize,
}

/// Probing is light: a few at a time.
pub static PROBES: Limiter = Limiter::new(4);
/// Crop/interlace analysis, sample encodes and VMAF: CPU/GPU heavy.
pub static HEAVY: Limiter = Limiter::new(2);

pub struct Slot(&'static Limiter);

impl Limiter {
    pub const fn new(max: usize) -> Self {
        Self {
            used: Mutex::new(0),
            freed: std::sync::Condvar::new(),
            max,
        }
    }

    /// Wait for a free slot; `None` if `cancel` fires while waiting.
    pub fn acquire(&'static self, cancel: Option<&Cancel>) -> Option<Slot> {
        let mut used = self.used.lock().unwrap();
        while *used >= self.max {
            if cancel.is_some_and(Cancel::is_cancelled) {
                return None;
            }
            used = self
                .freed
                .wait_timeout(used, Duration::from_millis(100))
                .unwrap()
                .0;
        }
        if cancel.is_some_and(Cancel::is_cancelled) {
            return None;
        }
        *used += 1;
        Some(Slot(self))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() -= 1;
        self.0.freed.notify_one();
    }
}

pub const STDERR_LINES: usize = 20;

/// A `Command` whose child gets SIGTERM if this process dies (logout, crash,
/// `kill`), so an encode never carries on as an orphan. The signal follows the
/// spawning thread, which here always waits for its child.
fn command(program: &str) -> Command {
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn prctl(option: i32, ...) -> i32;
    }
    const PR_SET_PDEATHSIG: i32 = 1;
    const SIGTERM: u64 = 15;
    let mut c = Command::new(program);
    // SAFETY: prctl is async-signal-safe and touches no memory of ours.
    unsafe {
        c.pre_exec(|| {
            prctl(PR_SET_PDEATHSIG, SIGTERM);
            Ok(())
        });
    }
    c
}

/// Lines of a byte stream, decoding invalid UTF-8 lossily: ffmpeg prints
/// Latin-1 stream titles and file names as raw bytes, and a strict reader
/// would stop at the first one.
fn lossy_lines(r: impl Read) -> impl Iterator<Item = String> {
    BufReader::new(r)
        .split(b'\n')
        .map_while(Result::ok)
        .map(|b| {
            let s = String::from_utf8_lossy(&b);
            s.strip_suffix('\r').unwrap_or(&s).to_string()
        })
}

#[derive(Debug, Default)]
pub struct RunResult {
    pub success: bool,
    pub cancelled: bool,
    pub stderr_tail: Vec<String>,
    pub error: Option<String>,
}

fn spawn_tail(
    stderr: impl Read + Send + 'static,
    keep: usize,
) -> (Arc<Mutex<VecDeque<String>>>, thread::JoinHandle<()>) {
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let t = tail.clone();
    let h = thread::spawn(move || {
        for line in lossy_lines(stderr) {
            let mut q = t.lock().unwrap();
            if q.len() == keep {
                q.pop_front();
            }
            q.push_back(line);
        }
    });
    (tail, h)
}

/// Ask ffmpeg to stop with `q`, then kill it if it hasn't exited in 4 s.
fn stop(child: &mut Child, stdin: &mut Option<ChildStdin>) -> Option<ExitStatus> {
    if let Some(mut s) = stdin.take() {
        let _ = s.write_all(b"q\n");
        let _ = s.flush();
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(st);
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    child.wait().ok()
}

/// Run ffmpeg with `-progress pipe:1`, reporting each progress block.
pub fn run_ffmpeg(
    program: &str,
    args: &[OsString],
    cancel: &Cancel,
    mut on_progress: impl FnMut(Progress),
) -> RunResult {
    let mut child = match command(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return RunResult {
                error: Some(format!("Could not start {program}: {e}")),
                ..Default::default()
            };
        }
    };
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped");
    let (tail, tail_h) = spawn_tail(child.stderr.take().expect("piped"), STDERR_LINES * 2);
    let (tx, rx) = mpsc::channel::<String>();
    let out_h = thread::spawn(move || {
        for line in lossy_lines(stdout) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut parser = ProgressParser::default();
    let mut cancelled = false;
    let status = loop {
        if cancel.is_cancelled() {
            cancelled = true;
            break stop(&mut child, &mut stdin);
        }
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(line) => {
                if let Some(p) = parser.feed(&line) {
                    on_progress(p);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(st)) = child.try_wait() {
                    break Some(st);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break child.wait().ok(),
        }
    };
    let _ = out_h.join();
    let _ = tail_h.join();
    let lines: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
    RunResult {
        success: !cancelled && status.is_some_and(|s| s.success()),
        cancelled,
        stderr_tail: lines,
        error: None,
    }
}

#[derive(Debug, Default)]
pub struct Captured {
    pub success: bool,
    pub cancelled: bool,
    pub stdout: String,
    pub stderr: String,
}

fn read_all(r: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut b = Vec::new();
        let _ = BufReader::new(r).read_to_end(&mut b);
        String::from_utf8_lossy(&b).into_owned()
    })
}

fn wait_cancellable(child: &mut Child, cancel: &Cancel) -> (Option<ExitStatus>, bool) {
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            return (child.wait().ok(), true);
        }
        match child.try_wait() {
            Ok(Some(st)) => return (Some(st), false),
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => return (None, false),
        }
    }
}

/// Run a short helper command, capturing output; killed on cancel.
pub fn run_capture(program: &str, args: &[OsString], cancel: &Cancel) -> Result<Captured, String> {
    let mut child = command(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start {program}: {e}"))?;
    let out = read_all(child.stdout.take().expect("piped"));
    let err = read_all(child.stderr.take().expect("piped"));
    let (status, cancelled) = wait_cancellable(&mut child, cancel);
    Ok(Captured {
        success: !cancelled && status.is_some_and(|s| s.success()),
        cancelled,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// `a | b` without a shell: a's stdout feeds b's stdin.
pub fn run_pipe(
    a: (&str, &[OsString]),
    b: (&str, &[OsString]),
    cancel: &Cancel,
) -> Result<Captured, String> {
    let mut first = command(a.0)
        .args(a.1)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not start {}: {e}", a.0))?;
    let pipe = first.stdout.take().expect("piped");
    let mut second = match command(b.0)
        .args(b.1)
        .stdin(Stdio::from(pipe))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = first.kill();
            let _ = first.wait();
            return Err(format!("Could not start {}: {e}", b.0));
        }
    };
    let err1 = read_all(first.stderr.take().expect("piped"));
    let out2 = read_all(second.stdout.take().expect("piped"));
    let err2 = read_all(second.stderr.take().expect("piped"));
    let (st2, cancelled) = wait_cancellable(&mut second, cancel);
    if cancelled {
        let _ = first.kill();
    }
    let st1 = first.wait().ok();
    Ok(Captured {
        success: !cancelled && st1.is_some_and(|s| s.success()) && st2.is_some_and(|s| s.success()),
        cancelled,
        stdout: out2.join().unwrap_or_default(),
        stderr: format!(
            "{}{}",
            err1.join().unwrap_or_default(),
            err2.join().unwrap_or_default()
        ),
    })
}

/// The line of ffmpeg's stderr that names the root cause, without the
/// `[component @ 0x…]` prefix; generic follow-up lines are skipped.
pub fn error_summary(stderr: &str) -> String {
    const NOISE: [&str; 8] = [
        "conversion failed",
        "nothing was written",
        "terminating thread",
        "task finished with error",
        "error sending frames",
        "could not open encoder before eof",
        "error while opening encoder",
        "exiting with exit code",
    ];
    const CAUSE: [&str; 8] = [
        "error",
        "not support",
        "invalid",
        "failed",
        "unable",
        "cannot",
        "no such",
        "not found",
    ];
    let strip = |l: &str| -> String {
        let mut s = l.trim();
        while s.starts_with('[') {
            match s.find("] ") {
                Some(i) => s = &s[i + 2..],
                None => break,
            }
        }
        s.to_string()
    };
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .find(|l| {
            let low = l.to_lowercase();
            CAUSE.iter().any(|c| low.contains(c)) && !NOISE.iter().any(|n| low.contains(n))
        })
        .or(lines.last())
        .map(|l| strip(l))
        .unwrap_or_else(|| "unknown error".into())
}

/// Is `program` runnable (found on PATH or as a path)?
pub fn which(program: &str) -> bool {
    if program.contains('/') {
        return std::path::Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_caps_and_cancels() {
        static L: Limiter = Limiter::new(1);
        let a = L.acquire(None).unwrap();
        let c = Cancel::default();
        c.cancel();
        // Full and cancelled: gives up instead of waiting.
        assert!(L.acquire(Some(&c)).is_none());
        drop(a);
        assert!(L.acquire(None).is_some());
    }

    #[test]
    fn invalid_utf8_lines_survive() {
        let bytes: &[u8] = b"one\ncaf\xe9 title\r\nthree\n";
        let lines: Vec<String> = lossy_lines(bytes).collect();
        assert_eq!(lines, vec!["one", "caf\u{fffd} title", "three"]);
    }

    #[test]
    fn helpers_wait_for_running_threads() {
        let h = Helpers::default();
        let g = h.enter();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(g);
        });
        let start = Instant::now();
        h.wait_idle(Duration::from_secs(2));
        assert!(h.0.load(Ordering::SeqCst) == 0);
        assert!(start.elapsed() < Duration::from_secs(1));
        t.join().unwrap();
        // A stuck helper only delays the quit by the timeout.
        let _stuck = h.enter();
        let start = Instant::now();
        h.wait_idle(Duration::from_millis(100));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn error_summary_finds_root_cause() {
        let log = "Stream mapping:\n  Stream #0:1 -> #0:0 (h264 (native) -> hevc (hevc_vaapi))\n\
            [hevc_vaapi @ 0x562f] Hardware does not support encoding at size 320x240 (constraints: width 384-8192 height 128-4352).\n\
            [vost#0:0/hevc_vaapi @ 0x56] [enc:hevc_vaapi @ 0x56] Error while opening encoder - maybe incorrect parameters\n\
            Conversion failed!\n";
        assert_eq!(
            error_summary(log),
            "Hardware does not support encoding at size 320x240 (constraints: width 384-8192 height 128-4352)."
        );
        assert_eq!(
            error_summary("just text\nConversion failed!"),
            "Conversion failed!"
        );
        assert_eq!(error_summary(""), "unknown error");
    }
}
