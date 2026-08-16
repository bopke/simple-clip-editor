//! Locating and running the ffmpeg/ffprobe child processes.
//!
//! Everything is a child process: a decoder crash on a malformed file kills a
//! child, not the editor with unsaved work, and cancellation is `kill()`
//! rather than shared state and callbacks.

use anyhow::{anyhow, bail, Context, Result};
use clip_core::progress::{ProgressParser, ProgressUpdate};
use crossbeam_channel::Receiver;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Override used during development, before the downloader exists on a given
/// machine: `CLIP_EDITOR_FFMPEG=/path/to/bin`.
pub const ENV_OVERRIDE: &str = "CLIP_EDITOR_FFMPEG";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub origin: Origin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Downloaded into our own data dir.
    Managed,
    /// Found on `$PATH` or picked by the user.
    System,
    /// `CLIP_EDITOR_FFMPEG`.
    Env,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Managed => "downloaded",
            Origin::System => "system",
            Origin::Env => "environment override",
        }
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Both tools in a directory, if both are actually there.
pub fn tools_in_dir(dir: &Path, origin: Origin) -> Option<Tools> {
    let ffmpeg = dir.join(exe("ffmpeg"));
    let ffprobe = dir.join(exe("ffprobe"));
    (ffmpeg.is_file() && ffprobe.is_file()).then_some(Tools {
        ffmpeg,
        ffprobe,
        origin,
    })
}

/// `$PATH` lookup — the escape hatch for offline users, corporate networks,
/// and Linux users who already have ffmpeg and don't want a second copy.
pub fn find_on_path() -> Option<Tools> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if let Some(t) = tools_in_dir(&dir, Origin::System) {
            return Some(t);
        }
    }
    None
}

pub fn from_env() -> Option<Tools> {
    let dir = std::env::var_os(ENV_OVERRIDE)?;
    let dir = PathBuf::from(dir);
    // Accept either the directory or the ffmpeg binary itself.
    if dir.is_file() {
        let parent = dir.parent()?.to_path_buf();
        return tools_in_dir(&parent, Origin::Env);
    }
    tools_in_dir(&dir, Origin::Env)
}

impl Tools {
    pub fn verify(&self) -> Result<String> {
        let out = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-version"])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", self.ffmpeg.display()))?;
        if !out.status.success() {
            bail!("{} -version failed", self.ffmpeg.display());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text.lines().next().unwrap_or_default().to_string())
    }

    /// Run ffprobe and return stdout.
    pub fn probe(&self, args: &[String]) -> Result<String> {
        let out = Command::new(&self.ffprobe)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("spawning ffprobe")?;
        if !out.status.success() {
            bail!(
                "ffprobe failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Run ffmpeg and collect its stdout as raw bytes — used for single-frame
    /// rawvideo extraction.
    pub fn run_capture_stdout(&self, args: &[String], cancel: &Cancel) -> Result<Vec<u8>> {
        let mut child = Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg")?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let id = cancel.install(child)?;

        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let mut buf = Vec::new();
        stdout
            .read_to_end(&mut buf)
            .context("reading ffmpeg output")?;
        let status = cancel.wait(id)?;
        cancel.detach(id);
        let err = err_thread.join().unwrap_or_default();
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        if !status.success() {
            bail!("ffmpeg failed: {}", err.trim());
        }
        Ok(buf)
    }

    /// Read fixed-size rawvideo frames from ffmpeg's stdout as they arrive.
    /// `on_frame` returning false stops the stream (the consumer went away).
    ///
    /// Frames are handed over by move, and `spares` is where the consumer sends
    /// buffers back when it has finished with them. This used to
    /// `on_frame(buf.clone())` — a fresh allocation and a full copy of every
    /// frame, ~100 MB/s at 854×480/60 for bytes that are read once and dropped.
    /// A consumer with nothing to return simply sends nothing and each frame
    /// costs one allocation instead of a copy.
    pub fn stream_frames(
        &self,
        args: &[String],
        cancel: &Cancel,
        frame_bytes: usize,
        spares: &Receiver<Vec<u8>>,
        mut on_frame: impl FnMut(Vec<u8>) -> bool,
    ) -> Result<()> {
        let mut child = Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning ffmpeg")?;
        let mut stdout = child.stdout.take().expect("piped");
        let id = cancel.install(child)?;

        let mut buf = take_buffer(spares, frame_bytes);
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let mut filled = 0;
            while filled < frame_bytes {
                match stdout.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => break,
                }
            }
            if filled < frame_bytes {
                break;
            }
            // `take` leaves an empty Vec behind rather than allocating one, and
            // the next buffer comes from the consumer if it has returned any.
            if !on_frame(std::mem::take(&mut buf)) {
                break;
            }
            buf = take_buffer(spares, frame_bytes);
        }
        cancel.abort();
        let _ = cancel.wait(id);
        cancel.detach(id);
        Ok(())
    }

    /// Run ffmpeg, streaming `-progress` updates to `on_progress`.
    pub fn run_with_progress(
        &self,
        args: &[String],
        cancel: &Cancel,
        mut on_progress: impl FnMut(ProgressUpdate),
    ) -> Result<()> {
        let mut child = Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg")?;
        let stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let id = cancel.install(child)?;

        // stderr must be drained concurrently or a chatty ffmpeg blocks on a
        // full pipe and the whole job deadlocks.
        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });

        let mut parser = ProgressParser::new();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(update) = parser.feed_line(&line) {
                on_progress(update);
            }
        }
        let status = cancel.wait(id)?;
        cancel.detach(id);
        let err = err_thread.join().unwrap_or_default();
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        if !status.success() {
            bail!("ffmpeg failed: {}", tail(&err));
        }
        Ok(())
    }

    /// Which hardware encoders this build actually has. Only what exists gets
    /// shown in the UI.
    pub fn available_hw_encoders(&self) -> Vec<clip_core::command::HwEncoder> {
        let Ok(out) = Command::new(&self.ffmpeg)
            .args(clip_core::command::list_encoders_args())
            .stdin(Stdio::null())
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out.stdout);
        clip_core::command::HwEncoder::ALL
            .iter()
            .copied()
            .filter(|e| text.contains(e.ffmpeg_name()))
            .collect()
    }
}

/// A buffer to decode the next frame into: a returned one if the consumer has
/// sent any back, otherwise a new one.
///
/// A recycled buffer is handed back without being cleared. Every byte of it is
/// overwritten before anyone reads it — that is what `frame_bytes` means — and
/// zeroing it first would put back the memory traffic this exists to avoid.
fn take_buffer(spares: &Receiver<Vec<u8>>, frame_bytes: usize) -> Vec<u8> {
    match spares.try_recv() {
        Ok(mut buf) => {
            // A buffer from a stream at a different resolution is the one case
            // where it has to be resized rather than trusted.
            if buf.len() != frame_bytes {
                buf.clear();
                buf.resize(frame_bytes, 0);
            }
            buf
        }
        Err(_) => vec![0u8; frame_bytes],
    }
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(4);
    lines[start..].join("; ")
}

/// First and longest gap between `try_wait` polls. See [`Cancel::wait`].
const POLL_MIN: std::time::Duration = std::time::Duration::from_millis(1);
const POLL_MAX: std::time::Duration = std::time::Duration::from_millis(15);

/// Cancel token for a job. Every job needs one — users will open the wrong
/// file and hit export.
#[derive(Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    /// Set only when a human asked to stop, so an internal abort (one failed
    /// segment stopping its siblings) does not masquerade as a cancellation
    /// and hide the real error.
    by_user: Arc<AtomicBool>,
    /// Every child started under this token. Phase A runs several segments at
    /// once on one token, so this is a map rather than a single slot — with a
    /// single slot the workers clobber each other's handles.
    children: Arc<Mutex<HashMap<u64, Child>>>,
    next_id: Arc<AtomicU64>,
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Signal cancellation and kill whatever child is running right now.
    pub fn cancel(&self) {
        self.by_user.store(true, Ordering::SeqCst);
        self.abort();
    }

    pub fn is_user_cancelled(&self) -> bool {
        self.by_user.load(Ordering::SeqCst)
    }

    /// Stop everything without claiming a human asked for it.
    pub fn abort(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.children.lock() {
            for child in guard.values_mut() {
                let _ = child.kill();
            }
        }
    }

    /// Hand a child to the token so `cancel()` can reach it. Returns the id
    /// used to wait on and release it.
    fn install(&self, mut child: Child) -> Result<u64> {
        if self.is_cancelled() {
            let _ = child.kill();
            bail!("cancelled");
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut guard = self
            .children
            .lock()
            .map_err(|_| anyhow!("cancel token poisoned"))?;
        guard.insert(id, child);
        Ok(id)
    }

    /// Wait by polling, so the lock is free between polls and `cancel()` never
    /// has to wait for the process it is trying to kill.
    ///
    /// The interval backs off rather than being fixed, because the two kinds of
    /// job here want opposite things. A single-frame decode — one per scrub
    /// position, thousands of them in a drag — finishes in a few milliseconds,
    /// and a flat 15 ms poll adds most of a frame's latency to every one of
    /// them for nothing. An export runs for minutes and does not care. Starting
    /// at [`POLL_MIN`] and doubling to [`POLL_MAX`] pays the short jobs back
    /// without spinning through the long ones.
    fn wait(&self, id: u64) -> Result<std::process::ExitStatus> {
        let mut interval = POLL_MIN;
        loop {
            {
                let mut guard = self
                    .children
                    .lock()
                    .map_err(|_| anyhow!("cancel token poisoned"))?;
                let child = guard
                    .get_mut(&id)
                    .ok_or_else(|| anyhow!("no child running"))?;
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
            }
            std::thread::sleep(interval);
            interval = (interval * 2).min(POLL_MAX);
        }
    }

    fn detach(&self, id: u64) {
        if let Ok(mut guard) = self.children.lock() {
            guard.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_accepts_a_file_or_a_dir() {
        // Only checks the shape of the logic; no ffmpeg needed.
        assert!(tools_in_dir(Path::new("/definitely/not/here"), Origin::System).is_none());
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\n\nb\nc\nd\ne"), "b; c; d; e");
    }

    /// The decoder side of the frame-buffer cycle: reuse what came back, and
    /// only allocate when there is nothing to reuse.
    #[test]
    fn a_returned_buffer_is_used_instead_of_a_new_one() {
        let (tx, rx) = crossbeam_channel::bounded::<Vec<u8>>(4);

        // Nothing returned yet: a buffer of the right size, zeroed.
        let fresh = take_buffer(&rx, 8);
        assert_eq!(fresh, vec![0u8; 8]);

        // A returned buffer is handed straight back, contents and all — every
        // byte is overwritten before it is read, and clearing it would put back
        // the memory traffic this exists to avoid.
        tx.send(vec![7u8; 8]).unwrap();
        assert_eq!(take_buffer(&rx, 8), vec![7u8; 8]);

        // One from a stream at a different resolution is the case where it has
        // to be resized rather than trusted.
        tx.send(vec![7u8; 4]).unwrap();
        assert_eq!(take_buffer(&rx, 8).len(), 8);
        tx.send(vec![7u8; 32]).unwrap();
        assert_eq!(take_buffer(&rx, 8).len(), 8);

        // A disconnected consumer is not an error; the decoder just allocates.
        drop(tx);
        assert_eq!(take_buffer(&rx, 8).len(), 8);
    }
}
